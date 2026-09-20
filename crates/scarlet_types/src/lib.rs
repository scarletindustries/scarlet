//! Scarlet's type layer: HM inference, type definitions, exhaustiveness checking,
//! and the labelled-slot matcher shared by the typechecker and elaborator.
//!
//! Depends only on `scarlet_syntax` and `scarlet_ir`: never on the compiler
//! (`scarlet_core`) or the runtime (`scarlet_vm`).

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

pub use scarlet_ir::intrinsic;
pub mod slots;
pub mod type_def;
pub mod types;
