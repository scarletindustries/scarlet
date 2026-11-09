//! Scarlet's type layer. It holds Hindley-Milner (HM) type inference, type
//! definitions, exhaustiveness checking, and the code that puts labelled
//! arguments into their slots for the type checker and the elaborator.
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
