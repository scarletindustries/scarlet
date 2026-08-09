//! AL's syntax layer: source text in, AST out, plus everything needed to talk
//! about source — spans, tokens, scanner, parser, formatter, diagnostics, and
//! module *identity* ([`module_path`]).
//!
//! It depends on no other workspace crate, so it stays usable by tooling that
//! never compiles a program. Module *resolution* lives in `al_core::module`,
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
pub mod diagnostic;
pub mod formatter;
pub mod module_path;
pub mod parser;
pub mod scanner;
pub mod span;
pub mod term;
pub mod token;
