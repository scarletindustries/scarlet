use super::*;
use crate::typed_ir::RTy;
use scarlet_ir::TypeId;

#[path = "perceus/tests/ownership.rs"]
pub(crate) mod ownership;

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

/// The constructor with fields that most drop tests are about.
pub(crate) fn variant() -> VariantRef {
    vref(0, 0)
}

pub(crate) fn ctor(fields: &[u32]) -> Atom {
    Atom::Ctor {
        variant: variant(),
        fields: fields.iter().copied().map(LocalId).collect(),
    }
}

pub(crate) fn func(params: Vec<CoreBind>, body: CoreExpr, ret_ty: RTy) -> CoreFn {
    CoreFn {
        params,
        body,
        ret_ty,
    }
}

pub(crate) fn ret(id: u32) -> CoreExpr {
    CoreExpr::Ret(LocalId(id))
}

pub(crate) fn jump(to: u32, args: &[u32]) -> CoreExpr {
    CoreExpr::Jump {
        to: JoinId(to),
        args: args.iter().copied().map(LocalId).collect(),
    }
}

pub(crate) fn tail_call(callee: Callee, args: &[u32]) -> CoreExpr {
    CoreExpr::TailCall {
        callee,
        args: args.iter().copied().map(LocalId).collect(),
    }
}

/// `let bind = rhs`, in front of `body`.
pub(crate) fn bound(bind: CoreBind, rhs: Atom, body: CoreExpr) -> CoreExpr {
    CoreExpr::Let {
        bind,
        rhs,
        body: Box::new(body),
    }
}

pub(crate) fn join(id: u32, params: Vec<CoreBind>, body: CoreExpr, cont: CoreExpr) -> CoreExpr {
    CoreExpr::Join {
        id: JoinId(id),
        params,
        cont: Box::new(cont),
        body: Box::new(body),
    }
}

pub(crate) fn iff(cond: u32, then: CoreExpr, els: CoreExpr) -> CoreExpr {
    CoreExpr::If {
        cond: LocalId(cond),
        then: Box::new(then),
        els: Box::new(els),
    }
}

/// A switch on which constructor of `variant()`'s type `scrut` is.
pub(crate) fn switch(
    scrut: u32,
    arms: Vec<(Vec<u16>, CoreExpr)>,
    default: Option<CoreExpr>,
) -> CoreExpr {
    let ty = variant().type_id;
    switch_on(scrut, Cases::Variant { ty, arms }, default)
}

pub(crate) fn switch_on(scrut: u32, cases: Cases, default: Option<CoreExpr>) -> CoreExpr {
    CoreExpr::Switch {
        scrut: LocalId(scrut),
        cases,
        default: default.map(Box::new),
    }
}

/// How many fields each constructor of `variant()`'s type has.
pub(crate) const FIELDS: [usize; 4] = [3, 0, 2, 0];

/// `let bind = field i of cell`, in front of `body`.
pub(crate) fn field(bind: CoreBind, cell: u32, i: u16, body: CoreExpr) -> CoreExpr {
    let read = Atom::prim(PrimOp::Field(i), vec![LocalId(cell)]);
    bound(bind, read, body)
}
