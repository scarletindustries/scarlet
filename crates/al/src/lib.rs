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
// All unsafe code lives in `al_vm`; this crate is the CLI/REPL/LSP driver.
#![forbid(unsafe_code)]

pub use al_core::*;

pub mod cli;
pub mod dis;
pub mod lsp;
pub mod repl;
pub mod vm;

#[allow(clippy::approx_constant, clippy::unreadable_literal, unused_imports)]
mod generated {
    include!(concat!(env!("OUT_DIR"), "/stdlib_generated.rs"));
}
/// The build-time precompiled stdlib, zero-cost `static` data.
pub use generated::STDLIB;
