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
// Unsafe code is confined to one designated module (vm::freeze Send/Sync
// impls), which carries its own `allow(unsafe_code)` and justification.
// Everything else is compiler-enforced safe.
#![deny(unsafe_code)]

pub use al_core::*;

pub mod cli;
pub mod lsp;
pub mod repl;
pub mod vm;

#[allow(clippy::approx_constant, clippy::unreadable_literal, unused_imports)]
mod generated {
    include!(concat!(env!("OUT_DIR"), "/stdlib_generated.rs"));
}
/// `STDLIB` is the build-time precompiled stdlib; `stdlib` is the generated
/// module of typed template handles. Both are zero-cost `static` data.
pub use generated::{STDLIB, stdlib};
