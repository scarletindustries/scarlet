//! The compiler: Scarlet source in, a [`Program`](crate::core_ir::Program) in
//! core IR out. The module is still called `bytecode` for historical reasons;
//! it no longer produces any.

mod analysis;
pub mod compiler;
mod prelude;
pub mod prelude_bindings;
mod session;

pub use compiler::*;
pub use prelude_bindings::{CtorRef, PreludeBindings, TypeRef};
pub use session::{HoverFact, IncrementalSession, Watermark};
