//! The AL virtual machine — the language-agnostic half of the toolchain.
//!
//! This crate is the runtime: the bytecode instruction set ([`bytecode`]),
//! the NaN-boxed value representation and heap object layouts
//! ([`bytecode::value`]), the per-process reference-counted heap ([`heap`]),
//! the program-lifetime frozen area ([`frozen`]), and the persistent
//! collections backing `Array` and `Map`. The interpreter, schedulers, and
//! JIT live under [`vm`].
//!
//! The load-bearing property is what this crate does *not* know: it has no
//! dependency on `al_core`, so it cannot name an AST node, a type scheme, or
//! a parser — the dependency edge points the other way, and Cargo enforces
//! it. Any front end that can produce a [`bytecode::Program`] (and hand the
//! VM a [`template::StdlibTemplates`] table for the values its I/O ops
//! construct) can run on this VM. AL's front end lives in `al_core`; the
//! `al` binary wires the two together.

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
// Unsafe code is confined to designated modules — bytecode::value,
// heap::proc_heap, frozen, the scoped allows on bytecode::fetch (in
// bytecode::mod), bytecode::scratch, bytecode::native's entry-pointer
// transmute, and the VM's runtime boundary (vm::freeze's Send/Sync impls,
// vm::native_shims' raw-pointer/raw-bits JIT boundary, vm::jit's publishing
// of finalized code addresses, and the vm::stack family that owns and
// switches the mmap'd native stacks) — each carries its own
// `allow(unsafe_code)` and justification. Everything else is
// compiler-enforced safe.
#![deny(unsafe_code)]

pub mod bytecode;
pub mod frozen;
pub mod heap;
mod ids;
pub mod native_rc;
pub mod template;
pub mod tivec;
pub mod vm;

pub use ids::{FuncIdx, TypeId};
pub use template::VariantTemplate;

/// Compile-time assertion that `T: Send`. Use as `const _: () = assert_send::<T>();`.
pub const fn assert_send<T: Send>() {}
/// Compile-time assertion that `T: Send + Sync`.
pub const fn assert_send_sync<T: Send + Sync>() {}
