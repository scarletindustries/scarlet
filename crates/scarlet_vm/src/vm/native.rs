//! The VM half of the native calling convention: how JIT-compiled bodies and
//! the interpreter hand a running process back and forth.
//!
//! The ABI ([`NativeStatus`], [`NativeEntry`]) is declared in
//! [`crate::bytecode::native`], where the VM cannot be named, so the entry's
//! `vmx` argument is opaque there. This module pins it down as `&mut VM` and
//! round-trips the status word to `VmResult<Step>`.
//!
//! Values do not cross as C arguments. The caller installs the callee's
//! [`CallFrame`](super::CallFrame) and leaves the arguments in slots
//! `[base_slot, base_slot + arity)`, and a `Done` callee has already applied
//! the interpreter's return protocol. Compiled code keeps the interpreter's
//! frame layout bit-for-bit; that is what makes per-function fallback,
//! preemption and process migration work unchanged.
//!
//! `Parked` and `Error` carry a payload the one-word status cannot, so
//! [`VM::status_from_outcome`] parks it in the VM while the status unwinds
//! the native frames, and [`VM::outcome_from_status`] rehydrates it at the
//! trampoline. Every suspending call compiles to
//! `status = call(...); if status != Done { return status; }`. Resume
//! re-enters at the top frame's `ip`, which for a native function is a
//! resume-point index (`0` = from the top).
//!
//! The `al_rt_*` shims below are the runtime half of each call shape a
//! compiled body can make, reached by symbol (see [`rt_symbols`]): known
//! call, dynamic call, cross-function tail call, self-tail back-edge, and
//! the fall back to the interpreter ([`al_rt_enter_interp`], which runs
//! `execute_slice` under a frame floor at the caller's depth). A tail call
//! returns [`NativeStatus::TailCall`] to [`VM::drive_top_frame`], which
//! dispatches the collapsed frame, so tail chains run on a flat machine
//! stack.
//!
//! `VM::native_reds` keeps reduction parity: entering a function charges one
//! reduction plus accrued reclamation debt, exactly like the interpreter's
//! `checkpoint!`, and exhaustion yields with the callee frame at `ip == 0`.

use crate::FuncIdx;
use crate::bytecode::{NativeEntry, NativeStatus, Value};
use crate::tivec::Idx;

use super::poll::Wait;
use super::{CallFrame, Step, VM, VmError, VmResult};

/// What a `prepare_*` / `ret_transfer` shim hands back to compiled code:
/// where control goes next. Two machine words, returned in registers on both
/// supported ABIs.
///
/// - `entry` non-null: `return_call_indirect(entry, [ctx, aux])` — `aux` is
///   the resume ordinal to enter at (0 for a fresh callee, the stored
///   continuation for a return transfer).
/// - `entry` null: `return aux` — a plain status for the trampoline
///   (`Done` = dispatch the new top frame / slice finished, `Yield`,
///   `Error`).
#[repr(C)]
pub(crate) struct PreparedCall {
    entry: *const u8,
    aux: u64,
}

impl PreparedCall {
    fn status(s: NativeStatus) -> PreparedCall {
        PreparedCall {
            entry: std::ptr::null(),
            aux: s as u64,
        }
    }

    fn enter(entry: NativeEntry, resume: i64) -> PreparedCall {
        PreparedCall {
            entry,
            aux: resume as u64,
        }
    }
}

/// The payload half of a [`NativeStatus::Parked`]/[`NativeStatus::Error`],
/// parked in the VM while the status word unwinds the native frames above
/// it. At most one is in flight per process.
///
/// Always boxed: the slot lives on every `Process` and is empty except
/// mid-unwind, so inlining these wide variants cost +76 B per parked process.
pub(super) enum NativePending {
    Parked(Wait),
    Error(VmError),
}

impl VM {
    /// Invoke a compiled function body. The caller must have pushed the
    /// callee `CallFrame` and left the arguments in
    /// `[base_slot, base_slot + arity)`.
    ///
    /// Must go through [`call_entry_preserving_pinned`]. `enable_pinned_reg`
    /// (jit.rs) gives the JIT the pinned register (x86_64 r15 / aarch64 x21)
    /// by dropping it from Cranelift's callee-save set, so compiled entries
    /// clobber a register the platform ABI says is callee-saved. The shim
    /// saves and restores it around the indirect call. Without it a Rust
    /// caller's live value is silently corrupted in release with no test
    /// failing.
    #[inline(never)]
    pub(super) fn call_native(&mut self, entry: NativeEntry, resume: i64) -> NativeStatus {
        self.native_ctx.vm = (self as *mut VM).cast();
        let tramp = self.program.native.trampoline();
        debug_assert!(!tramp.is_null(), "entry published without a trampoline");
        // SAFETY: `entry` and `tramp` are finalized code from this program's
        // native table, and the pointer is to this VM's live `native_ctx`.
        #[allow(unsafe_code)]
        unsafe {
            crate::bytecode::native::call_entry_preserving_pinned(
                (&raw mut self.native_ctx).cast(),
                tramp,
                entry,
                resume,
            )
        }
    }

    /// Encode a slice outcome as a [`NativeStatus`], parking any payload in
    /// the VM for [`VM::outcome_from_status`] to rehydrate.
    pub(super) fn status_from_outcome(&mut self, outcome: VmResult<Step>) -> NativeStatus {
        match outcome {
            // `Dispatch` never crosses the native boundary: it is minted by
            // the interpreter for `run_slice` alone.
            Ok(Step::Done | Step::Dispatch) => NativeStatus::Done,
            Ok(Step::Yield) => NativeStatus::Yield,
            Ok(Step::Parked(wait)) => {
                // A leftover payload means a native frame swallowed a
                // non-Done status instead of unwinding with it.
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

    /// Decode a [`NativeStatus`] back into an interpreter outcome,
    /// rehydrating the pending payload.
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
            // `drive_top_frame` consumes this; reaching here means a native
            // frame forwarded a status it was required to drive.
            NativeStatus::TailCall => Err(VmError::internal(
                "NativeStatus::TailCall escaped its trampoline",
            )),
        }
    }

    /// [`al_rt_enter_interp`]'s body: run the interpreter until the frame
    /// the native caller pushed returns, then encode the outcome as a status.
    /// The native twin of the interpreter's `checkpoint!`. Returns true when
    /// the budget is spent and the caller must yield; the caller must leave
    /// the top frame consistent at its resume point first.
    fn native_checkpoint(&mut self) -> bool {
        let mut reds = self.native_reds - 1;
        self.charge_reclamation(&mut reds);
        self.native_reds = reds;
        reds <= 0
    }

    /// The transfer decision after a frame push/collapse: charge the entry
    /// checkpoint, then hand back the target's entry (native) or a `Done`
    /// status (interpreted — the trampoline dispatches it).
    fn prepared(&mut self, target: i32) -> PreparedCall {
        if self.native_checkpoint() {
            return PreparedCall::status(NativeStatus::Yield);
        }
        match self
            .program
            .native
            .get(FuncIdx::from_usize(target as usize))
        {
            Some(entry) => PreparedCall::enter(entry, 0),
            None => {
                // The callee will interpret; count the call so a body only
                // ever reached from compiled callers can still warm. The call
                // that crosses the threshold compiles it — enter the fresh
                // entry right away (the frame sits at ip 0, the one point the
                // two ip coordinate spaces coincide).
                self.program
                    .native
                    .note_interpreted_call(FuncIdx::from_usize(target as usize));
                match self
                    .program
                    .native
                    .get(FuncIdx::from_usize(target as usize))
                {
                    Some(entry) => {
                        self.frame_mut().native = true;
                        PreparedCall::enter(entry, 0)
                    }
                    None => PreparedCall::status(NativeStatus::Done),
                }
            }
        }
    }

    /// `enter_frame!`'s non-tail branch for a known capture-free target.
    /// The caller must have stored its own resume ip already.
    fn push_known_frame(&mut self, target: i32) {
        let func = &self.program.functions[target as usize];
        let (arity, locals, code_start) = (func.arity, func.locals, func.code_start);
        debug_assert_eq!(func.capture_count, 0);
        let args_start = self.stack.len() - arity as usize;
        self.frames.push(CallFrame {
            func_idx: target,
            code_start,
            ip: 0,
            native: self
                .program
                .native
                .get(FuncIdx::from_usize(target as usize))
                .is_some(),
            base_slot: args_start,
            captures: Value::small_int(0),
        });
        for _ in arity..locals {
            self.stack.push(Value::small_int(0));
        }
    }

    /// `enter_frame!`'s tail branch for a known capture-free target (the
    /// `TailCallKnown` surgery). The collapsed frame is left at `ip == 0`, so
    /// a yield right after resumes the callee from the top.
    fn collapse_known_frame(&mut self, target: i32) {
        let func = &self.program.functions[target as usize];
        let (arity, locals, code_start) = (func.arity, func.locals, func.code_start);
        debug_assert_eq!(func.capture_count, 0);
        let args_start = self.stack.len() - arity as usize;
        let base = self.frame().base_slot;
        let native = self
            .program
            .native
            .get(FuncIdx::from_usize(target as usize))
            .is_some();
        self.collapse_tail_frame(base, args_start);
        let f = self.frame_mut();
        f.func_idx = target;
        f.code_start = code_start;
        f.ip = 0;
        // ip 0 is the one point the two ip coordinate spaces coincide, so the
        // collapsed frame may switch engines here.
        f.native = native;
        // Assigning the capture-free sentinel releases the caller's closure
        // handle, as the interpreter's tail branch does.
        f.captures = Value::small_int(0);
        for _ in arity..locals {
            self.stack.push(Value::small_int(0));
        }
    }

    /// The trampoline: drive the top frame to a boundary status, consuming
    /// [`NativeStatus::TailCall`] by re-dispatching the collapsed frame.
    /// Every native entry invocation must go through here, which is what
    /// The `Op::Call`/`Op::TailCall` callee checks: closure value, matching
    /// arity. Returns the `func_idx` both the entry table and the
    /// interpreter dispatch on.
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

    /// `enter_frame!`'s non-tail branch for a dynamic callee. The closure
    /// value itself becomes the frame's `captures` handle: one word moved,
    /// no captures clone, as in the interpreter's `Op::Call`.
    fn push_closure_frame(&mut self, callee: Value, argc: i64) -> VmResult<i32> {
        let target = self.dynamic_target(&callee, argc)?;
        let func = &self.program.functions[target as usize];
        let (arity, locals, code_start) = (func.arity, func.locals, func.code_start);
        let args_start = self.stack.len() - arity as usize;
        self.frames.push(CallFrame {
            func_idx: target,
            code_start,
            ip: 0,
            native: self
                .program
                .native
                .get(FuncIdx::from_usize(target as usize))
                .is_some(),
            base_slot: args_start,
            captures: callee,
        });
        for _ in arity..locals {
            self.stack.push(Value::small_int(0));
        }
        Ok(target)
    }

    /// `enter_frame!`'s tail branch for a dynamic callee. Assigning the
    /// closure as the collapsed frame's `captures` releases the caller's own
    /// handle, as the interpreter's tail branch does.
    fn collapse_closure_frame(&mut self, callee: Value, argc: i64) -> VmResult<i32> {
        let target = self.dynamic_target(&callee, argc)?;
        let func = &self.program.functions[target as usize];
        let (arity, locals, code_start) = (func.arity, func.locals, func.code_start);
        let args_start = self.stack.len() - arity as usize;
        let base = self.frame().base_slot;
        let native = self
            .program
            .native
            .get(FuncIdx::from_usize(target as usize))
            .is_some();
        self.collapse_tail_frame(base, args_start);
        let f = self.frame_mut();
        f.func_idx = target;
        f.code_start = code_start;
        f.ip = 0;
        f.native = native;
        f.captures = callee;
        for _ in arity..locals {
            self.stack.push(Value::small_int(0));
        }
        Ok(target)
    }
}

/// Move `argc` argument words from the native caller's scratch onto the
/// value stack. Ownership transfers with the bits, so there is no rc traffic.
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

/// `Op::MakeClosure` for a compiled body. Each capture word transfers one
/// owned reference in; the cell takes its own copy and the transferred
/// references are released. The returned bits carry one owned reference.
///
/// # Safety
/// `vmx` must point at the running scheduler's live `VM`; `caps` must be
/// valid for `count` reads of owned value words (it may be dangling when
/// `count == 0`).
#[allow(unsafe_code)] // the closure-allocation seam; contract above
pub(crate) unsafe extern "C" fn al_rt_make_closure(
    vmx: *mut VM,
    func_idx: i64,
    caps: *const u64,
    count: i64,
) -> u64 {
    // SAFETY: `vmx` is the running scheduler's live VM per the contract.
    let vm = unsafe { &mut *vmx };
    let n = count.max(0) as usize;
    // SAFETY: `caps` holds `n` value words per the contract.
    let borrowed = unsafe { Value::slice_from_words(caps, n) };
    let v = Value::closure_in(&mut vm.heap, func_idx as i32, borrowed);
    for i in 0..n {
        // SAFETY: releases the one reference each word transferred in.
        drop(unsafe { Value::from_bits(caps.add(i).read()) });
    }
    std::mem::ManuallyDrop::new(v).to_bits()
}

/// The reds checkpoint at a self-tail-call back-edge.
/// [`NativeStatus::Done`] means keep looping; `Yield` points `ip` at 0 so
/// re-entry re-runs the function with the already-updated arguments.
///
/// # Safety
/// `vmx` must point at the running scheduler's live `VM`, its top frame the
/// compiled function's, with the next iteration's arguments already written
/// into the argument slots and `stack.len() == base_slot + locals`.
#[allow(unsafe_code)] // the self-tail back-edge seam; contract above
pub(crate) unsafe extern "C" fn al_rt_checkpoint(vmx: *mut VM) -> NativeStatus {
    // On exhaustion the frame must resume at its head: a self-tail loop's
    // back-edge state is exactly "params rebound in slots, enter at 0".

    // SAFETY: `vmx` is the running scheduler's live VM per the contract.
    let vm = unsafe { &mut *vmx };
    if vm.native_checkpoint() {
        vm.frame_mut().ip = 0;
        return NativeStatus::Yield;
    }
    NativeStatus::Done
}

/// The address of the top frame's first slot; slot `i` is the word at
/// `base + 8*i`. Any `al_rt_*` call shim can grow the value stack and
/// invalidate this, so compiled code must re-fetch it after every such call.
///
/// # Safety
/// `vmx` must point at the running scheduler's live `VM` with the compiled
/// function's frame on top.
#[allow(unsafe_code)] // the frame-slot access seam; contract above
pub(crate) unsafe extern "C" fn al_rt_frame_base(vmx: *mut VM) -> *mut u64 {
    // SAFETY: `vmx` is the running scheduler's live VM per the contract.
    let vm = unsafe { &mut *vmx };
    let base = vm.frame().base_slot;
    // SAFETY: `base_slot < stack.len()` for a live frame, and `Value` is
    // `repr(transparent)` over its u64 bits.
    unsafe { vm.stack.as_mut_ptr().add(base).cast::<u64>() }
}

/// The compiled epilogue as a transfer: pop the frame, truncate to its base,
/// push the result, and say where control goes — the native parent's entry at
/// its stored resume ordinal, or `Done` for the trampoline (empty frames, or
/// an interpreted parent).
///
/// # Safety
/// `vmx` must point at the running scheduler's live `VM` with the returning
/// compiled function's frame on top; `result` must be an owned value word.
#[allow(unsafe_code)] // the native return seam; contract above
pub(crate) unsafe extern "C" fn al_rt_ret_transfer(vmx: *mut VM, result: u64) -> PreparedCall {
    // SAFETY: `vmx` is the running scheduler's live VM per the contract.
    let vm = unsafe { &mut *vmx };
    let Some(frame) = vm.frames.pop() else {
        crate::bytecode::value::proof_violation("al_rt_ret_transfer with no active call frame");
    };
    vm.stack.truncate(frame.base_slot);
    // SAFETY: `result` is an owned value word per the contract.
    vm.stack.push(unsafe { Value::from_bits(result) });
    drop(frame);
    let Some(parent) = vm.frames.last() else {
        return PreparedCall::status(NativeStatus::Done);
    };
    // Resume the parent the way it was *entered*: `parent.ip` is a resume
    // ordinal only for a frame that started native. A body can gain an entry
    // while one of its frames is live, so the table is the wrong thing to ask.
    let parent_entry = parent
        .native
        .then(|| {
            vm.program
                .native
                .get(FuncIdx::from_usize(parent.func_idx as usize))
        })
        .flatten();
    match parent_entry {
        Some(entry) => PreparedCall::enter(entry, i64::from(parent.ip)),
        None => PreparedCall::status(NativeStatus::Done),
    }
}

/// Non-tail call from a compiled body, as a transfer. Pushes the args and the
/// callee frame (caller's `ip` set to `resume`, the continuation ordinal) and
/// says where control goes.
///
/// # Safety
/// `vmx` must point at the running scheduler's live `VM`; `args` must point
/// at `argc` initialized owned value words the caller transfers.
#[allow(unsafe_code)] // the call seam; contracts above
pub(crate) unsafe extern "C" fn al_rt_prepare_call(
    vmx: *mut VM,
    target: i64,
    resume: i64,
    args: *const u64,
    argc: i64,
) -> PreparedCall {
    // SAFETY: per the contract above.
    let vm = unsafe { &mut *vmx };
    // SAFETY: `args`/`argc` per the contract.
    unsafe { push_args(vm, args, argc) };
    vm.frame_mut().ip = resume as i32;
    vm.push_known_frame(target as i32);
    vm.prepared(target as i32)
}

/// [`al_rt_prepare_call`] for a dynamic callee. A non-closure callee or an
/// arity mismatch surfaces as an `Error` status.
///
/// # Safety
/// As [`al_rt_prepare_call`], plus: `callee` must be the bits of a `Value`
/// whose reference the caller owns and transfers.
#[allow(unsafe_code)] // the dynamic-call seam; contracts above
pub(crate) unsafe extern "C" fn al_rt_prepare_call_value(
    vmx: *mut VM,
    callee: u64,
    resume: i64,
    args: *const u64,
    argc: i64,
) -> PreparedCall {
    // SAFETY: per the contract above.
    let vm = unsafe { &mut *vmx };
    // SAFETY: `args`/`argc` per the contract.
    unsafe { push_args(vm, args, argc) };
    // SAFETY: `callee` is an owned value word per the contract.
    let callee = unsafe { Value::from_bits(callee) };
    vm.frame_mut().ip = resume as i32;
    match vm.push_closure_frame(callee, argc) {
        Ok(target) => vm.prepared(target),
        Err(err) => PreparedCall::status(vm.status_from_outcome(Err(err))),
    }
}

/// Cross-function tail call, as a transfer: the interpreter's `TailCallKnown`
/// surgery (collapse in place, `ip = 0`), then where control goes.
///
/// # Safety
/// As [`al_rt_prepare_call`].
#[allow(unsafe_code)] // the tail-call seam; contracts above
pub(crate) unsafe extern "C" fn al_rt_prepare_tail(
    vmx: *mut VM,
    target: i64,
    args: *const u64,
    argc: i64,
) -> PreparedCall {
    // SAFETY: per the contract above.
    let vm = unsafe { &mut *vmx };
    // SAFETY: `args`/`argc` per the contract.
    unsafe { push_args(vm, args, argc) };
    vm.collapse_known_frame(target as i32);
    vm.prepared(target as i32)
}

/// [`al_rt_prepare_tail`] for a dynamic callee.
///
/// # Safety
/// As [`al_rt_prepare_call_value`].
#[allow(unsafe_code)] // the dynamic tail-call seam; contracts above
pub(crate) unsafe extern "C" fn al_rt_prepare_tail_value(
    vmx: *mut VM,
    callee: u64,
    args: *const u64,
    argc: i64,
) -> PreparedCall {
    // SAFETY: per the contract above.
    let vm = unsafe { &mut *vmx };
    // SAFETY: `args`/`argc` per the contract.
    unsafe { push_args(vm, args, argc) };
    // SAFETY: `callee` is an owned value word per the contract.
    let callee = unsafe { Value::from_bits(callee) };
    match vm.collapse_closure_frame(callee, argc) {
        Ok(target) => vm.prepared(target),
        Err(err) => PreparedCall::status(vm.status_from_outcome(Err(err))),
    }
}

#[repr(C)]
pub(crate) struct ContEntry {
    pub base: *mut u64,
    pub result: u64,
}

/// # Safety
/// `vmx` must point at the running scheduler's live `VM`, with the caller's
/// frame on top and the callee's result on the value stack.
#[allow(unsafe_code)] // the continuation seam; contract above
pub(crate) unsafe extern "C" fn al_rt_cont(vmx: *mut VM) -> ContEntry {
    // SAFETY: `vmx` is the running scheduler's live VM per the contract.
    let vm = unsafe { &mut *vmx };
    let Some(v) = vm.stack.pop() else {
        crate::bytecode::value::proof_violation("continuation with an empty stack")
    };
    let result = std::mem::ManuallyDrop::new(v).to_bits();
    let base_slot = vm.frame().base_slot;
    ContEntry {
        base: vm.stack.as_mut_ptr().wrapping_add(base_slot).cast(),
        result,
    }
}

/// Every `al_rt_*` boundary shim as a `(symbol name, address)` pair for JIT
/// symbol registration.
pub(crate) fn rt_symbols() -> [(&'static str, *const u8); 9] {
    [
        ("al_rt_make_closure", al_rt_make_closure as *const u8),
        ("al_rt_frame_base", al_rt_frame_base as *const u8),
        ("al_rt_prepare_call", al_rt_prepare_call as *const u8),
        (
            "al_rt_prepare_call_value",
            al_rt_prepare_call_value as *const u8,
        ),
        ("al_rt_prepare_tail", al_rt_prepare_tail as *const u8),
        (
            "al_rt_prepare_tail_value",
            al_rt_prepare_tail_value as *const u8,
        ),
        ("al_rt_ret_transfer", al_rt_ret_transfer as *const u8),
        ("al_rt_cont", al_rt_cont as *const u8),
        ("al_rt_checkpoint", al_rt_checkpoint as *const u8),
    ]
}

#[cfg(test)]
#[allow(unsafe_code)] // drives the extern "C" boundary shims directly
mod tests {
    use std::time::Instant;

    use super::super::{Crash, halt_test_vm};
    use super::*;

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
        let s = vm.status_from_outcome(Err(VmError::Crash(Crash::SliceOutOfBounds {
            lo: 1,
            hi: 9,
            len: 3,
        })));
        assert_eq!(s, NativeStatus::Error);
        match vm.outcome_from_status(s) {
            Err(VmError::Crash(Crash::SliceOutOfBounds {
                lo: 1,
                hi: 9,
                len: 3,
            })) => {}
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

    extern "C" fn yield_entry(_ctx: *mut core::ffi::c_void, _resume: i64) -> NativeStatus {
        NativeStatus::Yield
    }

    /// Stand-in for the module trampoline: test entries are plain Rust
    /// `extern "C"` fns, so the bridge invokes them as such.
    extern "C" fn test_trampoline(
        ctx: *mut core::ffi::c_void,
        entry: *const u8,
        resume: i64,
    ) -> NativeStatus {
        type SysvEntry = extern "C" fn(*mut core::ffi::c_void, i64) -> NativeStatus;
        let f: SysvEntry = unsafe { std::mem::transmute::<*const u8, SysvEntry>(entry) };
        f(ctx, resume)
    }

    #[test]
    fn status_crosses_the_extern_c_boundary() {
        let mut vm = halt_test_vm();
        vm.program
            .native
            .set_trampoline(test_trampoline as *const u8);
        let status = vm.call_native(yield_entry as *const u8, 0);
        assert!(matches!(vm.outcome_from_status(status), Ok(Step::Yield)));
    }

    use std::sync::Arc;

    use crate::bytecode::{Function, Instruction, NativeTable, Op, Program, Value, op};

    use super::super::new_vm;

    /// fn 0 ("main") is `[Nop, Halt]`; fn i+1 gets `(arity, locals, body)`.
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
        let frozen = Arc::new(crate::frozen::FrozenArea::new());
        let mut fb = frozen.builder();
        let (templates, abi) = crate::template::test_fixture::build(&mut fb);
        drop(fb);
        Program {
            constants,
            functions,
            code,
            entry: 0,
            frozen,
            native: NativeTable::new(fn_count),
            templates,
            abi,
            wire_templates: Default::default(),
            wire_descs: Default::default(),
        }
    }

    fn test_vm(program: Program) -> super::super::VM {
        let mut vm = new_vm(program).expect("test VM must construct");
        vm.native_reds = 1_000;
        vm.frames.push(CallFrame {
            func_idx: 0,
            code_start: 0,
            ip: 1,
            native: false,
            base_slot: 0,
            captures: Value::small_int(0),
        });
        vm
    }

    fn small(bits: u64) -> i64 {
        let v = std::mem::ManuallyDrop::new(unsafe { Value::from_bits(bits) });
        v.as_int().expect("expected an int result")
    }

    /// `prepare_call` pushes the callee frame, stamps the caller resume, and
    /// says "interpret it" for a non-native target.
    #[test]
    fn prepare_call_hands_an_interpreted_callee_to_the_trampoline() {
        let program = program_with(Vec::new(), vec![(1, 1, vec![op(Op::Halt)])]);
        let mut vm = test_vm(program);
        let args = [Value::small_int(37).to_bits()];
        let p = unsafe { al_rt_prepare_call(&raw mut vm, 1, 7, args.as_ptr(), 1) };
        assert!(p.entry.is_null());
        assert_eq!(p.aux, NativeStatus::Done as u64);
        assert_eq!(vm.frames.len(), 2);
        assert_eq!(vm.frames[0].ip, 7, "caller resume ordinal stamped");
        assert_eq!(vm.frames[1].ip, 0, "callee enters at its head");
        assert_eq!(small(vm.stack[0].to_bits()), 37);
    }

    /// A native target comes back as its entry pointer with resume 0.
    #[test]
    fn prepare_call_transfers_to_a_native_callee() {
        let program = program_with(Vec::new(), vec![(0, 0, vec![op(Op::Halt)])]);
        let mut vm = test_vm(program);
        vm.program
            .native
            .set_trampoline(test_trampoline as *const u8);
        vm.program
            .native
            .set(crate::FuncIdx::from_usize(1), yield_entry as *const u8);
        let p = unsafe { al_rt_prepare_call(&raw mut vm, 1, 3, std::ptr::null(), 0) };
        assert!(std::ptr::eq(p.entry, yield_entry as *const u8));
        assert_eq!(p.aux, 0, "fresh callee enters at its head");
    }

    /// Budget exhaustion at the entry checkpoint: the callee frame is
    /// consistent at ip 0, so a plain Yield unwinds and resume re-enters it.
    #[test]
    fn prepare_call_yields_on_an_exhausted_budget() {
        let program = program_with(Vec::new(), vec![(0, 0, vec![op(Op::Halt)])]);
        let mut vm = test_vm(program);
        vm.native_reds = 0;
        let p = unsafe { al_rt_prepare_call(&raw mut vm, 1, 3, std::ptr::null(), 0) };
        assert!(p.entry.is_null());
        assert_eq!(p.aux, NativeStatus::Yield as u64);
        assert_eq!(vm.frames.len(), 2);
    }

    /// `ret_transfer` pops the frame, delivers the result, and routes to the
    /// parent: `Done` for an interpreted parent, the entry+resume pair for a
    /// native one.
    #[test]
    fn ret_transfer_routes_by_parent_engine() {
        let program = program_with(Vec::new(), vec![(0, 0, vec![op(Op::Halt)])]);
        let mut vm = test_vm(program);
        // Interpreted parent (main): plain Done.
        vm.frames.push(CallFrame {
            func_idx: 1,
            code_start: 2,
            ip: 0,
            native: false,
            base_slot: 0,
            captures: Value::small_int(0),
        });
        let p = unsafe { al_rt_ret_transfer(&raw mut vm, Value::small_int(9).to_bits()) };
        assert!(p.entry.is_null());
        assert_eq!(p.aux, NativeStatus::Done as u64);
        assert_eq!(vm.frames.len(), 1);
        assert_eq!(small(unsafe { al_rt_cont(&raw mut vm) }.result), 9);

        // Native parent: its entry plus its stored resume ordinal.
        vm.program
            .native
            .set_trampoline(test_trampoline as *const u8);
        vm.program
            .native
            .set(crate::FuncIdx::from_usize(0), yield_entry as *const u8);
        vm.frames[0].ip = 5;
        // The parent must have been *entered* native for its `ip` to be a
        // resume ordinal; publishing an entry after the fact is not enough.
        vm.frames[0].native = true;
        vm.frames.push(CallFrame {
            func_idx: 1,
            code_start: 2,
            ip: 0,
            native: false,
            base_slot: 0,
            captures: Value::small_int(0),
        });
        let p = unsafe { al_rt_ret_transfer(&raw mut vm, Value::small_int(8).to_bits()) };
        assert!(std::ptr::eq(p.entry, yield_entry as *const u8));
        assert_eq!(p.aux, 5, "parent resumes at its stored continuation");
        assert_eq!(small(unsafe { al_rt_cont(&raw mut vm) }.result), 8);
    }

    /// `prepare_tail` collapses in place: same frame count, new function,
    /// ip back to 0.
    #[test]
    fn prepare_tail_collapses_the_frame_in_place() {
        let program = program_with(Vec::new(), vec![(1, 1, vec![op(Op::Halt)])]);
        let mut vm = test_vm(program);
        let args = [Value::small_int(4).to_bits()];
        let p = unsafe { al_rt_prepare_tail(&raw mut vm, 1, args.as_ptr(), 1) };
        assert!(p.entry.is_null());
        assert_eq!(p.aux, NativeStatus::Done as u64);
        assert_eq!(vm.frames.len(), 1, "tail call must not grow the frames");
        assert_eq!(vm.frames[0].func_idx, 1);
        assert_eq!(vm.frames[0].ip, 0);
    }
}
