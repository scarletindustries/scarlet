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
// The CLI/REPL/LSP driver has no use for unsafe code.
#![forbid(unsafe_code)]

pub use scarlet_core::*;

pub mod cli;
pub mod dis;
pub mod lsp;
pub mod repl;
