//! Scarlet's type layer: finished types, module interfaces, the inference
//! engine one module check uses, and exhaustiveness checking.
//!
//! Depends only on `scarlet_syntax` and `scarlet_ir`.

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

pub mod def;
pub mod engine;
pub mod exhaustiveness;
pub mod interface;
pub mod known;
pub mod name;
pub mod slots;
pub mod ty;

pub use scarlet_ir::intrinsic;
