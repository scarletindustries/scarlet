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

pub mod bytecode;
pub mod core_ir;
pub mod lint;
pub mod module;
pub mod reference;
pub mod tivec;
pub mod typed_ir;

// Re-exported at their historical paths so `scarlet_core::parser`,
// `scarlet_core::types` etc. keep naming one definition.
pub use scarlet_syntax::{
    ast, desugar, diagnostic, formatter, highlight, parser, scanner, span, term, token,
};
pub use scarlet_types::{type_def, types};

pub use bytecode::{CtorRef, PreludeBindings, TypeRef};
pub use type_def::TypeId;
