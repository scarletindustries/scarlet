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

/// The one anonymous variant most reuse and drop tests match on.
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
