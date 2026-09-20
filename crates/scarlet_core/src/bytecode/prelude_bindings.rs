//! Handles to every prelude type and constructor the compiler may know about.
//! Captured once after `src/std/scrl.scrl` loads; later identity checks compare
//! ids here, never string literals. Drift in `scrl.scrl` becomes a `capture` error
//! rather than a confused unify failure downstream.

use crate::type_def::TypeId;
use crate::types::{Scheme, TypeEnv, ValueKind};

/// Prelude type and constructor names. The ONLY place these strings may appear
/// in compiler-side Rust. The four primitive names come from
/// `type_def::prim_names`, which the InferType→Type resolver also uses.
pub mod names {
    pub use crate::type_def::prim_names::{ARRAY, FLOAT, INT, STRING};
    pub(crate) const BOOL: &str = "Bool";
    pub(crate) const BINARY: &str = "Binary";
    pub(crate) const MAP: &str = "Map";
    pub(crate) const NIL: &str = "Nil";
    pub(crate) const OPTION: &str = "Option";
    pub(crate) const RESULT: &str = "Result";
    pub(crate) const TRUE: &str = "True";
    pub(crate) const FALSE: &str = "False";
    pub(crate) const SOME: &str = "Some";
    pub(crate) const NONE: &str = "None";
    pub(crate) const OK: &str = "Ok";
    pub(crate) const ERR: &str = "Err";
}

#[derive(Debug, Clone, Copy)]
pub struct TypeRef {
    /// Compare via [`TypeRef::is`], never `==`: a pre-capture binding holds
    /// `TypeId::NONE`, which `==` would match.
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
    /// pre-capture binding never falsely matches.
    #[inline]
    pub(crate) fn is(&self, id: TypeId) -> bool {
        id != TypeId::NONE && id == self.id
    }

    /// Whether this binding has been resolved to a real type. A late binding
    /// reads `false` until its defining module loads.
    #[inline]
    pub(crate) fn is_bound(&self) -> bool {
        self.id != TypeId::NONE
    }
}

// No PartialEq on purpose: equality on a pre-capture binding would match.
// Identity goes through `CtorRef::is`.
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
}

/// Why [`PreludeBindings::capture`] rejected the loaded prelude.
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
        expected_arity: u16,
        found: CtorFound,
    },
}

/// What `capture` found bound to a constructor name, so the diagnostic can
/// report the drift and not just the expectation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CtorFound {
    /// The name is bound, but not to a constructor (e.g. shadowed by a `let`).
    NotAConstructor,
    /// A constructor of the wrong owner type and/or arity.
    Constructor { of: TypeId, arity: u16 },
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
            PreludeCaptureError::CtorShape {
                name,
                of,
                expected_arity,
                found,
            } => {
                write!(
                    f,
                    "prelude: '{name}' must be a {expected_arity}-arity constructor of '{of}'; "
                )?;
                match found {
                    CtorFound::NotAConstructor => write!(f, "found a non-constructor binding"),
                    CtorFound::Constructor { of, arity } => {
                        write!(f, "found a constructor of type #{of} with arity {arity}")
                    }
                }
            }
        }
    }
}

/// Declares every `PreludeBindings` field once. Expands to the struct, its
/// all-zero `Default` (needed while `scrl.scrl` itself is being analysed),
/// `capture()`, and the field iterators `build.rs` walks to bake `const
/// PRELUDE`.
macro_rules! prelude_bindings {
    (
        types: [ $( $tf:ident = ($tn:ident, $ta:literal) ),* $(,)? ],
        late_types: [ $( $lf:ident = ($ln:ident, $lm:expr, $la:literal) ),* $(,)? ],
        ctors: [ $( $cf:ident = ($cn:ident, $of:ident, $ar:literal) ),* $(,)? ],
    ) => {
        #[derive(Debug, Clone)]
        pub struct PreludeBindings {
            $( $tf: TypeRef, )*
            $( $lf: TypeRef, )*
            $( $cf: CtorRef, )*
        }

        impl Default for PreludeBindings {
            fn default() -> Self {
                PreludeBindings {
                    $( $tf: TypeRef::pending(names::$tn), )*
                    $( $lf: TypeRef::pending(names::$ln), )*
                    $( $cf: CtorRef::ZERO, )*
                }
            }
        }

        impl PreludeBindings {
            /// Name/arity of every prelude type binding, for the tests below.
            #[cfg(test)]
            const TYPE_NAMES: &[(&str, usize)] = &[$( (names::$tn, $ta) ),*];

            /// Each late binding as (module path, type name, arity), for the
            /// tests below — so a new late type is covered without editing them.
            #[cfg(test)]
            const LATE_TYPES: &[(&str, &'static str, usize)] =
                &[$( ($lm, names::$ln, $la) ),*];

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
                        }) => {
                            if *type_id == of.id && *a == arity {
                                Ok(CtorRef {
                                    type_id: *type_id,
                                    variant_idx: *variant_idx,
                                    arity: *a,
                                })
                            } else {
                                Err(PreludeCaptureError::CtorShape {
                                    name,
                                    of: of.name,
                                    expected_arity: arity,
                                    found: CtorFound::Constructor { of: *type_id, arity: *a },
                                })
                            }
                        }
                        Some(_) => Err(PreludeCaptureError::CtorShape {
                            name,
                            of: of.name,
                            expected_arity: arity,
                            found: CtorFound::NotAConstructor,
                        }),
                        None => Err(PreludeCaptureError::MissingCtor(name)),
                    }
                };
                $( let $tf = ty(names::$tn, $ta)?; )*
                Ok(PreludeBindings {
                    $( $cf: ctor(names::$cn, &$of, $ar)?, )*
                    $( $tf, )*
                    // Late bindings stay pending here on purpose: their
                    // defining module has not loaded yet. See `bind_late`.
                    $( $lf: TypeRef::pending(names::$ln), )*
                })
            }

            /// Record the id of a late-bound type, given the interface of the
            /// module that defines it. `module` is the loaded module's key;
            /// bindings for other modules are left alone, so this is safe to
            /// call after every load.
            ///
            /// Identity comes from the defining module's own interface, never
            /// a by-name lookup in the ambient env — a user type called `Map`
            /// must not bind here.
            pub(crate) fn bind_late(
                &mut self,
                module: &str,
                lookup: impl Fn(&str) -> Option<(TypeId, usize)>,
            ) {
                $(
                    if module == $lm && !self.$lf.is_bound() {
                        if let Some((id, arity)) = lookup(names::$ln) {
                            if arity == $la {
                                self.$lf = TypeRef { id, name: names::$ln };
                            }
                        }
                    }
                )*
            }

            /// Nominal ids for the four structural primitives the inference
            /// engine recognises directly.
            pub fn prim_ids(&self) -> crate::types::PrimIds {
                crate::types::PrimIds {
                    int: self.int.id,
                    float: self.float.id,
                    string: self.string.id,
                    array: self.array.id,
                }
            }

            $(
                #[inline]
                pub const fn $tf(&self) -> TypeRef {
                    self.$tf
                }
            )*
            $(
                #[inline]
                pub const fn $lf(&self) -> TypeRef {
                    self.$lf
                }
            )*
            $(
                #[inline]
                pub const fn $cf(&self) -> CtorRef {
                    self.$cf
                }
            )*

            /// Positional constructor for the build-script codegen, which
            /// cannot name private fields from another crate. Parameter order
            /// is declaration order, the same order `type_fields` and
            /// `ctor_fields` iterate, so `emit_prelude` and this signature are
            /// generated from the same list and cannot drift.
            #[doc(hidden)]
            #[allow(clippy::too_many_arguments)] // positional by design; see above
            pub const fn baked($( $tf: TypeRef, )* $( $lf: TypeRef, )* $( $cf: CtorRef, )*) -> Self {
                PreludeBindings {
                    $( $tf, )*
                    $( $lf, )*
                    $( $cf, )*
                }
            }

            pub fn type_fields(&self) -> impl Iterator<Item = (&'static str, TypeRef)> {
                [$( (stringify!($tf), self.$tf) ),* $(, (stringify!($lf), self.$lf) )*].into_iter()
            }
            pub fn ctor_fields(&self) -> impl Iterator<Item = (&'static str, CtorRef)> {
                [$( (stringify!($cf), self.$cf) ),*].into_iter()
            }
        }
    };
}

impl PreludeBindings {}

prelude_bindings! {
    types: [
        int = (INT, 0), float = (FLOAT, 0), string = (STRING, 0), bool = (BOOL, 0),
        array = (ARRAY, 1), binary = (BINARY, 0), nil = (NIL, 0),
        option = (OPTION, 1), result = (RESULT, 2),
    ],
    // Bodiless stdlib types the compiler must recognise by id, defined outside
    // the prelude. `capture` cannot see them: it runs in `register_prelude`,
    // before any stdlib module loads. Bound by `bind_late` when their module
    // does load, which is also the first moment a value of the type can exist.
    late_types: [
        map = (MAP, "scarlet/map", 2),
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
    use crate::types::{ArenaSlice, StrId, Ty, new_env};

    #[test]
    fn capture_on_empty_env_reports_missing_type() {
        let env = new_env();
        let err = PreludeBindings::capture(&env).unwrap_err();
        assert_eq!(err, PreludeCaptureError::MissingType(names::INT));
        assert_eq!(err.to_string(), "prelude: type 'Int' is required");
    }

    /// Every prelude type at the right arity, but no constructors.
    fn env_with_prelude_types() -> (crate::types::TypeEnv, TypeId) {
        let mut env = new_env();
        let mut bool_id = TypeId::NONE;
        for &(n, arity) in PreludeBindings::TYPE_NAMES {
            let id = env.register_type_head(
                n,
                StrId(0),
                ArenaSlice::EMPTY,
                ArenaSlice::new(0, arity as u16),
            );
            if n == names::BOOL {
                bool_id = id;
            }
        }
        (env, bool_id)
    }

    /// Every prelude constructor at its declared shape, so `capture` reaches
    /// the late-type question instead of failing on a missing one.
    fn define_all_prelude_ctors(env: &mut crate::types::TypeEnv, bool_id: TypeId) {
        let id = |env: &crate::types::TypeEnv, n: &str| {
            #[allow(clippy::expect_used)]
            env.lookup_type_info(n).expect("registered above").id
        };
        let (nil, option, result) = (
            id(env, names::NIL),
            id(env, names::OPTION),
            id(env, names::RESULT),
        );
        for (name, type_id, variant_idx, arity) in [
            (names::TRUE, bool_id, 0, 0),
            (names::FALSE, bool_id, 1, 0),
            (names::NIL, nil, 0, 0),
            (names::SOME, option, 0, 1),
            (names::NONE, option, 1, 0),
            (names::OK, result, 0, 1),
            (names::ERR, result, 1, 1),
        ] {
            define_ctor(env, name, type_id, variant_idx, arity);
        }
    }

    fn define_ctor(
        env: &mut crate::types::TypeEnv,
        name: &str,
        type_id: TypeId,
        variant_idx: u16,
        arity: u16,
    ) {
        env.define(
            name,
            Scheme {
                quantified: ArenaSlice::EMPTY,
                ty: Ty::NONE,
                kind: ValueKind::Constructor {
                    type_name: StrId(0),
                    type_id,
                    variant_idx,
                    variant_name: StrId(0),
                    arity,
                    field_labels: ArenaSlice::EMPTY,
                },
                def: None,
            },
        );
    }

    fn define_true_ctor(env: &mut crate::types::TypeEnv, type_id: TypeId, arity: u16) {
        env.define(
            names::TRUE,
            Scheme {
                quantified: ArenaSlice::EMPTY,
                ty: Ty::NONE,
                kind: ValueKind::Constructor {
                    type_name: StrId(0),
                    type_id,
                    variant_idx: 0,
                    variant_name: StrId(0),
                    arity,
                    field_labels: ArenaSlice::EMPTY,
                },
                def: None,
            },
        );
    }

    /// A deleted prelude constructor must surface as a clean compile error,
    /// not a panic or a later unify failure.
    #[test]
    fn missing_ctor_message() {
        let (env, _) = env_with_prelude_types();
        let err = PreludeBindings::capture(&env).unwrap_err();
        assert_eq!(err, PreludeCaptureError::MissingCtor(names::TRUE));
        assert_eq!(err.to_string(), "prelude: constructor 'True' is required");
    }

    /// A constructor of the wrong shape must report what was found, not just
    /// what was expected.
    #[test]
    fn ctor_shape_mismatch_message() {
        let (mut env, bool_id) = env_with_prelude_types();
        define_true_ctor(&mut env, bool_id, 1);
        let err = PreludeBindings::capture(&env).unwrap_err();
        assert_eq!(
            err,
            PreludeCaptureError::CtorShape {
                name: names::TRUE,
                of: names::BOOL,
                expected_arity: 0,
                found: CtorFound::Constructor {
                    of: bool_id,
                    arity: 1
                },
            }
        );
        assert_eq!(
            err.to_string(),
            format!(
                "prelude: 'True' must be a 0-arity constructor of 'Bool'; \
                 found a constructor of type #{bool_id} with arity 1"
            )
        );
    }

    /// `True` shadowed by a non-constructor binding.
    #[test]
    fn ctor_not_a_constructor_message() {
        let (mut env, _) = env_with_prelude_types();
        env.define(names::TRUE, crate::types::mono(Ty::NONE));
        let err = PreludeBindings::capture(&env).unwrap_err();
        assert_eq!(
            err,
            PreludeCaptureError::CtorShape {
                name: names::TRUE,
                of: names::BOOL,
                expected_arity: 0,
                found: CtorFound::NotAConstructor,
            }
        );
        assert_eq!(
            err.to_string(),
            "prelude: 'True' must be a 0-arity constructor of 'Bool'; \
             found a non-constructor binding"
        );
    }

    /// Every late type names a module outside the prelude. One inside it would
    /// be a plain `types:` entry, and `capture` would be the place to demand it.
    #[test]
    fn late_types_are_declared_outside_the_prelude() {
        assert!(!PreludeBindings::LATE_TYPES.is_empty());
        for (module, name, _) in PreludeBindings::LATE_TYPES {
            assert_ne!(
                *module, "scarlet",
                "late type '{name}' is in the prelude; declare it under `types:`"
            );
        }
    }

    /// `capture` runs in `register_prelude`, before any stdlib module loads, so
    /// a late type must be pending there rather than required. Declaring `Map`
    /// under `types:` instead fails every compile with "prelude: type 'Map' is
    /// required".
    #[test]
    fn capture_leaves_late_types_pending() {
        let (mut env, bool_id) = env_with_prelude_types();
        define_all_prelude_ctors(&mut env, bool_id);
        #[allow(clippy::expect_used)]
        let b = PreludeBindings::capture(&env).expect("prelude types and ctors are all present");
        assert!(
            !b.map().is_bound(),
            "Map must not be bound by capture: scarlet/map has not loaded yet"
        );
        assert!(!b.map().is(TypeId::NONE), "an unbound ref matches nothing");
    }

    /// The defining module's interface is the identity. A user type also called
    /// `Map`, in some other module, must not bind here.
    #[test]
    fn bind_late_takes_only_its_own_module() {
        let mut b = PreludeBindings::default();
        b.bind_late("some/user/module", |_| Some((TypeId(77), 2)));
        assert!(!b.map().is_bound(), "another module's 'Map' must not bind");

        b.bind_late("scarlet/map", |name| {
            (name == names::MAP).then_some((TypeId(42), 2))
        });
        assert!(b.map().is(TypeId(42)));
        assert_eq!(b.map().name, "Map");
    }

    /// A drifted arity is refused rather than bound, the same judgement
    /// `capture` makes for a prelude type.
    #[test]
    fn bind_late_refuses_a_drifted_arity() {
        let mut b = PreludeBindings::default();
        b.bind_late("scarlet/map", |_| Some((TypeId(42), 1)));
        assert!(!b.map().is_bound(), "Map(k, v) has arity 2, not 1");
    }

    /// Binding is idempotent: `load_module` calls it on every import, and the
    /// first id must win rather than the last caller's.
    #[test]
    fn bind_late_does_not_rebind() {
        let mut b = PreludeBindings::default();
        b.bind_late("scarlet/map", |_| Some((TypeId(42), 2)));
        b.bind_late("scarlet/map", |_| Some((TypeId(99), 2)));
        assert!(b.map().is(TypeId(42)));
    }

    /// `build.rs` bakes `const PRELUDE` by walking `type_fields` and feeding
    /// `baked` positionally, so a late field missing from the iterator ships a
    /// static stdlib whose Map binding is silently absent.
    #[test]
    fn type_fields_carries_every_late_type() {
        let b = PreludeBindings::default();
        let names: Vec<_> = b.type_fields().map(|(n, _)| n).collect();
        assert!(
            names.contains(&"map"),
            "type_fields is missing 'map': {names:?}"
        );
    }

    /// A prelude type at the wrong arity must fail here, not as a unify error
    /// the first time an array literal is typed.
    #[test]
    fn type_arity_mismatch_message() {
        let mut env = new_env();
        for &(n, _) in PreludeBindings::TYPE_NAMES {
            env.register_type_head(n, StrId(0), ArenaSlice::EMPTY, ArenaSlice::EMPTY);
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
