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
    /// Compare via [`TypeRef::is`], never `==`: a pre-capture binding holds
    /// `TypeId::NONE` and a bare comparison has no guard against it.
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

// Deliberately no PartialEq: like `TypeRef`, equality on a pre-capture
// (all-`TypeId::NONE`) binding would falsely match. Identity checks go
// through [`CtorRef::is`], which guards on `type_id != TypeId::NONE`.
#[derive(Debug, Clone, Copy)]
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

    /// Whether `(type_id, variant_idx)` is this prelude constructor. Guards on
    /// `type_id != NONE` so a pre-capture (all-zero) binding never falsely
    /// matches — the same guard as [`TypeRef::is`].
    #[inline]
    pub fn is(&self, type_id: TypeId, variant_idx: u16) -> bool {
        type_id != TypeId::NONE && type_id == self.type_id && variant_idx == self.variant_idx
    }
}

/// Why [`PreludeBindings::capture`] rejected the loaded prelude. Each variant
/// names the exact `al.al` drift; `Display` renders the message the compiler
/// reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreludeCaptureError {
    MissingType(&'static str),
    TypeArity {
        name: &'static str,
        expected: usize,
        found: usize,
    },
    MissingCtor(&'static str),
    CtorShape {
        name: &'static str,
        of: &'static str,
        arity: u16,
    },
}

impl std::fmt::Display for PreludeCaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PreludeCaptureError::MissingType(name) => {
                write!(f, "prelude: type '{name}' is required")
            }
            PreludeCaptureError::TypeArity {
                name,
                expected,
                found,
            } => write!(
                f,
                "prelude: type '{name}' must have arity {expected}, found {found}"
            ),
            PreludeCaptureError::MissingCtor(name) => {
                write!(f, "prelude: constructor '{name}' is required")
            }
            PreludeCaptureError::CtorShape { name, of, arity } => write!(
                f,
                "prelude: '{name}' must be a {arity}-arity constructor of '{of}'"
            ),
        }
    }
}

/// Declares every `PreludeBindings` field exactly once. Expands to the struct,
/// its `Default` (all-zero ids so it can exist before the prelude loads — see
/// `compile_impl` analysing `al.al` itself), `capture()`, and the
/// `(field_name, value)` iterators `build.rs` walks to emit the baked
/// `const PRELUDE`. Identity checks go through `TypeRef::is`, which guards on
/// `id != TypeId::NONE` so a zero binding never falsely matches.
macro_rules! prelude_bindings {
    (
        types: [ $( $tf:ident = ($tn:ident, $ta:literal) ),* $(,)? ],
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
            /// Name/arity of every prelude type binding. Exists for the
            /// capture-error tests below, which register the types without
            /// their constructors.
            #[cfg(test)]
            const TYPE_NAMES: &[(&str, usize)] = &[$( (names::$tn, $ta) ),*];

            pub fn capture(env: &TypeEnv) -> Result<Self, PreludeCaptureError> {
                let ty = |name: &'static str, expected: usize| -> Result<TypeRef, PreludeCaptureError> {
                    let ti = env
                        .lookup_type_info(name)
                        .ok_or(PreludeCaptureError::MissingType(name))?;
                    if ti.arity() != expected {
                        return Err(PreludeCaptureError::TypeArity {
                            name,
                            expected,
                            found: ti.arity(),
                        });
                    }
                    Ok(TypeRef { id: ti.id, name })
                };
                let ctor = |name: &'static str, of: &TypeRef, arity: u16| -> Result<CtorRef, PreludeCaptureError> {
                    match env.lookup(name) {
                        Some(Scheme {
                            kind: ValueKind::Constructor { type_id, variant_idx, arity: a, .. },
                            ..
                        }) if *type_id == of.id && *a == arity => Ok(CtorRef {
                            type_id: *type_id,
                            variant_idx: *variant_idx,
                            arity: *a,
                        }),
                        Some(_) => Err(PreludeCaptureError::CtorShape {
                            name,
                            of: of.name,
                            arity,
                        }),
                        None => Err(PreludeCaptureError::MissingCtor(name)),
                    }
                };
                $( let $tf = ty(names::$tn, $ta)?; )*
                Ok(PreludeBindings {
                    $( $cf: ctor(names::$cn, &$of, $ar)?, )*
                    $( $tf, )*
                })
            }

            /// Nominal ids for the four structural primitives the inference
            /// engine recognises directly. Single source of truth for
            /// `InferEngine::set_prim_ids` — adding a fifth primitive is one
            /// edit here plus one field on `PrimIds`.
            pub fn prim_ids(&self) -> crate::types::PrimIds {
                crate::types::PrimIds {
                    int: self.int.id,
                    float: self.float.id,
                    string: self.string.id,
                    array: self.array.id,
                }
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
        int = (INT, 0), float = (FLOAT, 0), string = (STRING, 0), bool = (BOOL, 0),
        array = (ARRAY, 1), binary = (BINARY, 0), nil = (NIL, 0),
        option = (OPTION, 1), result = (RESULT, 2),
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
        assert_eq!(err, PreludeCaptureError::MissingType(names::INT));
        assert_eq!(err.to_string(), "prelude: type 'Int' is required");
    }

    /// Broken `al.al` (e.g. `True` deleted) must surface as a clean compile
    /// error, not a panic or a confused unify failure later. This test edits
    /// nothing on disk; it asserts the *shape* of the message that
    /// `register_prelude` would emit if `True` were missing.
    #[test]
    fn ctor_shape_mismatch_message() {
        // Build an env that defines every prelude type with the correct arity
        // (so type lookups pass) but no constructors, to exercise the second
        // failure mode.
        let mut env = new_env();
        for &(n, arity) in PreludeBindings::TYPE_NAMES {
            env.register_type_head(n, 0, ArenaSlice::EMPTY, ArenaSlice::new(0, arity as u16));
        }
        let err = PreludeBindings::capture(&env).unwrap_err();
        assert_eq!(err, PreludeCaptureError::MissingCtor(names::TRUE));
        assert_eq!(err.to_string(), "prelude: constructor 'True' is required");
    }

    /// A prelude type declared with the wrong number of parameters (e.g.
    /// `type Array` instead of `type Array(a)`) must fail here, not surface as
    /// a confused unify error the first time an array literal is typed.
    #[test]
    fn type_arity_mismatch_message() {
        let mut env = new_env();
        for &(n, _) in PreludeBindings::TYPE_NAMES {
            env.register_type_head(n, 0, ArenaSlice::EMPTY, ArenaSlice::EMPTY);
        }
        let err = PreludeBindings::capture(&env).unwrap_err();
        assert_eq!(
            err,
            PreludeCaptureError::TypeArity {
                name: names::ARRAY,
                expected: 1,
                found: 0
            }
        );
        assert_eq!(
            err.to_string(),
            "prelude: type 'Array' must have arity 1, found 0"
        );
    }
}
