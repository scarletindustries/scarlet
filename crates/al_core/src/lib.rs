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
// Unsafe code is confined to designated modules (bytecode::value, heap::proc_heap,
// frozen, plus the scoped allows on bytecode::fetch and bytecode::seq) — each carries
// its own `allow(unsafe_code)` and justification. Everything else is
// compiler-enforced safe.
#![deny(unsafe_code)]

pub mod ast;
pub mod bytecode;
pub mod core_ir;
pub mod diagnostic;
pub mod formatter;
pub mod frozen;
pub mod heap;
pub mod module;
pub mod parser;
pub mod precompile;
pub mod reference;
pub mod scanner;
pub mod span;
pub mod static_ir;
pub mod term;
pub mod tivec;
pub mod token;
pub mod type_def;
pub mod typed_ir;
pub mod types;

pub use bytecode::{CtorRef, PreludeBindings, TypeRef};
pub use frozen::{FrozenArea, FrozenBuilder};
pub use heap::ProcHeap;
pub use indexmap::IndexMap;
pub use precompile::{PrecompileOutput, precompile_stdlib};
pub use static_ir::{StaticStdlib, VariantTemplate};
pub use type_def::TypeId;

/// Compile-time assertion that `T: Send`. Use as `const _: () = assert_send::<T>();`.
pub const fn assert_send<T: Send>() {}
/// Compile-time assertion that `T: Send + Sync`.
pub const fn assert_send_sync<T: Send + Sync>() {}
