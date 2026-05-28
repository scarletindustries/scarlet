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

pub use al_core::*;

pub mod cli;
pub mod lsp;
pub mod repl;
pub mod vm;

#[allow(clippy::approx_constant, clippy::unreadable_literal, unused_imports)]
mod generated {
    include!(concat!(env!("OUT_DIR"), "/stdlib_generated.rs"));
}
pub use generated::{NEXT_TYPE_ID, PRELUDE, RESERVED, STDLIB, STDLIB_LOCAL_COUNT, stdlib};

/// The build-time precompiled stdlib as `&'static StaticStdlib`. Zero
/// construction cost — every field is a `static` array reference.
#[inline]
pub fn stdlib() -> &'static StaticStdlib {
    &STDLIB
}
