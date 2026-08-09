//! The VM half of the native calling convention: how JIT-compiled function
//! bodies and the interpreter hand a running process back and forth.
//!
//! The ABI itself — the [`NativeStatus`] word and the [`NativeEntry`]
//! signature `extern "C" fn(vmx: *mut c_void) -> NativeStatus` — is defined
//! once in [`crate::bytecode::native`] beside the per-program entry
//! table; the front end compiles bodies but cannot name the VM, so `vmx` is
//! opaque there. This module pins down what `vmx` actually is (`&mut VM`,
//! see [`VM::call_native`]) and round-trips the status word to the
//! interpreter's `VmResult<Step>` outcomes.
//!
//! The value-passing convention has no C-level arguments or return: the
//! caller has already installed the callee's [`CallFrame`](super::CallFrame)
//! and placed the arguments in the callee's frame slots at
//! `[base_slot, base_slot + arity)` — exactly the layout `enter_frame!`
//! produces in the interpreter (`exec.rs`) — and on [`NativeStatus::Done`]
//! the callee has already applied the interpreter's return protocol
//! (`stack.truncate(base_slot)`, push result, pop frame). Compiled code
//! keeps the interpreter's frame layout bit-for-bit, which is what makes
//! per-function fallback, preemption, and process migration work unchanged.
//!
//! Two of the four outcomes carry a payload the one-word status cannot
//! ([`Step::Parked`]'s `Wait`, and a `VmError`); [`VM::status_from_outcome`]
//! parks the payload in the VM while the status word unwinds the native
//! frames — every runtime call a compiled body makes that can suspend
//! compiles to `status = call(...); if status != Done { return status; }` —
//! and [`VM::outcome_from_status`] rehydrates it at the trampoline, which
//! then suspends, yields, or raises exactly as the interpreter's dispatch
//! loop does today. Resume re-enters at the top frame's `frame.ip`, which
//! for a native function is a resume-point index (`0` = enter from the top).
//!
//! The reverse direction — a compiled body calling an interpreter-only
//! function — goes through [`al_rt_enter_interp`], which runs
//! `execute_slice` with a *frame floor* at the caller's depth and encodes
//! the slice's outcome as the same status word. That shim is why the whole
//! unwind protocol must exist even though stage-A0 compiled code cannot
//! park by construction: an interpreted *callee* of a native caller can
//! park (the ten `io.rs` ops), and its `Parked` has to unwind the native
//! machine frames above it by plain returns before the scheduler can
//! `suspend_current` + `park` the process.
//!
//! # The five call kinds
//!
//! Every call a compiled body makes is one of five shapes; the `al_rt_*`
//! shims below are the runtime half of each, reached by symbol like the
//! [`native_shims`](super::native_shims) table (see [`rt_symbols`]):
//!
//! 1. **native → native (known):** [`al_rt_call`] — the caller's resume ip
//!    is stored in its own frame, the callee `CallFrame` is pushed exactly
//!    as `enter_frame!` does (capture-free sentinel, zeroed locals), the
//!    entry-table hit is called directly, and the returned status is tested:
//!    `Done` continues with the result, anything else unwinds.
//! 2. **native → interpreter-only fn:** the same [`al_rt_call`], whose
//!    table miss runs the callee under [`al_rt_enter_interp`]'s frame floor.
//! 3. **native → @vm op:** stage A2. The registry seam is already in place —
//!    one shim + one `(name, address)` row in the symbol table + one import
//!    (`vm::jit::RuntimeFns`) per op, nothing structural.
//! 4. **RC helpers:** stage A1 (`al_native_release_at_zero` and the value
//!    shims exist; A0 bodies only reach them through the dynamic drop gate).
//! 5. **tail calls:** a *self*-tail call is a native loop back-edge whose
//!    reds checkpoint is [`al_rt_checkpoint`] (yield resumes from the top —
//!    the frame's locals already are the next iteration's arguments, the
//!    interpreter's `TailCallSelf` contract). A *cross-function* tail call
//!    is trampoline-mediated: [`al_rt_tail_call`] collapses the caller frame
//!    in place (the `TailCallKnown` surgery) and returns
//!    [`NativeStatus::TailCall`], which the caller returns verbatim; the
//!    driver loop ([`VM::drive_top_frame`]) that invoked the function then
//!    dispatches the collapsed frame, so arbitrarily long tail chains run on
//!    a flat machine stack.
//!
//! Reduction parity across the boundary lives in `VM::native_reds`: entering
//! a function charges one reduction (plus accrued reclamation debt) exactly
//! like the interpreter's `checkpoint!`, whether the entry is a call, a tail
//! call, or a self-tail back-edge, and exhaustion yields with the callee
//! frame consistent at `ip == 0`.

use crate::FuncIdx;
use crate::bytecode::{NativeEntry, NativeStatus, Value};
use crate::tivec::Idx;

use super::poll::Wait;
use super::{CallFrame, Step, VM, VmError, VmResult};

thread_local! {
    /// The scheduler's VM for this OS thread, re-published at the top of
    /// every `scheduler_loop` iteration (C1 scaffolding). When compiled
    /// code stops carrying `vmx` in its frames — the machine-stack plan —
    /// every shim re-derives the VM from here instead, so a frame parked on
    /// thread A and resumed on thread B reads B's scheduler by
    /// construction; the migration corruption (a spilled `&mut VM` crossing
    /// threads inside a parked frame) becomes unrepresentable. Dormant
    /// today: nothing reads it yet except through [`current_vm`].
    static CURRENT_VM: std::cell::Cell<*mut VM> = const { std::cell::Cell::new(std::ptr::null_mut()) };
}

/// Publish `vm` as this scheduler thread's VM. Called once per
/// `scheduler_loop` iteration; the pointer is stable for the VM's lifetime,
/// so the repeated store is a discipline (re-derive after every suspension
/// point), not a correctness need today.
pub(super) fn set_current_vm(vm: *mut VM) {
    CURRENT_VM.with(|c| c.set(vm));
}

/// The VM last published on this thread. Null before the first
/// `scheduler_loop` iteration (or on a non-scheduler thread); the caller
/// owns the resulting aliasing obligations — this is the re-derivation
/// seam, and only the shim trampoline should ever dereference it.
pub fn current_vm() -> *mut VM {
    CURRENT_VM.with(std::cell::Cell::get)
}

/// The payload half of a [`NativeStatus::Parked`]/[`NativeStatus::Error`],
/// parked in the VM while the status word unwinds the native frames above
/// it. At most one is in flight per process: the trampoline consumes it
/// before the process suspends or the error propagates.
///
/// Always handled boxed: the variants are wide (a `Wait`, a `VmError`), the
/// slot is empty except mid-unwind, and the slot lives on every `Process` —
/// inline it would cost every parked process the payload's full width in
/// RSS (measured +76 B/proc at 100k parked). One allocation per park/error
/// unwind is noise next to the park itself.
pub(super) enum NativePending {
    Parked(Wait),
    Error(VmError),
}

impl VM {
    /// Invoke a compiled function body. The one place the entry's context
    /// is minted: `ctx.vm` is re-published as this scheduler's `&mut VM` on
    /// every invocation — the C1 re-derivation store, the reason a frame
    /// resumed on another scheduler can only ever observe that scheduler's
    /// VM — and the entry (or a shim it calls) is the only thing that
    /// dereferences it for the duration of the call.
    ///
    /// The caller must have completed the frame handshake first: callee
    /// `CallFrame` pushed, arguments in `[base_slot, base_slot + arity)`.
    ///
    /// Entered through [`call_entry_preserving_pinned`], which is the whole
    /// reason this is sound: `enable_pinned_reg` (jit.rs) hands the JIT the
    /// pinned register (x86_64 r15 / aarch64 x21) by *removing it from
    /// Cranelift's callee-save set* — every compiled entry writes it and
    /// never restores it. That register is callee-saved under SysV/AAPCS64,
    /// so Rust callers legitimately keep live values in it across this call.
    /// The shim brackets the indirect call with a save/restore, which is the
    /// contract the ABI already promised and the JIT silently stopped
    /// keeping. Without it the clobber lands in whichever caller's frame is
    /// unlucky — `execute_slice_budgeted` is 105 KB of register pressure and
    /// pushes the pinned register in its own prologue — and the symptom is a
    /// wrong answer, in release, with no test failing.
    ///
    /// `#[inline(never)]` is kept as a memory barrier: the entry reaches the
    /// VM only through the pointer stored in `native_ctx`, and while that
    /// escape is visible to LLVM's capture analysis, keeping the boundary
    /// opaque costs nothing and makes the reload obvious in the disassembly.
    /// It is NOT what makes the pinned register safe — that is the shim. An
    /// earlier revision believed the opposite and shipped the clobber.
    #[inline(never)]
    pub(super) fn call_native(&mut self, entry: NativeEntry) -> NativeStatus {
        self.native_ctx.vm = (self as *mut VM).cast();
        // SAFETY: `entry` is a finalized JIT entry registered in the native
        // table for this program, and the pointer is to this VM's live
        // `native_ctx` — the two arguments the shim forwards unchanged.
        #[allow(unsafe_code)]
        unsafe {
            crate::bytecode::native::call_entry_preserving_pinned(
                (&raw mut self.native_ctx).cast(),
                entry,
            )
        }
    }

    /// Encode a slice outcome as the one-word [`NativeStatus`] native
    /// frames unwind with, parking any payload (`Wait`, `VmError`) in the
    /// VM for [`VM::outcome_from_status`] to rehydrate.
    pub(super) fn status_from_outcome(&mut self, outcome: VmResult<Step>) -> NativeStatus {
        match outcome {
            Ok(Step::Done) => NativeStatus::Done,
            Ok(Step::Yield) => NativeStatus::Yield,
            Ok(Step::Parked(wait)) => {
                // A pending payload is consumed by the trampoline before any
                // new suspension can be produced; a leftover here means a
                // native frame swallowed a non-Done status.
                debug_assert!(self.native_pending.is_none());
                self.native_pending = Some(Box::new(NativePending::Parked(wait)));
                NativeStatus::Parked
            }
            Err(err) => {
                debug_assert!(self.native_pending.is_none());
                self.native_pending = Some(Box::new(NativePending::Error(err)));
                NativeStatus::Error
            }
        }
    }

    /// Decode a [`NativeStatus`] returned by native code back into the
    /// interpreter's outcome, rehydrating the pending payload.
    pub(super) fn outcome_from_status(&mut self, status: NativeStatus) -> VmResult<Step> {
        match status {
            NativeStatus::Done => Ok(Step::Done),
            NativeStatus::Yield => Ok(Step::Yield),
            NativeStatus::Parked => match self.native_pending.take().map(|p| *p) {
                Some(NativePending::Parked(wait)) => Ok(Step::Parked(wait)),
                _ => Err(VmError::internal(
                    "native code returned NativeStatus::Parked with no pending wait",
                )),
            },
            NativeStatus::Error => match self.native_pending.take().map(|p| *p) {
                Some(NativePending::Error(err)) => Err(err),
                _ => Err(VmError::internal(
                    "native code returned NativeStatus::Error with no pending error",
                )),
            },
            // Consumed by `drive_top_frame`; every entry invocation runs
            // under that trampoline, so this arm firing means a native frame
            // forwarded a status it was required to drive.
            NativeStatus::TailCall => Err(VmError::internal(
                "NativeStatus::TailCall escaped its trampoline",
            )),
        }
    }

    /// [`al_rt_enter_interp`]'s body: run the interpreter until the frame
    /// the native caller pushed returns, then encode the slice's outcome as
    /// the status word the native frames unwind with.
    fn enter_interp(&mut self) -> NativeStatus {
        // Re-entries nest (native → interp → native → interp …); each
        // restores the floor it found, so by the time a non-`Done` status
        // has unwound every native frame and reached `scheduler_loop`, the
        // floor is back at 0 — the whole-process floor the next slice (and
        // the resume-after-park slice) runs under.
        let saved = self.native_floor;
        debug_assert!(
            self.frames.len() > saved,
            "native→interp call without a pushed callee frame"
        );
        self.native_floor = self.frames.len() - 1;
        let floor = self.native_floor;
        // The nested slice resumes the caller's remaining budget — the
        // interp side of the `native_reds` contract — and its `Done` exit
        // writes the remainder back, so a native fn looping over interpreted
        // callees yields exactly as often as the all-interpreted program.
        let outcome = self.execute_slice_budgeted(self.native_reds);
        self.native_floor = saved;
        match outcome {
            // `Ret` popped the callee chain back to the floor: the callee
            // returned, and the return protocol is already applied (operands
            // truncated, result on top of the stack) — exactly the state a
            // `Done` promises the native caller.
            Ok(Step::Done) if self.frames.len() == floor => NativeStatus::Done,
            // `Step::Done` away from the floor is `Op::Halt`, which only
            // `__main__`'s body carries — and `__main__` is frame 0,
            // always-interpreted glue that can never sit above a native
            // caller. Unreachable by construction; refuse rather than hand
            // the native caller a stack it no longer owns.
            Ok(Step::Done) => self.status_from_outcome(Err(VmError::internal(
                "process halted under a native caller",
            ))),
            other => self.status_from_outcome(other),
        }
    }

    /// The reds checkpoint on the native side of the boundary — the exact
    /// twin of the interpreter's `checkpoint!`: one reduction per function
    /// application, plus whatever reclamation debt accrued since the last
    /// checkpoint. Returns true when the budget is spent and the caller must
    /// yield; the caller is responsible for leaving the top frame consistent
    /// at its resume point first.
    fn native_checkpoint(&mut self) -> bool {
        let mut reds = self.native_reds - 1;
        self.charge_reclamation(&mut reds);
        self.native_reds = reds;
        reds <= 0
    }

    /// `enter_frame!`'s non-tail branch for a known capture-free target (the
    /// `call_known!` shape): push the callee `CallFrame` over the `arity`
    /// argument words already on the stack top and zero-fill the remaining
    /// locals. The caller has stored its own resume ip already.
    fn push_known_frame(&mut self, target: i32) {
        let func = &self.program.functions[target as usize];
        let (arity, locals, code_start) = (func.arity, func.locals, func.code_start);
        debug_assert_eq!(func.capture_count, 0);
        let args_start = self.stack.len() - arity as usize;
        self.frames.push(CallFrame {
            func_idx: target,
            code_start,
            ip: 0,
            base_slot: args_start,
            captures: Value::small_int(0),
        });
        for _ in arity..locals {
            self.stack.push(Value::small_int(0));
        }
    }

    /// `enter_frame!`'s tail branch for a known capture-free target (the
    /// `TailCallKnown` surgery): drop the caller's slots, slide the `arity`
    /// argument words down to its base, retarget the frame in place, and
    /// zero-fill the remaining locals. The collapsed frame is consistent at
    /// `ip == 0`, so a yield right after resumes the *callee* from the top.
    fn collapse_known_frame(&mut self, target: i32) {
        let func = &self.program.functions[target as usize];
        let (arity, locals, code_start) = (func.arity, func.locals, func.code_start);
        debug_assert_eq!(func.capture_count, 0);
        let args_start = self.stack.len() - arity as usize;
        let base = self.frame().base_slot;
        self.collapse_tail_frame(base, args_start);
        let f = self.frame_mut();
        f.func_idx = target;
        f.code_start = code_start;
        f.ip = 0;
        // The sentinel a capture-free callee carries; assigning it releases
        // the caller's closure handle, as the interpreter's tail branch does.
        f.captures = Value::small_int(0);
        for _ in arity..locals {
            self.stack.push(Value::small_int(0));
        }
    }

    /// The trampoline: drive the top frame to a boundary status — a native
    /// entry when the table covers its function, the frame-floor interpreter
    /// otherwise — consuming [`NativeStatus::TailCall`] by re-dispatching
    /// the collapsed frame. Every native entry invocation goes through here
    /// (both the interp→native seam and [`al_rt_call`]), which is what keeps
    /// cross-function tail chains on a flat machine stack and guarantees
    /// `TailCall` never crosses the interpreter boundary.
    pub(super) fn drive_top_frame(&mut self) -> NativeStatus {
        loop {
            let idx = FuncIdx::from_usize(self.frame().func_idx as usize);
            let status = match self.program.native.get(idx) {
                Some(entry) => self.call_native(entry),
                None => self.enter_interp(),
            };
            if status != NativeStatus::TailCall {
                return status;
            }
        }
    }

    /// The `Op::Call`/`Op::TailCall` callee checks: the value must be a
    /// closure and its function's arity must match the argument count. The
    /// resolved `func_idx` is the dispatch key the entry table (and the
    /// interpreter) share.
    fn dynamic_target(&self, callee: &Value, argc: i64) -> VmResult<i32> {
        let Some(cl) = callee.as_closure() else {
            return Err(VmError::internal("call target is not a function"));
        };
        let target = cl.func_idx();
        let func_arity = self.program.functions[target as usize].arity;
        if argc as i32 != func_arity {
            return Err(VmError::internal(format!(
                "call arity mismatch: expected {func_arity}, got {argc}"
            )));
        }
        Ok(target)
    }

    /// `enter_frame!`'s non-tail branch for a dynamic callee (the `Op::Call`
    /// shape): resolve the closure's target, push the callee `CallFrame`
    /// with the closure value itself as its `captures` handle — one word
    /// moved, no captures clone, exactly the interpreter's `Op::Call`.
    fn push_closure_frame(&mut self, callee: Value, argc: i64) -> VmResult<()> {
        let target = self.dynamic_target(&callee, argc)?;
        let func = &self.program.functions[target as usize];
        let (arity, locals, code_start) = (func.arity, func.locals, func.code_start);
        let args_start = self.stack.len() - arity as usize;
        self.frames.push(CallFrame {
            func_idx: target,
            code_start,
            ip: 0,
            base_slot: args_start,
            captures: callee,
        });
        for _ in arity..locals {
            self.stack.push(Value::small_int(0));
        }
        Ok(())
    }

    /// `enter_frame!`'s tail branch for a dynamic callee (`Op::TailCall`):
    /// same checks, collapse the caller's frame in place, the closure value
    /// becoming the collapsed frame's `captures` handle (assigning it
    /// releases the caller's own handle, as the interpreter's tail branch
    /// does).
    fn collapse_closure_frame(&mut self, callee: Value, argc: i64) -> VmResult<()> {
        let target = self.dynamic_target(&callee, argc)?;
        let func = &self.program.functions[target as usize];
        let (arity, locals, code_start) = (func.arity, func.locals, func.code_start);
        let args_start = self.stack.len() - arity as usize;
        let base = self.frame().base_slot;
        self.collapse_tail_frame(base, args_start);
        let f = self.frame_mut();
        f.func_idx = target;
        f.code_start = code_start;
        f.ip = 0;
        f.captures = callee;
        for _ in arity..locals {
            self.stack.push(Value::small_int(0));
        }
        Ok(())
    }

    /// [`al_rt_call`]'s body once the argument words are on the stack:
    /// store the caller's resume ip, install the callee frame, then
    /// checkpoint + dispatch ([`Self::drive_pushed_callee`]).
    fn rt_call(&mut self, target: i32, resume_ip: i32, out: *mut u64) -> NativeStatus {
        self.frame_mut().ip = resume_ip;
        self.push_known_frame(target);
        self.drive_pushed_callee(out)
    }

    /// [`al_rt_call_value`]'s body: as [`Self::rt_call`], but the callee is
    /// an owned closure value resolved through [`Self::push_closure_frame`];
    /// its `Op::Call` checks surface as an error status.
    fn rt_call_value(
        &mut self,
        callee: Value,
        resume_ip: i32,
        argc: i64,
        out: *mut u64,
    ) -> NativeStatus {
        self.frame_mut().ip = resume_ip;
        if let Err(err) = self.push_closure_frame(callee, argc) {
            return self.status_from_outcome(Err(err));
        }
        self.drive_pushed_callee(out)
    }

    /// The shared non-tail-call tail: charge the entry checkpoint, drive the
    /// freshly pushed callee frame, and on `Done` pop the result out to the
    /// native caller.
    #[allow(unsafe_code)] // one write into the caller-provided out-slot
    fn drive_pushed_callee(&mut self, out: *mut u64) -> NativeStatus {
        // Entry checkpoint, `checkpoint!`-parity: the callee frame is fully
        // consistent (ip == 0, args in slots), so exhaustion is a plain
        // unwind and resume re-enters the callee from the top.
        if self.native_checkpoint() {
            return NativeStatus::Yield;
        }
        let status = self.drive_top_frame();
        if status != NativeStatus::Done {
            return status;
        }
        // The return protocol left the result on the stack top (the callee's
        // old base slot); hand it to the native caller's out-slot. Ownership
        // of the reference moves with the bits.
        match self.stack.pop() {
            Some(result) => {
                // SAFETY: `out` is the caller's result slot per
                // `al_rt_call`'s contract, valid for one u64 write.
                unsafe { out.write(std::mem::ManuallyDrop::new(result).to_bits()) };
                NativeStatus::Done
            }
            None => self.status_from_outcome(Err(VmError::internal(
                "native call returned Done with an empty stack",
            ))),
        }
    }
}

/// Move `argc` argument value words from the native caller's scratch onto
/// the value stack — the state `Op::Call*` sites are in when `enter_frame!`
/// runs. Ownership of each reference transfers with its bits (the native
/// caller forgets them), so there is no rc traffic, exactly like the
/// interpreter whose args are already on the stack.
///
/// # Safety
/// `args` must be valid for `argc` reads of owned value words (it may be
/// dangling when `argc == 0`).
#[allow(unsafe_code)] // reads the caller's argument scratch; contract above
unsafe fn push_args(vm: &mut VM, args: *const u64, argc: i64) {
    if argc <= 0 {
        return;
    }
    // SAFETY: valid for `argc` reads per the contract above.
    let words = unsafe { std::slice::from_raw_parts(args, argc as usize) };
    for &bits in words {
        // SAFETY: each word is an owned value whose reference transfers here.
        vm.stack.push(unsafe { Value::from_bits(bits) });
    }
}

/// Call an interpreter-only function from a compiled body — call kind 2 of
/// the convention's five ("native → interpreter-only fn").
///
/// The caller completes the same frame handshake as for a native callee
/// (own resume ip stored in its own frame, callee `CallFrame` pushed,
/// arguments in `[base_slot, base_slot + arity)`), then calls this instead
/// of a table entry. The interpreter runs the callee with a frame floor at
/// the caller's depth: a `Ret` back down to the floor ends the slice with
/// [`NativeStatus::Done`] and the callee's result on top of the stack.
///
/// Any suspension inside the callee — a park in one of the ten `io.rs` ops,
/// a reduction-budget yield — comes back as a non-`Done` status, which the
/// caller's `if status != Done { return status; }` unwinding propagates:
/// no native frame survives it. The suspended continuation is entirely
/// `(stack, frames)`; on wake the process resumes in `execute_slice`, the
/// parked frame finishes interpreted, and when it `Ret`s into the native
/// caller's frame the interpreter carries on with that frame's *bytecode*
/// at the stored resume ip — compiled code keeps the interpreter frame
/// layout bit-for-bit, so the bytecode picks up exactly where the machine
/// code left off.
///
/// Generated code reaches this by symbol ([`enter_interp_symbol`]), like
/// the [`native_shims`](super::native_shims) table.
///
/// # Safety
/// `vmx` must point at the running scheduler's live `VM`, with the callee
/// frame installed as described above; no other reference to the VM may be
/// live for the duration of the call.
#[allow(unsafe_code)] // the designated native→interp re-entry point; contract above
pub unsafe extern "C" fn al_rt_enter_interp(vmx: *mut VM) -> NativeStatus {
    // SAFETY: `vmx` is the running scheduler's live VM per the contract
    // above; the exclusive borrow lasts exactly this call, mirroring the
    // `&mut VM` the interp→native seam lent to the compiled code.
    let vm = unsafe { &mut *vmx };
    vm.enter_interp()
}

/// The re-entry shim as a `(symbol name, address)` pair for JIT symbol
/// registration (`JITBuilder::symbol`), alongside
/// [`native_shims::shim_symbols`](super::native_shims::shim_symbols).
pub fn enter_interp_symbol() -> (&'static str, *const u8) {
    ("al_rt_enter_interp", al_rt_enter_interp as *const u8)
}

/// Call a known capture-free function from a compiled body — call kinds 1
/// and 2 of the convention's five. `target` is the callee's `FuncIdx`,
/// `resume_ip` the caller's own bytecode resume point (the instruction
/// after the corresponding call, so a suspension under the callee resumes
/// the caller's remainder *interpreted*), and `args`/`argc` the argument
/// value words, ownership transferring in.
///
/// The shim performs the interpreter's whole `CallKnown` sequence: resume ip
/// into the caller frame, argument push, `enter_frame!`-equivalent callee
/// frame, the entry reds checkpoint, then dispatch — the entry-table hit
/// directly, the miss under [`al_rt_enter_interp`]'s frame floor — driving
/// any tail chain the callee unwinds with. On [`NativeStatus::Done`] the
/// callee's result word is written to `out` (ownership to the caller) and
/// the caller's frame is the top frame again; any other status unwinds:
/// `status = al_rt_call(...); if status != Done { return status; }`.
///
/// # Safety
/// `vmx` must point at the running scheduler's live `VM` (no other reference
/// live for the duration); `args` must be valid for `argc` reads of owned
/// value words; `out` must be valid for one u64 write.
#[allow(unsafe_code)] // the native call seam; contracts above
pub unsafe extern "C" fn al_rt_call(
    vmx: *mut VM,
    target: i64,
    resume_ip: i64,
    args: *const u64,
    argc: i64,
    out: *mut u64,
) -> NativeStatus {
    // SAFETY: `vmx` is the running scheduler's live VM per the contract.
    let vm = unsafe { &mut *vmx };
    // SAFETY: `args`/`argc` per the contract.
    unsafe { push_args(vm, args, argc) };
    vm.rt_call(target as i32, resume_ip as i32, out)
}

/// Call a *dynamic* callee from a compiled body — the `Op::Call` half of
/// call kinds 1 and 2. `callee` is an owned closure value word (the
/// interpreter's popped callee): the shim resolves its `func_idx`, checks
/// the arity, and installs it as the callee frame's `captures` handle, so a
/// capture-carrying body finds its environment exactly where `PushCapture`
/// expects it. Dispatch then consults the same entry table as a known call
/// — a native-covered closure body runs native, anything else interprets
/// under the frame floor. A non-closure callee or an arity mismatch surfaces
/// as [`NativeStatus::Error`] with the interpreter's own message.
///
/// # Safety
/// As [`al_rt_call`], plus: `callee` must be the bits of a `Value` whose
/// reference the caller owns and transfers to this call.
#[allow(unsafe_code)] // the dynamic-call seam; contracts above
pub unsafe extern "C" fn al_rt_call_value(
    vmx: *mut VM,
    callee: u64,
    resume_ip: i64,
    args: *const u64,
    argc: i64,
    out: *mut u64,
) -> NativeStatus {
    // SAFETY: `vmx` is the running scheduler's live VM per the contract.
    let vm = unsafe { &mut *vmx };
    // SAFETY: `args`/`argc` per the contract.
    unsafe { push_args(vm, args, argc) };
    // SAFETY: `callee` is an owned value word per the contract.
    let callee = unsafe { Value::from_bits(callee) };
    vm.rt_call_value(callee, resume_ip as i32, argc, out)
}

/// The frame handshake half of a direct native→native call — [`al_rt_call`]
/// up to but not including `drive_top_frame`. Pushes the argument words,
/// stores the caller's resume ip, installs the callee `CallFrame` (the
/// `enter_frame!` push), and charges the entry checkpoint. The callee frame
/// is bit-identical to the interpreter's at ip 0, so on
/// [`NativeStatus::Yield`] the caller unwinds and re-entry runs the callee
/// from the top; on [`NativeStatus::Done`] the caller emits a direct machine
/// call to the peer's `NativeEntry` and hands the returned status to
/// [`al_rt_direct_result`]. Together the pair is exactly `al_rt_call` with
/// the trampoline's table lookup and indirect dispatch replaced by a
/// statically-resolved call.
///
/// # Safety
/// As [`al_rt_call`], minus `out`.
#[allow(unsafe_code)] // the direct-call frame-handshake seam; contracts above
pub unsafe extern "C" fn al_rt_push_frame(
    vmx: *mut VM,
    target: i64,
    resume_ip: i64,
    args: *const u64,
    argc: i64,
) -> NativeStatus {
    // SAFETY: `vmx` is the running scheduler's live VM per the contract.
    let vm = unsafe { &mut *vmx };
    // SAFETY: `args`/`argc` per the contract.
    unsafe { push_args(vm, args, argc) };
    vm.frame_mut().ip = resume_ip as i32;
    vm.push_known_frame(target as i32);
    if vm.native_checkpoint() {
        return NativeStatus::Yield;
    }
    NativeStatus::Done
}

/// The return-path half of a direct native→native call: `status` is what
/// the peer's `NativeEntry` returned. On [`NativeStatus::Done`] the callee's
/// `al_rt_ret` left the result on the stack top; pop it out to the caller's
/// result slot exactly as [`VM::drive_pushed_callee`] does. A
/// [`NativeStatus::TailCall`] is consumed here — the callee tail-called out,
/// so drive the collapsed frame to completion under the trampoline (the same
/// loop `al_rt_call` would have run) — never propagated: the caller's
/// `CallFrame` is under the collapsed one, so unwinding on `TailCall` would
/// leave it stranded. Any other status passes through to the caller's
/// mechanical unwind.
///
/// # Safety
/// `vmx` must point at the running scheduler's live `VM`; `out` must be
/// valid for one u64 write; `status` must be a value the direct callee's
/// `NativeEntry` returned.
#[allow(unsafe_code)] // the direct-call return-path seam; contracts above
pub unsafe extern "C" fn al_rt_direct_result(
    vmx: *mut VM,
    status: NativeStatus,
    out: *mut u64,
) -> NativeStatus {
    // SAFETY: `vmx` is the running scheduler's live VM per the contract.
    let vm = unsafe { &mut *vmx };
    let status = if status == NativeStatus::TailCall {
        vm.drive_top_frame()
    } else {
        status
    };
    if status != NativeStatus::Done {
        return status;
    }
    match vm.stack.pop() {
        Some(result) => {
            // SAFETY: `out` is the caller's result slot per the contract,
            // valid for one u64 write.
            unsafe { out.write(std::mem::ManuallyDrop::new(result).to_bits()) };
            NativeStatus::Done
        }
        None => vm.status_from_outcome(Err(VmError::internal(
            "native call returned Done with an empty stack",
        ))),
    }
}

/// Cross-function tail call from a compiled body — call kind 5's
/// trampoline-mediated form. Pushes the argument words, collapses the
/// caller's frame into the callee in place (the interpreter's
/// `TailCallKnown` surgery), charges the reds checkpoint, and returns
/// either [`NativeStatus::Yield`] (budget spent; the collapsed frame is
/// consistent at `ip == 0`, so resume enters the callee from the top) or
/// [`NativeStatus::TailCall`] for the driver that invoked the caller to
/// dispatch. A tail call site compiles to
/// `return al_rt_tail_call(...)` — the caller's machine frame is gone
/// before the callee runs, so tail chains never stack.
///
/// # Safety
/// As [`al_rt_call`], minus `out` — a tail caller never sees the result.
#[allow(unsafe_code)] // the native tail-call seam; contracts above
pub unsafe extern "C" fn al_rt_tail_call(
    vmx: *mut VM,
    target: i64,
    args: *const u64,
    argc: i64,
) -> NativeStatus {
    // SAFETY: `vmx` is the running scheduler's live VM per the contract.
    let vm = unsafe { &mut *vmx };
    // SAFETY: `args`/`argc` per the contract.
    unsafe { push_args(vm, args, argc) };
    vm.collapse_known_frame(target as i32);
    if vm.native_checkpoint() {
        return NativeStatus::Yield;
    }
    NativeStatus::TailCall
}

/// Dynamic cross-function tail call — `Op::TailCall`'s surgery behind the C
/// ABI: the caller's frame collapses into the closure's function in place,
/// the closure value becoming the collapsed frame's `captures` handle. The
/// caller returns the status verbatim (`return al_rt_tail_call_value(...)`),
/// so the driver that invoked it dispatches the collapsed frame.
///
/// # Safety
/// As [`al_rt_call_value`], minus `out` — a tail caller never sees the
/// result.
#[allow(unsafe_code)] // the dynamic tail-call seam; contracts above
pub unsafe extern "C" fn al_rt_tail_call_value(
    vmx: *mut VM,
    callee: u64,
    args: *const u64,
    argc: i64,
) -> NativeStatus {
    // SAFETY: `vmx` is the running scheduler's live VM per the contract.
    let vm = unsafe { &mut *vmx };
    // SAFETY: `args`/`argc` per the contract.
    unsafe { push_args(vm, args, argc) };
    // SAFETY: `callee` is an owned value word per the contract.
    let callee = unsafe { Value::from_bits(callee) };
    if let Err(err) = vm.collapse_closure_frame(callee, argc) {
        return vm.status_from_outcome(Err(err));
    }
    if vm.native_checkpoint() {
        return NativeStatus::Yield;
    }
    NativeStatus::TailCall
}

/// `Op::MakeClosure`'s allocation for a compiled body: build a closure cell
/// over `func_idx` from `count` capture words. Each capture word transfers
/// one owned reference in; the cell takes its own copy of each (the
/// interpreter's `closure_in` over the pushed captures) and the transferred
/// references are released — push-then-truncate, minus the stack traffic.
/// The returned bits carry the one owned reference to the fresh cell, whose
/// allocation lands in the same `ProcHeap` accounting as the interpreter's.
///
/// # Safety
/// `vmx` must point at the running scheduler's live `VM`; `caps` must be
/// valid for `count` reads of owned value words (it may be dangling when
/// `count == 0`).
#[allow(unsafe_code)] // the closure-allocation seam; contract above
pub unsafe extern "C" fn al_rt_make_closure(
    vmx: *mut VM,
    func_idx: i64,
    caps: *const u64,
    count: i64,
) -> u64 {
    // SAFETY: `vmx` is the running scheduler's live VM per the contract.
    let vm = unsafe { &mut *vmx };
    let n = count.max(0) as usize;
    // SAFETY: `Value` is `repr(transparent)` over its u64 bits, so the
    // caller's words read as a borrowed value slice for the copy.
    let borrowed: &[Value] = if n == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(caps.cast::<Value>(), n) }
    };
    let v = Value::closure_in(&mut vm.heap, func_idx as i32, borrowed);
    for i in 0..n {
        // SAFETY: releases exactly the one reference each word transferred
        // in — the interpreter's `stack.truncate` after `closure_in`.
        drop(unsafe { Value::from_bits(caps.add(i).read()) });
    }
    std::mem::ManuallyDrop::new(v).to_bits()
}

/// The reds checkpoint at a *self*-tail-call back-edge — call kind 5's
/// native-loop form. The compiled loop has already written the next
/// iteration's arguments into the frame's argument slots (and left
/// `stack.len() == base_slot + locals`, the `TailCallSelf` frame shape), so
/// on exhaustion the frame is made resumable by pointing `ip` at 0 and the
/// unwind is a plain return: re-entry re-runs the function from the top
/// with the already-updated arguments, exactly the interpreter's
/// `TailCallSelf` + `checkpoint!` behaviour. [`NativeStatus::Done`] means
/// "keep looping".
///
/// # Safety
/// `vmx` must point at the running scheduler's live `VM`, its top frame the
/// compiled function's, in the back-edge state described above.
#[allow(unsafe_code)] // the self-tail back-edge seam; contract above
pub unsafe extern "C" fn al_rt_checkpoint(vmx: *mut VM) -> NativeStatus {
    // SAFETY: `vmx` is the running scheduler's live VM per the contract.
    let vm = unsafe { &mut *vmx };
    if vm.native_checkpoint() {
        vm.frame_mut().ip = 0;
        return NativeStatus::Yield;
    }
    NativeStatus::Done
}

/// The address of the top frame's first slot (`stack[base_slot]`), the base
/// compiled code reads its arguments and writes its frame-slot locals
/// through — `Value` is a `repr(transparent)` u64, so slot `i` is the word
/// at `base + 8*i`. The pointer is invalidated by anything that can grow
/// the value stack (any `al_rt_*` call shim), so compiled code re-fetches
/// it after every such seam, mirroring how the interpreter re-hoists its
/// frame state after calls.
///
/// # Safety
/// `vmx` must point at the running scheduler's live `VM` with the compiled
/// function's frame on top.
#[allow(unsafe_code)] // the frame-slot access seam; contract above
pub unsafe extern "C" fn al_rt_frame_base(vmx: *mut VM) -> *mut u64 {
    // SAFETY: `vmx` is the running scheduler's live VM per the contract.
    let vm = unsafe { &mut *vmx };
    let base = vm.frame().base_slot;
    // SAFETY: `base_slot < stack.len()` for a live frame; the cast is sound
    // because `Value` is `repr(transparent)` over its u64 bits.
    unsafe { vm.stack.as_mut_ptr().add(base).cast::<u64>() }
}

/// The compiled epilogue — `ret!`'s stack surgery behind the C ABI: pop the
/// frame, truncate the operand stack to its base (releasing the frame's
/// remaining locals, exactly the interpreter's decref-on-truncate), and
/// push the result. The entry then returns [`NativeStatus::Done`], leaving
/// the state every caller of the convention expects: caller frame on top,
/// result on the stack top. Ownership of `result` transfers in.
///
/// # Safety
/// `vmx` must point at the running scheduler's live `VM` with the returning
/// compiled function's frame on top; `result` must be an owned value word.
#[allow(unsafe_code)] // the native return seam; contract above
pub unsafe extern "C" fn al_rt_ret(vmx: *mut VM, result: u64) {
    // SAFETY: `vmx` is the running scheduler's live VM per the contract.
    let vm = unsafe { &mut *vmx };
    // A compiled body only runs with its frame installed, so the pop cannot
    // miss; degrade to a no-op rather than unwind across the C boundary.
    let Some(frame) = vm.frames.pop() else {
        debug_assert!(false, "al_rt_ret with no active call frame");
        return;
    };
    vm.stack.truncate(frame.base_slot);
    // SAFETY: `result` is an owned value word per the contract.
    vm.stack.push(unsafe { Value::from_bits(result) });
    // `frame` drops here, releasing its captures handle like `ret!`'s
    // popped frame does.
}

/// Every `al_rt_*` boundary shim, as `(symbol name, address)` pairs for JIT
/// symbol registration — the call-kind half of the registry seam, beside
/// [`native_shims::shim_symbols`](super::native_shims::shim_symbols). A2's
/// @vm-op shims extend the same tables: one shim, one row, one import.
pub fn rt_symbols() -> [(&'static str, *const u8); 11] {
    [
        enter_interp_symbol(),
        ("al_rt_call", al_rt_call as *const u8),
        ("al_rt_tail_call", al_rt_tail_call as *const u8),
        ("al_rt_push_frame", al_rt_push_frame as *const u8),
        ("al_rt_direct_result", al_rt_direct_result as *const u8),
        ("al_rt_call_value", al_rt_call_value as *const u8),
        ("al_rt_tail_call_value", al_rt_tail_call_value as *const u8),
        ("al_rt_make_closure", al_rt_make_closure as *const u8),
        ("al_rt_checkpoint", al_rt_checkpoint as *const u8),
        ("al_rt_frame_base", al_rt_frame_base as *const u8),
        ("al_rt_ret", al_rt_ret as *const u8),
    ]
}

#[cfg(test)]
#[allow(unsafe_code)] // drives the extern "C" boundary shims directly
mod tests {
    use std::time::Instant;

    use super::super::halt_test_vm;
    use super::*;

    // The C1 re-derivation seam: CURRENT_VM is per OS thread — a value
    // published on one scheduler thread is invisible on another, which is
    // the property that makes "re-derive after every suspension point"
    // migration-safe.
    #[test]
    fn current_vm_is_thread_local() {
        let mut vm = halt_test_vm();
        set_current_vm(&mut vm);
        assert_eq!(current_vm(), (&raw mut vm).cast());
        let other = std::thread::spawn(|| current_vm() as usize)
            .join()
            .expect("probe thread");
        assert_eq!(
            other as *mut VM,
            std::ptr::null_mut(),
            "another thread must not observe this scheduler's VM"
        );
        assert_eq!(current_vm(), (&raw mut vm).cast());
    }

    #[test]
    fn done_and_yield_round_trip() {
        let mut vm = halt_test_vm();
        let s = vm.status_from_outcome(Ok(Step::Done));
        assert_eq!(s, NativeStatus::Done);
        assert!(matches!(vm.outcome_from_status(s), Ok(Step::Done)));
        let s = vm.status_from_outcome(Ok(Step::Yield));
        assert_eq!(s, NativeStatus::Yield);
        assert!(matches!(vm.outcome_from_status(s), Ok(Step::Yield)));
    }

    #[test]
    fn parked_round_trips_its_wait() {
        let mut vm = halt_test_vm();
        let deadline = Instant::now();
        let s = vm.status_from_outcome(Ok(Step::Parked(Wait::Timer(deadline))));
        assert_eq!(s, NativeStatus::Parked);
        match vm.outcome_from_status(s) {
            Ok(Step::Parked(Wait::Timer(t))) => assert_eq!(t, deadline),
            _ => panic!("expected the parked timer back"),
        }
        assert!(vm.native_pending.is_none());
    }

    #[test]
    fn error_round_trips_its_payload() {
        let mut vm = halt_test_vm();
        let s = vm.status_from_outcome(Err(VmError::SliceOutOfBounds {
            lo: 1,
            hi: 9,
            len: 3,
        }));
        assert_eq!(s, NativeStatus::Error);
        match vm.outcome_from_status(s) {
            Err(VmError::SliceOutOfBounds {
                lo: 1,
                hi: 9,
                len: 3,
            }) => {}
            _ => panic!("expected the slice error back"),
        }
        assert!(vm.native_pending.is_none());
    }

    #[test]
    fn payload_status_without_pending_is_an_internal_error() {
        let mut vm = halt_test_vm();
        assert!(matches!(
            vm.outcome_from_status(NativeStatus::Parked),
            Err(VmError::Internal(_))
        ));
        assert!(matches!(
            vm.outcome_from_status(NativeStatus::Error),
            Err(VmError::Internal(_))
        ));
    }

    // A stand-in native entry: proves the signature crosses a real C-ABI
    // call and that `call_native` hands over a non-null context. Reading
    // the VM out of it is [`vm_from_ctx`]'s job in the stubs below.
    extern "C" fn yield_entry(ctx: *mut core::ffi::c_void) -> NativeStatus {
        assert!(!ctx.is_null());
        NativeStatus::Yield
    }

    /// What a compiled prologue does with its argument: the entry receives
    /// a [`crate::bytecode::NativeCtx`] and the VM lives behind its `vm`
    /// field — stubs must read it the same way generated code does.
    fn vm_from_ctx(ctx: *mut core::ffi::c_void) -> *mut VM {
        unsafe { (*ctx.cast::<crate::bytecode::NativeCtx>()).vm.cast() }
    }

    #[test]
    fn status_crosses_the_extern_c_boundary() {
        let mut vm = halt_test_vm();
        let entry: NativeEntry = yield_entry;
        let status = vm.call_native(entry);
        assert!(matches!(vm.outcome_from_status(status), Ok(Step::Yield)));
    }

    use std::sync::Arc;

    use crate::bytecode::{Function, Instruction, Op, Program, Value, op, op_arg};

    use super::super::{CallFrame, new_vm};

    // A two-function program for driving `al_rt_enter_interp` directly:
    // fn 0 ("main", the entry) is a single `Halt` at address 0 — the
    // stand-in *bytecode* body for the native-caller frame, exercised only
    // by the resume-after-park path — and fn 1 ("callee") is `callee` at
    // addresses [1, 1 + len).
    fn caller_callee_program(constants: Vec<Value>, callee: Vec<Instruction>) -> Program {
        let callee_len = callee.len() as i32;
        let mut code = vec![op(Op::Halt)];
        code.extend(callee);
        Program {
            constants,
            functions: vec![
                Function {
                    name: "main".into(),
                    arity: 0,
                    locals: 0,
                    capture_count: 0,
                    code_start: 0,
                    code_len: 1,
                },
                Function {
                    name: "callee".into(),
                    arity: 0,
                    locals: 0,
                    capture_count: 0,
                    code_start: 1,
                    code_len: callee_len,
                },
            ],
            code,
            entry: 0,
            frozen: Arc::new(crate::frozen::FrozenArea::new()),
            native: Default::default(),
        }
    }

    // The frame shape a native caller leaves right before its
    // `al_rt_enter_interp` call: its own frame below (resume ip already
    // stored — here 0, pointing at main's `Halt` as the stand-in resume
    // body), the interpreted callee's freshly pushed frame on top.
    fn vm_with_callee_pushed(constants: Vec<Value>, callee: Vec<Instruction>) -> VM {
        let mut vm = new_vm(
            caller_callee_program(constants, callee),
            &crate::template::test_fixture::TEST_STDLIB,
        )
        .expect("test VM must construct");
        vm.frames.push(CallFrame {
            func_idx: 0,
            code_start: 0,
            ip: 0,
            base_slot: 0,
            captures: Value::small_int(0),
        });
        vm.frames.push(CallFrame {
            func_idx: 1,
            code_start: 1,
            ip: 0,
            base_slot: 0,
            captures: Value::small_int(0),
        });
        vm
    }

    #[test]
    fn interp_callee_return_stops_at_the_frame_floor() {
        let mut vm = vm_with_callee_pushed(
            vec![Value::small_int(42)],
            vec![op_arg(Op::PushConst, 0), op(Op::Ret)],
        );
        let status = unsafe { al_rt_enter_interp(&raw mut vm) };
        assert_eq!(status, NativeStatus::Done);
        // The return protocol is already applied: callee frame popped, the
        // caller's frame on top again, result on top of the stack.
        assert_eq!(vm.frames.len(), 1);
        assert_eq!(vm.stack.len(), 1);
        assert_eq!(vm.stack[0].to_bits(), Value::small_int(42).to_bits());
        assert_eq!(vm.native_floor, 0);
        assert!(vm.native_pending.is_none());
    }

    #[test]
    fn interp_callee_park_unwinds_and_resume_finishes_interpreted() {
        // callee: sleep(1ms) — parks via the io.rs protocol — then return
        // the nil the wake left on top.
        let mut vm = vm_with_callee_pushed(
            vec![Value::small_int(1)],
            vec![op_arg(Op::PushConst, 0), op(Op::Sleep), op(Op::Ret)],
        );
        let status = unsafe { al_rt_enter_interp(&raw mut vm) };
        // The park surfaces as the status word; the native frames above
        // unwind with it by plain returns. The floor is back at 0 and the
        // parked continuation is entirely (stack, frames): the callee frame
        // is intact, `ip` on its bytecode resume point (the `Ret` after
        // `Sleep`).
        assert_eq!(status, NativeStatus::Parked);
        assert_eq!(vm.native_floor, 0);
        assert_eq!(vm.frames.len(), 2);
        assert_eq!(vm.frames[1].ip, 2);
        // The trampoline half: rehydrate exactly the step whose
        // `scheduler_loop` arm does `suspend_current` + `park` today.
        match vm.outcome_from_status(status) {
            Ok(Step::Parked(Wait::Timer(_))) => {}
            _ => panic!("expected the sleep's timer park back"),
        }
        // Resume-after-park re-enters the interpreter: the callee finishes
        // (returns the nil `sleep` pushed), and the native caller's frame
        // continues as bytecode from its stored resume ip — here main's
        // `Halt`, which ends the process with the callee's result on top.
        let resumed = vm.execute_slice().expect("resume must not error");
        assert!(matches!(resumed, Step::Done));
        assert_eq!(vm.stack.len(), 1);
        // `sleep` pushes the VM's Nil enum (a frozen template word), not the
        // raw immediate — compare against the same constructor.
        assert_eq!(vm.stack[0].to_bits(), vm.make_nil().to_bits());
    }

    #[test]
    fn interp_callee_yield_unwinds_with_a_resumable_frame() {
        // callee: an infinite self-tail loop — exhausts the reduction budget
        // and yields at the back-edge checkpoint with `ip == 0` (re-enter
        // from the top), the TailCallSelf frame-at-yield contract.
        let mut vm = vm_with_callee_pushed(Vec::new(), vec![op_arg(Op::TailCallSelf, 0)]);
        let status = unsafe { al_rt_enter_interp(&raw mut vm) };
        assert_eq!(status, NativeStatus::Yield);
        assert_eq!(vm.native_floor, 0);
        assert_eq!(vm.frames.len(), 2);
        assert_eq!(vm.frames[1].ip, 0);
        assert!(matches!(vm.outcome_from_status(status), Ok(Step::Yield)));
    }

    #[test]
    fn interp_callee_error_unwinds_with_its_payload() {
        // callee: `Sleep` over an empty stack — a runtime error.
        let mut vm = vm_with_callee_pushed(Vec::new(), vec![op(Op::Sleep), op(Op::Ret)]);
        let status = unsafe { al_rt_enter_interp(&raw mut vm) };
        assert_eq!(status, NativeStatus::Error);
        assert_eq!(vm.native_floor, 0);
        assert!(matches!(
            vm.outcome_from_status(status),
            Err(VmError::Internal(_))
        ));
    }

    #[test]
    fn nested_re_entry_restores_the_outer_floor() {
        // As if an outer native→interp re-entry (floor 1) is on the machine
        // stack and the interpreted code under it called back into native
        // code, which now calls a further interpreted callee.
        let mut vm = vm_with_callee_pushed(
            vec![Value::small_int(7)],
            vec![op_arg(Op::PushConst, 0), op(Op::Ret)],
        );
        vm.native_floor = 1;
        vm.frames.push(CallFrame {
            func_idx: 1,
            code_start: 1,
            ip: 0,
            base_slot: vm.stack.len(),
            captures: Value::small_int(0),
        });
        let status = unsafe { al_rt_enter_interp(&raw mut vm) };
        assert_eq!(status, NativeStatus::Done);
        assert_eq!(vm.frames.len(), 2);
        assert_eq!(vm.native_floor, 1);
        assert_eq!(vm.stack.len(), 1);
        assert_eq!(vm.stack[0].to_bits(), Value::small_int(7).to_bits());
    }

    #[test]
    fn enter_interp_symbol_names_the_shim() {
        let (name, addr) = enter_interp_symbol();
        assert_eq!(name, "al_rt_enter_interp");
        assert!(!addr.is_null());
    }

    // ---- The call kinds: al_rt_call / al_rt_tail_call / al_rt_checkpoint --

    use std::mem::ManuallyDrop;

    use crate::bytecode::NativeTable;

    /// A program whose entry (fn 0, "main") is `[Nop, Halt]` — ip 1 is a
    /// realistic non-zero resume point — plus the given functions, laid out
    /// consecutively: fn i+1 gets `(arity, locals, body)`.
    fn program_with(constants: Vec<Value>, fns: Vec<(i32, i32, Vec<Instruction>)>) -> Program {
        let mut code = vec![op(Op::Nop), op(Op::Halt)];
        let mut functions = vec![Function {
            name: "main".into(),
            arity: 0,
            locals: 0,
            capture_count: 0,
            code_start: 0,
            code_len: 2,
        }];
        for (i, (arity, locals, body)) in fns.into_iter().enumerate() {
            let code_start = code.len() as i32;
            let code_len = body.len() as i32;
            code.extend(body);
            functions.push(Function {
                name: format!("f{}", i + 1).into(),
                arity,
                locals,
                capture_count: 0,
                code_start,
                code_len,
            });
        }
        let fn_count = functions.len();
        Program {
            constants,
            functions,
            code,
            entry: 0,
            frozen: Arc::new(crate::frozen::FrozenArea::new()),
            native: NativeTable::new(fn_count),
        }
    }

    /// A VM over `program` in the state a *native caller* runs in: main's
    /// frame installed (the stand-in native caller — its bytecode is the
    /// resume fallback), the boundary reds counter seeded.
    fn native_caller_vm(program: Program, reds: i32) -> VM {
        let mut vm = new_vm(program, &crate::template::test_fixture::TEST_STDLIB)
            .expect("test VM must construct");
        vm.frames.push(CallFrame {
            func_idx: 0,
            code_start: 0,
            ip: 0,
            base_slot: 0,
            captures: Value::small_int(0),
        });
        vm.native_reds = reds;
        vm
    }

    fn small(bits: u64) -> i64 {
        ManuallyDrop::new(unsafe { Value::from_bits(bits) }).as_int_typed()
    }

    /// A compiled body stand-in for `fn add_five(n) { n + 5 }`: reads its
    /// argument through the frame base, applies the return protocol via
    /// `al_rt_ret`, reports `Done` — the callee side of call kind 1.
    extern "C" fn add_five_entry(ctx: *mut core::ffi::c_void) -> NativeStatus {
        let vm: *mut VM = vm_from_ctx(ctx);
        let base = unsafe { al_rt_frame_base(vm) };
        let n = small(unsafe { base.read() });
        unsafe { al_rt_ret(vm, Value::small_int(n + 5).to_bits()) };
        NativeStatus::Done
    }

    #[test]
    fn known_call_dispatches_directly_via_the_entry_table() {
        let program = program_with(Vec::new(), vec![(1, 1, vec![op(Op::Halt)])]);
        let mut vm = native_caller_vm(program, 100);
        vm.program
            .native
            .set(FuncIdx::from_usize(1), add_five_entry);

        let args = [Value::small_int(37).to_bits()];
        let mut out = 0u64;
        let status = unsafe { al_rt_call(&raw mut vm, 1, 1, args.as_ptr(), 1, &raw mut out) };

        assert_eq!(status, NativeStatus::Done);
        assert_eq!(small(out), 42);
        // The caller's frame is the top frame again, its resume ip stored;
        // the result left the stack through the out-slot.
        assert_eq!(vm.frames.len(), 1);
        assert_eq!(vm.frames[0].ip, 1);
        assert_eq!(vm.stack.len(), 0);
        // Entry checkpoint parity: exactly one reduction charged.
        assert_eq!(vm.native_reds, 99);
    }

    #[test]
    fn known_call_falls_back_to_the_frame_floor_for_an_interp_callee() {
        // fn1 is interpreter-only: the identity function `[PushLocal 0, Ret]`.
        let program = program_with(
            Vec::new(),
            vec![(1, 1, vec![op_arg(Op::PushLocal, 0), op(Op::Ret)])],
        );
        let mut vm = native_caller_vm(program, 100);

        let args = [Value::small_int(37).to_bits()];
        let mut out = 0u64;
        let status = unsafe { al_rt_call(&raw mut vm, 1, 1, args.as_ptr(), 1, &raw mut out) };

        assert_eq!(status, NativeStatus::Done);
        assert_eq!(small(out), 37);
        assert_eq!(vm.frames.len(), 1);
        assert_eq!(vm.stack.len(), 0);
        assert_eq!(vm.native_floor, 0);
    }

    #[test]
    fn call_checkpoint_exhaustion_yields_with_the_callee_resumable() {
        let program = program_with(Vec::new(), vec![(1, 2, vec![op(Op::Halt)])]);
        let mut vm = native_caller_vm(program, 1);
        vm.program
            .native
            .set(FuncIdx::from_usize(1), add_five_entry);

        let args = [Value::small_int(37).to_bits()];
        let mut out = 0u64;
        let status = unsafe { al_rt_call(&raw mut vm, 1, 1, args.as_ptr(), 1, &raw mut out) };

        // The budget died at the entry checkpoint: the callee never ran, but
        // its frame is fully consistent (ip 0, argument in its slot, locals
        // zero-filled) — resume re-enters it from the top, like the
        // interpreter's `checkpoint!` after `enter_frame!`.
        assert_eq!(status, NativeStatus::Yield);
        assert_eq!(vm.frames.len(), 2);
        assert_eq!(vm.frames[0].ip, 1);
        assert_eq!(vm.frames[1].ip, 0);
        assert_eq!(vm.frames[1].func_idx, 1);
        assert_eq!(vm.frames[1].base_slot, 0);
        assert_eq!(vm.stack.len(), 2);
        assert_eq!(small(vm.stack[0].to_bits()), 37);
        assert_eq!(small(vm.stack[1].to_bits()), 0);
    }

    /// A compiled body stand-in for `fn f1(n) { f2(n + 1) }` with the call
    /// in tail position: the cross-function tail-call site compiles to
    /// `return al_rt_tail_call(...)`.
    extern "C" fn tail_to_f2_entry(ctx: *mut core::ffi::c_void) -> NativeStatus {
        let vm: *mut VM = vm_from_ctx(ctx);
        let base = unsafe { al_rt_frame_base(vm) };
        let n = small(unsafe { base.read() });
        let args = [Value::small_int(n + 1).to_bits()];
        unsafe { al_rt_tail_call(vm, 2, args.as_ptr(), 1) }
    }

    #[test]
    fn cross_fn_tail_call_collapses_and_the_trampoline_drives_the_target() {
        // fn1: native, tail-calls fn2. fn2: interpreter-only identity.
        let program = program_with(
            Vec::new(),
            vec![
                (1, 1, vec![op(Op::Halt)]),
                (1, 1, vec![op_arg(Op::PushLocal, 0), op(Op::Ret)]),
            ],
        );
        let mut vm = native_caller_vm(program, 100);
        vm.program
            .native
            .set(FuncIdx::from_usize(1), tail_to_f2_entry);

        let args = [Value::small_int(41).to_bits()];
        let mut out = 0u64;
        let status = unsafe { al_rt_call(&raw mut vm, 1, 1, args.as_ptr(), 1, &raw mut out) };

        // f1's machine frame returned `TailCall` before f2 ran; the driver
        // loop inside `al_rt_call` dispatched the collapsed frame (f2, here
        // through the frame floor) and only `Done` came back out.
        assert_eq!(status, NativeStatus::Done);
        assert_eq!(small(out), 42);
        assert_eq!(vm.frames.len(), 1);
        assert_eq!(vm.stack.len(), 0);
        // Two applications, two reductions: the call and the tail call —
        // the interpreter's `CallKnown` + `TailCallKnown` charge.
        assert_eq!(vm.native_reds, 98);
    }

    #[test]
    fn tail_call_checkpoint_exhaustion_yields_the_collapsed_frame() {
        let program = program_with(
            Vec::new(),
            vec![
                (1, 1, vec![op(Op::Halt)]),
                (1, 1, vec![op_arg(Op::PushLocal, 0), op(Op::Ret)]),
            ],
        );
        // Budget 2: the entry checkpoint takes it to 1, the tail-call
        // checkpoint to 0 — the yield lands exactly at the tail edge.
        let mut vm = native_caller_vm(program, 2);
        vm.program
            .native
            .set(FuncIdx::from_usize(1), tail_to_f2_entry);

        let args = [Value::small_int(41).to_bits()];
        let mut out = 0u64;
        let status = unsafe { al_rt_call(&raw mut vm, 1, 1, args.as_ptr(), 1, &raw mut out) };

        // The caller frame was already collapsed into the callee, so the
        // suspension is the interpreter's TailCallKnown-then-yield state:
        // top frame is f2 at ip 0 with its argument in place.
        assert_eq!(status, NativeStatus::Yield);
        assert_eq!(vm.frames.len(), 2);
        assert_eq!(vm.frames[1].func_idx, 2);
        assert_eq!(vm.frames[1].ip, 0);
        assert_eq!(vm.frames[1].base_slot, 0);
        assert_eq!(vm.stack.len(), 1);
        assert_eq!(small(vm.stack[0].to_bits()), 42);
        // Resume finishes the program: f2 returns interpreted, main's
        // bytecode halts with the result on top.
        let resumed = vm.execute_slice().expect("resume must not error");
        assert!(matches!(resumed, Step::Done));
        assert_eq!(vm.stack.len(), 1);
        assert_eq!(small(vm.stack[0].to_bits()), 42);
    }

    #[test]
    fn self_tail_checkpoint_charges_one_reduction_per_back_edge() {
        let program = program_with(Vec::new(), vec![(1, 1, vec![op(Op::Halt)])]);
        let mut vm = native_caller_vm(program, 5);
        vm.frames.push(CallFrame {
            func_idx: 1,
            code_start: 2,
            ip: 9, // stale; only a yield may (and must) reset it
            base_slot: 0,
            captures: Value::small_int(0),
        });

        // Budget left: keep looping, ip untouched.
        assert_eq!(unsafe { al_rt_checkpoint(&raw mut vm) }, NativeStatus::Done);
        assert_eq!(vm.native_reds, 4);
        assert_eq!(vm.frames[1].ip, 9);

        // Exhaustion: yield with the frame resumable from the top — the
        // locals are the next iteration's arguments (TailCallSelf shape).
        vm.native_reds = 1;
        assert_eq!(
            unsafe { al_rt_checkpoint(&raw mut vm) },
            NativeStatus::Yield
        );
        assert_eq!(vm.frames[1].ip, 0);
        assert!(vm.native_reds <= 0);
    }

    /// A compiled body stand-in for `fn f1() { loop { f2() } }`: calls the
    /// interpreter-only f2 forever. The yield must come out of the *shared*
    /// slice budget, not a fresh one per native→interp re-entry.
    extern "C" fn call_interp_forever_entry(ctx: *mut core::ffi::c_void) -> NativeStatus {
        let vm: *mut VM = vm_from_ctx(ctx);
        loop {
            let mut out = 0u64;
            let status = unsafe { al_rt_call(vm, 2, 0, std::ptr::null(), 0, &raw mut out) };
            if status != NativeStatus::Done {
                return status;
            }
            // The result is a small int; forgetting the bits releases nothing.
        }
    }

    #[test]
    fn interp_callee_spends_the_native_callers_budget() {
        // f1: native, loops calling f2. f2: interpreter-only, one `CallKnown`
        // to f3 per invocation — each spends one reduction *inside* the
        // interpreter. f3: returns a constant.
        let program = program_with(
            vec![Value::small_int(7)],
            vec![
                (0, 0, vec![op(Op::Halt)]),
                (0, 0, vec![op_arg(Op::CallKnown, 3), op(Op::Ret)]),
                (0, 0, vec![op_arg(Op::PushConst, 0), op(Op::Ret)]),
            ],
        );
        let mut vm = native_caller_vm(program, 10);
        vm.program
            .native
            .set(FuncIdx::from_usize(1), call_interp_forever_entry);
        vm.frames.push(CallFrame {
            func_idx: 1,
            code_start: 2,
            ip: 0,
            base_slot: 0,
            captures: Value::small_int(0),
        });

        let status = vm.drive_top_frame();

        // Each iteration costs two reductions — `al_rt_call`'s entry
        // checkpoint plus f2's `CallKnown` checkpoint — so a budget of 10
        // dies at interpreter parity, mid-callee on the fifth iteration:
        // the `checkpoint!` after f3's frame was entered. Were each
        // re-entry a fresh interpreter budget, the yield would instead land
        // on the tenth entry checkpoint with f2's frame (ip 0) on top.
        assert_eq!(status, NativeStatus::Yield);
        assert_eq!(vm.frames.len(), 4);
        assert_eq!(vm.frames.last().unwrap().func_idx, 3);
        assert_eq!(vm.frames.last().unwrap().ip, 0);
        assert_eq!(vm.native_floor, 0);
    }

    use crate::bytecode::value::take_freed_objects;

    #[test]
    fn make_closure_transfers_capture_ownership() {
        let mut vm = halt_test_vm();
        let big = Value::int_in(&mut vm.heap, i64::MAX); // rc 1
        // Transfer one extra reference in as the capture word.
        let word = ManuallyDrop::new(big.clone()).to_bits(); // rc 2
        take_freed_objects();
        let bits = unsafe { al_rt_make_closure(&raw mut vm, 7, &word, 1) };
        // SAFETY: the shim returned one owned reference to a fresh closure.
        let cl = unsafe { Value::from_bits(bits) };
        {
            let cr = cl.as_closure().expect("a closure cell");
            assert_eq!(cr.func_idx(), 7);
            assert_eq!(cr.captures().len(), 1);
            assert_eq!(cr.captures()[0].as_int(), Some(i64::MAX));
        }
        // Net ownership transfer: the shim's internal retain/release pair
        // cancels, nothing freed yet.
        assert_eq!(take_freed_objects(), 0);
        drop(cl); // frees the cell, releasing its capture reference
        drop(big); // the last reference to the box
        assert_eq!(take_freed_objects(), 2);
    }

    #[test]
    fn dynamic_call_dispatches_through_the_closure_func_idx() {
        let program = program_with(Vec::new(), vec![(1, 1, vec![op(Op::Halt)])]);
        let mut vm = native_caller_vm(program, 100);
        vm.program
            .native
            .set(FuncIdx::from_usize(1), add_five_entry);

        let cl = Value::closure_in(&mut vm.heap, 1, &[]);
        let args = [Value::small_int(37).to_bits()];
        let mut out = 0u64;
        take_freed_objects();
        let status = unsafe {
            al_rt_call_value(
                &raw mut vm,
                ManuallyDrop::new(cl).to_bits(),
                1,
                args.as_ptr(),
                1,
                &raw mut out,
            )
        };

        assert_eq!(status, NativeStatus::Done);
        assert_eq!(small(out), 42);
        assert_eq!(vm.frames.len(), 1);
        assert_eq!(vm.frames[0].ip, 1);
        assert_eq!(vm.stack.len(), 0);
        assert_eq!(vm.native_reds, 99);
        // The frame pop released the callee handle — the closure's only
        // reference — so the cell is already freed.
        assert_eq!(take_freed_objects(), 1);
    }

    #[test]
    fn dynamic_call_checks_the_callee_and_arity() {
        let program = program_with(Vec::new(), vec![(1, 1, vec![op(Op::Halt)])]);
        let mut vm = native_caller_vm(program, 100);

        // Not a closure: the interpreter's `Op::Call` type error.
        let mut out = 0u64;
        let status = unsafe {
            al_rt_call_value(
                &raw mut vm,
                Value::small_int(3).to_bits(),
                1,
                std::ptr::null(),
                0,
                &raw mut out,
            )
        };
        assert_eq!(status, NativeStatus::Error);
        assert!(matches!(
            vm.outcome_from_status(status),
            Err(VmError::Internal(_))
        ));

        // Arity mismatch: fn 1 takes one argument, none supplied.
        let cl = Value::closure_in(&mut vm.heap, 1, &[]);
        let status = unsafe {
            al_rt_call_value(
                &raw mut vm,
                ManuallyDrop::new(cl).to_bits(),
                1,
                std::ptr::null(),
                0,
                &raw mut out,
            )
        };
        assert_eq!(status, NativeStatus::Error);
        assert!(matches!(
            vm.outcome_from_status(status),
            Err(VmError::Internal(_))
        ));
        // No frame was pushed by either failure.
        assert_eq!(vm.frames.len(), 1);
    }

    #[test]
    fn dynamic_tail_call_collapses_with_the_closure_handle() {
        // fn1: interpreter-only identity — the collapsed frame is driven
        // under the frame floor when the caller's driver dispatches it.
        let program = program_with(
            Vec::new(),
            vec![(1, 1, vec![op_arg(Op::PushLocal, 0), op(Op::Ret)])],
        );
        let mut vm = native_caller_vm(program, 100);

        let cl = Value::closure_in(&mut vm.heap, 1, &[]);
        let args = [Value::small_int(37).to_bits()];
        take_freed_objects();
        let status = unsafe {
            al_rt_tail_call_value(
                &raw mut vm,
                ManuallyDrop::new(cl).to_bits(),
                args.as_ptr(),
                1,
            )
        };
        // The caller frame is collapsed in place, the closure installed as
        // its `captures` handle, and the driver is told to dispatch it.
        assert_eq!(status, NativeStatus::TailCall);
        assert_eq!(vm.frames.len(), 1);
        assert_eq!(vm.frames[0].func_idx, 1);
        assert_eq!(vm.frames[0].ip, 0);
        assert!(vm.frames[0].captures.as_closure().is_some());
        assert_eq!(take_freed_objects(), 0);

        let status = vm.drive_top_frame();
        assert_eq!(status, NativeStatus::Done);
        assert_eq!(vm.frames.len(), 0);
        assert_eq!(vm.stack.len(), 1);
        assert_eq!(small(vm.stack[0].to_bits()), 37);
        // The frame pop released the handle: the closure is freed.
        assert_eq!(take_freed_objects(), 1);
    }

    #[test]
    fn rt_symbols_are_unique_and_non_null() {
        let syms = rt_symbols();
        for (i, (name, addr)) in syms.iter().enumerate() {
            assert!(!addr.is_null(), "{name} has a null address");
            assert!(
                syms[..i].iter().all(|(n, _)| n != name),
                "duplicate symbol {name}"
            );
        }
    }
}
