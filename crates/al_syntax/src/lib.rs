//! AL's syntax layer: source text in, AST out, and everything needed to talk
//! about source — spans, tokens, the scanner and parser, the formatter, the
//! diagnostic machinery, and module *identity* ([`module_path`]).
//!
//! This crate knows nothing about types, bytecode, or the runtime: it
//! depends on neither `al_types` nor `al_vm`, so it compiles in parallel
//! with the runtime and is reusable by tooling (formatters, linters,
//! highlighters) that never compiles a program. Module *resolution* — the
//! table, caching, import handling — lives in `al_core::module`, built on
//! the identity types here.

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
