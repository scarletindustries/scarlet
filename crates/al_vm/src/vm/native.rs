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

thread_local! {
    /// The scheduler's VM for this OS thread, re-published every
    /// `scheduler_loop` iteration. Nothing reads it yet except
    /// [`current_vm`]; it exists so shims can re-derive the VM instead of
    /// trusting a `vmx` spilled into a frame that may resume on another
    /// thread.
    static CURRENT_VM: std::cell::Cell<*mut VM> = const { std::cell::Cell::new(std::ptr::null_mut()) };
}

/// Publish `vm` as this scheduler thread's VM.
pub(super) fn set_current_vm(vm: *mut VM) {
    CURRENT_VM.with(|c| c.set(vm));
}

/// The VM last published on this thread, null on a non-scheduler thread.
/// Only the shim trampoline should dereference it; the caller owns the
/// aliasing obligations.
pub fn current_vm() -> *mut VM {
    CURRENT_VM.with(std::cell::Cell::get)
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
    pub(super) fn call_native(&mut self, entry: NativeEntry) -> NativeStatus {
        self.native_ctx.vm = (self as *mut VM).cast();
        // SAFETY: `entry` is a finalized JIT entry from this program's native
        // table, and the pointer is to this VM's live `native_ctx`.
        #[allow(unsafe_code)]
        unsafe {
            crate::bytecode::native::call_entry_preserving_pinned(
                (&raw mut self.native_ctx).cast(),
                entry,
            )
        }
    }

    /// Encode a slice outcome as a [`NativeStatus`], parking any payload in
    /// the VM for [`VM::outcome_from_status`] to rehydrate.
    pub(super) fn status_from_outcome(&mut self, outcome: VmResult<Step>) -> NativeStatus {
        match outcome {
            Ok(Step::Done) => NativeStatus::Done,
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
    fn enter_interp(&mut self) -> NativeStatus {
        // Re-entries nest, and each restores the floor it found, so once a
        // non-Done status has unwound to `scheduler_loop` the floor is 0.
        let saved = self.native_floor;
        debug_assert!(
            self.frames.len() > saved,
            "native→interp call without a pushed callee frame"
        );
        self.native_floor = self.frames.len() - 1;
        let floor = self.native_floor;
        // The nested slice spends the caller's remaining budget, so a native
        // fn looping over interpreted callees yields as often as the
        // all-interpreted program would.
        let outcome = self.execute_slice_budgeted(self.native_reds);
        self.native_floor = saved;
        match outcome {
            Ok(Step::Done) if self.frames.len() == floor => NativeStatus::Done,
            // Done away from the floor is `Op::Halt`, which only `__main__`
            // carries, and `__main__` can never sit above a native caller.
            // Refuse rather than hand the caller a stack it no longer owns.
            Ok(Step::Done) => self.status_from_outcome(Err(VmError::internal(
                "process halted under a native caller",
            ))),
            other => self.status_from_outcome(other),
        }
    }

    /// The native twin of the interpreter's `checkpoint!`. Returns true when
    /// the budget is spent and the caller must yield; the caller must leave
    /// the top frame consistent at its resume point first.
    fn native_checkpoint(&mut self) -> bool {
        let mut reds = self.native_reds - 1;
        self.charge_reclamation(&mut reds);
        self.native_reds = reds;
        reds <= 0
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
        self.collapse_tail_frame(base, args_start);
        let f = self.frame_mut();
        f.func_idx = target;
        f.code_start = code_start;
        f.ip = 0;
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
    /// keeps tail chains flat and `TailCall` off the interpreter boundary.
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

    /// `enter_frame!`'s tail branch for a dynamic callee. Assigning the
    /// closure as the collapsed frame's `captures` releases the caller's own
    /// handle, as the interpreter's tail branch does.
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

    /// [`al_rt_call`]'s body once the argument words are on the stack.
    fn rt_call(&mut self, target: i32, resume_ip: i32, out: *mut u64) -> NativeStatus {
        self.frame_mut().ip = resume_ip;
        self.push_known_frame(target);
        self.drive_pushed_callee(out)
    }

    /// [`al_rt_call_value`]'s body. A failed callee check surfaces as an
    /// error status.
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

    /// Charge the entry checkpoint, drive the freshly pushed callee frame,
    /// and on `Done` pop the result out to the native caller.
    #[allow(unsafe_code)] // one write into the caller-provided out-slot
    fn drive_pushed_callee(&mut self, out: *mut u64) -> NativeStatus {
        // The callee frame is already consistent at ip 0, so exhaustion is a
        // plain unwind and resume re-enters it from the top.
        if self.native_checkpoint() {
            return NativeStatus::Yield;
        }
        let status = self.drive_top_frame();
        if status != NativeStatus::Done {
            return status;
        }
        // Ownership of the reference moves with the bits.
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

/// Call an interpreter-only function from a compiled body.
///
/// The caller completes the same frame handshake as for a native callee,
/// then calls this instead of a table entry. The interpreter runs the callee
/// with a frame floor at the caller's depth: a `Ret` down to the floor ends
/// the slice with [`NativeStatus::Done`] and the result on the stack top.
///
/// A suspension inside the callee comes back as a non-`Done` status and no
/// native frame survives it. On wake the parked frame finishes interpreted,
/// and the native caller's frame then continues as *bytecode* from its
/// stored resume ip.
///
/// # Safety
/// `vmx` must point at the running scheduler's live `VM`, with the callee
/// frame installed as described above; no other reference to the VM may be
/// live for the duration of the call.
#[allow(unsafe_code)] // the designated native→interp re-entry point; contract above
pub unsafe extern "C" fn al_rt_enter_interp(vmx: *mut VM) -> NativeStatus {
    // SAFETY: `vmx` is the running scheduler's live VM per the contract.
    let vm = unsafe { &mut *vmx };
    vm.enter_interp()
}

/// The re-entry shim as a `(symbol name, address)` pair for
/// `JITBuilder::symbol`.
pub fn enter_interp_symbol() -> (&'static str, *const u8) {
    ("al_rt_enter_interp", al_rt_enter_interp as *const u8)
}

/// Call a known capture-free function from a compiled body: the whole
/// interpreter `CallKnown` sequence behind the C ABI. `resume_ip` is the
/// caller's own *bytecode* resume point, so a suspension under the callee
/// resumes the caller's remainder interpreted. `args` ownership transfers
/// in; on `Done` the result word is written to `out`.
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

/// Call a dynamic callee from a compiled body. `callee` is an owned closure
/// word; it is installed as the callee frame's `captures` handle, where
/// `PushCapture` expects to find the environment. A non-closure callee or an
/// arity mismatch surfaces as [`NativeStatus::Error`].
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

/// The frame-handshake half of a direct native→native call: [`al_rt_call`]
/// up to but not including `drive_top_frame`. On `Done` the caller emits a
/// direct machine call to the peer's `NativeEntry` and hands the status to
/// [`al_rt_direct_result`]. The pair is `al_rt_call` with the table lookup
/// and indirect dispatch replaced by a statically resolved call.
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

/// The return-path half of a direct native→native call. `status` is what the
/// peer's `NativeEntry` returned; on `Done` the result is popped into `out`.
///
/// [`NativeStatus::TailCall`] must be consumed here, never propagated: the
/// caller's `CallFrame` sits under the collapsed one, so unwinding on it
/// would strand that frame.
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
            // SAFETY: `out` is valid for one u64 write per the contract.
            unsafe { out.write(std::mem::ManuallyDrop::new(result).to_bits()) };
            NativeStatus::Done
        }
        None => vm.status_from_outcome(Err(VmError::internal(
            "native call returned Done with an empty stack",
        ))),
    }
}

/// Cross-function tail call from a compiled body: the interpreter's
/// `TailCallKnown` surgery behind the C ABI. Returns
/// [`NativeStatus::TailCall`] for the driver that invoked the caller to
/// dispatch, or `Yield` if the budget ran out (collapsed frame at `ip == 0`).
/// A tail-call site compiles to `return al_rt_tail_call(...)`, so the
/// caller's machine frame is gone before the callee runs.
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

/// Dynamic cross-function tail call: `Op::TailCall`'s surgery behind the C
/// ABI, the closure becoming the collapsed frame's `captures` handle. The
/// caller returns the status verbatim.
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

/// `Op::MakeClosure` for a compiled body. Each capture word transfers one
/// owned reference in; the cell takes its own copy and the transferred
/// references are released. The returned bits carry one owned reference.
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
    // SAFETY: `Value` is `repr(transparent)` over its u64 bits.
    let borrowed: &[Value] = if n == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(caps.cast::<Value>(), n) }
    };
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
pub unsafe extern "C" fn al_rt_checkpoint(vmx: *mut VM) -> NativeStatus {
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
pub unsafe extern "C" fn al_rt_frame_base(vmx: *mut VM) -> *mut u64 {
    // SAFETY: `vmx` is the running scheduler's live VM per the contract.
    let vm = unsafe { &mut *vmx };
    let base = vm.frame().base_slot;
    // SAFETY: `base_slot < stack.len()` for a live frame, and `Value` is
    // `repr(transparent)` over its u64 bits.
    unsafe { vm.stack.as_mut_ptr().add(base).cast::<u64>() }
}

/// The compiled epilogue: `ret!`'s stack surgery behind the C ABI. Leaves
/// the caller frame on top with the result on the stack top. Ownership of
/// `result` transfers in.
///
/// # Safety
/// `vmx` must point at the running scheduler's live `VM` with the returning
/// compiled function's frame on top; `result` must be an owned value word.
#[allow(unsafe_code)] // the native return seam; contract above
pub unsafe extern "C" fn al_rt_ret(vmx: *mut VM, result: u64) {
    // SAFETY: `vmx` is the running scheduler's live VM per the contract.
    let vm = unsafe { &mut *vmx };
    // Unreachable with a frame installed; degrade to a no-op rather than
    // unwind across the C boundary.
    let Some(frame) = vm.frames.pop() else {
        debug_assert!(false, "al_rt_ret with no active call frame");
        return;
    };
    vm.stack.truncate(frame.base_slot);
    // SAFETY: `result` is an owned value word per the contract.
    vm.stack.push(unsafe { Value::from_bits(result) });
    // `frame` drops here, releasing its captures handle.
}

/// Every `al_rt_*` boundary shim as a `(symbol name, address)` pair for JIT
/// symbol registration.
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

    extern "C" fn yield_entry(ctx: *mut core::ffi::c_void) -> NativeStatus {
        assert!(!ctx.is_null());
        NativeStatus::Yield
    }

    /// Reads the VM out of the entry's `NativeCtx` the way a compiled
    /// prologue does.
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

    // fn 0 ("main") is a single `Halt`, standing in for the native caller's
    // bytecode body; fn 1 is `callee` at addresses [1, 1 + len).
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
    // `al_rt_enter_interp` call: its own frame below with the resume ip
    // stored, the interpreted callee's fresh frame on top.
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
        assert_eq!(vm.frames.len(), 1);
        assert_eq!(vm.stack.len(), 1);
        assert_eq!(vm.stack[0].to_bits(), Value::small_int(42).to_bits());
        assert_eq!(vm.native_floor, 0);
        assert!(vm.native_pending.is_none());
    }

    #[test]
    fn interp_callee_park_unwinds_and_resume_finishes_interpreted() {
        // callee: sleep(1ms), then return the nil the wake left on top.
        let mut vm = vm_with_callee_pushed(
            vec![Value::small_int(1)],
            vec![op_arg(Op::PushConst, 0), op(Op::Sleep), op(Op::Ret)],
        );
        let status = unsafe { al_rt_enter_interp(&raw mut vm) };
        // The parked continuation is entirely (stack, frames): callee frame
        // intact, `ip` on the `Ret` after `Sleep`.
        assert_eq!(status, NativeStatus::Parked);
        assert_eq!(vm.native_floor, 0);
        assert_eq!(vm.frames.len(), 2);
        assert_eq!(vm.frames[1].ip, 2);
        match vm.outcome_from_status(status) {
            Ok(Step::Parked(Wait::Timer(_))) => {}
            _ => panic!("expected the sleep's timer park back"),
        }
        // Resume finishes the callee interpreted, then the native caller's
        // frame continues as bytecode from its stored resume ip.
        let resumed = vm.execute_slice().expect("resume must not error");
        assert!(matches!(resumed, Step::Done));
        assert_eq!(vm.stack.len(), 1);
        // `sleep` pushes the VM's Nil enum, not the raw immediate.
        assert_eq!(vm.stack[0].to_bits(), vm.make_nil().to_bits());
    }

    #[test]
    fn interp_callee_yield_unwinds_with_a_resumable_frame() {
        // callee: an infinite self-tail loop, so it yields at the back-edge
        // checkpoint with `ip == 0`.
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
        // callee: `Sleep` over an empty stack.
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
        // As if an outer re-entry (floor 1) is on the machine stack and the
        // native code under it now calls a further interpreted callee.
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

    use std::mem::ManuallyDrop;

    use crate::bytecode::NativeTable;

    /// Entry fn 0 ("main") is `[Nop, Halt]`, so ip 1 is a realistic non-zero
    /// resume point; fn i+1 gets `(arity, locals, body)`.
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

    /// A VM in the state a native caller runs in: main's frame installed as
    /// the stand-in caller, boundary reds counter seeded.
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

    /// A compiled body stand-in for `fn add_five(n) { n + 5 }`.
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
        assert_eq!(vm.frames.len(), 1);
        assert_eq!(vm.frames[0].ip, 1);
        assert_eq!(vm.stack.len(), 0);
        // Exactly one reduction charged, matching the interpreter.
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
        // its frame is consistent, so resume re-enters it from the top.
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

    /// A compiled body stand-in for `fn f1(n) { f2(n + 1) }`, the call in
    /// tail position.
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

        // f1 returned `TailCall` before f2 ran; the driver loop inside
        // `al_rt_call` dispatched the collapsed frame, so only `Done` escapes.
        assert_eq!(status, NativeStatus::Done);
        assert_eq!(small(out), 42);
        assert_eq!(vm.frames.len(), 1);
        assert_eq!(vm.stack.len(), 0);
        // Two applications, two reductions: the call and the tail call.
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
        // Budget 2 puts the yield exactly at the tail edge.
        let mut vm = native_caller_vm(program, 2);
        vm.program
            .native
            .set(FuncIdx::from_usize(1), tail_to_f2_entry);

        let args = [Value::small_int(41).to_bits()];
        let mut out = 0u64;
        let status = unsafe { al_rt_call(&raw mut vm, 1, 1, args.as_ptr(), 1, &raw mut out) };

        // The caller frame was already collapsed, so the top frame is f2 at
        // ip 0 with its argument in place.
        assert_eq!(status, NativeStatus::Yield);
        assert_eq!(vm.frames.len(), 2);
        assert_eq!(vm.frames[1].func_idx, 2);
        assert_eq!(vm.frames[1].ip, 0);
        assert_eq!(vm.frames[1].base_slot, 0);
        assert_eq!(vm.stack.len(), 1);
        assert_eq!(small(vm.stack[0].to_bits()), 42);
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

        // Exhaustion: yield with the frame resumable from the top.
        vm.native_reds = 1;
        assert_eq!(
            unsafe { al_rt_checkpoint(&raw mut vm) },
            NativeStatus::Yield
        );
        assert_eq!(vm.frames[1].ip, 0);
        assert!(vm.native_reds <= 0);
    }

    /// A compiled body stand-in for `fn f1() { loop { f2() } }` over an
    /// interpreter-only f2.
    extern "C" fn call_interp_forever_entry(ctx: *mut core::ffi::c_void) -> NativeStatus {
        let vm: *mut VM = vm_from_ctx(ctx);
        loop {
            let mut out = 0u64;
            let status = unsafe { al_rt_call(vm, 2, 0, std::ptr::null(), 0, &raw mut out) };
            if status != NativeStatus::Done {
                return status;
            }
        }
    }

    #[test]
    fn interp_callee_spends_the_native_callers_budget() {
        // f1: native, loops calling f2. f2: interpreter-only, one `CallKnown`
        // to f3 per invocation. f3: returns a constant.
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

        // Each iteration costs two reductions, so a budget of 10 dies
        // mid-callee on the fifth. If each native→interp re-entry got a
        // fresh budget, the yield would land on the tenth entry checkpoint
        // with f2's frame on top instead.
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
        // The shim's internal retain/release pair cancels: nothing freed yet.
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
        // The frame pop released the closure's only reference.
        assert_eq!(take_freed_objects(), 1);
    }

    #[test]
    fn dynamic_call_checks_the_callee_and_arity() {
        let program = program_with(Vec::new(), vec![(1, 1, vec![op(Op::Halt)])]);
        let mut vm = native_caller_vm(program, 100);

        // Not a closure.
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
        // fn1: interpreter-only identity.
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
