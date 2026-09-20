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
// All unsafe lives in `scarlet_vm`, except one scoped allow in `core_ir::clif`'s
// tests.
#![deny(unsafe_code)]

pub mod bytecode;
pub mod core_ir;
pub mod lint;
pub mod module;
pub mod reference;
pub mod tivec;
pub mod typed_ir;

// Re-exported at their historical paths so `scarlet_core::parser`,
// `scarlet_core::types`, `scarlet_core::heap` etc. keep naming one definition.
pub use scarlet_syntax::{
    ast, desugar, diagnostic, formatter, highlight, parser, scanner, span, term, token,
};
pub use scarlet_types::{type_def, types};
pub use scarlet_vm::{assert_send, assert_send_sync, frozen, heap};

pub use bytecode::{CtorRef, PreludeBindings, TypeRef};
pub use precompile::{PrecompileError, PrecompileOutput, precompile_stdlib};
pub use static_ir::StaticStdlib;
pub use type_def::TypeId;
