//! Core IR: typed ANF between the typed IR and a backend.
//!
//! The IR itself is the compiler/VM contract and lives in `scarlet_ir`; this
//! module re-exports it and holds the compiler's passes over it: `lower`
//! (typed IR to Core) and `perceus` (reference counting and reuse).

pub(crate) mod lower;
pub(crate) mod perceus;

use std::fmt;

pub use scarlet_ir::core_ir::*;

/// Whole-module lowering: every function plus the module toplevel as its own
/// expression (module init).
#[derive(Debug, Clone)]
pub struct CoreProgram {
    pub(crate) fns: Vec<CoreFn>,
    pub(crate) toplevel: CoreExpr,
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
    use crate::type_def::TypeId;
    use crate::typed_ir::RTy;

    pub(crate) fn bind(id: u32, ty: RTy) -> CoreBind {
        CoreBind::new(LocalId(id), ty)
    }

    pub(crate) fn local(id: u32) -> LocalId {
        LocalId(id)
    }

    fn vref(tid: i32, idx: u16) -> VariantRef {
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
