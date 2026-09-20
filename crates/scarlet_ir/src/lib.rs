//! The contract between the compiler and the VM: the vocabulary a compiled
//! program is written in. The compiler produces it and the VM runs it, and
//! neither needs anything else from the other.
//!
//! Depends on nothing in the workspace.

#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
    )
)]
#![forbid(unsafe_code)]

use std::fmt;

pub mod core_ir;
pub mod intrinsic;
pub mod rty;
pub mod tivec;

/// A nominal type's identity: which `type` declaration a value or pattern
/// belongs to. A constructor is a `TypeId` plus a variant index, so the VM
/// needs it as much as the type checker does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(transparent)]
pub struct TypeId(pub i32);

impl TypeId {
    /// Sentinel meaning "no nominal type"; real ids start at 1. Deliberately
    /// not `Default`, so a derived `Default` cannot manufacture it.
    pub const NONE: TypeId = TypeId(0);
}

impl fmt::Display for TypeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
