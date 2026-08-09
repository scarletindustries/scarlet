//! The stack-switch primitives: the only module in the crate that moves the
//! machine stack pointer. Everything here is `global_asm!`-free hand-rolled
//! `naked_asm!` — no Rust frame exists inside a switch, so there is nothing
//! the compiler could miscompile around it.
//!
//! Both supported arches are implemented in full (x86_64 SysV, aarch64
//! AAPCS64); the test suite runs on both and the asm is not cfg'd away.
//!
//! Why hand-rolled rather than Cranelift's own `stack_switch` IR
//! instruction: that instruction is x64-only (`isa/aarch64/` has zero hits),
//! requires `stack_switch_model=basic` (defaults `none`), and its emission
//! is `ALL_CLOBBERS`. Hand-rolled asm covers both arches and clobbers
//! nothing.
//!
//! # Ownership rule (stated once, from the Stage-1 spec)
//!
//! *The C stack is owned by the OS thread; the process stack is owned by the
//! `Process`. The stack pointer points at the process stack **only** between
//! [`run_on_stack`]'s switch-in and its switch-out (or the matching
//! [`swap_context`]), and never while any Rust frame is live above it.*
//!
//! # Contracts the asm relies on
//!
//! - **Alignment.** Cranelift requires 16-byte stack alignment
//!   unconditionally, and SysV requires `rsp % 16 == 0` immediately before a
//!   `call` so the callee sees `rsp ≡ 8 (mod 16)` at its first instruction
//!   (AAPCS64: `sp ≡ 0 (mod 16)` always). Region tops are 16-aligned
//!   ([`super::slab::StackHandle::top`]) and every switch below re-aligns
//!   before its call; `alignment_contract` in the tests pins this.
//! - **The old stack pointer is saved on the stack being entered**, not in
//!   scheduler-side state: that is what makes a parked stack self-contained
//!   and migratable — resuming needs nothing from the thread that parked it.
//! - **The pinned register** (x86_64: r15, the only register Cranelift's
//!   `enable_pinned_reg` offers; aarch64: x21, Cranelift's aarch64 pinned
//!   register) is loaded with the process pointer on entry. It is
//!   callee-saved in both ABIs, so ordinary Rust shims preserve it for free.

#![allow(unsafe_code)]

use std::arch::naked_asm;
use std::cell::Cell;
use std::ffi::c_void;
use std::ptr;

/// The signature every function entered on a switched stack must have.
/// `extern "C"` is load-bearing: rustc emits a non-unwinding shim on these,
/// so a panic inside aborts instead of unwinding across the switch.
pub type Entry = unsafe extern "C" fn(*mut c_void) -> u64;

/// A parked computation: the stack pointer of a suspended stack whose top
/// frames are the callee-saved registers [`swap_context`] pushed. Plain
/// data — everything else the resume needs lives *on* the parked stack,
/// which is what makes the pair (stack region, `SavedContext`) a complete,
/// self-contained continuation.
#[repr(C)]
#[derive(Debug)]
pub struct SavedContext {
    sp: *mut u8,
}

// SAFETY: the sp names memory inside a stack region owned by the same
// `Process` that owns this context; moving both to another OS thread moves
// exclusive ownership of everything the sp can reach. This impl is one half
// of the process-stack purity invariant (see the module doc in `stack`):
// it is sound ONLY while no word on the parked stack names anything owned
// by the scheduler that parked it.
unsafe impl Send for SavedContext {}

impl SavedContext {
    /// A context that has never parked anything. Swapping into it is a bug;
    /// the null sp faults immediately rather than corrupting.
    pub fn empty() -> Self {
        SavedContext {
            sp: ptr::null_mut(),
        }
    }
}

thread_local! {
    /// The scheduler C-stack switch point, published by [`run_on_stack`] on
    /// every entry: the value the stack pointer had the instant it left the
    /// C stack. Everything below it on the C stack is dead space, which is
    /// exactly what [`call_on_sched_stack`] runs shims on. Thread-local
    /// because it names scheduler-owned memory: it must be re-read after
    /// every suspension point, never carried in a frame or a register
    /// (process-stack purity), so a process resumed on another scheduler
    /// switches to *that* scheduler's C stack by construction.
    static SCHED_SP: Cell<*mut u8> = const { Cell::new(ptr::null_mut()) };
}

/// Run `entry(arg)` with the stack pointer moved onto a process stack and
/// the process pointer pinned (r15 / x21). This is the process ↔ scheduler
/// switch for the ordinary, non-parking path: `entry` runs to completion and
/// its return switches back. The scheduler-side switch point is saved on the
/// process stack (self-containment) *and* published to this thread's
/// [`SCHED_SP`] for [`call_on_sched_stack`].
///
/// All callee-saved registers are saved on the *caller's* (scheduler's)
/// stack — they describe the scheduler's frames, not the process's.
///
/// # Safety
/// `new_sp` must be the 16-aligned top of a live, writable region with
/// nothing else running on it ([`super::slab::StackHandle::top`]); `proc`
/// must outlive the call; `entry` must not unwind and must not itself move
/// the stack pointer off this stack except through the primitives here.
pub unsafe fn run_on_stack(
    proc: *mut c_void,
    new_sp: *mut u8,
    entry: Entry,
    arg: *mut c_void,
) -> u64 {
    let sched_slot = SCHED_SP.with(Cell::as_ptr);
    // Entries NEST: a shim runs on the C stack, re-enters the interpreter,
    // and the interpreter calls a compiled body, which lands here again. The
    // inner entry publishes a switch point deeper down the C stack; without
    // restoring the outer one on the way out, a *flat* loop of
    // native->shim->interp->native round-trips would republish one frame-pair
    // lower every iteration and walk the scheduler's thread stack to death —
    // with no guard page there to name it. Depth must be a stack discipline,
    // not a ratchet.
    let prev = SCHED_SP.with(Cell::get);
    let restore = RestoreSchedSp(prev);
    // SAFETY: forwarded contract; `sched_slot` is this thread's live TLS
    // slot for the duration of the call.
    let out = unsafe { run_on_stack_raw(proc, new_sp, entry, arg, sched_slot) };
    drop(restore);
    out
}

/// Puts the enclosing entry's switch point back, on every exit path
/// including an abort-in-progress — a leaked inner switch point is a silent
/// stack walk, so this must not depend on reaching the end of a function.
struct RestoreSchedSp(*mut u8);

impl Drop for RestoreSchedSp {
    fn drop(&mut self) {
        SCHED_SP.with(|c| c.set(self.0));
    }
}

/// [`run_on_stack`] without the TLS publication — the raw switch.
///
/// # Safety
/// As [`run_on_stack`]; additionally `saved_sp_out` must be valid to write.
#[cfg(target_arch = "x86_64")]
#[unsafe(naked)]
pub unsafe extern "C" fn run_on_stack_raw(
    proc: *mut c_void,
    new_sp: *mut u8,
    entry: Entry,
    arg: *mut c_void,
    saved_sp_out: *mut *mut u8,
) -> u64 {
    // rdi = proc, rsi = new_sp, rdx = entry, rcx = arg, r8 = saved_sp_out
    //
    // Entry rsp ≡ 8 (mod 16); six pushes (48 bytes) keep it ≡ 8. new_sp is
    // 16-aligned; `sub rsp, 16` leaves rsp ≡ 0 at the call, so the callee
    // sees ≡ 8 — a normal SysV entry.
    naked_asm!(
        "push rbx",
        "push rbp",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        "mov r11, rsp", // r11 is caller-saved: free scratch
        "mov rsp, rsi", // switch to the process stack
        "sub rsp, 16",  // slot for the saved sp; keeps rsp % 16 == 0 at the call
        "mov [rsp], r11",
        "mov [r8], r11", // publish the switch point (SCHED_SP)
        "mov r15, rdi",  // pin the process
        "mov rdi, rcx",  // arg -> first parameter
        "call rdx",
        "mov r11, [rsp]",
        "mov rsp, r11", // back to the scheduler stack
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbp",
        "pop rbx",
        "ret",
    )
}

/// # Safety
/// As [`run_on_stack`]; additionally `saved_sp_out` must be valid to write.
#[cfg(target_arch = "aarch64")]
#[unsafe(naked)]
pub unsafe extern "C" fn run_on_stack_raw(
    proc: *mut c_void,
    new_sp: *mut u8,
    entry: Entry,
    arg: *mut c_void,
    saved_sp_out: *mut *mut u8,
) -> u64 {
    // x0 = proc, x1 = new_sp, x2 = entry, x3 = arg, x4 = saved_sp_out
    //
    // sp stays 16-aligned throughout (AAPCS64 hardware requirement). The
    // callee-saved set is x19-x28, x29 (fp), x30 (lr), d8-d15: 160 bytes.
    naked_asm!(
        "stp x29, x30, [sp, #-160]!",
        "stp x19, x20, [sp, #16]",
        "stp x21, x22, [sp, #32]",
        "stp x23, x24, [sp, #48]",
        "stp x25, x26, [sp, #64]",
        "stp x27, x28, [sp, #80]",
        "stp d8, d9, [sp, #96]",
        "stp d10, d11, [sp, #112]",
        "stp d12, d13, [sp, #128]",
        "stp d14, d15, [sp, #144]",
        "mov x9, sp",
        "sub x10, x1, #16", // slot for the saved sp, sp stays 16-aligned
        "mov sp, x10",
        "str x9, [sp]",
        "str x9, [x4]", // publish the switch point (SCHED_SP)
        "mov x21, x0",  // pin the process
        "mov x0, x3",   // arg -> first parameter
        "blr x2",
        "ldr x9, [sp]",
        "mov sp, x9", // back to the scheduler stack
        "ldp x19, x20, [sp, #16]",
        "ldp x21, x22, [sp, #32]",
        "ldp x23, x24, [sp, #48]",
        "ldp x25, x26, [sp, #64]",
        "ldp x27, x28, [sp, #80]",
        "ldp d8, d9, [sp, #96]",
        "ldp d10, d11, [sp, #112]",
        "ldp d12, d13, [sp, #128]",
        "ldp d14, d15, [sp, #144]",
        "ldp x29, x30, [sp], #160",
        "ret",
    )
}

/// Run `f(arg)` back on the scheduler's C stack — the shim bracket
/// (`enter_runtime`/`leave_runtime`, C4): every Rust shim a compiled body
/// calls runs here, so the process stack only ever holds Cranelift frames,
/// whose sizes Cranelift reports at compile time. The target sp is this
/// thread's [`SCHED_SP`] switch point; everything below it on the C stack is
/// dead space belonging to no live frame. This wrapper is the single place a
/// scheduler-owned address enters compiled-code-adjacent execution, and a
/// call through it can never be parked.
///
/// # Safety
/// Must be called while running on a process stack entered through
/// [`run_on_stack`] on this thread (so `SCHED_SP` is live); `f` must not
/// unwind and must return without moving the stack pointer.
//
// `inline(never)`: the compiler assumes a function never changes threads,
// so if this body is inlined into a caller whose frame spans a park, the
// TLS address for SCHED_SP can be computed before the park and restored —
// stale — by the cross-thread resume. A real call re-derives it from the
// thread pointer at execution time, on whichever thread is running now.
#[inline(never)]
pub unsafe fn call_on_sched_stack(f: Entry, arg: *mut c_void) -> u64 {
    let sched_sp = SCHED_SP.with(Cell::get);
    debug_assert!(
        !sched_sp.is_null(),
        "call_on_sched_stack outside a run_on_stack entry"
    );
    // SAFETY: sched_sp is the switch point run_on_stack published on this
    // thread; the space below it is dead until run_on_stack returns.
    unsafe { call_on_c_stack_raw(sched_sp, f, arg) }
}

/// Resume a parked context, publishing THIS thread's switch point first.
///
/// [`swap_context`] is the raw register/sp exchange: it restores the
/// callee-saved registers the parked context saved, including the pinned
/// register and — critically — leaving [`SCHED_SP`] untouched. A process
/// parked on scheduler A and resumed on scheduler B would therefore run with
/// B's TLS still holding whatever switch point B's last *completed*
/// `run_on_stack` left behind, which sits ABOVE B's current C-stack depth.
/// The resumed body's first [`call_on_sched_stack`] would then set
/// `rsp` to that stale address and the shim's frames would overwrite every
/// live frame beneath it — including the one that called us. Re-reading a
/// stale thread-local is not re-derivation.
///
/// This is the only sanctioned way to enter a [`SavedContext`]: it saves the
/// current context into `save`, publishes the switch point that `save`
/// occupies, and hands control to `resume`.
///
/// # Safety
/// As [`swap_context`]: both pointers must be valid, `resume` must name a
/// context parked by this module, and the resumed stack must be live.
pub unsafe fn resume_on_stack(save: *mut SavedContext, resume: *const SavedContext) {
    let sched_slot = SCHED_SP.with(Cell::as_ptr);
    // The switch point must be the sp this thread parks at, which only the
    // asm can see — it is the very value `swap_context` writes into `save`.
    // Publishing anything computable from Rust here is wrong: `save` itself
    // is a `SavedContext` living in some *caller's* frame, which on a
    // migrated resume is a frame on the wrong thread's stack entirely.
    // SAFETY: forwarded contract; `sched_slot` is this thread's live TLS slot.
    unsafe { resume_context_raw(save, resume, sched_slot) }
}

/// [`swap_context`] plus one store: publish the parking sp as this thread's
/// [`SCHED_SP`] switch point. This is the resume half of the C4 contract —
/// everything below the returned sp is dead C stack that a shim may use.
///
/// # Safety
/// As [`swap_context`]; additionally `sched_slot` must be valid to write.
#[cfg(target_arch = "x86_64")]
#[unsafe(naked)]
unsafe extern "C" fn resume_context_raw(
    save: *mut SavedContext,
    resume: *const SavedContext,
    sched_slot: *mut *mut u8,
) {
    // rdi = save, rsi = resume, rdx = sched_slot. Identical to swap_context
    // but for `mov [rdx], rsp`: the saved sp IS the switch point.
    naked_asm!(
        "push rbp",
        "push rbx",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        "mov [rdi], rsp",
        "mov [rdx], rsp", // publish the switch point (SCHED_SP)
        "mov rsp, [rsi]",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbx",
        "pop rbp",
        "ret",
    )
}

/// # Safety
/// As the x86_64 twin.
#[cfg(target_arch = "aarch64")]
#[unsafe(naked)]
unsafe extern "C" fn resume_context_raw(
    save: *mut SavedContext,
    resume: *const SavedContext,
    sched_slot: *mut *mut u8,
) {
    // x0 = save, x1 = resume, x2 = sched_slot.
    naked_asm!(
        "stp x29, x30, [sp, #-160]!",
        "stp x19, x20, [sp, #16]",
        "stp x21, x22, [sp, #32]",
        "stp x23, x24, [sp, #48]",
        "stp x25, x26, [sp, #64]",
        "stp x27, x28, [sp, #80]",
        "stp d8, d9, [sp, #96]",
        "stp d10, d11, [sp, #112]",
        "stp d12, d13, [sp, #128]",
        "stp d14, d15, [sp, #144]",
        "mov x9, sp",
        "str x9, [x0]",
        "str x9, [x2]", // publish the switch point (SCHED_SP)
        "ldr x9, [x1]",
        "mov sp, x9",
        "ldp x19, x20, [sp, #16]",
        "ldp x21, x22, [sp, #32]",
        "ldp x23, x24, [sp, #48]",
        "ldp x25, x26, [sp, #64]",
        "ldp x27, x28, [sp, #80]",
        "ldp d8, d9, [sp, #96]",
        "ldp d10, d11, [sp, #112]",
        "ldp d12, d13, [sp, #128]",
        "ldp d14, d15, [sp, #144]",
        "ldp x29, x30, [sp], #160",
        "ret",
    )
}

/// # Safety
/// `c_sp` must be a dead-below switch point on this thread's C stack with
/// the alignment [`run_on_stack_raw`] produced (x86_64: ≡ 8 mod 16;
/// aarch64: ≡ 0 mod 16); `f` must not unwind.
#[cfg(target_arch = "x86_64")]
#[unsafe(naked)]
pub unsafe extern "C" fn call_on_c_stack_raw(c_sp: *mut u8, f: Entry, arg: *mut c_void) -> u64 {
    // rdi = c_sp, rsi = f, rdx = arg. c_sp ≡ 8 (mod 16) — the push realigns
    // to 0 at the call.
    naked_asm!(
        "mov r11, rsp",
        "mov rsp, rdi",
        "push r11", // save the process-side sp on the C stack
        "mov rdi, rdx",
        "call rsi",
        "pop r11",
        "mov rsp, r11", // back to the process stack
        "ret",
    )
}

/// # Safety
/// As the x86_64 twin, with c_sp ≡ 0 (mod 16).
#[cfg(target_arch = "aarch64")]
#[unsafe(naked)]
pub unsafe extern "C" fn call_on_c_stack_raw(c_sp: *mut u8, f: Entry, arg: *mut c_void) -> u64 {
    // x0 = c_sp, x1 = f, x2 = arg.
    naked_asm!(
        "mov x9, sp",
        "sub x10, x0, #16",
        "mov sp, x10",
        "stp x9, x30, [sp]", // process-side sp and our return address
        "mov x0, x2",
        "blr x1",
        "ldp x9, x30, [sp]",
        "mov sp, x9", // back to the process stack
        "ret",
    )
}

/// Symmetric context switch — the park and the resume are the same
/// operation. Saves the callee-saved registers and the return address on the
/// *current* stack, records the resulting sp in `save`, then installs the sp
/// from `resume` and returns into whatever [`swap_context`] (or
/// [`prepare_stack`] seed) produced it. The call "returns" when some later
/// swap resumes `save`.
///
/// # Safety
/// `resume` must hold a context produced by a prior `swap_context` save or
/// [`prepare_stack`], whose stack region is live and not currently running;
/// `save` must be valid to write. The current stack must be one it is sound
/// to suspend (nothing above the caller may be borrowed by the code being
/// resumed).
#[cfg(target_arch = "x86_64")]
#[unsafe(naked)]
pub unsafe extern "C" fn swap_context(save: *mut SavedContext, resume: *const SavedContext) {
    // rdi = save, rsi = resume. Layout under the saved sp, low to high:
    // r15 r14 r13 r12 rbx rbp <return address>.
    naked_asm!(
        "push rbp",
        "push rbx",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        "mov [rdi], rsp",
        "mov rsp, [rsi]",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbx",
        "pop rbp",
        "ret",
    )
}

/// # Safety
/// As the x86_64 twin.
#[cfg(target_arch = "aarch64")]
#[unsafe(naked)]
pub unsafe extern "C" fn swap_context(save: *mut SavedContext, resume: *const SavedContext) {
    // x0 = save, x1 = resume. Frame layout (160 bytes): [+0] x29, [+8] x30,
    // [+16..] x19-x28, [+96..] d8-d15. `ret` continues at the restored x30.
    naked_asm!(
        "stp x29, x30, [sp, #-160]!",
        "stp x19, x20, [sp, #16]",
        "stp x21, x22, [sp, #32]",
        "stp x23, x24, [sp, #48]",
        "stp x25, x26, [sp, #64]",
        "stp x27, x28, [sp, #80]",
        "stp d8, d9, [sp, #96]",
        "stp d10, d11, [sp, #112]",
        "stp d12, d13, [sp, #128]",
        "stp d14, d15, [sp, #144]",
        "mov x9, sp",
        "str x9, [x0]",
        "ldr x9, [x1]",
        "mov sp, x9",
        "ldp x19, x20, [sp, #16]",
        "ldp x21, x22, [sp, #32]",
        "ldp x23, x24, [sp, #48]",
        "ldp x25, x26, [sp, #64]",
        "ldp x27, x28, [sp, #80]",
        "ldp d8, d9, [sp, #96]",
        "ldp d10, d11, [sp, #112]",
        "ldp d12, d13, [sp, #128]",
        "ldp d14, d15, [sp, #144]",
        "ldp x29, x30, [sp], #160",
        "ret",
    )
}

/// The landing pad a fresh context's first [`swap_context`] returns into:
/// moves the seeded entry/arg out of callee-saved registers and calls the
/// entry. A prepared entry must never return — it parks with
/// [`swap_context`] and is resumed by construction; if it does return there
/// is no frame below it to return to, so trap immediately rather than
/// walk off the stack.
#[cfg(target_arch = "x86_64")]
#[unsafe(naked)]
unsafe extern "C" fn context_bootstrap() {
    // Seeded by prepare_stack: r14 = entry, r13 = arg. On landing rsp ≡ 8
    // (mod 16) — a normal entry — so one push realigns for the call.
    naked_asm!("mov rdi, r13", "sub rsp, 8", "call r14", "ud2")
}

/// As the x86_64 twin.
#[cfg(target_arch = "aarch64")]
#[unsafe(naked)]
unsafe extern "C" fn context_bootstrap() {
    // Seeded by prepare_stack: x19 = entry, x20 = arg. sp lands 16-aligned.
    naked_asm!("mov x0, x20", "blr x19", "brk #0x1")
}

/// Seed a fresh, never-run stack so the first [`swap_context`] into the
/// returned context starts `entry(arg)` on it (via [`context_bootstrap`]).
///
/// # Safety
/// `top` must be the 16-aligned top of a live, writable, unaliased region
/// (nothing else may run on it until the context is dead).
pub unsafe fn prepare_stack(top: *mut u8, entry: Entry, arg: *mut c_void) -> SavedContext {
    // SAFETY: writes land in [top - FRAME, top), inside the caller's region.
    unsafe {
        #[cfg(target_arch = "x86_64")]
        {
            // Mirror swap_context's restore order (see its layout comment).
            let w = |i: usize, v: u64| top.cast::<u64>().sub(i).write(v);
            let bootstrap: unsafe extern "C" fn() = context_bootstrap;
            w(1, 0); // backtrace terminator
            w(2, bootstrap as usize as u64); // ret target
            w(3, 0); // rbp
            w(4, 0); // rbx
            w(5, 0); // r12
            w(6, arg as u64); // r13
            w(7, entry as usize as u64); // r14
            w(8, 0); // r15
            SavedContext { sp: top.sub(8 * 8) }
        }
        #[cfg(target_arch = "aarch64")]
        {
            // Mirror swap_context's frame: 160 bytes below top.
            let base = top.sub(160).cast::<u64>();
            let bootstrap: unsafe extern "C" fn() = context_bootstrap;
            for i in 0..20 {
                base.add(i).write(0);
            }
            base.add(1).write(bootstrap as usize as u64); // x30
            base.add(2).write(entry as usize as u64); // x19
            base.add(3).write(arg as u64); // x20
            SavedContext {
                sp: base.cast::<u8>(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::slab::{STACK_BYTES, StackPool};
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    // §5.4/4 — the callee observes the ABI's entry alignment on the
    // switched stack: rsp ≡ 8 (mod 16) on x86_64 (return address pushed),
    // sp ≡ 0 (mod 16) on aarch64. A naked probe returns the raw sp before
    // any prologue can hide it.
    #[cfg(target_arch = "x86_64")]
    #[unsafe(naked)]
    unsafe extern "C" fn sp_probe(_arg: *mut c_void) -> u64 {
        naked_asm!("mov rax, rsp", "ret")
    }
    #[cfg(target_arch = "aarch64")]
    #[unsafe(naked)]
    unsafe extern "C" fn sp_probe(_arg: *mut c_void) -> u64 {
        naked_asm!("mov x0, sp", "ret")
    }

    #[test]
    fn alignment_contract() {
        let mut pool = StackPool::new();
        let h = pool.acquire(1).expect("stack");
        let sp = unsafe { run_on_stack(ptr::null_mut(), h.top(), sp_probe, ptr::null_mut()) };
        #[cfg(target_arch = "x86_64")]
        assert_eq!(sp % 16, 8, "SysV callee entry alignment");
        #[cfg(target_arch = "aarch64")]
        assert_eq!(sp % 16, 0, "AAPCS64 callee entry alignment");
        // ...and the callee really was on the process stack.
        let top = h.top() as u64;
        assert!(sp < top && sp > top - STACK_BYTES as u64);
        pool.release(h);
    }

    // §5.4/1 — run on a switched stack, write a sentinel through the arg,
    // return a value; sp is implicitly proven restored by returning here,
    // and the callee-saved contract is proven by the sentinel test below.
    unsafe extern "C" fn write_and_return(arg: *mut c_void) -> u64 {
        unsafe { *arg.cast::<u64>() = 0xC0FFEE };
        0xF00D
    }

    #[test]
    fn switch_to_stack_run_and_return() {
        let mut pool = StackPool::new();
        let h = pool.acquire(1).expect("stack");
        let mut sentinel: u64 = 0;
        let out = unsafe {
            run_on_stack(
                ptr::null_mut(),
                h.top(),
                write_and_return,
                (&raw mut sentinel).cast(),
            )
        };
        assert_eq!(out, 0xF00D);
        assert_eq!(sentinel, 0xC0FFEE);
        pool.release(h);
    }

    // §5.4/1's register half — every callee-saved register round-trips a
    // known value across the whole switch cycle. The checker is itself
    // naked: it plants sentinels, calls `f` (which does a full
    // run_on_stack), and returns a bitmask of corrupted registers.
    #[cfg(target_arch = "x86_64")]
    #[unsafe(naked)]
    unsafe extern "C" fn callee_saved_survive(f: unsafe extern "C" fn() -> u64) -> u64 {
        naked_asm!(
            "push rbx",
            "push rbp",
            "push r12",
            "push r13",
            "push r14",
            "push r15",
            "mov rbx, 0x1B1B1B1B",
            "mov r12, 0x12121212",
            "mov r13, 0x13131313",
            "mov r14, 0x14141414",
            "mov r15, 0x15151515",
            "sub rsp, 8",
            "call rdi",
            "add rsp, 8",
            "xor rax, rax",
            "cmp rbx, 0x1B1B1B1B",
            "je 2f",
            "or rax, 1",
            "2:",
            "cmp r12, 0x12121212",
            "je 3f",
            "or rax, 2",
            "3:",
            "cmp r13, 0x13131313",
            "je 4f",
            "or rax, 4",
            "4:",
            "cmp r14, 0x14141414",
            "je 5f",
            "or rax, 8",
            "5:",
            "cmp r15, 0x15151515",
            "je 6f",
            "or rax, 16",
            "6:",
            "pop r15",
            "pop r14",
            "pop r13",
            "pop r12",
            "pop rbp",
            "pop rbx",
            "ret",
        )
    }
    #[cfg(target_arch = "aarch64")]
    #[unsafe(naked)]
    unsafe extern "C" fn callee_saved_survive(f: unsafe extern "C" fn() -> u64) -> u64 {
        // Sentinel x19-x28 and d8/d15 (the ends of the fp range), call f,
        // OR a bit per corrupted register into x0.
        naked_asm!(
            "stp x29, x30, [sp, #-96]!",
            "stp x19, x20, [sp, #16]",
            "stp x21, x22, [sp, #32]",
            "stp x23, x24, [sp, #48]",
            "stp x25, x26, [sp, #64]",
            "stp x27, x28, [sp, #80]",
            "mov x19, #0x19",
            "mov x20, #0x20",
            "mov x21, #0x21",
            "mov x22, #0x22",
            "mov x23, #0x23",
            "mov x24, #0x24",
            "mov x25, #0x25",
            "mov x26, #0x26",
            "mov x27, #0x27",
            "mov x28, #0x28",
            "mov x9, #0xD8",
            "fmov d8, x9",
            "mov x9, #0xD15",
            "fmov d15, x9",
            "mov x9, x0",
            "blr x9",
            "mov x0, #0",
            "cmp x19, #0x19",
            "cinc x0, x0, ne",
            "lsl x0, x0, #1",
            "cmp x20, #0x20",
            "cinc x0, x0, ne",
            "lsl x0, x0, #1",
            "cmp x21, #0x21",
            "cinc x0, x0, ne",
            "lsl x0, x0, #1",
            "cmp x22, #0x22",
            "cinc x0, x0, ne",
            "lsl x0, x0, #1",
            "cmp x23, #0x23",
            "cinc x0, x0, ne",
            "lsl x0, x0, #1",
            "cmp x24, #0x24",
            "cinc x0, x0, ne",
            "lsl x0, x0, #1",
            "cmp x25, #0x25",
            "cinc x0, x0, ne",
            "lsl x0, x0, #1",
            "cmp x26, #0x26",
            "cinc x0, x0, ne",
            "lsl x0, x0, #1",
            "cmp x27, #0x27",
            "cinc x0, x0, ne",
            "lsl x0, x0, #1",
            "cmp x28, #0x28",
            "cinc x0, x0, ne",
            "lsl x0, x0, #1",
            "fmov x9, d8",
            "cmp x9, #0xD8",
            "cinc x0, x0, ne",
            "lsl x0, x0, #1",
            "fmov x9, d15",
            "mov x10, #0xD15",
            "cmp x9, x10",
            "cinc x0, x0, ne",
            "ldp x19, x20, [sp, #16]",
            "ldp x21, x22, [sp, #32]",
            "ldp x23, x24, [sp, #48]",
            "ldp x25, x26, [sp, #64]",
            "ldp x27, x28, [sp, #80]",
            "ldp x29, x30, [sp], #96",
            "ret",
        )
    }

    unsafe extern "C" fn nop_entry(_arg: *mut c_void) -> u64 {
        7
    }

    unsafe extern "C" fn full_cycle() -> u64 {
        let mut pool = StackPool::new();
        let h = pool.acquire(1).expect("stack");
        let out = unsafe { run_on_stack(ptr::null_mut(), h.top(), nop_entry, ptr::null_mut()) };
        pool.release(h);
        out
    }

    #[test]
    fn callee_saved_registers_round_trip() {
        let corrupted = unsafe { callee_saved_survive(full_cycle) };
        assert_eq!(corrupted, 0, "bitmask of corrupted callee-saved registers");
    }

    // §5.4/2 — park on a process stack and resume it: locals on the parked
    // stack survive byte-for-byte across the suspension.
    struct ParkState {
        main: SavedContext,
        proc: SavedContext,
        log: Vec<u64>,
    }

    unsafe extern "C" fn parking_entry(arg: *mut c_void) -> u64 {
        let st = unsafe { &mut *arg.cast::<ParkState>() };
        // Locals that must survive the park, byte-for-byte.
        let a: u64 = 0xAAAA_BBBB_CCCC_DDDD;
        let b: [u8; 9] = [1, 2, 3, 4, 5, 6, 7, 8, 9];
        st.log.push(1);
        // Park: switch back to the scheduler context.
        unsafe { swap_context(&raw mut st.proc, &raw const st.main) };
        // Resumed. The parked frame's locals are intact.
        assert_eq!(a, 0xAAAA_BBBB_CCCC_DDDD);
        assert_eq!(b, [1, 2, 3, 4, 5, 6, 7, 8, 9]);
        st.log.push(2);
        // Park forever; the test never resumes this context again.
        unsafe { swap_context(&raw mut st.proc, &raw const st.main) };
        unreachable!("parked context resumed after the test ended");
    }

    #[test]
    fn park_and_resume_across_a_switch() {
        let mut pool = StackPool::new();
        let h = pool.acquire(1).expect("stack");
        let mut st = ParkState {
            main: SavedContext::empty(),
            proc: SavedContext::empty(),
            log: Vec::new(),
        };
        let seeded = unsafe { prepare_stack(h.top(), parking_entry, (&raw mut st).cast()) };
        st.proc = seeded;
        // First swap starts the entry; it parks and control returns here.
        unsafe { swap_context(&raw mut st.main, &raw const st.proc) };
        assert_eq!(st.log, vec![1]);
        // Resume; it checks its locals, logs, parks again.
        unsafe { swap_context(&raw mut st.main, &raw const st.proc) };
        assert_eq!(st.log, vec![1, 2]);
        pool.release(h);
    }

    // §5.4/3 — the F1 regression shape: park on thread A, move the whole
    // parked continuation to thread B, resume there. The resumed code must
    // see thread B's thread-local state, never a value captured on A —
    // scheduler-derived state is re-derived after every suspension point.
    thread_local! {
        static THREAD_TAG: AtomicU64 = const { AtomicU64::new(0) };
    }

    /// The sanctioned way to read TLS from code that spans a park: a real
    /// call, so the address is re-derived on the executing thread. Read
    /// inline and the compiler may compute the TLS address before the park
    /// and restore it — stale — across a cross-thread resume, which is the
    /// very bug this test exists to catch in the runtime, not in itself.
    #[inline(never)]
    fn read_thread_tag() -> u64 {
        THREAD_TAG.with(|t| t.load(Ordering::Relaxed))
    }

    struct CrossState {
        sched: SavedContext,
        proc: SavedContext,
        seen_tag: AtomicU64,
        /// Address of a local inside a shim invoked via
        /// `call_on_sched_stack` AFTER the cross-thread resume. It must land
        /// on the resuming thread's C stack; if the resume did not republish
        /// the switch point it lands on the parking thread's, which is the
        /// bug.
        shim_local: AtomicU64,
    }

    /// Runs on the C stack via `call_on_sched_stack`; reports the address of
    /// one of its own locals so the caller can tell whose C stack it got.
    unsafe extern "C" fn where_am_i(arg: *mut c_void) -> u64 {
        let probe = 0u64;
        let st = unsafe { &*arg.cast::<CrossState>() };
        st.shim_local
            .store((&raw const probe) as u64, Ordering::Relaxed);
        0
    }

    unsafe extern "C" fn cross_entry(arg: *mut c_void) -> u64 {
        let st = unsafe { &*arg.cast::<CrossState>() };
        // Read this thread's tag, park, read again after resume.
        let before = read_thread_tag();
        assert_eq!(before, 0xA);
        let stp = arg.cast::<CrossState>();
        unsafe { swap_context(&raw mut (*stp).proc, &raw const st.sched) };
        // Resumed — on thread B. Re-derivation must observe B, not A.
        let after = read_thread_tag();
        st.seen_tag.store(after, Ordering::Relaxed);
        // The load-bearing half: a shim call from the resumed process stack.
        // This is what a compiled body does first, and it reads SCHED_SP. If
        // the resume did not republish it, this switches to thread A's stale
        // switch point and scribbles over thread B's live frames.
        unsafe { call_on_sched_stack(where_am_i, arg) };
        unsafe { swap_context(&raw mut (*stp).proc, &raw const st.sched) };
        unreachable!("parked context resumed after the test ended");
    }

    #[test]
    fn resume_on_a_different_thread() {
        let mut pool = StackPool::new();
        let h = pool.acquire(1).expect("stack");
        let mut st = CrossState {
            sched: SavedContext::empty(),
            proc: SavedContext::empty(),
            seen_tag: AtomicU64::new(0),
            shim_local: AtomicU64::new(0),
        };
        st.proc = unsafe { prepare_stack(h.top(), cross_entry, (&raw mut st).cast()) };

        // Thread A: start the context; it parks.
        THREAD_TAG.with(|t| t.store(0xA, Ordering::Relaxed));
        unsafe { resume_on_stack(&raw mut st.sched, &raw const st.proc) };

        // Hand the parked continuation (stack + context) to thread B and
        // resume it there — the same plain move a migrating Process makes.
        // Addresses cross as integers so no `&mut` aliases the state the
        // parked context itself writes through.
        let sched_addr = (&raw mut st.sched) as usize;
        let proc_addr = (&raw const st.proc) as usize;
        let b_frame = std::sync::atomic::AtomicU64::new(0);
        std::thread::scope(|s| {
            s.spawn(|| {
                THREAD_TAG.with(|t| t.store(0xB, Ordering::Relaxed));
                // A local in THIS thread's frame: the yardstick for "B's C
                // stack". The resumed shim must land near it, not near A's.
                let here = 0u64;
                b_frame.store((&raw const here) as u64, Ordering::Relaxed);
                unsafe {
                    resume_on_stack(
                        sched_addr as *mut SavedContext,
                        proc_addr as *const SavedContext,
                    )
                };
            });
        });
        assert_eq!(
            st.seen_tag.load(Ordering::Relaxed),
            0xB,
            "resumed code must observe the resuming thread's state"
        );
        // Thread stacks are megabytes apart; same-stack frames are within a
        // few KB. Before resume_on_stack published the switch point, the shim
        // ran on thread A's stack and this distance was enormous — or the
        // TLS was null and release built `mov rsp, 0`.
        let shim = st.shim_local.load(Ordering::Relaxed);
        let b = b_frame.load(Ordering::Relaxed);
        assert!(shim != 0, "the shim never ran");
        assert!(
            shim.abs_diff(b) < 1 << 20,
            "a shim called after a cross-thread resume ran on the wrong C stack: \
             shim local {shim:#x}, resuming thread's frame {b:#x}"
        );
        pool.release(h);
    }

    // The C4 bracket: a shim call from a process stack runs on the C stack
    // (above the switch point) and returns to the process stack.
    static C_STACK_SP: AtomicU64 = AtomicU64::new(0);
    static PROC_STACK_SP: AtomicU64 = AtomicU64::new(0);

    unsafe extern "C" fn shim_probe(arg: *mut c_void) -> u64 {
        let x = 0u8;
        C_STACK_SP.store(&raw const x as u64, Ordering::Relaxed);
        arg as u64
    }

    unsafe extern "C" fn bracket_entry(arg: *mut c_void) -> u64 {
        let x = 0u8;
        PROC_STACK_SP.store(&raw const x as u64, Ordering::Relaxed);
        unsafe { call_on_sched_stack(shim_probe, arg) }
    }

    #[test]
    fn shims_run_on_the_scheduler_stack() {
        let mut pool = StackPool::new();
        let h = pool.acquire(1).expect("stack");
        let out = unsafe {
            run_on_stack(
                ptr::null_mut(),
                h.top(),
                bracket_entry,
                0x5A5A as *mut c_void,
            )
        };
        assert_eq!(out, 0x5A5A, "shim result crossed both switches");
        let proc_sp = PROC_STACK_SP.load(Ordering::Relaxed);
        let c_sp = C_STACK_SP.load(Ordering::Relaxed);
        let top = h.top() as u64;
        let bottom = top - STACK_BYTES as u64;
        assert!(
            proc_sp < top && proc_sp > bottom,
            "entry local must live on the process stack"
        );
        assert!(
            c_sp >= top || c_sp <= bottom,
            "shim local must NOT live on the process stack"
        );
        pool.release(h);
    }
}
