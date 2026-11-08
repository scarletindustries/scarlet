//! Core IR: typed ANF between the typed IR and a backend.
//!
//! The IR itself is the compiler/VM contract and lives in `scarlet_ir`; this
//! module re-exports it and holds the compiler's passes over it: `lower`
//! (typed IR to Core) and `perceus` (reference counting and reuse).

pub mod immediates;
pub mod lower;
pub mod perceus;

use std::fmt;

pub use scarlet_ir::core_ir::*;

/// Whole-module lowering: every function plus the module toplevel as its own
/// expression (module init).
#[derive(Debug, Clone)]
pub struct CoreProgram {
    pub fns: Vec<CoreFn>,
    pub toplevel: CoreExpr,
}

impl Default for CoreProgram {
    /// No functions, with a toplevel returning nil.
    fn default() -> Self {
        CoreProgram {
            fns: Vec::new(),
            toplevel: CoreExpr::Tail(Atom::Nil),
        }
    }
}

impl fmt::Display for CoreProgram {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for func in &self.fns {
            writeln!(f, "{func}")?;
        }
        writeln!(f, "toplevel:")?;
        write!(f, "{}", Indented(&self.toplevel, 1))
    }
}

/// Fixture builders shared by the `core_ir` test modules.
#[cfg(test)]
pub(crate) mod testkit {
    use super::*;
    use scarlet_elaborate::RTy;
    use scarlet_types::type_def::TypeId;

    pub(crate) fn bind(id: u32, ty: RTy) -> CoreBind {
        CoreBind::new(LocalId(id), ty)
    }

    pub(crate) fn local(id: u32) -> LocalId {
        LocalId(id)
    }

    pub(crate) fn vref(tid: i32, idx: u16) -> VariantRef {
        VariantRef {
            type_id: TypeId(tid),
            variant_idx: idx,
        }
    }

    /// The single anonymous variant most reuse/drop tests scrutinise.
    pub(crate) fn variant() -> VariantRef {
        vref(0, 0)
    }

    pub(crate) fn ctor(fields: &[u32]) -> Atom {
        Atom::Ctor {
            variant: variant(),
            fields: fields.iter().copied().map(LocalId).collect(),
            reuse: None,
        }
    }

    pub(crate) fn func(params: Vec<CoreBind>, body: CoreExpr, ret_ty: RTy) -> CoreFn {
        CoreFn {
            params,
            body,
            ret_ty,
        }
    }
}

/// What `lower` makes of a program the elaborator built.
#[cfg(test)]
mod elaborated_program_tests {
    use crate::{ConstId, lower};
    use scarlet_elaborate::*;
    use scarlet_types::intrinsic::Intrinsic;
    use scarlet_types::type_def::TypeId;
    use scarlet_types::types::{InferEngine, NullaryPrim, Prim, PrimIds, StrId, Ty, new_engine};

    /// `Bool` and `Binary` are ordinary nominal types, so their ids fall
    /// outside [`PrimIds`].
    const BOOL_ID: TypeId = TypeId(5);
    const BINARY_ID: TypeId = TypeId(6);

    /// A [`PreludeTys`] over a real [`InferEngine`], seeded the way the prelude
    /// seeds one.
    struct PreludeCtx {
        eng: InferEngine,
    }

    impl PreludeCtx {
        fn new() -> PreludeCtx {
            let mut eng = new_engine();
            eng.set_prim_ids(PrimIds {
                int: TypeId(1),
                float: TypeId(2),
                string: TypeId(3),
                array: TypeId(4),
            });
            PreludeCtx { eng }
        }
    }

    impl PreludeTys for PreludeCtx {
        fn resolve_rty(&mut self, pool: &mut ResolvedPool, t: Ty) -> RTy {
            Zonker::new(&self.eng).zonk_or_opaque(pool, t).0
        }
        fn ty_int(&mut self) -> Ty {
            let id = self.eng.prim_ids().int;
            self.eng.nullary_con(NullaryPrim::Int, id, "Int")
        }
        fn ty_string(&mut self) -> Ty {
            let id = self.eng.prim_ids().string;
            self.eng.nullary_con(NullaryPrim::String, id, "String")
        }
        fn ty_bool(&mut self) -> Ty {
            self.eng.nullary_con(NullaryPrim::Bool, BOOL_ID, "Bool")
        }
        fn ty_binary(&mut self) -> Ty {
            self.eng
                .nullary_con(NullaryPrim::Binary, BINARY_ID, "Binary")
        }
    }

    /// A prelude that gave `Bool` a `PrimIds` id would silently stop `perceus`
    /// emitting `Drop`s for it. This is the assertion that fails.
    #[test]
    fn interned_temps_are_the_shapes_perceus_expects() {
        let mut ctx = PreludeCtx::new();
        let mut pool = ResolvedPool::new(ctx.eng.prim_ids());
        let t = TempTys::intern(&mut ctx, &mut pool);

        assert!(
            pool.is_heap(t.bool),
            "Bool is nominal: perceus must Drop it"
        );
        assert!(
            pool.is_heap(t.binary),
            "Binary is nominal: perceus must Drop it"
        );
        assert!(pool.is_heap(t.int_pair), "a tuple is always a heap cell");
        assert!(!pool.is_heap(t.int));
        assert!(!pool.is_heap(t.string));

        assert_eq!(pool.prim_of(t.int), Some(Prim::Int));
        assert_eq!(pool.prim_of(t.string), Some(Prim::String));
        assert!(matches!(pool.node(t.int_pair), ResolvedNode::Tuple { elems } if elems.len == 2));
    }

    /// A pool holding the five [`TempTys`] nodes with the shapes
    /// [`TempTys::intern`] gives them, but reachable without an `ElabCtx`.
    fn pool_and_temps() -> (ResolvedPool, TempTys) {
        let mut p = ResolvedPool::new(PrimIds::default());
        let prims = p.prims();
        let int = p.mk_con(prims.int, &[]);
        let boolean = p.mk_con(TypeId(100), &[]);
        let string = p.mk_con(prims.string, &[]);
        let binary = p.mk_con(TypeId(101), &[]);
        let int_pair = p.mk_tuple(&[int, int]);
        (
            p,
            TempTys {
                int,
                bool: boolean,
                string,
                binary,
                int_pair,
            },
        )
    }

    fn nullary(name: StrId, ret: RTy, body: TypedExpr) -> TypedFn {
        TypedFn {
            name,
            params: Vec::new(),
            ret,
            body,
            binds: 0,
        }
    }

    /// `TypedProgram::consts` is not a second pool that emit merges: lowering
    /// preserves every `ConstId` in it.
    #[test]
    fn typed_program_consts_are_stable_ids() {
        let (pool, temps) = pool_and_temps();
        let int = temps.int;
        let p = TypedProgram {
            fns: vec![nullary(
                StrId::NONE,
                int,
                TypedExpr::Const {
                    ty: int,
                    value: ConstId(1),
                },
            )],
            toplevel: nullary(
                StrId::NONE,
                int,
                TypedExpr::Const {
                    ty: int,
                    value: ConstId(0),
                },
            ),
            pool,
            temps,
        };

        // Every constant lives in the compiler's one pool, and `lower` has
        // none of its own: an id the elaborator minted names the same constant
        // after lowering.
        let out = format!("{}", lower::lower(&p));
        assert!(out.contains("ret c1\n"), "the fn's constant:\n{out}");
        assert!(
            out.ends_with("toplevel:\n  ret c0\n"),
            "the toplevel's constant:\n{out}"
        );
    }

    /// A builtin call reaches the core IR as a call to that same intrinsic.
    #[test]
    fn a_builtin_call_reaches_the_core_ir_as_its_intrinsic() {
        let (pool, temps) = pool_and_temps();
        let binary = temps.binary;
        let p = TypedProgram {
            fns: Vec::new(),
            toplevel: nullary(
                StrId::NONE,
                binary,
                TypedExpr::Call {
                    ty: binary,
                    callee: TypedCallee::Builtin {
                        intrinsic: Intrinsic::StringLength,
                    },
                    args: vec![TypedExpr::Const {
                        ty: binary,
                        value: ConstId(0),
                    }],
                },
            ),
            pool,
            temps,
        };

        let out = lower::lower(&p);
        let rendered = out.toplevel.to_string();
        assert!(
            rendered.contains("StringLength"),
            "the intrinsic must survive lowering, got: {rendered}"
        );
    }

    /// The constants `lower` needs but no source literal spells (here an array
    /// pattern's length check) are carried on the node, so the pool cannot grow
    /// during lowering.
    #[test]
    fn lower_cannot_mint_a_constant_the_elaborator_did_not_pool() {
        let (pool, temps) = pool_and_temps();
        let int = temps.int;
        let array = {
            let mut pool = pool;
            let prims = pool.prims();
            let a = pool.mk_con(prims.array, &[int]);
            (pool, a)
        };
        let (pool, arr_ty) = array;

        // `match xs { [_, _] -> 1, _ -> 0 }`
        let scrut = TypedExpr::Var {
            ty: arr_ty,
            place: ValueRef::Global(GlobalSlot(0)),
        };
        let arms = vec![
            TypedArm {
                pat: TypedPat::Array {
                    ty: arr_ty,
                    elem_ty: int,
                    len: ConstId(0),
                    prefix: vec![TypedPat::Wild { ty: int }, TypedPat::Wild { ty: int }],
                    rest: PatRest::None,
                },
                guard: None,
                body: TypedExpr::Const {
                    ty: int,
                    value: ConstId(1),
                },
            },
            TypedArm {
                pat: TypedPat::Wild { ty: arr_ty },
                guard: None,
                body: TypedExpr::Const {
                    ty: int,
                    value: ConstId(2),
                },
            },
        ];
        let p = TypedProgram {
            fns: Vec::new(),
            toplevel: nullary(
                StrId::NONE,
                int,
                TypedExpr::Match {
                    ty: int,
                    scrut: Box::new(scrut),
                    arms,
                },
            ),
            pool,
            temps,
        };

        // The array arm's length check compares against `TypedPat::Array`'s
        // pooled `len`, `c0`: `lower` has no pool to mint a constant into, so
        // every constant it emits is one the elaborator already pooled.
        let out = format!("{}", lower::lower(&p));
        assert!(out.contains("c0"), "the length check:\n{out}");
        assert!(!out.contains("c3"), "a constant nobody pooled:\n{out}");
    }
}
