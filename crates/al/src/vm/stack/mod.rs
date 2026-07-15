//! Per-process native stacks: the regions, the switches, and the fault
//! naming. Stage 1 of the machine-stack plan — **dormant**: nothing in the
//! VM runs on a process stack yet; this module exists so later stages land
//! on proven, unit-tested primitives instead of discovering them under a
//! JIT. (`docs/stage1-spec.md` is the contract; its corrections C1-C7
//! bound everything here.)
//!
//! The pieces:
//!
//! - [`slab`] — pooled stack regions: guard page + 256K usable per slot,
//!   `MADV_NOHUGEPAGE`, per-scheduler freelists, and a spawn cap derived
//!   from `vm.max_map_count` so exhaustion is a `VmError`, not an ENOMEM.
//! - [`switch`] — the stack-pointer moves: enter a process stack
//!   ([`switch::run_on_stack`]), run a shim back on the scheduler's C stack
//!   ([`switch::call_on_sched_stack`]), and park/resume a computation in
//!   place ([`switch::swap_context`]). The only asm in the crate.
//! - [`fault`] — a SIGSEGV handler that recognizes guard-page hits and
//!   reports which AL process overflowed, instead of a bare crash.
//!
//! # INVARIANT (process-stack purity)
//!
//! No machine word on a process stack, and no value in a pinned or
//! callee-saved register at a suspension point, may name anything owned by
//! a scheduler. Everything scheduler-derived is re-derived after every
//! suspension point.
//!
//! This is the invariant that makes a parked stack migratable (the F1
//! finding: a scheduler's `&mut VM` spilled into a parked frame and resumed
//! on another thread is memory corruption, and no `Send` bound can see it
//! inside a byte region). It cannot be checked by the compiler; it is
//! discharged by construction — scheduler state lives in thread-locals
//! ([`switch`]'s `SCHED_SP`, the VM's `CURRENT_VM`) that resumed code
//! re-reads, never in anything that suspends.
//!
//! # Suspension has two mechanisms, not one
//!
//! All Rust runs on the scheduler's C stack (the [`switch::call_on_sched_stack`]
//! bracket), because interpreter re-entries nest without bound and Rust
//! frame sizes are unknowable; only Cranelift frames, whose sizes are
//! reported at compile time, live on the 256K process stacks. The
//! consequence (spec C4): a park that begins inside an *interpreted* callee
//! cannot stack-switch in place — the interpreter's Rust frames are on the
//! shared C stack, which the next process reuses. So Rust frames unwind by
//! status return up to the bracket, and only then does the process side
//! switch stacks. A native caller parked over an interpreted callee
//! therefore resumes by RE-CALLING `al_rt_enter_interp` — its resume label
//! sits *before* that call, the opposite of every other status-return site.

pub mod fault;
pub mod slab;
pub mod switch;

pub use slab::{STACK_BYTES, StackHandle, StackPool};
pub use switch::SavedContext;

/// A suspended computation living on its own stack region: the region plus
/// the switch point to resume it. This pairing is what Stage 2's
/// suspend-in-place produces and what makes the two frame representations
/// mutually exclusive (spec C3): a process is EITHER fully described by its
/// interpreter `frames` (today, always) OR carries the machine half here —
/// never both, so a missed dispatch is a missing-frame panic, not a double
/// execution.
#[derive(Debug)]
pub struct ParkedStack {
    pub stack: StackHandle,
    pub saved: SavedContext,
}
