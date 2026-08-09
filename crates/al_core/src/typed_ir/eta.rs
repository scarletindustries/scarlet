//! Eta wrappers: a constructor or builtin used as a first-class value.
//!
//! `array.map(xs, W)` passes the constructor `W` where a `fn(Int) W` is wanted;
//! `array.fold(xs, 0, add)` passes a `@vm` builtin where a closure is wanted.
//! Neither is a runtime value on its own, so both get a synthesised
//! `fn(a0..aN-1) { <apply>(a0..aN-1) }`.
//!
//! This module synthesises functions and must never emit code. Writing the
//! wrapper's instructions into `Program::code` mid-pass shifts the base address
//! the enclosing body's jumps were computed against, so a branch after
//! `array.map(xs, W)` lands on a stale address. [`eta_wrapper`] therefore takes
//! only `&mut FnTable` and returns a [`FuncIdx`], never an address; the wrapper
//! is an ordinary [`TypedFn`] emitted by the same loop as every other function.

use crate::core_ir::FuncIdx;
use crate::tivec::TiVec;
use crate::types::StrId;

use super::resolve::EtaTarget;
use super::{
    Arity, BindingId, RTy, ResolvedNode, ResolvedPool, TypedBind, TypedCallee, TypedExpr, TypedFn,
    ValueRef,
};

/// [`TypedProgram::fns`] under construction, and the only minter of a
/// [`FuncIdx`]. Indexing by `FuncIdx` rather than `usize` is what stops a
/// wrapper being numbered against one table and appended to another.
///
/// [`TypedProgram::fns`]: super::TypedProgram::fns
pub type FnTable = TiVec<FuncIdx, TypedFn>;

/// An [`RTy`] known to be `fn(params...) ret`. [`FnRTy::of`] is the only
/// constructor, so [`eta_wrapper`] can never be handed a non-function type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FnRTy {
    ty: RTy,
    params: Vec<RTy>,
    ret: RTy,
}

impl FnRTy {
    /// `None` when `ty` is not a function type.
    pub fn of(pool: &ResolvedPool, ty: RTy) -> Option<FnRTy> {
        match pool.node(ty) {
            ResolvedNode::Fun { params, ret } => Some(FnRTy {
                ty,
                params: pool.children(params).to_vec(),
                ret,
            }),
            ResolvedNode::Bound(_) | ResolvedNode::Con { .. } | ResolvedNode::Tuple { .. } => None,
        }
    }

    /// The function type itself.
    pub fn ty(&self) -> RTy {
        self.ty
    }

    pub fn params(&self) -> &[RTy] {
        &self.params
    }

    pub fn ret(&self) -> RTy {
        self.ret
    }

    pub fn arity(&self) -> Arity {
        Arity::of(&self.params)
    }
}

/// Append `fn(a0..aN-1) { <target>(a0..aN-1) }` to `fns` and return the
/// zero-capture [`TypedExpr::Closure`] that names it.
///
/// `name` is the source name of the constructor or builtin, so stack traces
/// read `W`. `param_name` is display-only; the body addresses parameters by
/// [`BindingId`].
///
/// A nullary constructor never reaches here: `resolve` classifies it as
/// [`super::ValueForm::Ctor`].
///
/// # Panics
///
/// If the constructor's declared field count disagrees with the instantiated
/// function type's arity.
pub fn eta_wrapper(
    fns: &mut FnTable,
    name: StrId,
    param_name: StrId,
    target: EtaTarget,
    fn_ty: &FnRTy,
) -> TypedExpr {
    // A fresh function, so its `BindingId` space starts at zero.
    let params: Vec<TypedBind> = fn_ty
        .params()
        .iter()
        .enumerate()
        .map(|(i, &ty)| TypedBind {
            id: BindingId(i as u32),
            name: param_name,
            ty,
            global: None,
        })
        .collect();
    let args: Vec<TypedExpr> = params
        .iter()
        .map(|b| TypedExpr::Var {
            ty: b.ty,
            place: ValueRef::Local(b.id),
        })
        .collect();
    let ret = fn_ty.ret();

    let body = match target {
        EtaTarget::Ctor { variant, arity } => {
            // Not a debug_assert: a type that disagrees with the declaration
            // builds a `MakeEnumPayload` of the wrong width and silently
            // corrupts the heap. Runs once per eta site, at compile time.
            assert_eq!(
                arity,
                fn_ty.arity(),
                "a constructor's scheme has one parameter per declared field"
            );
            TypedExpr::Ctor {
                ty: ret,
                variant,
                args,
            }
        }
        EtaTarget::Builtin { op } => TypedExpr::Call {
            ty: ret,
            callee: TypedCallee::Builtin(op),
            args,
        },
    };

    let binds = params.len() as u32;
    let func_idx = fns.push(TypedFn {
        name,
        params,
        ret,
        body,
        binds,
    });

    TypedExpr::Closure {
        ty: fn_ty.ty(),
        func_idx,
        captures: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::Op;
    use crate::core_ir::VariantRef;
    use crate::type_def::TypeId;
    use crate::types::PrimIds;

    const NAME: StrId = StrId(1);
    const PARAM: StrId = StrId(2);

    fn pool() -> ResolvedPool {
        ResolvedPool::new(PrimIds {
            int: TypeId(1),
            float: TypeId(2),
            string: TypeId(3),
            array: TypeId(4),
        })
    }

    fn variant() -> VariantRef {
        VariantRef {
            type_id: TypeId(9),
            variant_idx: 0,
            type_name: StrId(10),
            variant_name: NAME,
        }
    }

    #[test]
    fn only_a_function_type_makes_an_fn_rty() {
        let mut p = pool();
        let int = p.mk_con(TypeId(1), StrId(0), &[]);
        let tup = p.mk_tuple(&[int, int]);
        let f = p.mk_fun(&[int, int], tup);
        assert!(FnRTy::of(&p, int).is_none());
        assert!(FnRTy::of(&p, tup).is_none());
        let f = FnRTy::of(&p, f).expect("a Fun node");
        assert_eq!(f.arity(), Arity(2));
        assert_eq!(f.params(), &[int, int]);
        assert_eq!(f.ret(), tup);
    }

    /// `array.map(xs, W)` for `type W { W(v Int) }`.
    #[test]
    fn a_constructor_wrapper_constructs_from_its_parameters() {
        let mut p = pool();
        let int = p.mk_con(TypeId(1), StrId(0), &[]);
        let w = p.mk_con(TypeId(9), StrId(10), &[]);
        let ty = p.mk_fun(&[int], w);
        let fn_ty = FnRTy::of(&p, ty).expect("a Fun node");

        let mut fns = FnTable::new();
        let value = eta_wrapper(
            &mut fns,
            NAME,
            PARAM,
            EtaTarget::Ctor {
                variant: variant(),
                arity: Arity(1),
            },
            &fn_ty,
        );

        assert_eq!(
            value,
            TypedExpr::Closure {
                ty: fn_ty.ty(),
                func_idx: FuncIdx(0),
                captures: Vec::new(),
            }
        );
        assert_eq!(fns.len(), 1);
        let f = &fns[FuncIdx(0)];
        assert_eq!(f.name, NAME);
        assert_eq!(
            f.params,
            vec![TypedBind {
                id: BindingId(0),
                name: PARAM,
                ty: int,
                global: None,
            }]
        );
        assert_eq!(f.ret, w);
        assert_eq!(f.binds, 1);
        assert_eq!(
            f.body,
            TypedExpr::Ctor {
                ty: w,
                variant: variant(),
                args: vec![TypedExpr::Var {
                    ty: int,
                    place: ValueRef::Local(BindingId(0)),
                }],
            }
        );
    }

    #[test]
    fn a_builtin_wrapper_calls_the_opcode_with_its_parameters_in_order() {
        let mut p = pool();
        let int = p.mk_con(TypeId(1), StrId(0), &[]);
        let ty = p.mk_fun(&[int, int], int);
        let fn_ty = FnRTy::of(&p, ty).expect("a Fun node");

        let mut fns = FnTable::new();
        let value = eta_wrapper(
            &mut fns,
            NAME,
            PARAM,
            EtaTarget::Builtin { op: Op::Add },
            &fn_ty,
        );

        assert_eq!(
            value,
            TypedExpr::Closure {
                ty: fn_ty.ty(),
                func_idx: FuncIdx(0),
                captures: Vec::new(),
            }
        );
        assert_eq!(fns[FuncIdx(0)].binds, 2);
        assert_eq!(
            fns[FuncIdx(0)].body,
            TypedExpr::Call {
                ty: int,
                callee: TypedCallee::Builtin(Op::Add),
                args: vec![
                    TypedExpr::Var {
                        ty: int,
                        place: ValueRef::Local(BindingId(0)),
                    },
                    TypedExpr::Var {
                        ty: int,
                        place: ValueRef::Local(BindingId(1)),
                    },
                ],
            }
        );
    }

    #[test]
    fn each_wrapper_is_named_by_its_index_in_fns() {
        let mut p = pool();
        let int = p.mk_con(TypeId(1), StrId(0), &[]);
        let w = p.mk_con(TypeId(9), StrId(10), &[]);
        let cty = p.mk_fun(&[int], w);
        let aty = p.mk_fun(&[int, int], int);
        let ctor_ty = FnRTy::of(&p, cty).expect("a Fun node");
        let add_ty = FnRTy::of(&p, aty).expect("a Fun node");

        let mut fns = FnTable::new();
        let first = eta_wrapper(
            &mut fns,
            NAME,
            PARAM,
            EtaTarget::Ctor {
                variant: variant(),
                arity: Arity(1),
            },
            &ctor_ty,
        );
        let second = eta_wrapper(
            &mut fns,
            NAME,
            PARAM,
            EtaTarget::Builtin { op: Op::Add },
            &add_ty,
        );

        let idx = |e: &TypedExpr| match e {
            TypedExpr::Closure { func_idx, .. } => *func_idx,
            _ => panic!("eta_wrapper returns a Closure"),
        };
        assert_eq!(idx(&first), FuncIdx(0));
        assert_eq!(idx(&second), FuncIdx(1));
        assert_eq!(fns.len(), 2);
        assert_eq!(fns[idx(&second)].binds, 2);
    }

    #[test]
    fn a_wrapper_names_its_slot_in_the_table_it_was_appended_to() {
        let mut p = pool();
        let int = p.mk_con(TypeId(1), StrId(0), &[]);
        let w = p.mk_con(TypeId(9), StrId(10), &[]);
        let ty = p.mk_fun(&[int], w);
        let fn_ty = FnRTy::of(&p, ty).expect("a Fun node");

        let existing = |name: StrId| TypedFn {
            name,
            params: vec![],
            ret: int,
            body: TypedExpr::Var {
                ty: int,
                place: ValueRef::Local(BindingId(0)),
            },
            binds: 0,
        };
        let mut fns = FnTable::new();
        fns.push(existing(StrId(100)));
        fns.push(existing(StrId(101)));

        let value = eta_wrapper(
            &mut fns,
            NAME,
            PARAM,
            EtaTarget::Ctor {
                variant: variant(),
                arity: Arity(1),
            },
            &fn_ty,
        );

        let TypedExpr::Closure { func_idx, .. } = value else {
            panic!("eta_wrapper returns a Closure");
        };
        assert_eq!(func_idx, FuncIdx(2));
        assert_eq!(fns.len(), 3);
        assert_eq!(fns[func_idx].name, NAME);
    }

    #[test]
    #[should_panic(expected = "one parameter per declared field")]
    fn a_constructor_whose_type_disagrees_with_its_declaration_is_rejected() {
        let mut p = pool();
        let int = p.mk_con(TypeId(1), StrId(0), &[]);
        let w = p.mk_con(TypeId(9), StrId(10), &[]);
        let ty = p.mk_fun(&[int], w);
        let fn_ty = FnRTy::of(&p, ty).expect("a Fun node");

        let mut fns = FnTable::new();
        eta_wrapper(
            &mut fns,
            NAME,
            PARAM,
            EtaTarget::Ctor {
                variant: variant(),
                arity: Arity(2),
            },
            &fn_ty,
        );
    }
}
