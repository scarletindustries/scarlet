//! Lowering: checked modules in, one [`Program`] out.
//!
//! `program` hands out the ids the VM uses and drives the rest. `translate`
//! turns each module's typed tree into typed IR, `lower` turns typed IR into
//! Core IR, `immediates` turns `Bool` and `Nil` constructors into plain
//! values, and `perceus` adds reference counting and memory reuse.
//!
//! Core IR is the typed ANF between typed IR and a backend. ANF (A-normal
//! form) gives every intermediate value a name, so each operation only takes
//! names as operands. It is the contract between the compiler and the VM, so
//! it lives in `scarlet_ir`, and this crate re-exports it.

pub mod immediates;
pub(crate) mod lower;
pub(crate) mod perceus;
pub mod program;
mod translate;
pub mod typed_ir;

pub use scarlet_ir::core_ir::*;

/// Fixture builders shared by the `core_ir` test modules.
#[cfg(test)]
pub(crate) mod testkit {
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
}
