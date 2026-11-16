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
// The CLI/REPL/LSP driver has no use for unsafe code.
#![forbid(unsafe_code)]

pub use scarlet_core::*;

pub mod cli;
pub mod dis;
pub mod lsp;
pub mod output;
pub mod repl;
pub mod stop;
