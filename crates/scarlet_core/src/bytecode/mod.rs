//! The compiler side of the bytecode boundary: everything that produces a
//! runnable [`Program`] from Scarlet source. The ISA, [`Program`], the NaN-boxed
//! `value` and the heap live in `scarlet_vm` and are re-exported here, so
//! `scarlet_core::bytecode::*` is the one import for both halves of the contract.

mod analysis;
pub mod compiler;
mod prelude;
pub mod prelude_bindings;
mod session;

pub use compiler::*;
pub use prelude_bindings::{CtorRef, PreludeBindings, TypeRef};
pub use scarlet_vm::bytecode::*;
pub use session::{HoverFact, IncrementalSession, Watermark};
