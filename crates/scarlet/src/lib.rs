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
// All unsafe code lives in `scarlet_vm`; this crate is the CLI/REPL/LSP driver.
#![forbid(unsafe_code)]

pub use scarlet_core::*;

pub mod cli;
pub mod lsp;
pub mod repl;

#[allow(clippy::approx_constant, clippy::unreadable_literal, unused_imports)]
mod generated {
    include!(concat!(env!("OUT_DIR"), "/stdlib_generated.rs"));
}
/// The build-time precompiled stdlib, zero-cost `static` data.
pub use generated::{STDLIB, STDLIB_CORE_BYTES, STDLIB_CORE_INDEX};
