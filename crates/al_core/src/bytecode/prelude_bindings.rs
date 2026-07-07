//! Strongly-typed handles to every prelude type and constructor the compiler
//! is allowed to know about. Captured exactly once after `src/std/al.al` loads;
//! every later identity check compares against the ids here, never against a
//! string literal. If `al.al` drifts from what Rust expects, `capture` returns
//! an `Err` describing the mismatch instead of letting it surface as a confused
//! unify error downstream.

use crate::type_def::TypeId;
use crate::types::{Scheme, TypeEnv, ValueKind};

/// Prelude type and constructor names. This module is the ONLY place these
/// strings may appear in compiler-side Rust; everything else compares against
/// captured `PreludeBindings` ids. The four structural-primitive names are
/// re-exported from `type_def::prim_names` so the InferType→Type resolver
/// (which cannot depend on `bytecode/`) shares the same constants.
pub mod names {
    pub use crate::type_def::prim_names::{ARRAY, FLOAT, INT, STRING};
    pub const BOOL: &str = "Bool";
    pub const BINARY: &str = "Binary";
    pub const NIL: &str = "Nil";
    pub const OPTION: &str = "Option";
    pub const RESULT: &str = "Result";
    pub const TRUE: &str = "True";
    pub const FALSE: &str = "False";
    pub const SOME: &str = "Some";
    pub const NONE: &str = "None";
    pub const OK: &str = "Ok";
    pub const ERR: &str = "Err";
}

#[derive(Debug, Clone, Copy)]
pub struct TypeRef {
    pub id: TypeId,
    pub name: &'static str,
}

impl TypeRef {
    const fn pending(name: &'static str) -> Self {
        TypeRef {
            id: TypeId::NONE,
            name,
        }
    }

    /// Whether `id` is this prelude type. Guards on `id != NONE` so a
    /// pre-capture (all-zero) binding never falsely matches — see
    /// `PreludeBindings::default`.
    #[inline]
    pub fn is(&self, id: TypeId) -> bool {
        id != TypeId::NONE && id == self.id
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CtorRef {
    pub type_id: TypeId,
    pub variant_idx: u16,
    pub arity: u16,
}

impl CtorRef {
    const ZERO: Self = CtorRef {
        type_id: TypeId::NONE,
        variant_idx: 0,
        arity: 0,
    };
}

/// Declares every `PreludeBindings` field exactly once. Expands to the struct,
/// its `Default` (all-zero ids so it can exist before the prelude loads — see
/// `compile_impl` analysing `al.al` itself), `capture()`, and the
/// `(field_name, value)` iterators `build.rs` walks to emit the baked
/// `const PRELUDE`. Identity checks go through `TypeRef::is`, which guards on
/// `id != TypeId::NONE` so a zero binding never falsely matches.
macro_rules! prelude_bindings {
    (
        types: [ $( $tf:ident = $tn:ident ),* $(,)? ],
        ctors: [ $( $cf:ident = ($cn:ident, $of:ident, $ar:literal) ),* $(,)? ],
    ) => {
        #[derive(Debug, Clone)]
        pub struct PreludeBindings {
            $( pub $tf: TypeRef, )*
            $( pub $cf: CtorRef, )*
        }

        impl Default for PreludeBindings {
            fn default() -> Self {
                PreludeBindings {
                    $( $tf: TypeRef::pending(names::$tn), )*
                    $( $cf: CtorRef::ZERO, )*
                }
            }
        }

        impl PreludeBindings {
            pub const TYPE_NAMES: &[&str] = &[$( names::$tn ),*];

            pub fn capture(env: &TypeEnv) -> Result<Self, String> {
                let ty = |name: &'static str| -> Result<TypeRef, String> {
                    env.lookup_type_info(name)
                        .map(|ti| TypeRef { id: ti.id, name })
                        .ok_or_else(|| format!("prelude: type '{name}' is required"))
                };
                let ctor = |name: &str, of: &TypeRef, arity: u16| -> Result<CtorRef, String> {
                    match env.lookup(name) {
                        Some(Scheme {
                            kind: ValueKind::Constructor { type_id, variant_idx, arity: a, .. },
                            ..
                        }) if *type_id == of.id && *a == arity => Ok(CtorRef {
                            type_id: *type_id,
                            variant_idx: *variant_idx,
                            arity: *a,
                        }),
                        Some(_) => Err(format!(
                            "prelude: '{name}' must be a {arity}-arity constructor of '{}'",
                            of.name
                        )),
                        None => Err(format!("prelude: constructor '{name}' is required")),
                    }
                };
                $( let $tf = ty(names::$tn)?; )*
                Ok(PreludeBindings {
                    $( $cf: ctor(names::$cn, &$of, $ar)?, )*
                    $( $tf, )*
                })
            }

            pub fn type_fields(&self) -> impl Iterator<Item = (&'static str, TypeRef)> {
                [$( (stringify!($tf), self.$tf) ),*].into_iter()
            }
            pub fn ctor_fields(&self) -> impl Iterator<Item = (&'static str, CtorRef)> {
                [$( (stringify!($cf), self.$cf) ),*].into_iter()
            }
        }
    };
}

prelude_bindings! {
    types: [
        int = INT, float = FLOAT, string = STRING, bool = BOOL, array = ARRAY,
        binary = BINARY, nil = NIL, option = OPTION, result = RESULT,
    ],
    ctors: [
        true_ = (TRUE, bool, 0),
        false_ = (FALSE, bool, 0),
        nil_ctor = (NIL, nil, 0),
        some = (SOME, option, 1),
        none = (NONE, option, 0),
        ok = (OK, result, 1),
        err = (ERR, result, 1),
    ],
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ArenaSlice, new_env};

    #[test]
    fn capture_on_empty_env_reports_missing_type() {
        let env = new_env();
        let err = PreludeBindings::capture(&env).unwrap_err();
        assert!(
            err.contains("prelude: type 'Int' is required"),
            "got: {err}"
        );
    }

    /// Broken `al.al` (e.g. `True` deleted) must surface as a clean compile
    /// error, not a panic or a confused unify failure later. This test edits
    /// nothing on disk; it asserts the *shape* of the message that
    /// `register_prelude` would emit if `True` were missing.
    #[test]
    fn ctor_shape_mismatch_message() {
        // Build an env that defines every prelude type (so type lookups pass)
        // but no constructors, to exercise the second failure mode.
        let mut env = new_env();
        for n in PreludeBindings::TYPE_NAMES {
            env.register_type_head(n, 0, ArenaSlice::EMPTY, ArenaSlice::EMPTY);
        }
        let err = PreludeBindings::capture(&env).unwrap_err();
        assert_eq!(err, "prelude: constructor 'True' is required");
    }
}
