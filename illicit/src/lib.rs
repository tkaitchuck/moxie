//! Type-indexed scoped singletons, propagated through an implicit backing context.
//!
//! # Requiring references from the environment
//!
//! The `from_env` macro provides an attribute for functions that require access to a singleton in
//! their environment. Here, the contrived function requires a `u8` to add one to:
//!
//! ```
//! #[illicit::from_env(num: &u8)]
//! fn env_num_plus_one() -> u8 {
//!     num + 1
//! }
//!
//! illicit::child_env!(u8 => 1).enter(|| {
//!     assert_eq!(env_num_plus_one(), 2u8);
//! });
//! ```
//!
//! This provides convenient sugar for values stored in the current `Env` as an alternative to
//! thread-locals or a manually propagated context object. However this approach incurs a
//! significant cost in that the following code will panic without the right type having been added
//! to the environment:
//!
//! ```should_panic
//! # #[illicit::from_env(num: &u8)]
//! # fn env_num_plus_one() -> u8 {
//! #    num + 1
//! # }
//! // thread 'main' panicked at 'expected a value from the environment, found none'
//! env_num_plus_one();
//! ```
//!
//! See the attribute's documentation for more details, and please consider whether this is
//! appropriate for your use case before taking it on as a dependency.

#![forbid(unsafe_code)]
#![deny(clippy::all, missing_docs, intra_doc_link_resolution_failure)]

mod anon_rc;

use anon_rc::AnonRc;
use std::{
    any::{Any, TypeId},
    cell::RefCell,
    collections::{BTreeMap, HashMap},
    fmt::{Debug, Formatter, Result as FmtResult},
    mem::replace,
    ops::Deref,
    rc::Rc,
};

#[doc(inline)]
pub use illicit_macro::from_env;

thread_local! {
    /// The current dynamic scope.
    static CURRENT_SCOPE: RefCell<Rc<Env>> = RefCell::new(Rc::new(
        Env {
            depth: 0,
            location: (file!(), column!(), line!()),
            values: Default::default(),
        }
    ));
}

/// Declare additional environment values to expose to a child topological function's call tree.
#[macro_export]
macro_rules! child_env {
    ($($env_item_ty:ty => $env_item:expr),*) => {{
        let location = (file!(), line!(), column!());
        #[allow(unused_mut)]
        let mut _new_env = $crate::Env::unstable_new(location);
        $( _new_env.unstable_insert::<$env_item_ty>($env_item); )*
        _new_env
    }}
}

/// Immutable environment container for the current scope. Environment values can be
/// provided by parent environments, but child functions can only mutate their environment through
/// interior mutability.
///
/// The environment is type-indexed, and each `Scope` holds 0-1 references to every
/// `[std::any::Any] + 'static` type. Access is provided through read-only references.
///
/// Aside: one interesting implication of the above is the ability to define "private scoped global
/// values" which are private to functions which are nonetheless propagating the values with
/// their control flow. This can be useful for runtimes to offer themselves execution-local values
/// in functions which are invoked by external code. It can also be severely abused, like any
/// implicit state, and should be used with caution.
#[derive(Clone)]
pub struct Env {
    depth: u32,
    location: (&'static str, u32, u32),
    values: HashMap<TypeId, AnonRc>,
}

impl Env {
    #[doc(hidden)]
    pub fn unstable_new(location: (&'static str, u32, u32)) -> Self {
        let mut values = HashMap::new();
        let mut depth = 0;

        CURRENT_SCOPE.with(|current| {
            let current = current.borrow();
            depth = current.depth + 1;
            for anon in current.values.values() {
                values.insert(anon.id(), anon.clone());
            }
        });

        Self {
            values,
            depth,
            location,
        }
    }

    #[doc(hidden)]
    pub fn unstable_insert<E>(&mut self, new_item: E)
    where
        E: Debug + 'static,
    {
        let anon = AnonRc::new(new_item, self.location, self.depth);
        self.values.insert(anon.id(), anon);
    }

    /// The number of parent environments from which this environment descends.
    pub fn depth(&self) -> u32 {
        self.depth
    }

    /// TODO
    pub fn enter<R>(self, child_fn: impl FnOnce() -> R) -> R {
        let _reset_when_done_please = CURRENT_SCOPE.with(|parent| {
            let mut parent = parent.borrow_mut();
            let parent = replace(&mut *parent, Rc::new(self));

            scopeguard::guard(parent, move |prev| {
                CURRENT_SCOPE.with(|p| p.replace(prev));
            })
        });

        // call this out here so these calls can be nested
        child_fn()
    }

    /// Returns a reference to a value in the current environment if it has been added to the
    /// environment by parent/enclosing [`call`] invocations.
    pub fn get<E>() -> Option<impl Deref<Target = E> + 'static>
    where
        E: Any + 'static,
    {
        let key = TypeId::of::<E>();
        let anon = CURRENT_SCOPE.with(|current| current.borrow().values.get(&key).cloned());
        if let Some(anon) = anon {
            Some(anon.downcast_deref())
        } else {
            None
        }
    }

    /// Returns a reference to a value in the current environment, as [`Env::get`] does, but panics
    /// if the value has not been set in the environment.
    // TODO typename for debugging here would be v. nice
    pub fn expect<E>() -> impl Deref<Target = E> + 'static
    where
        E: Any + 'static,
    {
        if let Some(val) = Self::get() {
            val
        } else {
            panic!(
                "expected a `{}` from the environment, did not find it in current env: {:#?}",
                std::any::type_name::<E>(),
                Env::snapshot(),
            )
        }
    }

    /// Returns a snapshot of the current dynamic scope. Most useful for debugging the contained
    /// `Env`.
    pub fn snapshot() -> EnvSnapshot {
        let mut stacked = EnvSnapshot {
            by_depth: BTreeMap::new(),
        };
        CURRENT_SCOPE.with(|e| {
            for anon in e.borrow().values.values() {
                stacked
                    .by_depth
                    .entry(anon.depth())
                    .or_insert_with(|| Env {
                        values: HashMap::new(),
                        depth: anon.depth(),
                        location: anon.location(), // depth -> location is 1:1
                    })
                    .values
                    .insert(anon.id(), anon.clone());
            }
        });
        stacked
    }

    /// Removes the provided type from the current environment for the remainder of its scope.
    /// Parent environments may still possess a reference to the value.
    pub fn hide<E: 'static>() {
        CURRENT_SCOPE.with(|current| {
            let mut env = current.borrow_mut();
            let mut without_e = env.values.clone();
            let excluded_ty = TypeId::of::<E>();
            without_e.retain(|ty, _| ty != &excluded_ty);
            *env = Rc::new(Env {
                values: without_e,
                depth: env.depth,
                location: env.location,
            });
        })
    }
}

impl Debug for Env {
    fn fmt(&self, f: &mut Formatter) -> FmtResult {
        let (file, line, col) = self.location;
        let env_w_loc = format!("Env @ {}:{}:{}", file, line, col);

        let mut f = f.debug_struct(&env_w_loc);
        for (ty, anon) in self
            .values
            .values()
            .map(|v| (v.ty(), v))
            .collect::<BTreeMap<_, _>>()
        {
            f.field(ty, anon.debug());
        }
        f.finish()
    }
}

/// An alternative representation of the current scope's environment, optimized for debug printing.
pub struct EnvSnapshot {
    by_depth: BTreeMap<u32, Env>,
}

impl Debug for EnvSnapshot {
    fn fmt(&self, f: &mut Formatter) -> FmtResult {
        f.debug_list().entries(self.by_depth.values()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_debug_looks_right() {
        let current_scope = format!("{:?}", Env::snapshot());
        assert_eq!(
            "[]", &current_scope,
            "environment should be empty and validly formatted"
        );

        child_env!(u8 => 42).enter(|| {
            let u8_present_fragment =
                format!("Env @ {}:{}:{} {{ u8: 42 }}", file!(), line!() - 2, 9);
            let expected = format!("[{}]", &u8_present_fragment);
            let current_scope = format!("{:?}", Env::snapshot());
            assert_eq!(
                expected, current_scope,
                "environment should have a u8 in it with value printed"
            );

            child_env!(String => String::from("owo")).enter(|| {
                let string_present_fragment = format!(
                    "Env @ {}:{}:{} {{ alloc::string::String: \"owo\" }}",
                    file!(),
                    line!() - 4,
                    13
                );
                let expected = format!("[{}, {}]", &u8_present_fragment, &string_present_fragment);
                let current_scope = format!("{:?}", Env::snapshot());
                assert_eq!(
                    expected, current_scope,
                    "environment should have both a u8 and a String in it, with values printed"
                );
            });
        });
    }

    #[test]
    fn child_env_replaces_parent_env_values() {
        let mut first_called = false;
        let mut second_called = false;

        assert!(Env::get::<u8>().is_none());
        child_env!(u8 => 0u8).enter(|| {
            let curr_byte = *Env::expect::<u8>();
            assert_eq!(curr_byte, 0);
            first_called = true;

            child_env!(u8 => 1u8).enter(|| {
                let curr_byte = *Env::expect::<u8>();
                assert_eq!(curr_byte, 1);
                second_called = true;
            });

            assert!(second_called);
            assert_eq!(curr_byte, 0);
        });
        assert!(first_called);
        assert!(Env::get::<u8>().is_none());
    }

    #[test]
    fn child_sees_parent_env() {
        assert!(Env::get::<u8>().is_none());
        child_env!(u8 => 0u8).enter(|| {
            let curr_byte = *Env::expect::<u8>();
            assert_eq!(curr_byte, 0);

            child_env!(u16 => 1u16).enter(|| {
                let curr_byte = *Env::expect::<u8>();
                assert_eq!(curr_byte, 0, "must see u8 from enclosing environment");

                let curr_uh_twobyte = *Env::expect::<u16>();
                assert_eq!(curr_uh_twobyte, 1, "must see locally installed u16");
            });

            assert_eq!(curr_byte, 0, "must see 0");
        });
        assert!(Env::get::<u8>().is_none());
    }

    #[test]
    fn removing_from_env() {
        assert!(Env::get::<u8>().is_none());

        child_env!(u8 => 2).enter(|| {
            assert_eq!(*Env::expect::<u8>(), 2, "just added 2u8");

            child_env!().enter(|| {
                assert_eq!(*Env::expect::<u8>(), 2, "parent added 2u8");
                Env::hide::<u8>();
                assert!(Env::get::<u8>().is_none(), "just removed u8 from Env");
            });

            assert_eq!(
                *Env::get::<u8>().unwrap(),
                2,
                "returned to parent Env with 2u8"
            );

            Env::hide::<u8>();
            assert!(Env::get::<u8>().is_none(), "just removed u8 from Env");
        })
    }
}
