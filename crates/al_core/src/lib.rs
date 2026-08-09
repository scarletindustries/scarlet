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
// All unsafe code lives in `al_vm` (the runtime crate); this crate — the
// language front end — is compiler-enforced safe, with the one scoped allow
// on `core_ir::clif`'s tests, which drive JIT'd code through a C-ABI mock
// runtime.
#![deny(unsafe_code)]

pub mod bytecode;
pub mod core_ir;
pub mod module;
pub mod precompile;
pub mod reference;
pub mod static_ir;
pub mod typed_ir;

// The syntax layer (source → AST, diagnostics, formatting, module identity),
// the type layer (HM inference, type definitions, exhaustiveness), and the
// runtime substrate (values, heap, frozen area, index vectors) live in their
// own crates; re-exported at the historical paths so `al_core::parser`,
// `al_core::types`, `al_core::heap` etc. keep naming the one shared
// definition.
pub use al_syntax::{ast, diagnostic, formatter, parser, scanner, span, term, token};
pub use al_types::{type_def, types};
pub use al_vm::{assert_send, assert_send_sync, frozen, heap, tivec};

pub use bytecode::{CtorRef, PreludeBindings, TypeRef};
pub use precompile::{PrecompileOutput, precompile_stdlib};
pub use static_ir::{StaticStdlib, VariantTemplate};
pub use type_def::TypeId;
