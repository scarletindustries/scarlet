//! Core IR is the typed ANF that sits between typed IR and a backend. ANF
//! (A-normal form) gives every intermediate value a name, so each operation
//! only takes names as operands.
//!
//! The IR itself is the contract between the compiler and the VM, so it lives
//! in `scarlet_ir`. This crate re-exports it and holds the compiler's passes
//! over it. `lower` turns typed IR into Core IR, `immediates` turns `Bool` and
//! `Nil` constructors into plain values, and `perceus` adds reference counting
//! and memory reuse.

pub mod immediates;
pub mod lower;
pub mod perceus;
pub mod program;
mod translate;
pub mod typed_ir;

use std::fmt;

pub use scarlet_ir::core_ir::*;

/// A whole lowered module. It holds every function, plus the module toplevel
/// as its own expression, which is the module's initialiser.
#[derive(Debug, Clone)]
pub struct CoreProgram {
    pub fns: Vec<CoreFn>,
    pub toplevel: CoreExpr,
}

impl Default for CoreProgram {
    /// No functions, and a toplevel that returns nil.
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
