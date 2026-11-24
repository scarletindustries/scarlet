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

#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing,
        clippy::string_slice,
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss,
        clippy::arithmetic_side_effects,
    )
)]

pub mod immediates;
pub(crate) mod lower;
pub(crate) mod perceus;
pub mod program;
mod translate;
pub mod typed_ir;

pub use scarlet_ir::core_ir::*;

/// Fixture builders shared by the `core_ir` test modules.
#[cfg(test)]
pub(crate) mod testkit;
