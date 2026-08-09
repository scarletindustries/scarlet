//! AL's type layer: the HM inference engine ([`types::infer`]), the type
//! environment and definitions ([`type_def`]), exhaustiveness checking, and
//! the generic labelled-slot matcher ([`slots`]) shared by the typechecker
//! and the elaborator.
//!
//! Depends on `al_syntax` (it types the AST) and `al_vm` (nominal
//! [`TypeId`](al_vm::TypeId)s and `Op` for `@vm` intrinsic schemes) — but
//! not on the compiler: lowering, elaboration, and codegen live above this
//! crate in `al_core`.

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

pub mod slots;
pub mod type_def;
pub mod types;
