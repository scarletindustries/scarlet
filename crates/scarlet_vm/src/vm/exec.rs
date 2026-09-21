//! The bytecode interpreter: one process's execution slice.
//!
//! [`VM::execute_slice`] is the hot loop. It hoists the active frame's scalar
//! state (`ip`, `code_start`, `base_slot`, `func_idx`) into locals and syncs
//! back only on call and return, which is what makes preemption at a call a
//! plain `return`.
//!
//! Fat opcode bodies live out of line: [`super::collections`],
//! [`super::text`], [`super::io`] (the parking ops).
//!
//! Dispatch is replicated, not shared: selected arms end in their own copy of
//! fetch-and-switch (`dispatch!`) so the predictor gets a branch-target entry
//! per site. Only arms with a near-deterministic successor carry a copy —
//! replicating the four most-executed arms instead measured slower. The copy
//! decodes only small register-shaped opcodes; anything else leaves `ip` on
//! its instruction and falls out to the shared loop head to be re-fetched.

use crate::FuncIdx;
use crate::abi::AbiSlot;
use crate::bytecode::value::ReuseAddr;
use crate::bytecode::{
    Op, Value, ValueView, freed_objects_pending, take_freed_objects, values_equal,
};
use crate::heap::ProcHeap;
use crate::tivec::Idx;
use smallvec::SmallVec;
use std::time::{SystemTime, UNIX_EPOCH};

use super::mailbox::Delivery;
use super::poll::{Resume, monotonic_now_ms};
use super::processes::Link;
use super::{
    CallFrame, IO_REDUCTION_COST, REDUCTION_BUDGET, Step, VM, VmError, VmResult, freeze, inspect,
    value_type_name,
};

/// Objects freed per reduction charged by [`VM::charge_reclamation`]. Tuned so
/// only big cascading drops (thousands of objects) cost the process budget.
const FREES_PER_REDUCTION: u64 = 256;

/// The arithmetic half of [`VM::charge_reclamation`], split out so the wrap
/// window is reachable in a unit test: `freed` there comes from a
/// thread-local (`FREED_OBJECTS` in `bytecode::value`) incremented one object
/// at a time with no bulk setter, so driving it past `i32::MAX` through the
/// real path means actually freeing ~3 billion objects. This function takes
/// `freed` as a plain argument instead, the same shape `wire::charge_wire`
/// already uses for the same reason.
///
/// `freed / FREES_PER_REDUCTION` is `u64`; `try_from` (rather than `as`)
/// keeps a value past `i32::MAX` from wrapping negative and having
/// `saturating_sub` hand back reductions instead of spending them — same
/// shape as `wire::charge_wire`.
fn charge_for_frees(reds: &mut i32, freed: u64) {
    let charge = i32::try_from(freed / FREES_PER_REDUCTION).unwrap_or(i32::MAX);
    *reds = reds.saturating_sub(charge);
}

impl VM {
    /// One scheduling slice of the current process, dispatched on the top
    /// frame's resume point. `ip == 0` is the only resume point at which a
    /// compiled body may be entered; every other resume point runs the
    /// interpreter, so bytecode is kept for every function as its fallback.
    /// One scheduling slice: the per-frame trampoline. Dispatch the top
    /// frame to whichever engine owns it — a native-table function enters
    /// compiled code at its stored resume ordinal, everything else
    /// interprets — until the slice ends (`Done` with no frames, `Yield`,
    /// `Parked`, `Error`). One reduction budget spans the whole slice no
    /// matter which engine spends it.
    // Non-Done steps are forwarded to the caller unchanged, whatever they are.
    #[allow(unknown_lints, wildcard_over_own_enum)]
    pub(super) fn run_slice(&mut self) -> VmResult<Step> {
        take_freed_objects();
        self.native_reds = REDUCTION_BUDGET;
        loop {
            let f = self.frame();
            let fi = FuncIdx::from_usize(f.func_idx as usize);
            // Dispatch on how the frame was *entered*, not on whether the body
            // has an entry now: `f.ip` is only a resume ordinal for a frame
            // that started native.
            let entry = f.native.then(|| self.program.native.get(fi)).flatten();
            if let Some(entry) = entry {
                let resume = i64::from(f.ip);
                let status = self.call_native(entry, resume);
                match self.outcome_from_status(status)? {
                    Step::Done if self.frames.is_empty() => return Ok(Step::Done),
                    // A transfer to an interpreted frame (call or return):
                    // dispatch the new top.
                    Step::Done => continue,
                    step => return Ok(step),
                }
            } else {
                match self.execute_slice_budgeted(self.native_reds)? {
                    Step::Dispatch => continue,
                    step => return Ok(step),
                }
            }
        }
    }

    /// As [`VM::execute_slice`], but resuming from `budget` remaining
    /// reductions. One budget governs a whole scheduling slice no matter which
    /// backend spends it: a native re-entry resumes the same count, and on a
    /// `Done` exit the remainder goes back to `native_reds`.
    fn execute_slice_budgeted(&mut self, budget: i32) -> VmResult<Step> {
        #[cfg(feature = "op-histogram")]
        super::op_histogram::maybe_dump(&self.program);

        // Hoisted so the per-instruction path avoids two Vec indexes. Synced
        // back to self.frames on Call/TailCall/Ret.
        let (mut ip, mut code_start, mut base_slot, mut func_idx) = {
            let f = self.frame();
            (f.ip, f.code_start, f.base_slot, f.func_idx)
        };
        // No freed-object discard here: a mid-slice continuation must keep the
        // native portion's accrued reclamation debt, charged at the next call
        // checkpoint.
        let mut reds = budget;

        // These macros are defined before the loop so their free
        // `self`/`ip`/`base_slot`/`code_start`/`func_idx`/`reds` resolve to
        // these locals by macro_rules definition-site hygiene. The fetched
        // instruction is threaded in as `$instr` because every dispatch site
        // fetches its own.
        macro_rules! bin {
            ($acc:ident, $ctor:ident, |$a:ident, $b:ident| $body:expr) => {{
                let $b = self.pop()?.$acc();
                let $a = self.pop()?.$acc();
                self.stack.push(Value::$ctor($body));
            }};
        }
        // `push_int` boxes the rare out-of-range spill.
        macro_rules! bin_int {
            ($acc:ident, |$a:ident, $b:ident| $body:expr) => {{
                let $b = self.pop()?.$acc();
                let $a = self.pop()?.$acc();
                self.push_int($body);
            }};
        }
        macro_rules! un_int {
            ($acc:ident, |$a:ident| $body:expr) => {{
                let $a = self.pop()?.$acc();
                self.push_int($body);
            }};
        }
        macro_rules! lc_arith {
            ($instr:ident, |$a:ident, $b:ident| $body:expr) => {{
                let $a = self.stack[base_slot + $instr.a as usize].as_int_typed();
                let $b = self.program.constants[$instr.b as usize].as_int_typed();
                self.push_int($body);
            }};
        }
        macro_rules! lc_jump {
            ($instr:ident, |$a:ident, $b:ident| $cond:expr) => {{
                let $a = self.stack[base_slot + $instr.a as usize].as_int_typed();
                let $b = self.program.constants[$instr.b as usize].as_int_typed();
                if $cond {
                    ip = $instr.operand;
                }
            }};
        }
        // `Some(step)` means the process parked and the slice is over.
        // The op says *where* to resume; only here is it known that a
        // resume point is a bytecode index, so the arithmetic lives here.
        macro_rules! park {
            ($call:expr) => {{
                if let Some(parked) = $call? {
                    self.frame_mut().ip = match parked.resume {
                        Resume::Retry => ip - 1,
                        Resume::Continue => ip,
                    };
                    return Ok(Step::Parked(parked.wait));
                }
            }};
        }
        // `$captures` is expanded into exactly one of the two branches, so a
        // moved value is moved once.
        macro_rules! enter_frame {
            ($target_idx:expr, $arity:expr, $func_locals:expr, $func_code_start:expr, $captures:expr, $tail:expr) => {{
                let target_idx = $target_idx;
                let arity = $arity;
                let func_locals = $func_locals;
                let func_code_start = $func_code_start;
                let args_start = self.stack.len() - arity as usize;
                // One warmth tick per call of a body with no entry yet; the
                // call that crosses the threshold compiles it, so the flag
                // read below already sees the fresh entry.
                let target_native = self
                    .program
                    .native
                    .get(FuncIdx::from_usize(target_idx as usize))
                    .is_some();
                let target_native = if target_native {
                    true
                } else {
                    self.program
                        .native
                        .note_interpreted_call(FuncIdx::from_usize(target_idx as usize));
                    self.program
                        .native
                        .get(FuncIdx::from_usize(target_idx as usize))
                        .is_some()
                };

                if $tail {
                    self.collapse_tail_frame(base_slot, args_start);
                    let f = self.frame_mut();
                    f.func_idx = target_idx;
                    f.code_start = func_code_start;
                    f.ip = 0;
                    // The collapsed frame sits at ip 0, where the bytecode
                    // and resume-ordinal coordinate spaces coincide — the one
                    // point a frame may switch engines.
                    f.native = target_native;
                    f.captures = $captures;
                } else {
                    self.frame_mut().ip = ip;
                    self.frames.push(CallFrame {
                        func_idx: target_idx,
                        code_start: func_code_start,
                        ip: 0,
                        native: target_native,
                        base_slot: args_start,
                        captures: $captures,
                    });
                    base_slot = args_start;
                }

                for _ in arity..func_locals {
                    self.stack.push(Value::small_int(0));
                }

                ip = 0;
                code_start = func_code_start;
                func_idx = target_idx;
            }};
        }
        // Preemption checkpoint at a function application: one reduction plus
        // any reclamation debt accrued since the last one. The debt is paid
        // behind a test because draining unconditionally measured ~1.5x on a
        // tail-recursive loop. `reds` is at least 1 here (a non-positive value
        // yielded at the last checkpoint), so the decrement cannot wrap.
        macro_rules! checkpoint {
            () => {{
                reds -= 1;
                self.charge_reclamation(&mut reds);
                if reds <= 0 {
                    return Ok(Step::Yield);
                }
            }};
        }
        // The interp→native seam. After `enter_frame!` (push or tail
        // collapse) the callee frame sits at `ip == 0`, exactly what a native
        // entry expects, so a table hit hands it to the trampoline. Firing on
        // tail calls too is required, not optional: a native-table function
        // must never be interpreted, or its `frame.ip` would advance to a
        // bytecode position the trampoline later misreads as a resume ordinal.
        macro_rules! native_entry_check {
            ($target:expr) => {{
                // The frame the caller just entered recorded whether it runs
                // native; warmth was already counted at the call edge.
                if self.frame().native {
                    let _ = $target;
                    // Hand the pushed frame to the trampoline; it re-enters
                    // the interpreter for the caller when the callee's return
                    // transfer lands back on it.
                    self.native_reds = reds;
                    return Ok(Step::Dispatch);
                }
            }};
        }

        // Hot-opcode bodies, written once and expanded at both dispatch
        // levels. Cold opcodes are written inline in the full match below.
        macro_rules! push_const {
            ($instr:ident) => {{
                self.stack
                    .push(self.program.constants[$instr.operand as usize].clone());
            }};
        }
        macro_rules! push_local {
            ($instr:ident) => {{
                let slot = base_slot + $instr.operand as usize;
                self.stack.push(self.stack[slot].clone());
            }};
        }
        // The entry frame's slots ARE the program's globals, so a store there
        // freezes and publishes. The frame is identified by function identity
        // (`func_idx == entry`), never by guessing from `base_slot == 0` —
        // a zero-argument call could sit at slot 0 too.
        macro_rules! store_local {
            ($instr:ident) => {{
                let slot = base_slot + $instr.operand as usize;
                let v = self.pop()?;
                self.stack[slot] = v;
                if func_idx == self.program.entry && self.current_is_main {
                    self.publish_toplevel(slot);
                }
            }};
        }
        // Jump operands are frame-relative, which is exactly what `ip` is, so
        // a branch is an assignment and never touches `code_start`.
        macro_rules! jump {
            ($instr:ident) => {{
                ip = $instr.operand;
            }};
        }
        macro_rules! jump_if_false {
            ($instr:ident) => {{
                let cond = self.pop()?;
                if !is_truthy(&cond)? {
                    ip = $instr.operand;
                }
            }};
        }
        // Self-recursion fast path: the callee is the live frame's function, so
        // the closure pop, tag match and arity check are all skipped (the arity
        // is statically guaranteed by `compile_call`).
        macro_rules! call_self {
            ($instr:ident) => {{
                let arity = $instr.operand;
                let func = &self.program.functions[func_idx as usize];
                let func_locals = func.locals;
                let args_start = self.stack.len() - arity as usize;

                if $instr.op == Op::TailCallSelf {
                    // Non-argument locals are preserved across the collapse so
                    // a Perceus `Drop` at end-of-body can hand its hollowed
                    // cell to a `Reuse` at start-of-body next iteration; the
                    // zero-fill below is skipped for that reason.
                    self.collapse_tail_frame_self(
                        base_slot,
                        args_start,
                        arity as usize,
                        func_locals as usize,
                    );
                    // A back-edge counts for warmth like a call: a loop
                    // entered once must still cross the threshold (a server's
                    // accept loop is called exactly once and spins for the
                    // process's life). The edge that crosses it compiles the
                    // body, and the flag flip below moves this very loop onto
                    // the compiled code at its next iteration.
                    self.program
                        .native
                        .note_interpreted_call(FuncIdx::from_usize(func_idx as usize));
                    let now_native = self
                        .program
                        .native
                        .get(FuncIdx::from_usize(func_idx as usize))
                        .is_some();
                    let f = self.frame_mut();
                    f.ip = 0;
                    // Sound only because the frame sits at ip 0, where the
                    // two ip coordinate spaces coincide.
                    f.native = now_native;
                } else {
                    // A self-call from inside a capture-carrying closure must
                    // see the same captures.
                    self.program
                        .native
                        .note_interpreted_call(FuncIdx::from_usize(func_idx as usize));
                    let captures = self.frame().captures.clone();
                    self.frame_mut().ip = ip;
                    self.frames.push(CallFrame {
                        func_idx,
                        code_start,
                        ip: 0,
                        native: self
                            .program
                            .native
                            .get(FuncIdx::from_usize(func_idx as usize))
                            .is_some(),
                        base_slot: args_start,
                        captures,
                    });
                    base_slot = args_start;
                    for _ in arity..func_locals {
                        self.stack.push(Value::small_int(0));
                    }
                }
                ip = 0;

                // The checkpoint runs AFTER the frame work, so a yield at a
                // self-tail back-edge suspends with `frame.ip == 0` and the
                // next iteration's arguments already in the locals. Any other
                // execution backend must reproduce this frame-at-yield shape;
                // pinned by `tests/vm_fairness.rs`.
                checkpoint!();
                // Fire for tail calls too: the collapsed frame sits at ip 0,
                // exactly a fresh entry, and a native-table function must
                // never be interpreted — the trampoline enters it at resume 0.
                native_entry_check!(func_idx);
            }};
        }
        // Known top-level target: the callee is provably capture-free and is
        // not on the stack, so the pop, tag check and arity check `Op::Call`
        // pays are all skipped. The frame's `captures` is a sentinel immediate;
        // the callee body never emits `PushCapture`/`PushSelf` because a
        // top-level fn loads itself as a value via `PushGlobal`.
        macro_rules! call_known {
            ($instr:ident) => {{
                let target_idx = $instr.operand;
                let arity = i32::from($instr.b);
                let func = &self.program.functions[target_idx as usize];
                let (func_locals, func_code_start) = (func.locals, func.code_start);
                debug_assert_eq!(func.capture_count, 0);
                debug_assert_eq!(func.arity, arity);

                enter_frame!(
                    target_idx,
                    arity,
                    func_locals,
                    func_code_start,
                    Value::small_int(0),
                    $instr.op == Op::TailCallKnown
                );
                checkpoint!();
                native_entry_check!(target_idx);
            }};
        }
        macro_rules! ret {
            () => {{
                let ret_val = self.pop()?;
                let Some(old_frame) = self.frames.pop() else {
                    return Err(VmError::internal("return with no active call frame"));
                };

                self.stack.truncate(old_frame.base_slot);

                self.stack.push(ret_val);

                match self.frames.last() {
                    None => break,
                    Some(f) if f.native => {
                        // Returning into a compiled caller: its `ip` is a
                        // resume ordinal only the trampoline may dispatch.
                        self.native_reds = reds;
                        return Ok(Step::Dispatch);
                    }
                    Some(f) => {
                        ip = f.ip;
                        code_start = f.code_start;
                        base_slot = f.base_slot;
                        func_idx = f.func_idx;
                    }
                }
            }};
        }

        // One copy of fetch-and-switch, expanded at the tail of the arms with
        // a predictable successor; see the module docs. Instrumented builds
        // collapse it to nothing so the shared loop head is the only counting
        // site — semantics are unchanged, since that is already the path for
        // every opcode the copy does not decode.
        #[cfg(feature = "op-histogram")]
        macro_rules! dispatch {
            () => {{}};
        }
        #[cfg(not(feature = "op-histogram"))]
        macro_rules! dispatch {
            () => {{
                let addr = code_start + ip;
                let Some(instr) = crate::bytecode::fetch(&self.program.code, addr as usize) else {
                    break;
                };
                match instr.op {
                    // Not an opcode ([`Op::Count`]); no emitter produces it.
                    Op::Count => {
                        return Err(VmError::internal("Op::Count executed"));
                    }
                    Op::PushLocal => {
                        ip += 1;
                        push_local!(instr);
                        continue;
                    }
                    Op::AddInt => {
                        ip += 1;
                        bin_int!(as_int_typed, |a, b| a.wrapping_add(b));
                        continue;
                    }
                    Op::SubInt => {
                        ip += 1;
                        bin_int!(as_int_typed, |a, b| a.wrapping_sub(b));
                        continue;
                    }
                    Op::AddIntLC => {
                        ip += 1;
                        lc_arith!(instr, |a, b| a.wrapping_add(b));
                        continue;
                    }
                    Op::SubIntLC => {
                        ip += 1;
                        lc_arith!(instr, |a, b| a.wrapping_sub(b));
                        continue;
                    }
                    Op::JumpGeIntLC => {
                        ip += 1;
                        lc_jump!(instr, |a, b| a >= b);
                        continue;
                    }
                    Op::JumpNeIntLC => {
                        ip += 1;
                        lc_jump!(instr, |a, b| a != b);
                        continue;
                    }
                    Op::Jump => {
                        jump!(instr);
                        continue;
                    }
                    Op::StoreLocal => {
                        ip += 1;
                        store_local!(instr);
                        continue;
                    }
                    Op::Ret => {
                        ret!();
                        continue;
                    }
                    Op::PushConst => {
                        ip += 1;
                        push_const!(instr);
                        continue;
                    }
                    Op::LtInt => {
                        ip += 1;
                        bin!(as_int_typed, bool, |a, b| a < b);
                        continue;
                    }
                    Op::EqInt => {
                        ip += 1;
                        bin!(as_int_typed, bool, |a, b| a == b);
                        continue;
                    }
                    Op::JumpIfFalse => {
                        ip += 1;
                        jump_if_false!(instr);
                        continue;
                    }
                    // Perceus `Drop` sits between the loads and the call in
                    // every reuse-shaped body, so it earns a copy here.
                    Op::Drop => {
                        ip += 1;
                        let slot = base_slot + instr.operand as usize;
                        let v = &mut self.stack[slot];
                        if v.is_unique() {
                            v.hollow_for_reuse();
                        } else {
                            *v = Value::small_int(0);
                        }
                        continue;
                    }
                    _ => {}
                }
            }};
        }

        loop {
            let addr = code_start + ip;
            let Some(instr) = crate::bytecode::fetch(&self.program.code, addr as usize) else {
                break;
            };
            ip += 1;

            #[cfg(feature = "op-histogram")]
            super::op_histogram::record(instr.op, func_idx);

            match instr.op {
                // Not an opcode ([`Op::Count`]); no emitter produces it.
                Op::Count => {
                    return Err(VmError::internal("Op::Count executed"));
                }
                Op::PushConst => push_const!(instr),
                Op::PushLocal => push_local!(instr),
                Op::PushGlobal => {
                    // The global area is shared by every process on this
                    // scheduler.
                    let slot = instr.operand as usize;
                    self.stack.push(self.globals[slot].clone());
                }
                Op::StoreLocal => store_local!(instr),
                Op::PushNil => {
                    let nil = self.make_nil()?;
                    self.stack.push(nil);
                }
                Op::PushTrue => {
                    self.stack.push(Value::bool(true));
                }
                Op::PushFalse => {
                    self.stack.push(Value::bool(false));
                }
                Op::Pop => {
                    self.pop()?;
                }
                Op::Dup => {
                    let v = self.peek()?.clone();
                    self.stack.push(v);
                }
                // Untyped fallbacks; the specialized arms below handle
                // operands the checker proved concrete.
                Op::Add => self.add()?,
                Op::Sub => self.sub()?,
                Op::Mul => self.mul()?,
                Op::Div => self.div()?,
                Op::Mod => self.rem()?,
                Op::Neg => self.neg()?,

                // Emitted only when unification proved both operands concrete,
                // so the tag is a debug-only invariant (`as_*_typed`). Totality
                // matches the untyped forms: wrap on overflow, x/0=0, x%0=x,
                // non-finite float → 0.0.
                Op::AddInt => bin_int!(as_int_typed, |a, b| a.wrapping_add(b)),
                Op::SubInt => bin_int!(as_int_typed, |a, b| a.wrapping_sub(b)),
                Op::MulInt => bin_int!(as_int_typed, |a, b| a.wrapping_mul(b)),
                Op::DivInt => {
                    bin_int!(as_int_typed, |a, b| if b == 0 {
                        0
                    } else {
                        a.wrapping_div(b)
                    })
                }
                Op::ModInt => {
                    bin_int!(as_int_typed, |a, b| if b == 0 {
                        a
                    } else {
                        a.wrapping_rem(b)
                    })
                }
                Op::NegInt => un_int!(as_int_typed, |a| a.wrapping_neg()),
                Op::AddFloat => self.add_float()?,
                Op::SubFloat => self.sub_float()?,
                Op::MulFloat => self.mul_float()?,
                Op::DivFloat => self.div_float()?,
                Op::NegFloat => self.neg_float()?,
                Op::AddStr => self.str_concat2()?,

                Op::Eq => self.eq_values()?,
                Op::Neq => self.neq_values()?,
                Op::Lt => self.compare_push(|o| o.is_lt())?,
                Op::Gt => self.compare_push(|o| o.is_gt())?,
                Op::Lte => self.compare_push(|o| o.is_le())?,
                Op::Gte => self.compare_push(|o| o.is_ge())?,

                Op::LtInt => bin!(as_int_typed, bool, |a, b| a < b),
                Op::GtInt => bin!(as_int_typed, bool, |a, b| a > b),
                Op::LteInt => bin!(as_int_typed, bool, |a, b| a <= b),
                Op::GteInt => bin!(as_int_typed, bool, |a, b| a >= b),
                Op::EqInt => bin!(as_int_typed, bool, |a, b| a == b),
                Op::NeqInt => bin!(as_int_typed, bool, |a, b| a != b),
                Op::LtFloat => self.lt_float()?,
                Op::GtFloat => self.gt_float()?,
                Op::LteFloat => self.lte_float()?,
                Op::GteFloat => self.gte_float()?,

                Op::Not => {
                    let a = self.pop()?;
                    self.stack.push(Value::bool(!is_truthy(&a)?));
                }
                Op::Jump => jump!(instr),
                Op::JumpIfFalse => jump_if_false!(instr),
                // Peephole-fused (PushLocal a; PushConst b; <op>) sequences,
                // operands packed into Instruction.{a,b}.
                Op::AddIntLC => {
                    lc_arith!(instr, |a, b| a.wrapping_add(b));
                    dispatch!()
                }
                Op::SubIntLC => {
                    lc_arith!(instr, |a, b| a.wrapping_sub(b));
                    dispatch!()
                }
                Op::JumpGeIntLC => {
                    lc_jump!(instr, |a, b| a >= b);
                    dispatch!()
                }
                Op::JumpNeIntLC => {
                    lc_jump!(instr, |a, b| a != b);
                    dispatch!()
                }
                Op::Nop => {}

                Op::Call | Op::TailCall => {
                    let arity = instr.operand;
                    let callee = self.pop()?;

                    let Some(cl) = callee.as_closure() else {
                        return Err(VmError::internal("call target is not a function"));
                    };
                    let cl_func_idx = cl.func_idx();
                    let func = &self.program.functions[cl_func_idx as usize];
                    let (func_arity, func_locals, func_code_start) =
                        (func.arity, func.locals, func.code_start);

                    if arity != func_arity {
                        return Err(VmError::internal(format!(
                            "call arity mismatch: expected {func_arity}, got {arity}"
                        )));
                    }

                    // The callee value itself becomes the frame's `captures`
                    // handle: one word copied, no captures clone.
                    enter_frame!(
                        cl_func_idx,
                        arity,
                        func_locals,
                        func_code_start,
                        callee,
                        instr.op == Op::TailCall
                    );
                    checkpoint!();
                    native_entry_check!(cl_func_idx);
                }
                Op::CallSelf | Op::TailCallSelf => {
                    call_self!(instr);
                    dispatch!()
                }
                Op::CallKnown | Op::TailCallKnown => {
                    call_known!(instr);
                    dispatch!()
                }
                Op::Ret => {
                    ret!();
                    dispatch!()
                }
                Op::MakeArray => self.make_array(instr.operand)?,
                Op::MakeTuple => self.make_tuple(instr.operand)?,
                Op::TupleIndex => self.tuple_index(instr.operand)?,
                Op::MakeRange => self.make_range()?,
                Op::Index => self.seq_index()?,
                Op::IndexOr => self.seq_index_or(instr.operand)?,
                Op::ElemAt => self.elem_at(instr.operand)?,
                Op::ArrayLen => self.seq_len()?,
                Op::ArraySlice => self.seq_slice()?,
                Op::ArrayConcat => self.seq_concat()?,
                Op::Prepend => self.seq_prepend(instr.operand)?,
                Op::SeqDrop => self.seq_drop()?,
                Op::Append => self.seq_append(instr.operand)?,
                Op::GetField => self.get_field(instr.operand)?,
                Op::GetFieldUnchecked => {
                    let val = self.pop()?;
                    self.stack
                        .push(val.enum_field_typed(instr.operand as usize));
                }
                Op::MakeClosure => self.make_closure(instr.operand)?,
                Op::PushCapture => {
                    let capture_idx = instr.operand as usize;
                    let v = match self
                        .frame()
                        .captures
                        .as_closure()
                        .and_then(|cl| cl.captures().get(capture_idx).cloned())
                    {
                        Some(v) => v,
                        None => {
                            return Err(VmError::internal(format!(
                                "capture index {capture_idx} out of bounds"
                            )));
                        }
                    };
                    self.stack.push(v);
                }
                Op::PushSelf => {
                    // The frame's `captures` handle IS the closure being run.
                    let val = self.frame().captures.clone();
                    self.stack.push(val);
                }
                Op::Print => self.print_op(&mut reds)?,
                Op::StackDepth => self.stack_depth()?,
                Op::LiveSubjects => self.live_subjects()?,
                Op::BlockingThreads => self.blocking_threads()?,
                Op::Monotonic => self.monotonic()?,
                Op::WallClock => self.wall_clock()?,
                Op::RandomBytes => self.random_bytes()?,
                Op::Sha256 => self.sha256()?,
                Op::Sha512 => self.sha512()?,
                Op::HmacSha256 => self.hmac_sha256()?,
                Op::ConstEq => self.const_eq()?,
                Op::P256Verify => self.sig_verify(true)?,
                Op::Ed25519Verify => self.sig_verify(false)?,
                Op::Argv => self.argv()?,
                Op::EnvMap => self.env_map()?,
                Op::MapGet => self.map_get()?,
                Op::MapHas => self.map_has()?,
                Op::MapKeys => self.map_keys()?,
                Op::MapValues => self.map_values()?,
                Op::MapSize => self.map_size()?,
                Op::MapNew => self.map_new()?,
                Op::MapSet => self.map_set()?,
                Op::MapDelete => self.map_delete()?,
                Op::MapToList => self.map_to_list()?,
                Op::JsonParse => self.json_parse()?,
                Op::JsonKind => self.json_kind()?,
                Op::JsonLen => self.json_len()?,
                Op::JsonField => self.json_field()?,
                Op::JsonIndex => self.json_index()?,
                Op::JsonEntries => self.json_entries()?,
                Op::JsonElements => self.json_elements()?,
                Op::JsonString => self.json_string()?,
                Op::JsonInt => self.json_int()?,
                Op::JsonIntText => self.json_int_text()?,
                Op::JsonFloat => self.json_float()?,
                Op::JsonBool => self.json_bool()?,
                Op::JsonEncode => self.json_encode()?,
                Op::WireEncode => self.wire_encode(instr.operand, &mut reds)?,
                Op::WireDecode => self.wire_decode(instr.operand, &mut reds)?,
                Op::MakeEnumPayload => {
                    self.make_enum_payload(instr.operand, instr.b as usize, instr.a != 0)?
                }
                Op::MatchEnum => {
                    let tag_val = self.pop()?;
                    let val = self.pop()?;

                    let Some(tag) = tag_val.as_int() else {
                        return Err(VmError::internal("enum variant tag must be int"));
                    };

                    if let Some(ev) = val.as_enum() {
                        // Payload word 0, which is the word the native ladder
                        // compares and the word every constructor writes
                        // through `pack_variant`. Name interning differs
                        // between `MakeEnumPayload` and the prelude templates,
                        // so keeping it out of the decision is what stops the
                        // two engines resolving one arm differently.
                        self.stack.push(Value::bool(ev.variant_tag() == tag));
                    } else {
                        self.stack.push(Value::bool(false));
                    }
                }
                Op::UnwrapEnum => {
                    let enum_val = self.pop()?;
                    if let Some(ev) = enum_val.as_enum() {
                        for p in ev.payload() {
                            self.stack.push(p.clone());
                        }
                    } else {
                        return Err(VmError::internal("unwrap on non-enum value"));
                    }
                }
                Op::SwitchTag => {
                    // Computed jump by variant index. Emitted only for a
                    // resolved enum matched exhaustively, so `idx < a` holds by
                    // construction of the table.
                    let scrutinee = self.pop()?;
                    let Some(ev) = scrutinee.as_enum() else {
                        return Err(VmError::internal("SwitchTag on non-enum value"));
                    };
                    let idx = ev.variant_idx() as i32;
                    debug_assert!((idx as u32) < instr.a as u32);
                    // `operand` is the table base, frame-relative like every
                    // other target, so indexing `program.code` needs
                    // `code_start` added back.
                    ip = self.program.code[(code_start + instr.operand + idx) as usize].operand;
                }
                Op::ToString => self.op_to_string()?,
                Op::StrConcatN => self.str_concat_n(instr.operand as usize)?,
                Op::Halt => {
                    break;
                }
                // I/O, timer and process opcodes: anything that can park the
                // process or offload to the blocking pool (see `vm::io`).
                Op::FileRead => park!(self.file_read(&mut reds)),
                Op::FileWrite => park!(self.file_write(&mut reds)),
                Op::TcpListen => self.tcp_listen()?,
                Op::TcpAccept => park!(self.tcp_accept(&mut reds)),
                Op::TcpConnect => park!(self.tcp_connect(&mut reds)),
                Op::TcpConnectUntil => park!(self.tcp_connect_until(&mut reds)),
                Op::TcpRead => park!(self.tcp_read(&mut reds)),
                Op::TcpReadUntil => park!(self.tcp_read_until(&mut reds)),
                Op::TcpWrite => park!(self.tcp_write(&mut reds)),
                Op::TcpWriteParts => park!(self.tcp_write_parts(&mut reds)),
                Op::TcpClose => self.tcp_close(&mut reds)?,
                Op::TcpGive => self.tcp_give()?,
                Op::TcpCloseServer => self.tcp_close_server()?,
                Op::TcpLocalAddr => self.tcp_local_addr()?,
                Op::DnsResolve => park!(self.dns_resolve(&mut reds)),
                Op::DnsResolveUntil => park!(self.dns_resolve_until(&mut reds)),
                Op::IpParse => self.ip_parse()?,
                Op::PortSpawn => park!(self.port_spawn(&mut reds)),
                Op::PortClose => park!(self.port_close(&mut reds)),
                Op::TlsHandshake => park!(self.tls_handshake(&mut reds)),
                Op::TlsHandshakeUntil => park!(self.tls_handshake_until(&mut reds)),
                Op::TlsRead => park!(self.tls_read(&mut reds)),
                Op::TlsReadUntil => park!(self.tls_read_until(&mut reds)),
                Op::TlsWrite => park!(self.tls_write(&mut reds)),
                Op::TlsClose => self.tls_close(&mut reds)?,
                Op::ProcessSpawn => self.process_spawn(&mut reds, Link::ToParent)?,
                Op::ProcessSpawnUnlinked => self.process_spawn(&mut reds, Link::None)?,
                Op::ProcessKill => self.process_kill(&mut reds)?,
                Op::ProcessSelf => self.process_self(),
                Op::ProcessMonitor => self.process_monitor(&mut reds)?,
                Op::SupervisorNew => self.supervisor_new()?,
                Op::SupervisorWorker => self.supervisor_worker(&mut reds)?,
                Op::FactoryNew => self.factory_new(&mut reds)?,
                Op::FactoryLookupOrStart => self.factory_lookup_or_start(&mut reds)?,
                Op::FactoryLookup => self.factory_lookup()?,
                Op::SupervisedOf => self.supervised_of()?,
                Op::SupervisedParent => self.supervised_parent()?,
                Op::SupervisedChildren => self.supervised_children()?,
                Op::SupervisedCount => self.supervised_count()?,
                Op::SupervisedInfo => self.supervised_info()?,
                Op::WatchNew => self.watch_new(&mut reds)?,
                Op::WatchCancel => self.watch_cancel()?,
                Op::ProcessDemonitor => self.process_demonitor()?,
                Op::SupervisorWorkerOnEach => self.supervisor_worker_on_each(&mut reds)?,
                Op::FactorySpawn => self.factory_spawn(&mut reds)?,
                Op::Sleep => park!(self.sleep()),
                Op::SubjectNew => self.subject_new()?,
                Op::SubjectSend => self.subject_send(&mut reds, Delivery::Back)?,
                Op::SubjectSendUrgent => self.subject_send(&mut reds, Delivery::Front)?,
                Op::SubjectReceive => park!(self.subject_receive(&mut reds)),
                Op::SubjectReceiveUntil => park!(self.subject_receive_until(&mut reds)),
                // String and binary builtins (see `vm::text`).
                Op::StrSplit => self.str_split()?,
                Op::StrLen => self.str_len()?,
                Op::StrContains => self.str_contains()?,
                Op::StrTrim => self.str_trim()?,
                Op::StrToGraphemes => self.str_to_graphemes()?,
                Op::IntToString => self.int_to_string()?,
                Op::IntFromString => self.int_from_string()?,
                // Integer bitwise builtins (scarlet/int).
                Op::BitAnd => self.bit_and()?,
                Op::BitOr => self.bit_or()?,
                Op::BitXor => self.bit_xor()?,
                Op::BitNot => self.bit_not()?,
                Op::BitShl => self.bit_shl()?,
                Op::BitShr => self.bit_shr()?,
                Op::BinFromString => self.bin_from_string()?,
                Op::BinToString => self.bin_to_string()?,
                Op::BinBitSize => self.bin_bit_size()?,
                Op::BinByteSize => self.bin_byte_size()?,
                Op::BinSlice => self.bin_slice()?,
                Op::BinAppend => self.bin_append()?,
                Op::BinConcatN => self.bin_concat_n(instr.operand as usize)?,
                Op::BinFromInt => self.bin_from_int()?,
                Op::BinReadInt => self.bin_read_int()?,
                Op::BinTake => self.bin_take()?,
                Op::BinReadUtf8 => self.bin_read_utf8()?,
                Op::BinMatchPrefix => self.bin_match_prefix()?,
                Op::BinView => self.bin_view()?,
                // ASCII builtins: never-inline methods so their bodies stay out
                // of the dispatch loop and leave the hot integer arms' codegen
                // undisturbed.
                Op::BinIndexOf => self.bin_index_of()?,
                Op::BinByteAt => self.bin_byte_at()?,
                Op::BinParseInt => self.bin_parse_int()?,
                Op::BinEqIgnoreAsciiCase => self.bin_eq_ignore_ascii_case()?,
                Op::BinToAsciiLower => self.bin_to_ascii_lower()?,
                Op::BinFromIntAscii => self.bin_from_int_ascii()?,
                // HTTP/1.1 protocol ops, cold for the same reason.
                Op::HttpParseHead => self.http_parse_head()?,
                Op::HttpParseResponseHead => self.http_parse_response_head()?,
                Op::HttpFraming => self.http_framing()?,
                Op::HttpChunkDecode => self.http_chunk_decode()?,
                Op::HttpHeaderGet => self.http_header_get()?,
                Op::HttpHeaderHas => self.http_header_has()?,
                Op::HttpHeadersValid => self.http_headers_valid()?,
                Op::HttpSerializeHead => self.http_serialize_head()?,
                // Float→Int casts use saturating `as i64`; Value floats are
                // canonicalized finite (no NaN/Inf), so these stay total.
                Op::FloatFloor => self.float_floor()?,
                Op::FloatCeil => self.float_ceil()?,
                Op::FloatRound => self.float_round()?,
                Op::FloatTruncate => self.float_truncate()?,
                Op::FloatFromInt => self.float_from_int()?,
                Op::FloatToString => self.float_to_string()?,
                // Perceus drop-guided reuse (frame-limited, ICFP'22).
                Op::Drop => {
                    // Last use of this local. If the frame holds the only
                    // reference, keep the allocation in the slot for a
                    // following `Reuse` but release its children NOW: reuse
                    // only propagates down a recursive chain when the parent
                    // cons stops holding the tail before the recursive call
                    // sees it. The slot IS the per-frame reuse table.
                    let slot = base_slot + instr.operand as usize;
                    let v = &mut self.stack[slot];
                    if v.is_unique() {
                        v.hollow_for_reuse();
                    } else {
                        *v = Value::small_int(0);
                    }
                }
                Op::Reuse => {
                    // Take the candidate cell the preceding `Drop` left in the
                    // slot and push it as the address the following
                    // constructor overwrites in place. Ownership transfers via
                    // the stack so rc stays 1. A nil push means no candidate
                    // and the constructor allocates fresh.
                    let slot = base_slot + instr.operand as usize;
                    let cell = std::mem::replace(&mut self.stack[slot], Value::small_int(0));
                    if cell.is_unique() {
                        self.stack.push(cell);
                    } else {
                        self.stack.push(Value::nil());
                    }
                }
            }
        }

        // Publish the remaining budget for a native caller sitting under the
        // floor; for a finished process the next slice re-seeds it anyway.
        self.native_reds = reds;
        Ok(Step::Done)
    }

    /// Collapse the active frame for a tail call: discard the slots in
    /// `[base, args_start)` and slide the freshly-pushed argument words at
    /// `[args_start, len)` down to `base`.
    #[inline]
    pub(super) fn collapse_tail_frame(&mut self, base: usize, args_start: usize) {
        debug_assert!(base <= args_start && args_start <= self.stack.len());
        self.stack.drain(base..args_start);
    }

    /// Collapse the active frame for a *self*-tail-call: swap the `arity` new
    /// argument words at `[args_start, len)` down into the parameter slots,
    /// then truncate to `[base, base+locals)`.
    ///
    /// The non-argument locals are left in place on purpose: they are the
    /// per-frame reuse table, and `Op::Reuse` at start-of-body consumes the
    /// hollowed cells `Op::Drop` parked there last iteration. The caller skips
    /// the zero-fill for that reason.
    #[inline]
    fn collapse_tail_frame_self(
        &mut self,
        base: usize,
        args_start: usize,
        arity: usize,
        locals: usize,
    ) {
        debug_assert!(arity <= locals);
        debug_assert!(base + locals <= args_start);
        debug_assert_eq!(args_start + arity, self.stack.len());
        for i in 0..arity {
            self.stack.swap(base + i, args_start + i);
        }
        self.stack.truncate(base + locals);
    }

    /// `Op::StoreLocal` into the main process's entry frame: freeze the
    /// binding's graph into the program-wide frozen area and mirror the frozen
    /// root into the global area. The global table holds only frozen words, so
    /// it is not a GC root and `PushGlobal` is a word push on every scheduler.
    ///
    /// The caller's `base_slot == 0 && current_is_main` guard singles out the
    /// entry frame. No callee in main can land at base_slot 0 only because
    /// `__main__` opens with the stdlib's binding stores; remove that floor and
    /// a zero-arity module-scope call would publish its own locals as globals.
    ///
    /// The publish is unconditional, so a slot stored twice is published twice.
    /// The contract is publish-before-read, not publish-once.
    #[cold]
    #[inline(never)]
    fn publish_toplevel(&mut self, slot: usize) {
        if slot >= self.globals.len() {
            self.globals.resize(slot + 1, Value::nil());
        }
        // Re-storing exactly the published value (an immediate, or a frozen
        // constant read back and stored again) must not copy the graph into
        // the never-reclaimed frozen area a second time. Distinct heap values
        // still freeze per store: or-pattern and cursor rebinds at top level
        // pay one frozen copy per distinct value until publication moves to
        // spawn boundaries.
        if self.globals_published.get(slot).copied().unwrap_or(false)
            && self.globals[slot].to_bits() == self.stack[slot].to_bits()
        {
            return;
        }
        let frozen = freeze::freeze_global(&mut self.frozen, &self.stack[slot]);
        self.globals[slot] = frozen.value();
        if slot >= self.globals_published.len() {
            self.globals_published.resize(slot + 1, false);
        }
        self.globals_published[slot] = true;
        self.runtime.publish_global(slot, frozen);
    }

    /// `Op::MakeClosure`: build a closure over `func_idx` from `capture_count`
    /// capture words on top of the stack. When `with_reuse`, an `Op::Reuse`
    /// token sits on top of the captures: pop it first and — if it names a
    /// live cell — overwrite that allocation in place.
    fn make_closure(&mut self, func_idx: i32) -> VmResult<()> {
        let cc = self.program.functions[func_idx as usize].capture_count as usize;
        let base = self.operand_base(cc)?;
        let v = Value::closure_in(&mut self.heap, func_idx, &self.stack[base..]);
        self.stack.truncate(base);
        self.stack.push(v);
        Ok(())
    }

    /// `Op::MakeEnumPayload`: build a tagged enum value from the four header
    /// words (`type_id`, enum name, variant name, field-label array) plus
    /// `payload_count` payload words on top of the stack. When `with_reuse`, an
    /// `Op::Reuse` token sits above the payload and names a cell to overwrite
    /// in place.
    fn make_enum_payload(
        &mut self,
        _prehash_idx: i32,
        payload_count: usize,
        with_reuse: bool,
    ) -> VmResult<()> {
        let reuse = if with_reuse {
            self.take_reuse_addr()?
        } else {
            ReuseAddr::none()
        };
        // Names and labels are constant-pool references; only the enum cell and
        // its payload slots are fresh.
        let base = self.operand_base(payload_count + 4)?;
        let payload_base = base + 4;

        let type_id_val = &self.stack[base];
        let enum_name_val = self.stack[base + 1].clone();
        let variant_name_val = self.stack[base + 2].clone();
        let labels_val = &self.stack[base + 3];

        let Some(packed) = type_id_val.as_int() else {
            return Err(VmError::internal("enum type id must be int"));
        };
        let (type_id, variant_idx) = crate::bytecode::value::unpack_variant(packed);

        if enum_name_val.as_str().is_none() {
            return Err(VmError::internal("enum name must be string"));
        }

        // The field-label array is a per-ctor-site pooled constant, and
        // `PushConst` copies the constant word, so its address is stable for
        // the program's lifetime and is a sound memo key.
        let labels_key = match (labels_val.as_array(), labels_val.object_addr()) {
            (Some(_), Some(addr)) => addr,
            _ => return Err(VmError::internal("field labels must be an array")),
        };
        let field_labels = match self.label_cache.get(&labels_key) {
            Some(cached) => cached.clone(),
            None => {
                // `publish_frozen` is the only door from a runtime `Value` to
                // a `FrozenConst` child, and is a passthrough for these
                // already-frozen label strings.
                let labels = expect_string_array(labels_val)?
                    .iter()
                    .map(|l| ProcHeap::publish_frozen(&mut self.frozen, l))
                    .collect();
                let tuple = self.frozen.tuple(labels).into_value();
                self.label_cache.insert(labels_key, tuple.clone());
                tuple
            }
        };

        if variant_name_val.as_str().is_none() {
            return Err(VmError::internal("variant name must be string"));
        }

        // `0` marks the cell "not yet hashed"; `EnumRef::hash` computes and
        // caches on first use. Constructing is far more common than hashing,
        // and hashing here measured ~5% of a keep-alive HTTP request.
        let hash = 0u64;
        let v = Value::enum_reuse_in(
            &mut self.heap,
            reuse,
            type_id,
            variant_idx,
            hash,
            enum_name_val,
            variant_name_val,
            field_labels,
            &self.stack[payload_base..],
        );
        self.stack.truncate(base);
        self.stack.push(v);
        Ok(())
    }

    pub(super) fn pop(&mut self) -> VmResult<Value> {
        self.stack
            .pop()
            .ok_or_else(|| VmError::internal("stack underflow"))
    }

    /// Pop and decode the Perceus reuse token an `Op::Reuse` pushed above a
    /// constructor's operands. A live address means ownership transfers to the
    /// constructor, which overwrites the cell in place.
    #[inline]
    fn take_reuse_addr(&mut self) -> VmResult<ReuseAddr> {
        Ok(self.pop()?.into_reuse_addr())
    }

    /// Index of the first of the top `n` operand slots, left in place. The
    /// caller reads them via `&self.stack[base..]` and truncates to `base`
    /// afterwards, so no temporary buffer is needed.
    pub(super) fn operand_base(&self, n: usize) -> VmResult<usize> {
        let len = self.stack.len();
        if n > len {
            return Err(VmError::internal("stack underflow"));
        }
        Ok(len - n)
    }

    fn peek(&self) -> VmResult<&Value> {
        self.stack
            .last()
            .ok_or_else(|| VmError::internal("stack underflow"))
    }

    /// The operand `d` slots below the top of the stack (0 = top), or `None`
    /// if the stack is shallower than that. Reads without popping.
    pub(super) fn peek_at(&self, d: usize) -> Option<&Value> {
        self.stack.len().checked_sub(1 + d).map(|i| &self.stack[i])
    }

    /// [`peek_at`](Self::peek_at) that reports underflow as a compiler bug.
    pub(super) fn peek_at_or(&self, d: usize) -> VmResult<&Value> {
        self.peek_at(d)
            .ok_or_else(|| VmError::internal("stack underflow"))
    }

    /// Bill `reds` for reference-counting reclamation done since the last call
    /// checkpoint, one reduction per [`FREES_PER_REDUCTION`], so a giant
    /// cascading free preempts at the next call instead of stalling the
    /// scheduler.
    #[inline]
    pub(super) fn charge_reclamation(&self, reds: &mut i32) {
        if freed_objects_pending() >= FREES_PER_REDUCTION {
            let freed = take_freed_objects();
            charge_for_frees(reds, freed);
        }
    }

    // Typed pop helpers for the stdlib and I/O ops. The popped `Value` is
    // returned whole so callers can borrow the arena contents through it; that
    // borrow survives a following `*_in` constructor because allocation never
    // moves existing objects.

    #[inline]
    pub(super) fn pop_str(&mut self, op: &'static str) -> VmResult<Value> {
        let v = self.pop()?;
        if v.as_str().is_some() {
            Ok(v)
        } else {
            Err(VmError::type_mismatch(op, "String", &v))
        }
    }

    #[inline]
    pub(super) fn pop_int(&mut self, op: &'static str) -> VmResult<i64> {
        let v = self.pop()?;
        match v.as_int() {
            Some(n) => Ok(n),
            None => Err(VmError::type_mismatch(op, "Int", &v)),
        }
    }

    /// Pop a numeric value as `f64`. Coerces Int, like `Op::Neg` and friends.
    #[inline]
    pub(super) fn pop_float(&mut self, op: &'static str) -> VmResult<f64> {
        let v = self.pop()?;
        if let Some(f) = v.as_float() {
            Ok(f)
        } else if let Some(n) = v.as_int() {
            Ok(n as f64)
        } else {
            Err(VmError::type_mismatch(op, "Float", &v))
        }
    }

    #[inline]
    pub(super) fn pop_binary(&mut self, op: &'static str) -> VmResult<Value> {
        let v = self.pop()?;
        if v.as_binary().is_some() {
            Ok(v)
        } else {
            Err(VmError::type_mismatch(op, "Binary", &v))
        }
    }

    // `make_nil`/`make_none` copy prebuilt frozen-area values and allocate
    // nothing. The payload-carrying constructors build one fresh wrapper enum.
    // All are fallible: the slot may be unbound (front-end bug, `Internal`).

    #[inline]
    pub(super) fn make_nil(&self) -> VmResult<Value> {
        self.templates.nullary(AbiSlot::Unit)
    }

    #[inline]
    pub(super) fn make_none(&self) -> VmResult<Value> {
        self.templates.nullary(AbiSlot::OptionNone)
    }

    #[inline]
    pub(super) fn make_some(&mut self, v: Value) -> VmResult<Value> {
        self.abi_make(AbiSlot::OptionSome, &[v])
    }

    #[inline]
    pub(super) fn make_ok(&mut self, v: Value) -> VmResult<Value> {
        self.abi_make(AbiSlot::ResultOk, &[v])
    }

    #[inline]
    pub(super) fn make_err(&mut self, v: Value) -> VmResult<Value> {
        self.abi_make(AbiSlot::ResultErr, &[v])
    }

    #[inline]
    pub(super) fn make_err_nil(&mut self) -> VmResult<Value> {
        let nil = self.make_nil()?;
        self.make_err(nil)
    }

    /// Instantiate the constructor bound to `slot` around `payload`. The
    /// template clone is a few words, as the old per-field clone was.
    #[inline]
    pub(super) fn abi_make(&mut self, slot: AbiSlot, payload: &[Value]) -> VmResult<Value> {
        let t = self.templates.get(slot)?.clone();
        Ok(t.instantiate(&mut self.heap, payload))
    }

    /// The pre-built value of a nullary slot.
    #[inline]
    pub(super) fn abi_nullary(&self, slot: AbiSlot) -> VmResult<Value> {
        self.templates.nullary(slot)
    }

    /// Concatenate the two strings on top of the stack.
    #[inline]
    /// `Op::Print` — write the operand's image to stdout. A write syscall
    /// blocks on a full pipe, so it is charged like the other I/O ops.
    pub(super) fn print_op(&mut self, reds: &mut i32) -> VmResult<()> {
        let val = self.pop()?;
        println!("{}", inspect(&val, &self.program));
        *reds -= IO_REDUCTION_COST;
        Ok(())
    }

    /// `Op::StackDepth` — the current call depth.
    pub(super) fn stack_depth(&mut self) -> VmResult<()> {
        self.stack.push(Value::small_int(self.frames.len() as i64));
        Ok(())
    }

    /// `Op::LiveSubjects` — see the opcode's docs.
    pub(super) fn live_subjects(&mut self) -> VmResult<()> {
        let n = self
            .runtime
            .live_subjects
            .load(std::sync::atomic::Ordering::Relaxed);
        self.stack.push(Value::small_int(n as i64));
        Ok(())
    }

    /// `Op::BlockingThreads` — see the opcode's docs.
    pub(super) fn blocking_threads(&mut self) -> VmResult<()> {
        let n = self.runtime.blocking_threads();
        self.stack.push(Value::small_int(n as i64));
        Ok(())
    }

    /// `Op::Monotonic` — the monotonic clock in milliseconds. A plain read:
    /// it never parks, so compiled code reaches it through the same bridge as
    /// the other pure ops.
    pub(super) fn monotonic(&mut self) -> VmResult<()> {
        self.stack.push(Value::small_int(monotonic_now_ms()));
        Ok(())
    }

    /// `Op::WallClock` — the wall clock in milliseconds since the Unix epoch.
    /// A plain read like [`VM::monotonic`], through the same bridge.
    ///
    /// `boxed_int` and not `Value::small_int`: this reading is whatever the
    /// host clock says rather than a length or a count, so nothing here knows
    /// it fits the 48-bit immediate. A clock set past the year 6429 boxes
    /// instead of tripping `small_int`'s debug assert.
    pub(super) fn wall_clock(&mut self) -> VmResult<()> {
        // `duration_since` reports a pre-1970 clock as an `Err` carrying the
        // magnitude, so the sign is put back rather than clamped: a clock set
        // to 1969 must read as negative and not as the epoch.
        let ms = match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(d) => i64::try_from(d.as_millis()).unwrap_or(i64::MAX),
            Err(before) => i64::try_from(before.duration().as_millis())
                .map(|ms| -ms)
                .unwrap_or(i64::MIN),
        };
        let v = self.boxed_int(ms);
        self.stack.push(v);
        Ok(())
    }

    /// `Op::RandomBytes` — `n` bytes from the OS CSPRNG. A host read that
    /// never parks: compiled code shares this body through the bridge.
    ///
    /// `Err(Nil)` covers a negative `n` and a failed `getrandom`. The second
    /// arm is the one that must not invent bytes — returning the zeroed
    /// `buf` on failure would be a working CSPRNG that is not.
    pub(super) fn random_bytes(&mut self) -> VmResult<()> {
        let n = self.pop_int("crypto.random_bytes")?;
        let v = if let Ok(n) = usize::try_from(n) {
            let mut buf = vec![0u8; n];
            match getrandom::fill(&mut buf) {
                Ok(()) => {
                    let bin = Value::binary_in(&mut self.heap, buf);
                    self.make_ok(bin)?
                }
                Err(_) => self.make_err_nil()?,
            }
        } else {
            self.make_err_nil()?
        };
        self.stack.push(v);
        Ok(())
    }

    /// `Op::Sha256` — the digest of the popped binary. Total: 32 bytes out
    /// for any input, via aws-lc (the crypto rustls already links).
    fn sha256(&mut self) -> VmResult<()> {
        let v = self.pop_binary("crypto.sha256")?;
        let digest =
            aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, &super::bin_ref(&v).full_bytes());
        let out = Value::binary_in(&mut self.heap, digest.as_ref().to_vec());
        self.stack.push(out);
        Ok(())
    }

    /// `Op::Sha512` — as `sha256`, 64 bytes out.
    fn sha512(&mut self) -> VmResult<()> {
        let v = self.pop_binary("crypto.sha512")?;
        let digest =
            aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA512, &super::bin_ref(&v).full_bytes());
        let out = Value::binary_in(&mut self.heap, digest.as_ref().to_vec());
        self.stack.push(out);
        Ok(())
    }

    /// `Op::HmacSha256` — the tag over `msg` under `key`. Argument order on
    /// the stack is push order, so `msg` pops first.
    fn hmac_sha256(&mut self) -> VmResult<()> {
        let msg_v = self.pop_binary("crypto.hmac_sha256")?;
        let key_v = self.pop_binary("crypto.hmac_sha256")?;
        let tag = {
            let key = aws_lc_rs::hmac::Key::new(
                aws_lc_rs::hmac::HMAC_SHA256,
                &super::bin_ref(&key_v).full_bytes(),
            );
            aws_lc_rs::hmac::sign(&key, &super::bin_ref(&msg_v).full_bytes())
        };
        let out = Value::binary_in(&mut self.heap, tag.as_ref().to_vec());
        self.stack.push(out);
        Ok(())
    }

    /// `Op::ConstEq` — equality whose running time is a function of the
    /// lengths alone. Unequal lengths answer `false` immediately; a length
    /// is not the secret, its bytes are.
    fn const_eq(&mut self) -> VmResult<()> {
        let b_v = self.pop_binary("crypto.const_eq")?;
        let a_v = self.pop_binary("crypto.const_eq")?;
        let equal = aws_lc_rs::constant_time::verify_slices_are_equal(
            &super::bin_ref(&a_v).full_bytes(),
            &super::bin_ref(&b_v).full_bytes(),
        )
        .is_ok();
        self.stack.push(Value::bool(equal));
        Ok(())
    }

    /// `Op::P256Verify` / `Op::Ed25519Verify` — pop sig, message, key and
    /// push whether the signature is valid under aws-lc. Every parse failure
    /// (bad key length, non-DER sig) is a `False`, so the op is total: a
    /// caller distinguishes "invalid" from "malformed" only if it needs to,
    /// by validating shape first.
    fn sig_verify(&mut self, p256: bool) -> VmResult<()> {
        let sig_v = self.pop_binary("crypto.verify")?;
        let msg_v = self.pop_binary("crypto.verify")?;
        let key_v = self.pop_binary("crypto.verify")?;
        let ok = {
            let alg: &dyn aws_lc_rs::signature::VerificationAlgorithm = if p256 {
                &aws_lc_rs::signature::ECDSA_P256_SHA256_ASN1
            } else {
                &aws_lc_rs::signature::ED25519
            };
            let key = aws_lc_rs::signature::UnparsedPublicKey::new(
                alg,
                super::bin_ref(&key_v).full_bytes().into_owned(),
            );
            key.verify(
                &super::bin_ref(&msg_v).full_bytes(),
                &super::bin_ref(&sig_v).full_bytes(),
            )
            .is_ok()
        };
        self.stack.push(Value::bool(ok));
        Ok(())
    }

    /// `Op::ToString`: the operand's string image (a Str is its own image).
    pub(super) fn op_to_string(&mut self) -> VmResult<()> {
        let val = self.pop()?;
        if val.as_str().is_some() {
            self.stack.push(val);
        } else {
            let s = inspect(&val, &self.program);
            let v = Value::str_in(&mut self.heap, &s);
            self.stack.push(v);
        }
        Ok(())
    }

    /// `Op::StrConcatN`: concatenate `n` Str operands on the stack.
    pub(super) fn str_concat_n(&mut self, n: usize) -> VmResult<()> {
        let base = self.operand_base(n)?;
        let v = {
            let mut parts: SmallVec<[&str; 8]> = SmallVec::with_capacity(n);
            for v in &self.stack[base..] {
                match v.as_str() {
                    Some(s) => parts.push(s),
                    None => return Err(VmError::internal("str_concat requires strings")),
                }
            }
            Value::str_from_parts_in(&mut self.heap, &parts)
        };
        self.stack.truncate(base);
        self.stack.push(v);
        Ok(())
    }

    pub(super) fn str_concat2(&mut self) -> VmResult<()> {
        let b = self.pop()?;
        let a = self.pop()?;
        match (a.as_str(), b.as_str()) {
            (Some(sa), Some(sb)) => {
                let v = Value::str_from_parts_in(&mut self.heap, &[sa, sb]);
                self.stack.push(v);
                Ok(())
            }
            _ => Err(VmError::internal("string concat on non-Str operands")),
        }
    }

    /// `+` — the one arithmetic op with a non-numeric case, Str + Str.
    /// `Op::Neg` — the untyped negation fallback.
    pub(super) fn neg(&mut self) -> VmResult<()> {
        let a = self.pop()?;
        if let Some(i) = a.as_int() {
            self.push_int(i.wrapping_neg());
        } else if let Some(f) = a.as_float() {
            self.stack.push(Value::float(-f));
        } else {
            return Err(VmError::type_mismatch("negate", "Int or Float", &a));
        }
        Ok(())
    }

    /// `Op::Eq` — structural equality over any two values.
    pub(super) fn eq_values(&mut self) -> VmResult<()> {
        let b = self.pop()?;
        let a = self.pop()?;
        self.stack.push(Value::bool(values_equal(&a, &b)));
        Ok(())
    }

    /// `Op::Neq` — the negation of [`Self::eq_values`].
    pub(super) fn neq_values(&mut self) -> VmResult<()> {
        let b = self.pop()?;
        let a = self.pop()?;
        self.stack.push(Value::bool(!values_equal(&a, &b)));
        Ok(())
    }

    pub(super) fn add(&mut self) -> VmResult<()> {
        let both_str = self.peek_at(1).is_some_and(|v| v.as_str().is_some())
            && self.peek_at(0).is_some_and(|v| v.as_str().is_some());
        if both_str {
            return self.str_concat2();
        }
        let b = self.pop()?;
        let a = self.pop()?;
        self.arith(a, b, |x, y| x.wrapping_add(y), |x, y| x + y)
    }

    pub(super) fn sub(&mut self) -> VmResult<()> {
        let b = self.pop()?;
        let a = self.pop()?;
        self.arith(a, b, |x, y| x.wrapping_sub(y), |x, y| x - y)
    }

    pub(super) fn mul(&mut self) -> VmResult<()> {
        let b = self.pop()?;
        let a = self.pop()?;
        self.arith(a, b, |x, y| x.wrapping_mul(y), |x, y| x * y)
    }

    /// `/` is TOTAL: `x / 0 = 0`, `x / 0.0 = 0.0`. The zero guard is
    /// load-bearing — `wrapping_div` still panics on a zero divisor.
    pub(super) fn div(&mut self) -> VmResult<()> {
        let b = self.pop()?;
        let a = self.pop()?;
        self.arith(
            a,
            b,
            |x, y| if y == 0 { 0 } else { x.wrapping_div(y) },
            |x, y| if y == 0.0 { 0.0 } else { x / y },
        )
    }

    /// `%` is TOTAL: `x % 0 = x`, preserving `a = (a/b)*b + a%b`.
    pub(super) fn rem(&mut self) -> VmResult<()> {
        let b = self.pop()?;
        let a = self.pop()?;
        self.arith(
            a,
            b,
            |x, y| if y == 0 { x } else { x.wrapping_rem(y) },
            |x, y| if y == 0.0 { x } else { x % y },
        )
    }

    pub(super) fn bit_and(&mut self) -> VmResult<()> {
        self.int_binop("bitwise_and", |x, y| x & y)
    }

    pub(super) fn bit_or(&mut self) -> VmResult<()> {
        self.int_binop("bitwise_or", |x, y| x | y)
    }

    pub(super) fn bit_xor(&mut self) -> VmResult<()> {
        self.int_binop("bitwise_xor", |x, y| x ^ y)
    }

    /// `bitwise_not` is the two's-complement complement, so it flips the sign
    /// bit with every other: `not 0 == -1` and `not n == -n - 1`. `Int` has no
    /// unsigned twin to land in, which is why the result of complementing a
    /// small non-negative number is negative rather than large.
    pub(super) fn bit_not(&mut self) -> VmResult<()> {
        let a = self.pop()?;
        let ValueView::Int(ai) = a.kind() else {
            return Err(VmError::internal(format!(
                "bitwise_not on '{}'",
                value_type_name(&a)
            )));
        };
        let v = self.boxed_int(!ai);
        self.stack.push(v);
        Ok(())
    }

    pub(super) fn bit_shl(&mut self) -> VmResult<()> {
        self.int_binop("bitwise_shift_left", shift_left_i64)
    }

    pub(super) fn bit_shr(&mut self) -> VmResult<()> {
        self.int_binop("bitwise_shift_right", shift_right_i64)
    }

    /// Integer core for the bitwise ops, mirroring [`Self::arith`]. There is no
    /// float arm: HM types all six as `Int -> … -> Int`, so a non-`Int` operand
    /// is a compiler bug rather than a user error.
    fn int_binop(&mut self, what: &str, f: fn(i64, i64) -> i64) -> VmResult<()> {
        let b = self.pop()?;
        let a = self.pop()?;
        let (ValueView::Int(ai), ValueView::Int(bi)) = (a.kind(), b.kind()) else {
            return Err(VmError::internal(format!(
                "{what} on '{}' and '{}'",
                value_type_name(&a),
                value_type_name(&b)
            )));
        };
        let v = self.boxed_int(f(ai, bi));
        self.stack.push(v);
        Ok(())
    }

    /// Numeric core for the untyped `+ - * / %` fallbacks. A mixed pair is a
    /// compiler bug: HM unifies both operands to one numeric type. Arithmetic
    /// is TOTAL — int overflow wraps, and `Value::float` collapses any
    /// non-finite result to `0.0`.
    fn arith(
        &mut self,
        a: Value,
        b: Value,
        int_f: fn(i64, i64) -> i64,
        float_f: fn(f64, f64) -> f64,
    ) -> VmResult<()> {
        let v = match (a.kind(), b.kind()) {
            (ValueView::Int(ai), ValueView::Int(bi)) => self.boxed_int(int_f(ai, bi)),
            (ValueView::Float(af), ValueView::Float(bf)) => Value::float(float_f(af, bf)),
            _ => {
                return Err(VmError::internal(format!(
                    "arithmetic on '{}' and '{}'",
                    value_type_name(&a),
                    value_type_name(&b)
                )));
            }
        };
        self.stack.push(v);
        Ok(())
    }

    /// Pop two operands and push whether their ordering satisfies `keep`.
    pub(super) fn compare_push(&mut self, keep: fn(std::cmp::Ordering) -> bool) -> VmResult<()> {
        let b = self.pop()?;
        let a = self.pop()?;
        let ord = compare_values(a, b)?;
        self.stack.push(Value::bool(keep(ord)));
        Ok(())
    }

    /// Box an i64 into a `Value`.
    #[inline]
    pub(super) fn boxed_int(&mut self, i: i64) -> Value {
        if Value::fits_small_int(i) {
            Value::small_int(i)
        } else {
            self.spill_int(i)
        }
    }

    /// The out-of-range half of [`boxed_int`](Self::boxed_int). Kept out of
    /// line so the allocation never inlines into the integer arithmetic arms,
    /// where it would cost registers and i-cache on every op.
    #[cold]
    #[inline(never)]
    fn spill_int(&mut self, i: i64) -> Value {
        Value::int_in(&mut self.heap, i)
    }

    /// Push the program's command-line arguments as an `Array(String)`.
    #[inline(never)]
    pub(super) fn argv(&mut self) -> VmResult<()> {
        let runtime = self.runtime.clone();
        let args = &runtime.argv;
        let mut items: Vec<Value> = Vec::with_capacity(args.len());
        for arg in args {
            items.push(Value::str_in(&mut self.heap, arg));
        }
        let v = Value::array_in(&mut self.heap, &items);
        self.stack.push(v);
        Ok(())
    }

    /// Push an integer result (see `boxed_int`).
    #[inline]
    pub(super) fn push_int(&mut self, i: i64) {
        let v = self.boxed_int(i);
        self.stack.push(v);
    }
}

fn expect_string_array(v: &Value) -> VmResult<Vec<Value>> {
    match v.as_array() {
        Some(items) => {
            let mut out: Vec<Value> = Vec::with_capacity(items.len());
            for it in items.iter() {
                if it.as_str().is_none() {
                    return Err(VmError::internal("field label must be string"));
                }
                out.push(it);
            }
            Ok(out)
        }
        None => Err(VmError::internal("field labels must be an array")),
    }
}

/// Total ordering of two same-typed numeric operands. Scarlet floats are canonical
/// finite, so `partial_cmp` always succeeds; the `Equal` fallback avoids an
/// `unwrap`.
fn compare_values(a: Value, b: Value) -> VmResult<std::cmp::Ordering> {
    match (a.kind(), b.kind()) {
        (ValueView::Int(ai), ValueView::Int(bi)) => Ok(ai.cmp(&bi)),
        (ValueView::Float(af), ValueView::Float(bf)) => {
            Ok(af.partial_cmp(&bf).unwrap_or(std::cmp::Ordering::Equal))
        }
        _ => Err(VmError::internal(format!(
            "compare '{}' with '{}'",
            value_type_name(&a),
            value_type_name(&b)
        ))),
    }
}

fn is_truthy(v: &Value) -> VmResult<bool> {
    v.as_bool()
        .ok_or_else(|| VmError::type_mismatch("condition", "Bool", v))
}

/// Shift left by `n`, TOTAL for every `i64` count.
///
/// Two rules, both chosen so the shift agrees with the arithmetic it stands
/// for rather than with the hardware instruction underneath it:
///
/// - **A count at or past the 64-bit width shifts every bit out**, giving 0.
///   It does *not* mask the count to 6 bits the way C and the x86/ARM shift
///   instructions do, so `1 shift_left 64` is 0, not 1. Masking is the single
///   most-hit bitwise footgun in C, and it is undetectable at the call site.
/// - **A negative count shifts the other way** by the same magnitude, so
///   `shift_left(x, -n) == shift_right(x, n)` for every `n` in
///   `i64::MIN + 1..=i64::MAX`. This keeps the pair total over the whole of
///   `i64` without a `Result` the caller would have to thread through a
///   bitfield expression. `saturating_neg` is load-bearing: plain negation of
///   `i64::MIN` overflows, and saturating to `i64::MAX` lands in the "past the
///   width" case, which is the right answer anyway.
///
///   `n == i64::MIN` is excluded because `-n` is not expressible there, not
///   because either side is wrong. Both saturate to `i64::MAX` and land past
///   the width in opposite directions: `shift_left` shifts right and keeps the
///   sign, `shift_right` shifts left and drops it, so for a negative `x` they
///   read -1 against 0. `negative_count_law_holds_except_at_the_most_negative_count`
///   pins the domain and the one exclusion.
///
/// The shift itself goes through `u64` so that shifting into or through the
/// sign bit is defined rather than an overflowing `i64 << n`.
pub(crate) fn shift_left_i64(x: i64, n: i64) -> i64 {
    if n < 0 {
        shift_right_nonneg(x, n.saturating_neg())
    } else if n >= 64 {
        0
    } else {
        ((x as u64) << n) as i64
    }
}

/// Shift right by `n`, TOTAL for every `i64` count, and **arithmetic**: the
/// sign bit propagates, so `-8 shift_right 1 == -4` and a negative value never
/// becomes positive. `Int` is signed with no unsigned counterpart, so a logical
/// shift would have no type to produce its result in.
///
/// A caller wanting logical behaviour shifts first and clears the copied sign
/// bits after — `shift_right(x, n) & shift_right(i64::MAX, n - 1)`, for any
/// `n >= 1`. Masking the INPUT does not work at any width: no positive `i64`
/// preserves bit 63, since the mask that would is `2^64 - 1` — `-1`, which
/// masks nothing. `logical_shift_right_idiom_matches_a_u64_shift` pins it.
///
/// The count rules match [`shift_left_i64`]: at or past the width every bit
/// shifts out, leaving the sign — 0 for a non-negative `x` and -1 for a
/// negative one — and a negative count shifts left instead.
pub(crate) fn shift_right_i64(x: i64, n: i64) -> i64 {
    if n < 0 {
        shift_left_nonneg(x, n.saturating_neg())
    } else {
        shift_right_nonneg(x, n)
    }
}

/// `shift_left_i64`'s positive-count half, split out so the two entry points
/// can delegate to each other without recursing.
fn shift_left_nonneg(x: i64, n: i64) -> i64 {
    debug_assert!(n >= 0);
    if n >= 64 { 0 } else { ((x as u64) << n) as i64 }
}

/// `shift_right_i64`'s positive-count half. `x >> 63` is the sign broadcast
/// across every bit, which is exactly what shifting all 64 bits out leaves.
fn shift_right_nonneg(x: i64, n: i64) -> i64 {
    debug_assert!(n >= 0);
    if n >= 64 { x >> 63 } else { x >> n }
}

#[cfg(test)]
mod bitwise_tests {
    use super::{shift_left_i64, shift_right_i64};

    #[test]
    fn shift_by_zero_is_identity() {
        for x in [0i64, 1, -1, i64::MAX, i64::MIN, 0x5555_5555_5555_5555] {
            assert_eq!(shift_left_i64(x, 0), x, "shl 0 on {x}");
            assert_eq!(shift_right_i64(x, 0), x, "shr 0 on {x}");
        }
    }

    /// The C/x86 masking trap: a masked count would make this 1.
    #[test]
    fn shift_past_the_width_does_not_mask_the_count() {
        assert_eq!(shift_left_i64(1, 64), 0);
        assert_eq!(shift_left_i64(1, 65), 0);
        assert_eq!(shift_left_i64(1, i64::MAX), 0);
        assert_eq!(shift_right_i64(i64::MAX, 64), 0);
        assert_eq!(shift_right_i64(1, i64::MAX), 0);
    }

    /// Arithmetic, not logical: the sign survives, and a full-width right
    /// shift of a negative leaves -1 rather than 0.
    #[test]
    fn right_shift_is_arithmetic() {
        assert_eq!(shift_right_i64(-8, 1), -4);
        assert_eq!(shift_right_i64(-1, 63), -1);
        assert_eq!(shift_right_i64(-1, 64), -1);
        assert_eq!(shift_right_i64(i64::MIN, 64), -1);
        assert_eq!(shift_right_i64(i64::MIN, 63), -1);
        assert_eq!(shift_right_i64(8, 1), 4);
    }

    #[test]
    fn shifting_into_and_through_the_sign_bit_is_defined() {
        assert_eq!(shift_left_i64(1, 63), i64::MIN);
        assert_eq!(shift_left_i64(1, 62), 4611686018427387904);
        assert_eq!(shift_left_i64(-1, 63), i64::MIN);
        assert_eq!(shift_left_i64(i64::MAX, 1), -2);
    }

    #[test]
    fn negative_count_shifts_the_other_way() {
        assert_eq!(shift_left_i64(8, -1), shift_right_i64(8, 1));
        assert_eq!(shift_right_i64(8, -1), shift_left_i64(8, 1));
        assert_eq!(shift_left_i64(-8, -1), -4);
    }

    /// The logical-shift idiom both shift docs hand to callers, against a real
    /// `u64` shift. It has to hold at `i64::MIN`, which is the case that killed
    /// the mask-the-input recipe these docs used to give: no positive mask
    /// preserves bit 63.
    #[test]
    fn logical_shift_right_idiom_matches_a_u64_shift() {
        let values = [
            i64::MIN,
            i64::MAX,
            -1,
            0,
            1,
            -8,
            255,
            0x5555_5555_5555_5555,
            0xAAAA_AAAA_AAAA_AAAA_u64 as i64,
            i64::MIN + 1,
        ];
        // n == 0 is excluded on purpose: the mask is -2 there and clears bit 0,
        // which is exactly why both docs state the domain as `n >= 1`.
        let counts = [1i64, 2, 3, 31, 32, 63, 64, 65, 200, i64::MAX];
        for x in values {
            for n in counts {
                let idiom = shift_right_i64(x, n) & shift_right_i64(i64::MAX, n - 1);
                let logical = if n >= 64 { 0 } else { ((x as u64) >> n) as i64 };
                assert_eq!(idiom, logical, "logical shift right of {x} by {n}");
            }
        }
        // The two figures the docs quote.
        assert_eq!(
            shift_right_i64(i64::MIN, 3) & shift_right_i64(i64::MAX, 2),
            1152921504606846976
        );
        assert_eq!(
            shift_right_i64(-1, 32) & shift_right_i64(i64::MAX, 31),
            4294967295
        );
    }

    /// The negative-count law holds for every count but `i64::MIN`, where
    /// `-n` is not expressible and the two sides land in opposite cases.
    #[test]
    fn negative_count_law_holds_except_at_the_most_negative_count() {
        let values = [i64::MIN, -8, -1, 0, 1, 255, i64::MAX];
        for x in values {
            for n in [1i64, 2, 63, 64, 200, i64::MAX, -1, -64, i64::MIN + 1] {
                assert_eq!(
                    shift_left_i64(x, n.wrapping_neg()),
                    shift_right_i64(x, n),
                    "law at x={x} n={n}"
                );
            }
        }
        // The one excluded count: `0 - i64::MIN` wraps back to `i64::MIN`, so
        // the left side shifts right and keeps the sign while the right side
        // shifts left and drops it.
        assert_eq!(shift_left_i64(-1, i64::MIN.wrapping_neg()), -1);
        assert_eq!(shift_right_i64(-1, i64::MIN), 0);
    }

    /// `i64::MIN` has no positive negation; saturating lands past the width,
    /// which is the same answer an exact `|i64::MIN|` would give.
    #[test]
    fn most_negative_count_does_not_overflow() {
        assert_eq!(shift_left_i64(-1, i64::MIN), -1);
        assert_eq!(shift_left_i64(1, i64::MIN), 0);
        assert_eq!(shift_right_i64(1, i64::MIN), 0);
        // Delegates *left* by |MIN| saturated to i64::MAX — past the width,
        // so every bit shifts out and nothing of the sign survives.
        assert_eq!(shift_right_i64(-1, i64::MIN), 0);
    }
}

#[cfg(test)]
mod charge_tests {
    use super::{FREES_PER_REDUCTION, charge_for_frees};

    /// **T-780.** `charge_reclamation`'s own wrap window needs ~3 billion real
    /// frees to reach through `FREED_OBJECTS`, which has no bulk setter — so
    /// this drives the extracted arithmetic directly instead, the same way
    /// `wire::charge_wire`'s
    /// `a_charge_past_i32_max_does_not_add_reductions` does for the other
    /// charge site T-779 fixed.
    ///
    /// `freed / FREES_PER_REDUCTION` is `u64`; casting it to `i32` with `as`
    /// truncates rather than saturates. Past `i32::MAX` the low 32 bits
    /// reinterpret as negative, and `saturating_sub` of a negative number is
    /// `saturating_add` — the charge would hand reductions back instead of
    /// spending them. `freed` here puts the divided work at 3_000_000_000,
    /// strictly between `i32::MAX` and `u32::MAX`.
    #[test]
    fn a_charge_past_i32_max_does_not_add_reductions() {
        let mut reds = 1_000_000i32;
        charge_for_frees(&mut reds, 3_000_000_000u64 * FREES_PER_REDUCTION);
        assert!(reds <= 1_000_000, "charging must never increase reds");
    }
}
