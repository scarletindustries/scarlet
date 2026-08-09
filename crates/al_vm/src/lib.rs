//! The AL virtual machine: the bytecode ISA ([`bytecode`]), NaN-boxed values
//! and heap layouts ([`bytecode::value`]), the per-process reference-counted
//! heap ([`heap`]), the program-lifetime frozen area ([`frozen`]), and the
//! interpreter, schedulers and JIT ([`vm`]).
//!
//! This crate must never depend on a language crate; Cargo enforces it. Any
//! front end that produces a [`bytecode::Program`] and a
//! [`template::StdlibTemplates`] table can run here.

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
// Unsafe is confined to designated modules, each with its own scoped
// `allow(unsafe_code)` and justification.
#![deny(unsafe_code)]

pub mod abi;
pub mod bytecode;
pub mod frozen;
pub mod heap;
mod ids;
pub mod native_rc;
pub mod template;
pub mod tivec;
pub mod vm;

pub use abi::{AbiSlot, TemplateIdx};
pub use ids::{FuncIdx, TypeId};
pub use template::{AbiTable, EnumTemplate};

/// Compile-time assertion that `T: Send`. Use as `const _: () = assert_send::<T>();`.
pub const fn assert_send<T: Send>() {}
/// Compile-time assertion that `T: Send + Sync`.
pub const fn assert_send_sync<T: Send + Sync>() {}
