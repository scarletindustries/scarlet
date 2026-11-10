//! Scarlet's syntax layer. It turns source text into an AST, and holds what the
//! rest of the compiler needs to talk about source: spans, tokens, the scanner,
//! parser and formatter, diagnostics, and module *identity* ([`module_path`]).
//!
//! It depends on no other workspace crate, so tooling that never compiles a
//! program can still use it. Module *resolution* lives in `scarlet_core::module`,
//! built on the identity types here.

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

pub mod ast;
pub mod desugar;
pub mod diagnostic;
pub mod formatter;
pub mod highlight;
pub mod module_path;
pub mod parser;
pub mod scanner;
pub mod source;
pub mod span;
pub mod term;
pub mod token;
