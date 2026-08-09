//! Per-process native stacks: pooled regions ([`slab`]), the stack-pointer
//! moves ([`switch`], the only asm in the crate), and guard-page fault naming
//! ([`fault`]). Nothing in the VM runs on a process stack yet.
//!
//! # INVARIANT (process-stack purity)
//!
//! No machine word on a process stack, and no value in a pinned or
//! callee-saved register at a suspension point, may name anything a scheduler
//! owns. Otherwise a parked stack resumed on another thread is memory
//! corruption, and no `Send` bound can see inside a byte region. The compiler
//! cannot check this; it holds because scheduler state lives in thread-locals
//! ([`switch`]'s `SCHED_SP`, the VM's `CURRENT_VM`) that resumed code re-reads.
//!
//! # Suspension has two mechanisms
//!
//! All Rust runs on the scheduler's C stack (the
//! [`switch::call_on_sched_stack`] bracket): interpreter re-entries nest
//! without bound and Rust frame sizes are unknowable, so only Cranelift frames
//! live on the 256K process stacks. So a park beginning inside an interpreted
//! callee cannot stack-switch in place — the next process reuses that C stack.
//! Rust frames unwind by status return up to the bracket and only then does the
//! process side switch. A native caller parked over an interpreted callee
//! resumes by RE-CALLING `al_rt_enter_interp`: its resume label sits *before*
//! that call, unlike every other status-return site.

pub mod fault;
pub mod slab;
pub mod switch;

pub use slab::{STACK_BYTES, StackHandle, StackPool};
pub use switch::SavedContext;

/// A suspended computation living on its own stack region: the region plus the
/// switch point to resume it. A process is EITHER fully described by its
/// interpreter `frames` or carries the machine half here, never both, so a
/// missed dispatch panics on a missing frame instead of executing twice.
#[derive(Debug)]
pub struct ParkedStack {
    pub stack: StackHandle,
    pub saved: SavedContext,
}
