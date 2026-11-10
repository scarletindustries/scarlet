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

pub mod build;
pub mod compile;
pub mod lint;
pub mod module;
pub use scarlet_ir::tivec;
pub use scarlet_check as check;
pub use scarlet_lower as core_ir;
pub use scarlet_types as types;

pub use scarlet_syntax::{
    ast, desugar, diagnostic, formatter, highlight, parser, scanner, span, term, token,
};
