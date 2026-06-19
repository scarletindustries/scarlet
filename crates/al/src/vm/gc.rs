//! Reclamation fairness and the operand-peek helper.
//!
//! There is no garbage collector under reference counting — nothing moves, so
//! there is no rooting rule, no safepoints, and no allocation reservation.
//!
//! What lives here is the call-checkpoint *fairness* charge. Freeing is not
//! free: dropping a large value graph cascades through thousands of objects in
//! one go. Reference counting does that work wherever the last reference goes
//! away — which can be deep inside a single opcode — so a process that drops a
//! big structure would otherwise stall its scheduler with un-budgeted work.
//!
//! The allocator counts objects it frees on this thread ([`take_freed_objects`]);
//! the `Call`/`TailCall`/`CallSelf`/`TailCallSelf` arms drain that count through
//! [`VM::charge_reclamation`] and bill it to the running process's reduction
//! budget, so a giant cascading free preempts at the next call instead of riding
//! free. The drain is one thread-local read on the per-call fast path, and only
//! a genuinely large free ([`FREES_PER_REDUCTION`] objects or more) moves the
//! budget at all.

use al_core::bytecode::{Value, freed_objects_pending, take_freed_objects};

use super::VM;

/// Objects freed per reduction charged. Tuned so only big cascading drops
/// (thousands of objects) cost the process budget; ordinary per-op churn is
/// far below it and never preempts.
const FREES_PER_REDUCTION: u64 = 256;

impl VM {
    /// Drain this thread's reclamation counter and bill `reds` for it. Called at
    /// every call checkpoint. Cheap when little was freed since the last call;
    /// charges one reduction per [`FREES_PER_REDUCTION`] objects freed.
    #[inline]
    pub(super) fn charge_reclamation(&self, reds: &mut i32) {
        // Fast path: one thread-local read. Only when a genuinely large free
        // has accumulated do we drain the counter and bill the budget.
        if freed_objects_pending() >= FREES_PER_REDUCTION {
            let freed = take_freed_objects();
            *reds = reds.saturating_sub((freed / FREES_PER_REDUCTION) as i32);
        }
    }

    /// The operand `d` slots below the top of the stack (0 = top), or `None`
    /// if the stack is shallower than that. Reads without popping.
    pub(super) fn peek_at(&self, d: usize) -> Option<&Value> {
        self.stack.len().checked_sub(1 + d).map(|i| &self.stack[i])
    }
}
