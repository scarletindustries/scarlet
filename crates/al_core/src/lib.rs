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

pub mod ast;
pub mod bytecode;
pub mod diagnostic;
pub mod flags;
pub mod formatter;
pub mod module;
pub mod parser;
pub mod precompile;
pub mod reference;
pub mod scanner;
pub mod span;
pub mod static_ir;
pub mod token;
pub mod type_def;
pub mod types;

pub use bytecode::{CtorRef, PreludeBindings, TypeRef};
pub use indexmap::IndexMap;
pub use precompile::{PrecompileOutput, precompile_stdlib};
pub use static_ir::{StaticStdlib, VariantTemplate};
