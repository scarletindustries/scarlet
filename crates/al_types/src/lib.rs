//! AL's type layer: HM inference, type definitions, exhaustiveness checking,
//! and the labelled-slot matcher shared by the typechecker and elaborator.
//!
//! May depend on `al_syntax` and `al_vm`, never on the compiler (`al_core`).

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
