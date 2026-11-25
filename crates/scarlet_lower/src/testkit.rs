use super::*;
use crate::typed_ir::RTy;
use scarlet_ir::TypeId;

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

/// The one anonymous variant most drop tests match on.
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

pub(crate) fn matches(scrut: u32, arms: Vec<(CorePat, CoreExpr)>) -> CoreExpr {
    CoreExpr::Match {
        scrut: LocalId(scrut),
        arms,
    }
}
