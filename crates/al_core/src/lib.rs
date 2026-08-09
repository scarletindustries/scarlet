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
// All unsafe lives in `al_vm`, except one scoped allow in `core_ir::clif`'s
// tests.
#![deny(unsafe_code)]

pub mod bytecode;
pub mod core_ir;
pub mod module;
pub mod precompile;
pub mod reference;
pub mod static_ir;
pub mod typed_ir;

// Re-exported at their historical paths so `al_core::parser`,
// `al_core::types`, `al_core::heap` etc. keep naming one definition.
pub use al_syntax::{ast, diagnostic, formatter, parser, scanner, span, term, token};
pub use al_types::{type_def, types};
pub use al_vm::{assert_send, assert_send_sync, frozen, heap, tivec};

pub use bytecode::{CtorRef, PreludeBindings, TypeRef};
pub use precompile::{PrecompileOutput, precompile_stdlib};
pub use static_ir::{StaticStdlib, VariantTemplate};
pub use type_def::TypeId;
