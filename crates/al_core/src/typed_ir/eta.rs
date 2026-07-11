//! Eta wrappers: a constructor or builtin used as a first-class value.
//!
//! `array.map(xs, W)` passes the *constructor* `W` where a `fn(Int) W` is
//! wanted, and `array.fold(xs, 0, add)` passes a `@vm` builtin where a closure
//! is wanted. Neither is a runtime value on its own: `W` is a header plus a
//! `MakeEnumPayload`, and `add` is an opcode. Both need
//! `fn(a0..aN-1) { <apply>(a0..aN-1) }`.
//!
//! ## Why this is a function synthesiser and not a code emitter
//!
//! `lower` used to ask its caller for a wrapper mid-pass. The caller reserved a
//! `Function` slot, then wrote the wrapper's instructions into
//! `Program::code` — the same vector whose length the caller had already read
//! (or was about to read) as the base address every jump in the *enclosing*
//! body is relative to. Insert instructions between those two moments and the
//! branch after `array.map(xs, W)` jumps to a stale address. That is
//! `crates/al/tests/type_semantics.rs::a_branch_after_an_eta_wrapper_jumps_to_the_right_place`,
//! and it is a bug this module makes unspellable rather than merely fixed:
//!
//! * [`eta_wrapper`] is handed `&mut Vec<TypedFn>` and nothing else mutable.
//!   There is no `Program`, no `code`, no address, and no `&mut Compiler` in
//!   its signature, so it *cannot* write an instruction.
//! * What it hands back is a zero-capture [`TypedExpr::Closure`] carrying a
//!   [`FuncIdx`] — an index into [`TypedProgram::fns`], not a code address.
//!   Nothing resolves that index until the emit loop walks `fns`, by which
//!   point every function's body is already known.
//! * The wrapper is an ordinary [`TypedFn`]. It is lowered, Perceus-annotated
//!   and emitted by the same loop as every other function, so there is no
//!   second code-writing path that could disagree with the first.
//!
//! The wrapper's constructor header (`type_id | variant_idx`, the type and
//! variant names, the field labels, the name prehash) is not pooled here
//! either: the body is a [`TypedExpr::Ctor`] carrying a [`VariantRef`], and
//! `emit` interns those constants for it exactly as it does for every other
//! construction in the program.
//!
//! [`TypedProgram::fns`]: super::TypedProgram::fns
//! [`VariantRef`]: crate::core_ir::VariantRef

use crate::core_ir::{Arity, FuncIdx};
use crate::types::StrId;

use super::resolve::EtaTarget;
use super::{
    BindingId, RTy, ResolvedNode, ResolvedPool, TypedCallee, TypedExpr, TypedFn, ValueRef,
};

/// An [`RTy`] known to be `fn(params...) ret`.
///
/// [`FnRTy::of`] is the only constructor and it is the only place the
/// [`ResolvedNode::Fun`] match happens, so [`eta_wrapper`] is total: it cannot
/// be handed a type with no parameter list and left to guess an arity.
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

    /// The function type itself — what a closure over it is typed with.
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
/// `name` names the wrapper (it is the source name of the constructor or
/// builtin, so a stack trace reads `W`, not `__eta__`). `param_name` names
/// every parameter; the wrapper's body addresses them by [`BindingId`], so this
/// is only ever displayed.
///
/// The `func_idx` the returned closure carries is `fns.len()` at entry, so
/// `fns` must *be* the program's whole function table — index `i` of it is
/// `FuncIdx(i)` for every other producer of a `FuncIdx` too (a lambda's
/// `TypedExpr::Closure`, a `TypedCallee::Known`). A caller that hands over a
/// vector numbered from zero while lambdas and known fns are numbered from
/// some other table will get a wrapper that names the wrong function.
///
/// # Panics
///
/// A constructor's declared field count is authoritative (see
/// [`EtaTarget::Ctor`]) and must agree with the arity of the instantiated
/// function type. They disagree only if the constructor's scheme was built
/// wrong, and the wrapper would then build a payload with the wrong field
/// count — a malformed heap value, silently. The check is a plain `assert!`,
/// not `debug_assert!`, so it holds in release builds too; it runs once per
/// eta site, at compile time.
///
/// A nullary constructor never reaches here: it is a construction, not a
/// wrapper, and `resolve` classifies it as [`super::ValueForm::Ctor`].
pub fn eta_wrapper(
    fns: &mut Vec<TypedFn>,
    name: StrId,
    param_name: StrId,
    target: EtaTarget,
    fn_ty: &FnRTy,
) -> TypedExpr {
    let params: Vec<(StrId, RTy)> = fn_ty.params().iter().map(|t| (param_name, *t)).collect();
    let args: Vec<TypedExpr> = fn_ty
        .params()
        .iter()
        .enumerate()
        .map(|(i, t)| TypedExpr::Var {
            ty: *t,
            place: ValueRef::Local(BindingId(i as u32)),
        })
        .collect();
    let ret = fn_ty.ret();

    let body = match target {
        EtaTarget::Ctor { variant, arity } => {
            // The declaration is authoritative, so it is what the emitted
            // `MakeEnumPayload`'s field count must come from: check the
            // instantiated type against it in every build, release included.
            // A wrapper built from a type that disagrees would construct a
            // payload of the wrong width and corrupt the heap silently.
            assert_eq!(
                Arity(arity),
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
    let func_idx = fns.len() as FuncIdx;
    fns.push(TypedFn {
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

    const NAME: StrId = 1;
    const PARAM: StrId = 2;

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
            type_name: 10,
            variant_name: NAME,
        }
    }

    #[test]
    fn only_a_function_type_makes_an_fn_rty() {
        let mut p = pool();
        let int = p.mk_con(TypeId(1), 0, &[]);
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
        let int = p.mk_con(TypeId(1), 0, &[]);
        let w = p.mk_con(TypeId(9), 10, &[]);
        let ty = p.mk_fun(&[int], w);
        let fn_ty = FnRTy::of(&p, ty).expect("a Fun node");

        let mut fns: Vec<TypedFn> = Vec::new();
        let value = eta_wrapper(
            &mut fns,
            NAME,
            PARAM,
            EtaTarget::Ctor {
                variant: variant(),
                arity: 1,
            },
            &fn_ty,
        );

        assert_eq!(
            value,
            TypedExpr::Closure {
                ty: fn_ty.ty(),
                func_idx: 0,
                captures: Vec::new(),
            }
        );
        assert_eq!(fns.len(), 1);
        let f = &fns[0];
        assert_eq!(f.name, NAME);
        assert_eq!(f.params, vec![(PARAM, int)]);
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

    /// A builtin's wrapper is a saturated call to the opcode; its parameters
    /// are forwarded left to right.
    #[test]
    fn a_builtin_wrapper_calls_the_opcode_with_its_parameters_in_order() {
        let mut p = pool();
        let int = p.mk_con(TypeId(1), 0, &[]);
        let ty = p.mk_fun(&[int, int], int);
        let fn_ty = FnRTy::of(&p, ty).expect("a Fun node");

        let mut fns: Vec<TypedFn> = Vec::new();
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
                func_idx: 0,
                captures: Vec::new(),
            }
        );
        assert_eq!(fns[0].binds, 2);
        assert_eq!(
            fns[0].body,
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

    /// The closure names the wrapper by its index in `fns`, never by an
    /// address, and two eta sites get two wrappers — the second one's index
    /// accounts for every function already appended.
    #[test]
    fn each_wrapper_is_named_by_its_index_in_fns() {
        let mut p = pool();
        let int = p.mk_con(TypeId(1), 0, &[]);
        let w = p.mk_con(TypeId(9), 10, &[]);
        let cty = p.mk_fun(&[int], w);
        let aty = p.mk_fun(&[int, int], int);
        let ctor_ty = FnRTy::of(&p, cty).expect("a Fun node");
        let add_ty = FnRTy::of(&p, aty).expect("a Fun node");

        let mut fns: Vec<TypedFn> = Vec::new();
        let first = eta_wrapper(
            &mut fns,
            NAME,
            PARAM,
            EtaTarget::Ctor {
                variant: variant(),
                arity: 1,
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
        assert_eq!(idx(&first), 0);
        assert_eq!(idx(&second), 1);
        assert_eq!(fns.len(), 2);
        assert_eq!(fns[idx(&second) as usize].binds, 2);
    }

    /// The index is `fns.len()`, not a count of wrappers: a wrapper synthesised
    /// into a table that already holds other functions names its own slot in
    /// *that* table. This is the invariant a caller violates by handing over a
    /// fresh vector while numbering lambdas and known fns from another one.
    #[test]
    fn a_wrapper_names_its_slot_in_the_table_it_was_appended_to() {
        let mut p = pool();
        let int = p.mk_con(TypeId(1), 0, &[]);
        let w = p.mk_con(TypeId(9), 10, &[]);
        let ty = p.mk_fun(&[int], w);
        let fn_ty = FnRTy::of(&p, ty).expect("a Fun node");

        // Two functions the caller already reserved — a lambda, a top-level fn.
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
        let mut fns: Vec<TypedFn> = vec![existing(100), existing(101)];

        let value = eta_wrapper(
            &mut fns,
            NAME,
            PARAM,
            EtaTarget::Ctor {
                variant: variant(),
                arity: 1,
            },
            &fn_ty,
        );

        let TypedExpr::Closure { func_idx, .. } = value else {
            panic!("eta_wrapper returns a Closure");
        };
        assert_eq!(func_idx, 2);
        assert_eq!(fns.len(), 3);
        assert_eq!(fns[func_idx as usize].name, NAME);
    }

    /// The declared field count is authoritative, and the check that the
    /// instantiated type agrees with it is present in release builds: a
    /// disagreement is a compiler bug, not a malformed payload.
    #[test]
    #[should_panic(expected = "one parameter per declared field")]
    fn a_constructor_whose_type_disagrees_with_its_declaration_is_rejected() {
        let mut p = pool();
        let int = p.mk_con(TypeId(1), 0, &[]);
        let w = p.mk_con(TypeId(9), 10, &[]);
        let ty = p.mk_fun(&[int], w);
        let fn_ty = FnRTy::of(&p, ty).expect("a Fun node");

        let mut fns: Vec<TypedFn> = Vec::new();
        eta_wrapper(
            &mut fns,
            NAME,
            PARAM,
            EtaTarget::Ctor {
                variant: variant(),
                arity: 2,
            },
            &fn_ty,
        );
    }
}
