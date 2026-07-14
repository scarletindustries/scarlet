//! The bytecode interpreter: one process's execution slice, instruction
//! by instruction.
//!
//! [`VM::execute_slice`] is the hot loop. It hoists the active frame's
//! scalar state (`ip`, `code_start`, `base_slot`, `func_idx`) into
//! locals, dispatches on each fetched opcode, and runs until the
//! process finishes (`Step::Done`), spends its reduction budget at a
//! call checkpoint (`Step::Yield`), or hits an op that cannot complete
//! now (`Step::Parked`). Calls and returns are the only places the
//! hoisted state is synced back to the frame — which is exactly what
//! makes preemption at a call a plain `return`.
//!
//! The dispatch match keeps only the register-shaped arms inline:
//! stack traffic, arithmetic and comparison (plus their
//! type-specialized and superinstruction forms), branches, calls,
//! closures, and enum construction/matching. Everything with a fatter
//! body routes to an out-of-line family handler on `self`:
//!
//! - [`super::collections`] — arrays, tuples, ranges, field access
//! - [`super::text`] — string/binary builtins, HTTP scanners
//! - [`super::io`] — files, sockets, DNS, sleep, spawn (the parking ops)
//!
//! Dispatch is *replicated*, not shared. A single `loop { fetch; match }`
//! compiles to one indirect branch for the whole instruction stream, and a
//! branch predictor with one entry for every opcode transition in the program
//! mispredicts on nearly every instruction. So selected arms end in their own
//! copy of fetch-and-switch (the `dispatch!` macro) — the direct-threaded
//! structure, written by hand rather than left to an optimizer's
//! tail-duplication heuristic.
//!
//! Two independent choices govern it, and they are not the same set.
//!
//! *Which arms carry a copy* (the `dispatch!()` tails below): only those whose
//! dynamic successor is near-deterministic. A private fetch-and-switch buys a
//! private branch-target entry, which is worth its I-cache footprint exactly
//! when that entry predicts. `Ret` is always followed by `StoreLocal`;
//! `TailCallSelf` by the callee's entry test; the `*IntLC` superinstructions
//! by one of two things. Those predict. `PushLocal` and `StoreLocal`, by
//! contrast, are followed by a wide spread of opcodes (measured top successor
//! ~1/3 and ~1/2 of executions) — a private entry there mispredicts anyway and
//! only costs code. Replicating the four *most executed* arms measured
//! *slower* on both benchmarks than replicating these seven.
//!
//! *Which opcodes the copy decodes* (the arms of `dispatch!` itself): the
//! small register-shaped ones. Fat bodies — calls, the collection and text
//! families — are not arms of it: they leave `ip` on their instruction and
//! fall out to the shared loop head, which re-fetches and runs the full match.
//! The wasted fetch is free relative to the work those bodies are about to do,
//! and inlining them into every copy bloats the loop's hot code enough to cost
//! more than the extra branch sites recover.
//!
//! Allocation is reference-counted, so an arm simply pops its operands and
//! builds its result: nothing moves, so there is no rooting rule and no
//! pre-allocation reservation. Popped operands are owned `Value`s and drop
//! (decrement) at the end of the arm. The typed pop helpers and the
//! `make_*`/`stdlib_*` constructors at the bottom of this file are the
//! shared vocabulary those arms (and the sibling handlers) build on.
//!
//! Value semantics live here too ([`values_equal`], `compare`,
//! `is_truthy`): structural equality with the Range/Array congruence
//! (a lazy range equals the array of its elements) and the hash
//! fast-reject on enums.

use al_core::bytecode::value::ReuseAddr;
use al_core::bytecode::{
    Op, Value, ValueView, freed_objects_pending, take_freed_objects, values_equal,
};
use al_core::core_ir::FuncIdx;
use al_core::heap::ProcHeap;
use al_core::static_ir::VariantTemplate;
use al_core::tivec::Idx;
use smallvec::SmallVec;

use super::poll::monotonic_now_ms;
use super::{
    CallFrame, EnumTemplate, IO_REDUCTION_COST, REDUCTION_BUDGET, Step, VM, VmError, VmResult,
    enum_template, f64_str, freeze, inspect, value_type_name,
};

/// Objects freed per reduction charged by [`VM::charge_reclamation`]. Tuned so
/// only big cascading drops (thousands of objects) cost the process budget;
/// ordinary per-op churn is far below it and never preempts.
const FREES_PER_REDUCTION: u64 = 256;

impl VM {
    /// One scheduling slice of the current process, dispatched on the top
    /// frame's resume point. Resume-after-suspension re-enters the
    /// *interpreter* at `frame.ip` — bytecode is kept for every function
    /// precisely as its fallback and resume path — except when `ip == 0`
    /// ("enter from the top"), the one resume point at which a compiled
    /// body may be (re-)entered: every suspension point inside a native
    /// function leaves `ip == 0` (the entry checkpoint of `al_rt_call` and
    /// the self-tail back-edge of `al_rt_checkpoint`, whose frame-at-yield
    /// shape is the `TailCallSelf` contract), while a native *caller*
    /// suspended under a callee holds a nonzero bytecode resume ip and so
    /// finishes interpreted. An `ip == 0` frame whose function the table
    /// does not cover — or any nonzero ip — runs `execute_slice` as always.
    pub(super) fn run_slice(&mut self) -> VmResult<Step> {
        debug_assert_eq!(
            self.native_floor, 0,
            "a suspended process must never carry a raised frame floor"
        );
        let f = self.frame();
        if f.ip == 0
            && self
                .program
                .native
                .get(FuncIdx::from_usize(f.func_idx as usize))
                .is_some()
        {
            // A fresh slice's whole budget, seeded on the native side of the
            // boundary (`native_reds`) — the counter the compiled body's
            // checkpoints spend, mirroring `execute_slice`'s hot-loop local.
            self.native_reds = REDUCTION_BUDGET;
            take_freed_objects();
            let status = self.drive_top_frame();
            return match self.outcome_from_status(status) {
                // The native entry returned into a frame below it. That
                // frame's `ip` is a bytecode resume point by convention
                // (callers store it before any call that can suspend), so
                // the rest of the slice interprets from there, the return
                // protocol already applied and the result on top of the
                // stack — on the budget the native side left, not a fresh
                // one: it is all still the same scheduling slice.
                Ok(Step::Done) if !self.frames.is_empty() => {
                    self.execute_slice_budgeted(self.native_reds)
                }
                outcome => outcome,
            };
        }
        self.execute_slice()
    }

    pub(super) fn execute_slice(&mut self) -> VmResult<Step> {
        // A fresh slice: discard any reclamation count left over from before
        // it (a previous process's death-free, scheduler bookkeeping) — it
        // must not be billed to the process about to run. `run_slice`'s
        // native entry does the same before `drive_top_frame`.
        take_freed_objects();
        self.execute_slice_budgeted(REDUCTION_BUDGET)
    }

    /// As [`VM::execute_slice`], but starting from `budget` remaining
    /// reductions instead of a fresh slice's worth — the interpreter half of
    /// the `native_reds` contract (see the field doc in `mod.rs`): a
    /// native→interp re-entry and the post-native continuation of a slice
    /// both resume the *same* budget the native side has been spending, so
    /// one budget governs the whole scheduling slice no matter which backend
    /// spends it. On a `Done` exit the remainder is written back to
    /// `native_reds` for the native caller to keep spending.
    pub(super) fn execute_slice_budgeted(&mut self, budget: i32) -> VmResult<Step> {
        // Instrumented builds print the accumulated op histogram here: a
        // served-load run never exits, so slice entry is the one recurring
        // point that can notice the sample is big enough. Compiled out
        // entirely by default.
        #[cfg(feature = "op-histogram")]
        super::op_histogram::maybe_dump(&self.program);

        // Hoist the active frame's scalar state into locals so the per-instruction
        // path avoids two Vec indexes. Synced back to self.frames on Call/TailCall/Ret.
        // `func_idx` is hoisted for `CallSelf`/`TailCallSelf`, which resolve the
        // callee from the live frame instead of popping a closure.
        let (mut ip, mut code_start, mut base_slot, mut func_idx) = {
            let f = self.frame();
            (f.ip, f.code_start, f.base_slot, f.func_idx)
        };
        // Remaining reduction budget for this slice; one function application
        // costs one reduction. Exhaustion preempts the process. No freed-object
        // discard here: the fresh-slice entries (`execute_slice`, `run_slice`'s
        // native path) drop pre-slice leftovers themselves, so a mid-slice
        // continuation (`enter_interp`, `run_slice`'s post-native `Done` arm)
        // keeps the native portion's accrued reclamation debt flowing — it is
        // charged at this loop's next call checkpoint (`charge_reclamation`),
        // the debt half of the cross-backend budget contract.
        let mut reds = budget;

        // Dispatch helpers. Defined before the loop so their free
        // `self`/`ip`/`base_slot`/`code_start`/`func_idx`/`reds` resolve to
        // these locals by macro_rules definition-site hygiene. The fetched
        // instruction is threaded in as `$instr` because every dispatch site
        // fetches its own (see `dispatch!`). Each expands to exactly the
        // hand-written arm body, so codegen is byte-identical.
        macro_rules! bin {
            ($acc:ident, $ctor:ident, |$a:ident, $b:ident| $body:expr) => {{
                let $b = self.pop()?.$acc();
                let $a = self.pop()?.$acc();
                self.stack.push(Value::$ctor($body));
            }};
        }
        macro_rules! un {
            ($acc:ident, $ctor:ident, |$a:ident| $body:expr) => {{
                let $a = self.pop()?.$acc();
                self.stack.push(Value::$ctor($body));
            }};
        }
        // Integer-result forms: `push_int` boxes the rare out-of-range
        // spill.
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
        // Parking-op arm body: a `Some(step)` return means the process
        // parked and the slice is over.
        macro_rules! park {
            ($call:expr) => {{
                if let Some(step) = $call? {
                    return Ok(step);
                }
            }};
        }
        // Frame-entry body shared by `Call`/`TailCall` and
        // `CallKnown`/`TailCallKnown`: install the callee frame (collapse
        // in place for a tail call, else push a child), zero-fill the
        // non-argument locals, and re-hoist the loop's cached frame
        // fields. `$captures` is expanded into exactly one of the two
        // branches at runtime, so a moved value is moved once.
        macro_rules! enter_frame {
            ($target_idx:expr, $arity:expr, $func_locals:expr, $func_code_start:expr, $captures:expr, $tail:expr) => {{
                let target_idx = $target_idx;
                let arity = $arity;
                let func_locals = $func_locals;
                let func_code_start = $func_code_start;
                let args_start = self.stack.len() - arity as usize;

                if $tail {
                    self.collapse_tail_frame(base_slot, args_start);
                    let f = self.frame_mut();
                    f.func_idx = target_idx;
                    f.code_start = func_code_start;
                    f.ip = 0;
                    f.captures = $captures;
                } else {
                    self.frame_mut().ip = ip;
                    self.frames.push(CallFrame {
                        func_idx: target_idx,
                        code_start: func_code_start,
                        ip: 0,
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
        // Preemption checkpoint at a function application: one reduction,
        // plus whatever collection work accrued since the last checkpoint
        // (GC charges reductions so GC-heavy processes yield fairly). The
        // debt is zero on every call that did not collect, so it is paid
        // behind a test: draining unconditionally puts a store and
        // saturating-sub chain on every call, which measured ~1.5x on a
        // tail-recursive loop. `reds` is at least 1 here (a non-positive
        // value yielded at the last checkpoint), so the plain decrement
        // cannot wrap. The callee frame is fully consistent at every use
        // site, so preemption is a plain return — resume re-hoists from
        // the frame.
        macro_rules! checkpoint {
            () => {{
                reds -= 1;
                self.charge_reclamation(&mut reds);
                if reds <= 0 {
                    return Ok(Step::Yield);
                }
            }};
        }
        // The interp→native seam, one per *non-tail* call kind: after
        // `enter_frame!` (and its reds checkpoint) the callee frame is
        // exactly the state a native entry expects — `CallFrame` pushed,
        // arguments in `[base_slot, base_slot+arity)`, `ip == 0` — so a
        // table hit hands the frame to the native trampoline instead of
        // interpreting the body. One budget governs the slice on both sides
        // of the boundary: the remaining `reds` cross into `native_reds` and
        // whatever the compiled code left crosses back. On `Done` the callee
        // has returned with the return protocol applied and the caller frame
        // on top again, so the hoisted frame state re-syncs and
        // interpretation continues at the stored resume ip. Any other status
        // ends the slice as the interpreter's own yield/park/error would.
        //
        // Tail calls deliberately do NOT take this seam: an interpreted tail
        // chain that bounced into native here (and back out through
        // `al_rt_enter_interp` when the native callee tail-calls an
        // uncovered function) would stack one Rust frame pair per bounce,
        // breaking the language's flat-tail-call guarantee for a mutually
        // recursive covered/uncovered pair. A tail-collapsed frame sits at
        // `ip == 0`, so the covered function goes native at the next slice
        // boundary (`run_slice`) instead — bounded, and the frame shape is
        // identical.
        macro_rules! native_entry_check {
            ($target:expr) => {{
                if self
                    .program
                    .native
                    .get(FuncIdx::from_usize($target as usize))
                    .is_some()
                {
                    self.native_reds = reds;
                    let status = self.drive_top_frame();
                    reds = self.native_reds;
                    match self.outcome_from_status(status)? {
                        Step::Done => {
                            debug_assert!(self.frames.len() > self.native_floor);
                            let f = self.frame();
                            ip = f.ip;
                            code_start = f.code_start;
                            base_slot = f.base_slot;
                            func_idx = f.func_idx;
                        }
                        step => return Ok(step),
                    }
                }
            }};
        }

        // ---- Hot-opcode bodies ------------------------------------------
        //
        // Written once, expanded at both dispatch levels (see `dispatch!`).
        // Everything else is written inline in the full match below, which is
        // the only place a cold opcode is ever decoded.
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
        // freezes and publishes (`publish_toplevel`, out of line: it runs a
        // handful of times per program, never in a function body).
        macro_rules! store_local {
            ($instr:ident) => {{
                let slot = base_slot + $instr.operand as usize;
                let v = self.pop()?;
                self.stack[slot] = v;
                if base_slot == 0 && self.current_is_main {
                    self.publish_toplevel(slot);
                }
            }};
        }
        // Jump operands are frame-relative: the compiler emits them as offsets
        // into the executing function's own code block, which is exactly what
        // `ip` is, so a branch is an assignment and never touches `code_start`.
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
        // Self-recursion fast path: callee is the live frame's function, so we
        // skip the closure pop, the `Value::Closure` tag match, and the arity
        // check (statically guaranteed by `compile_call`). `func_idx`/
        // `code_start` are already the target's, so the only frame work is slot
        // bookkeeping.
        macro_rules! call_self {
            ($instr:ident) => {{
                let arity = $instr.operand;
                let func = &self.program.functions[func_idx as usize];
                let func_locals = func.locals;
                let args_start = self.stack.len() - arity as usize;

                if $instr.op == Op::TailCallSelf {
                    // Reuse the frame in place. Captures are already the
                    // current frame's — self-recursion cannot change the
                    // closed-over environment — so no `captures.clone()`.
                    // Non-argument locals are PRESERVED across the collapse
                    // so a Perceus `Drop` at end-of-body can hand its
                    // hollowed cell to a `Reuse` at start-of-body next
                    // iteration (loop-carried reuse); the zero-fill below
                    // is therefore skipped — the surviving slots already
                    // occupy `[base+arity, base+locals)`.
                    self.collapse_tail_frame_self(
                        base_slot,
                        args_start,
                        arity as usize,
                        func_locals as usize,
                    );
                    let f = self.frame_mut();
                    f.ip = 0;
                } else {
                    // Push a child frame. A self-call from inside a
                    // capture-carrying closure must see the same captures,
                    // so the child takes a new reference to the current
                    // frame's closure handle.
                    let captures = self.frame().captures.clone();
                    self.frame_mut().ip = ip;
                    self.frames.push(CallFrame {
                        func_idx,
                        code_start,
                        ip: 0,
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
                // next iteration's arguments already in the locals: resume
                // re-enters the function from the top and the loop continues.
                // This frame-at-yield shape is a contract any alternative
                // execution backend must reproduce; pinned by
                // `tests/vm_fairness.rs`.
                checkpoint!();
                if $instr.op == Op::CallSelf {
                    native_entry_check!(func_idx);
                }
            }};
        }
        // Known top-level target: `func_idx` is an immediate operand and the
        // callee is provably capture-free, so there is no callee value on the
        // stack — we skip the pop, the closure tag check, the heap `func_idx()`
        // read, and the arity check that `Op::Call` pays. The frame's
        // `captures` is a sentinel immediate; the callee body never emits
        // `PushCapture` / `PushSelf` (a top-level fn's own name resolves to
        // `SelfGlobal`, so self-as-value loads via `PushGlobal`).
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
                if $instr.op == Op::CallKnown {
                    native_entry_check!(target_idx);
                }
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

                // Popping to the frame floor ends the slice with the return
                // protocol already applied (truncate + push result). The
                // floor is 0 for a whole process — the last frame's return —
                // and is raised across a native→interpreter re-entry
                // (`native::al_rt_enter_interp`) so control returns to the
                // native caller when the frame it pushed returns, instead of
                // interpreting on into the native caller's own body.
                if self.frames.len() == self.native_floor {
                    break;
                }
                match self.frames.last() {
                    None => break,
                    Some(f) => {
                        ip = f.ip;
                        code_start = f.code_start;
                        base_slot = f.base_slot;
                        func_idx = f.func_idx;
                    }
                }
            }};
        }

        // ---- Replicated dispatch ----------------------------------------
        //
        // One copy of fetch-and-switch, expanded at the tail of the seven arms
        // listed in the module docs — the arms with a predictable successor.
        // Each expansion is a distinct indirect branch, so the predictor gets
        // a branch-target entry per *site* rather than one entry shared by the
        // whole instruction stream. An entry only earns its footprint where it
        // predicts, which is why the tails sit on `Ret`, the calls and the
        // `*IntLC` superinstructions and not on the two most-executed arms.
        //
        // The arms below are the small register-shaped opcodes. An opcode not
        // listed leaves `ip` on its instruction and falls out to the shared
        // loop head, which re-fetches and runs the full match; those bodies
        // are calls and family handlers, so the extra fetch is free relative
        // to the work they are about to do.
        // Instrumented builds collapse the replicated tails: a `dispatch!()`
        // site that decodes nothing falls out to the shared loop head, which
        // re-fetches and runs the full match. Semantics are unchanged (that
        // is already the path for every opcode the copy does not decode) and
        // it is what makes the loop head the single counting site.
        #[cfg(feature = "op-histogram")]
        macro_rules! dispatch {
            () => {{}};
        }
        #[cfg(not(feature = "op-histogram"))]
        macro_rules! dispatch {
            () => {{
                let addr = code_start + ip;
                let Some(instr) = al_core::bytecode::fetch(&self.program.code, addr as usize)
                else {
                    break;
                };
                match instr.op {
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
                    // every reuse-shaped body, so it is the one non-arithmetic
                    // opcode worth its place here.
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
            let Some(instr) = al_core::bytecode::fetch(&self.program.code, addr as usize) else {
                break;
            };
            ip += 1;

            #[cfg(feature = "op-histogram")]
            super::op_histogram::record(instr.op, func_idx);

            match instr.op {
                Op::PushConst => push_const!(instr),
                Op::PushLocal => push_local!(instr),
                Op::PushGlobal => {
                    // Top-level bindings live in the global (literal) area,
                    // shared by every process on this scheduler.
                    let slot = instr.operand as usize;
                    self.stack.push(self.globals[slot].clone());
                }
                Op::StoreLocal => store_local!(instr),
                Op::PushNil => {
                    self.stack.push(self.make_nil());
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
                // Polymorphic arithmetic (the untyped fallbacks; the
                // type-specialized arms below handle proven operands).
                Op::Add => self.add()?,
                Op::Sub => self.sub()?,
                Op::Mul => self.mul()?,
                Op::Div => self.div()?,
                Op::Mod => self.rem()?,
                Op::Neg => {
                    let a = self.pop()?;
                    if let Some(i) = a.as_int() {
                        self.push_int(i.wrapping_neg());
                    } else if let Some(f) = a.as_float() {
                        self.stack.push(Value::float(-f));
                    } else {
                        return Err(VmError::type_mismatch("negate", "Int or Float", &a));
                    }
                }

                // ---- Type-specialized arithmetic ----------------------------
                // Emitted only when unification proved both operands concrete,
                // so the tag is a debug-only invariant (`as_*_typed`).
                // Totality matches `binary_op`/`float_op` exactly: wrap on
                // overflow, x/0=0, x%0=x, non-finite float → 0.0.
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
                Op::AddFloat => bin!(as_float_typed, float, |a, b| a + b),
                Op::SubFloat => bin!(as_float_typed, float, |a, b| a - b),
                Op::MulFloat => bin!(as_float_typed, float, |a, b| a * b),
                Op::DivFloat => {
                    bin!(as_float_typed, float, |a, b| if b == 0.0 {
                        0.0
                    } else {
                        a / b
                    })
                }
                Op::NegFloat => un!(as_float_typed, float, |a| -a),
                Op::AddStr => self.str_concat2()?,

                Op::Eq => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    self.stack.push(Value::bool(values_equal(&a, &b)));
                }
                Op::Neq => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    self.stack.push(Value::bool(!values_equal(&a, &b)));
                }
                Op::Lt => self.compare_push(|o| o.is_lt())?,
                Op::Gt => self.compare_push(|o| o.is_gt())?,
                Op::Lte => self.compare_push(|o| o.is_le())?,
                Op::Gte => self.compare_push(|o| o.is_ge())?,

                // ---- Type-specialized comparison ----------------------------
                Op::LtInt => bin!(as_int_typed, bool, |a, b| a < b),
                Op::GtInt => bin!(as_int_typed, bool, |a, b| a > b),
                Op::LteInt => bin!(as_int_typed, bool, |a, b| a <= b),
                Op::GteInt => bin!(as_int_typed, bool, |a, b| a >= b),
                Op::EqInt => bin!(as_int_typed, bool, |a, b| a == b),
                Op::NeqInt => bin!(as_int_typed, bool, |a, b| a != b),
                Op::LtFloat => bin!(as_float_typed, bool, |a, b| a < b),
                Op::GtFloat => bin!(as_float_typed, bool, |a, b| a > b),
                Op::LteFloat => bin!(as_float_typed, bool, |a, b| a <= b),
                Op::GteFloat => bin!(as_float_typed, bool, |a, b| a >= b),

                Op::Not => {
                    let a = self.pop()?;
                    self.stack.push(Value::bool(!is_truthy(&a)?));
                }
                Op::Jump => jump!(instr),
                Op::JumpIfFalse => jump_if_false!(instr),
                // ---- Superinstructions --------------------------------------
                // Peephole-fused (PushLocal a; PushConst b; <op>) sequences;
                // operands packed into Instruction.{a,b}. Zero stack traffic
                // for the compare+branch forms.
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
                    // handle — one word copied, no captures clone.
                    enter_frame!(
                        cl_func_idx,
                        arity,
                        func_locals,
                        func_code_start,
                        callee,
                        instr.op == Op::TailCall
                    );
                    checkpoint!();
                    if instr.op == Op::Call {
                        native_entry_check!(cl_func_idx);
                    }
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
                // Aggregate values (arrays, tuples, ranges, field access):
                // one method per op (see `vm::collections`).
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
                    // The frame's `captures` handle IS the closure being
                    // executed; pushing self takes a new reference to it.
                    let val = self.frame().captures.clone();
                    self.stack.push(val);
                }
                Op::Print => {
                    let val = self.pop()?;
                    println!("{}", inspect(&val, &self.program));
                    // A write syscall (and potentially a blocking one on a
                    // full pipe): charge it like other I/O.
                    reds -= IO_REDUCTION_COST;
                }
                Op::StackDepth => {
                    self.stack.push(Value::small_int(self.frames.len() as i64));
                }
                Op::Monotonic => {
                    self.stack.push(Value::small_int(monotonic_now_ms()));
                }
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
                Op::MakeEnumPayload => {
                    self.make_enum_payload(instr.operand, instr.b as usize, instr.a != 0)?
                }
                Op::MatchEnum => {
                    let variant_name = self.pop()?;
                    let type_id_val = self.pop()?;
                    let val = self.pop()?;

                    let Some(variant_str) = variant_name.as_str() else {
                        return Err(VmError::internal("enum/variant names must be strings"));
                    };
                    let Some(type_id) = type_id_val.as_int() else {
                        return Err(VmError::internal("enum type id must be int"));
                    };
                    let type_id = al_core::TypeId(type_id as i32);

                    if let Some(ev) = val.as_enum() {
                        // Both operands are frozen constant-pool `Str` values.
                        // For enums built via `MakeEnumPayload` the stored
                        // variant-name word IS the interned constant the match
                        // header pushed, so a raw-bits compare decides the arm
                        // in O(1). Fall through to a content compare only when
                        // the enum came through a different frozen builder
                        // (prelude templates), where the same name interns to a
                        // distinct address.
                        self.stack.push(Value::bool(
                            ev.type_id() == type_id
                                && (ev.variant_name_value().to_bits() == variant_name.to_bits()
                                    || ev.variant_name() == variant_str),
                        ));
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
                    // Computed jump by variant index. Emitted only when the
                    // scrutinee's type is a fully resolved enum and the match
                    // is exhaustive, so `as_enum` is a debug-only invariant and
                    // `idx < a` is guaranteed by construction of the table.
                    let scrutinee = self.pop()?;
                    let Some(ev) = scrutinee.as_enum() else {
                        return Err(VmError::internal("SwitchTag on non-enum value"));
                    };
                    let idx = ev.variant_idx() as i32;
                    debug_assert!((idx as u32) < instr.a as u32);
                    // `operand` is the table base, frame-relative like every
                    // other target, so indexing `program.code` needs
                    // `code_start` added back. The table's entries are ordinary
                    // `Jump`s, so their operands are frame-relative too.
                    ip = self.program.code[(code_start + instr.operand + idx) as usize].operand;
                }
                Op::ToString => {
                    let val = self.pop()?;
                    // An already-Str operand is its own string image: push the
                    // same value word back instead of round-tripping through
                    // inspect() plus a fresh arena Str allocation.
                    if val.as_str().is_some() {
                        self.stack.push(val);
                    } else {
                        let s = inspect(&val, &self.program);
                        let v = Value::str_in(&mut self.heap, &s);
                        self.stack.push(v);
                    }
                }
                Op::StrConcatN => {
                    let n = instr.operand as usize;
                    let base = self.operand_base(n)?;
                    let v = {
                        let mut parts: SmallVec<[&str; 8]> = SmallVec::with_capacity(n);
                        for v in &self.stack[base..] {
                            match v.as_str() {
                                Some(s) => parts.push(s),
                                None => {
                                    return Err(VmError::internal("str_concat requires strings"));
                                }
                            }
                        }
                        Value::str_from_parts_in(&mut self.heap, &parts)
                    };
                    self.stack.truncate(base);
                    self.stack.push(v);
                }
                Op::Halt => {
                    break;
                }
                // I/O, timer, and process opcodes — anything that can
                // park the process or offload to the blocking pool. One
                // method per op (see `vm::io`); their cost is the syscall,
                // not the call. A `Some(step)` return means the process
                // parked and the slice is over.
                Op::FileRead => park!(self.file_read(ip, &mut reds)),
                Op::FileWrite => park!(self.file_write(ip, &mut reds)),
                Op::TcpListen => self.tcp_listen()?,
                Op::TcpAccept => park!(self.tcp_accept(ip, &mut reds)),
                Op::TcpConnect => park!(self.tcp_connect(ip, &mut reds)),
                Op::TcpRead => park!(self.tcp_read(ip, &mut reds)),
                Op::TcpReadUntil => park!(self.tcp_read_until(ip, &mut reds)),
                Op::TcpWrite => park!(self.tcp_write(ip, &mut reds)),
                Op::TcpWriteParts => park!(self.tcp_write_parts(ip, &mut reds)),
                Op::TcpClose => self.tcp_close(&mut reds)?,
                Op::TcpCloseServer => self.tcp_close_server()?,
                Op::TcpLocalAddr => self.tcp_local_addr()?,
                Op::DnsResolve => park!(self.dns_resolve(ip, &mut reds)),
                Op::IpParse => self.ip_parse()?,
                Op::ProcessSpawn => self.process_spawn(&mut reds)?,
                Op::SpawnLocal => self.process_spawn_local(&mut reds)?,
                Op::SpawnOnEach => self.process_spawn_on_each(&mut reds)?,
                Op::Sleep => park!(self.sleep(ip)),
                // String and binary builtins: pure stack transformations,
                // one method per op (see `vm::text`).
                Op::StrSplit => self.str_split()?,
                Op::StrLen => self.str_len()?,
                Op::StrContains => self.str_contains()?,
                Op::StrTrim => self.str_trim()?,
                Op::IntToString => self.int_to_string()?,
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
                // Byte-oriented ASCII builtins: cold, never-inline methods so
                // their bodies (and the int<->ASCII helpers they inline) stay
                // out of the central dispatch loop, keeping the hot integer
                // arms' codegen undisturbed.
                Op::BinIndexOf => self.bin_index_of()?,
                Op::BinByteAt => self.bin_byte_at()?,
                Op::BinParseInt => self.bin_parse_int()?,
                Op::BinEqIgnoreAsciiCase => self.bin_eq_ignore_ascii_case()?,
                Op::BinToAsciiLower => self.bin_to_ascii_lower()?,
                Op::BinFromIntAscii => self.bin_from_int_ascii()?,
                // HTTP/1.1 protocol ops: native byte scanning + value assembly,
                // cold for the same reason as the ASCII builtins.
                Op::HttpParseHead => self.http_parse_head()?,
                Op::HttpFraming => self.http_framing()?,
                Op::HttpChunkDecode => self.http_chunk_decode()?,
                Op::HttpHeaderGet => self.http_header_get()?,
                Op::HttpHeaderHas => self.http_header_has()?,
                Op::HttpHeadersValid => self.http_headers_valid()?,
                Op::HttpSerializeHead => self.http_serialize_head()?,
                // Float→Int casts use saturating `as i64`; Value floats are
                // canonicalized finite (no NaN/Inf), so these stay total.
                Op::FloatFloor => {
                    let f = self.pop_float("float.floor")?;
                    self.push_int(f.floor() as i64);
                }
                Op::FloatCeil => {
                    let f = self.pop_float("float.ceil")?;
                    self.push_int(f.ceil() as i64);
                }
                Op::FloatRound => {
                    let f = self.pop_float("float.round")?;
                    self.push_int(f.round() as i64);
                }
                Op::FloatTruncate => {
                    let f = self.pop_float("float.truncate")?;
                    self.push_int(f.trunc() as i64);
                }
                Op::FloatFromInt => {
                    let n = self.pop_int("float.from_int")?;
                    self.stack.push(Value::float(n as f64));
                }
                Op::FloatToString => {
                    // f64 renders in at most 24 bytes through `f64_str`.
                    let f = self.pop_float("float.to_string")?;
                    let s = f64_str(f);
                    let v = Value::str_in(&mut self.heap, &s);
                    self.stack.push(v);
                }
                // Perceus drop-guided reuse (frame-limited, ICFP'22).
                Op::Drop => {
                    // Last use of this local in the frame. If the frame holds
                    // the ONLY reference (rc==1), keep the allocation in the
                    // slot for a following `Reuse` but release its children
                    // NOW: reuse only propagates down a recursive chain (the
                    // canonical `map`) when the parent cons stops holding the
                    // tail before the recursive call sees it. The slot IS the
                    // per-frame reuse table; the hollowed cell is released by
                    // a following `Reuse` (takes ownership) or by `Ret`'s
                    // truncate. Otherwise the value is shared (or non-heap):
                    // release the frame's reference now by overwriting the
                    // slot, so `Reuse` sees no candidate and allocates fresh.
                    let slot = base_slot + instr.operand as usize;
                    let v = &mut self.stack[slot];
                    if v.is_unique() {
                        v.hollow_for_reuse();
                    } else {
                        *v = Value::small_int(0);
                    }
                }
                Op::Reuse => {
                    // Take the candidate cell the preceding `Drop` left in
                    // the slot: still uniquely owned (rc==1) if reusable, else
                    // cleared. Push the cell as the address for the following
                    // constructor to overwrite in place — ownership transfers
                    // via the stack so rc stays 1 and the constructor releases
                    // the old children before writing new payload. Otherwise
                    // push nil and the constructor falls through to a fresh
                    // allocation; the non-reusable `cell` drops as a no-op
                    // (`Drop` already released the frame's reference).
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

        // Halt, Ret with no caller or to the frame floor, or running off the
        // end of the code. Publish the remaining budget for a native caller
        // sitting under the floor (`enter_interp` resumes spending it); for a
        // finished process the write is dead — the next slice re-seeds.
        self.native_reds = reds;
        Ok(Step::Done)
    }

    /// Collapse the active frame for a tail call: discard the slots in
    /// `[base, args_start)` and slide the freshly-pushed argument words sitting
    /// at `[args_start, len)` down to `base`. Under reference counting the
    /// discarded slots own references: `drain` drops each of them (decref) and
    /// shifts the surviving args down by *moving* them (no clone), which is
    /// exactly the required behaviour.
    #[inline]
    pub(super) fn collapse_tail_frame(&mut self, base: usize, args_start: usize) {
        debug_assert!(base <= args_start && args_start <= self.stack.len());
        self.stack.drain(base..args_start);
    }

    /// Collapse the active frame for a *self*-tail-call: swap the `arity` new
    /// argument words at `[args_start, len)` down into the parameter slots
    /// `[base, base+arity)`, then truncate to `[base, base+locals)` — dropping
    /// the temporaries in `[base+locals, args_start)` and the swapped-out old
    /// params now sitting at the tail.
    ///
    /// The non-argument locals `[base+arity, base+locals)` are left in place.
    /// Those slots ARE the per-frame reuse table: `Op::Drop` at end-of-body
    /// parked hollowed cells there for `Op::Reuse` at start-of-body to
    /// consume on the next iteration (loop-carried reuse across
    /// `TailCallSelf` — see `core_ir::perceus`). Perceus has already dropped
    /// every dead heap local before the tail edge, so the surviving slots
    /// hold either a hollowed reuse cell or an immediate zero; carrying them
    /// forward costs nothing, and the caller skips the zero-fill.
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
    /// binding's graph into the program-wide frozen area (the graph copy with
    /// the frozen builder as destination) and mirror the frozen root into the
    /// global area. The table holds only frozen words — never arena pointers —
    /// so it is not a GC root and `PushGlobal` is a zero-copy word push on
    /// every scheduler.
    ///
    /// Why `base_slot == 0 && current_is_main` singles out the entry frame:
    /// spawned processes also run their seed frame at base_slot 0, hence the
    /// is_main guard. Within main no other frame can sit at base_slot 0. A
    /// callee's base is the operand-stack depth at call time, and main's stack
    /// never drains below the entry frame's locals: `Ret` truncates only to the
    /// returning frame's own base, the entry frame exits via `Halt`, and the
    /// compiler marks tail position only inside function bodies, so a
    /// module-scope call is never a frame-collapsing `TailCall`. Those locals
    /// are never zero — `__main__` opens with the precompiled stdlib's binding
    /// stores (the `__pre*` slots seeded by `Compiler::seed_static`) — so even a
    /// zero-arity call at operand depth zero gets a base_slot of at least the
    /// stdlib binding count. Without that floor, such a call would land a callee
    /// at base_slot 0 and `StoreLocal` would silently freeze and publish the
    /// callee's locals as globals.
    ///
    /// The publish is unconditional — there is no per-slot "already published"
    /// filter — so a slot that is stored more than once is frozen and published
    /// more than once. The contract readers rely on is therefore not
    /// at-most-once publication but publish-before-read: all of a slot's stores
    /// happen inside the single top-level statement that owns it, before any
    /// closure that could `PushGlobal` it exists (`PushGlobal` is emitted only
    /// for entry-frame slots referenced from nested functions), so every reader
    /// observes exactly one stable published value per slot. Two compiler facts
    /// uphold that. Sequential rebindings never re-store: `get_or_create_local`
    /// gives every module-scope binding — including a shadowing rebinding of an
    /// existing name — a fresh entry-frame slot rather than reusing the old one.
    /// And the slots that are stored repeatedly — binary-pattern cursor temps
    /// (one store per dynamic segment) and or-pattern alternative bindings
    /// (re-stored when a later alternative re-binds the same name) — finish all
    /// their stores while the owning statement is still executing. A re-store
    /// does re-freeze the slot's graph into the never-collected frozen area;
    /// that waste is bounded by the owning statement.
    ///
    /// Out of line and `#[cold]` for the same reason as [`spill_int`](Self::spill_int):
    /// only the entry frame ever takes this path, and `StoreLocal` is replicated
    /// into every hot dispatch site.
    #[cold]
    #[inline(never)]
    fn publish_toplevel(&mut self, slot: usize) {
        if slot >= self.globals.len() {
            self.globals.resize(slot + 1, Value::nil());
        }
        let frozen = freeze::freeze_global(&mut self.frozen, &self.stack[slot]);
        self.globals[slot] = frozen.value();
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
    /// `payload_count` payload words on top of the stack. `prehash_idx` indexes
    /// the constant pool at the compile-time `enum_name_prefix_hash`. When
    /// `with_reuse`, an `Op::Reuse` token sits on top of the payload words:
    /// pop it first and — if it names a live cell — overwrite that allocation
    /// in place instead of allocating fresh.
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
        // its payload slots are fresh. The four header words sit at fixed
        // offsets just below the payload; all operands are read before the
        // single truncate that retires them.
        let base = self.operand_base(payload_count + 4)?;
        let payload_base = base + 4;

        let type_id_val = &self.stack[base];
        let enum_name_val = self.stack[base + 1].clone();
        let variant_name_val = self.stack[base + 2].clone();
        let labels_val = &self.stack[base + 3];

        let Some(packed) = type_id_val.as_int() else {
            return Err(VmError::internal("enum type id must be int"));
        };
        let type_id = al_core::TypeId(packed as i32);
        let variant_idx = (packed >> 32) as u16;

        if enum_name_val.as_str().is_none() {
            return Err(VmError::internal("enum name must be string"));
        }

        // The field-label array is a per-ctor-site pooled constant
        // (`emit_construct_header` → a frozen labels array), and `PushConst`
        // copies the constant word, so the popped value is pointer-identical to
        // the pool entry and stable for the program's lifetime. Freeze the
        // labels Tuple once per site, memoized by that address; later
        // constructions reuse the one frozen tuple.
        let labels_key = match (labels_val.as_array(), labels_val.object_addr()) {
            (Some(_), Some(addr)) => addr,
            _ => return Err(VmError::internal("field labels must be an array")),
        };
        let field_labels = match self.label_cache.get(&labels_key) {
            Some(cached) => cached.clone(),
            None => {
                // The label strings are frozen constants — `publish_frozen`
                // is a pure passthrough for them, and the only door from a
                // runtime `Value` to a `FrozenConst` child. The canonical
                // labels Tuple is built in the frozen area once per ctor
                // site, shared by every instance.
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

        // `prehash_idx` indexes the constant pool at the compile-time
        // `enum_name_prefix_hash(enum_name, variant_name)`. Folding only the
        // payload hashes here is bit-identical to hashing the name bytes then
        // the payloads in one pass, without re-walking the static name bytes.
        // Hash lazily: `0` marks the cell "not yet hashed"; `EnumRef::hash`
        // computes and caches on first use. Constructing is orders of
        // magnitude more common than hashing, and the payload walk here was a
        // measured ~5% of a keep-alive HTTP request. (The op still carries
        // its prehash-constant operand; nothing reads it at runtime now.)
        let hash = 0u64;
        // `enum_name`/`variant_name` are frozen constant-pool `Str` values —
        // stored as single reference words, never copied per construction.
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

    /// Pop and decode the Perceus reuse token an `Op::Reuse` pushed on top of
    /// a constructor's operands: `Some(addr)` when the compiler-paired cell was
    /// uniquely owned (rc==1 — ownership transfers to the raw address, which
    /// the constructor will overwrite in place), `None` when it wasn't
    /// (allocate fresh; the popped nil drops as a no-op).
    #[inline]
    pub(super) fn take_reuse_addr(&mut self) -> VmResult<ReuseAddr> {
        Ok(self.pop()?.into_reuse_addr())
    }

    /// Index of the first of the top `n` operand slots, left in place. The
    /// caller reads them via `&self.stack[base..]` (heap and stack are
    /// disjoint fields, so a `*_in` constructor borrow is legal alongside)
    /// and truncates to `base` afterwards — no temporary buffer.
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

    /// [`peek_at`](Self::peek_at) that reports underflow as a compiler-bug
    /// error, for callers that have already type-checked the operand shape.
    pub(super) fn peek_at_or(&self, d: usize) -> VmResult<&Value> {
        self.peek_at(d)
            .ok_or_else(|| VmError::internal("stack underflow"))
    }

    /// Bill `reds` for reference-counting reclamation done since the last call
    /// checkpoint. Freeing is not free: dropping a large value graph cascades
    /// through thousands of objects, anywhere a last reference goes away. The
    /// allocator counts objects it frees on this thread; this drains that count
    /// and charges one reduction per [`FREES_PER_REDUCTION`], so a giant
    /// cascading free preempts at the next call instead of stalling the
    /// scheduler. Fast path is a single thread-local read.
    #[inline]
    pub(super) fn charge_reclamation(&self, reds: &mut i32) {
        if freed_objects_pending() >= FREES_PER_REDUCTION {
            let freed = take_freed_objects();
            *reds = reds.saturating_sub((freed / FREES_PER_REDUCTION) as i32);
        }
    }

    // --- Typed pop helpers (stdlib / I/O ops only — not the arithmetic loop) --
    //
    // The popped `Value` is returned whole (a word); callers borrow the arena
    // contents through it (`str_ref`/`bin_ref`). The borrow stays valid across a
    // following `*_in` constructor because the heap and stack are disjoint VM
    // fields and allocation never moves existing objects.

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

    /// Pop a numeric value as `f64`. Accepts Int too (coerced), mirroring the
    /// numeric tolerance of the arithmetic loop (`Op::Neg` etc.).
    #[inline]
    fn pop_float(&mut self, op: &'static str) -> VmResult<f64> {
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

    // --- Prelude value constructors ------------------------------------------
    //
    // `make_nil`/`make_none` copy prebuilt frozen-area values and allocate
    // nothing. The payload-carrying constructors build one fresh wrapper enum.

    #[inline]
    pub(super) fn make_nil(&self) -> Value {
        self.templates.nil.clone()
    }

    #[inline]
    pub(super) fn make_none(&self) -> Value {
        self.templates.none.clone()
    }

    #[inline]
    pub(super) fn make_some(&mut self, v: Value) -> Value {
        self.templates
            .some
            .clone()
            .instantiate(&mut self.heap, &[v])
    }

    #[inline]
    pub(super) fn make_ok(&mut self, v: Value) -> Value {
        self.templates.ok.clone().instantiate(&mut self.heap, &[v])
    }

    #[inline]
    pub(super) fn make_err(&mut self, v: Value) -> Value {
        self.templates.err.clone().instantiate(&mut self.heap, &[v])
    }

    #[inline]
    pub(super) fn make_err_nil(&mut self) -> Value {
        let nil = self.make_nil();
        self.make_err(nil)
    }

    /// The frozen [`EnumTemplate`] for a stdlib variant, built on first use
    /// (interned names go into the program's frozen area) and memoized by
    /// template identity, so runtime error construction allocates only the
    /// enum cell itself — never names.
    pub(super) fn stdlib_template(&mut self, t: &'static VariantTemplate) -> EnumTemplate {
        let key = t as *const VariantTemplate as usize;
        if let Some(tpl) = self.template_cache.get(&key) {
            return tpl.clone();
        }
        let tpl = enum_template(&mut self.frozen, t);
        self.template_cache.insert(key, tpl.clone());
        tpl
    }

    /// A nullary stdlib enum instance.
    pub(super) fn stdlib_enum(&mut self, t: &'static VariantTemplate) -> Value {
        let tpl = self.stdlib_template(t);
        tpl.instantiate(&mut self.heap, &[])
    }

    /// Concatenate the two strings on top of the stack. Shared by the `AddStr`
    /// opcode and the Str + Str case of the untyped [`add`](Self::add).
    #[inline]
    fn str_concat2(&mut self) -> VmResult<()> {
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

    /// `+` — the one arithmetic op with a non-numeric case: Str + Str
    /// concatenation via [`str_concat2`](Self::str_concat2). Numeric pairs
    /// fall through to [`arith`](Self::arith).
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

    /// `/` is TOTAL: `x / 0 = 0` for ints (the Lean/Coq convention for
    /// keeping division a total function) and `x / 0.0 = 0.0` for floats.
    /// The zero guard is load-bearing — `wrapping_div` still panics on a
    /// zero divisor.
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

    /// `%` is TOTAL: `x % 0 = x` (preserving the identity
    /// `a = (a/b)*b + a%b`) for ints and floats alike.
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

    /// Polymorphic numeric core for the untyped `+ - * / %` fallbacks: int
    /// pairs use `int_f`, float pairs use `float_f`, anything else is a
    /// compiler-bug error (HM unifies both operands to one numeric type; a
    /// mixed pair here means the typechecker is wrong). Arithmetic is
    /// TOTAL — every numeric case yields a value: integer overflow wraps
    /// with two's-complement semantics, and `Value::float` collapses any
    /// non-finite float result (overflow to ±Inf, `0.0/0.0` NaN) to `0.0`.
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

    /// Pop two operands, order them ([`compare_values`]), and push whether
    /// the ordering satisfies `keep` — one method serves `< > <= >=` with
    /// no per-op match.
    pub(super) fn compare_push(&mut self, keep: fn(std::cmp::Ordering) -> bool) -> VmResult<()> {
        let b = self.pop()?;
        let a = self.pop()?;
        let ord = compare_values(a, b)?;
        self.stack.push(Value::bool(keep(ord)));
        Ok(())
    }

    /// Box an i64 into a `Value`. The spill path is `#[cold] #[inline(never)]`
    /// so heap allocation stays out of hot integer-arithmetic codegen.
    #[inline]
    pub(super) fn boxed_int(&mut self, i: i64) -> Value {
        if Value::fits_small_int(i) {
            Value::small_int(i)
        } else {
            self.spill_int(i)
        }
    }

    /// The out-of-range half of [`boxed_int`](Self::boxed_int). Kept out of
    /// line so the heap allocation never inlines into the integer arithmetic
    /// arms — the dispatch loop's hottest code — where it costs registers and
    /// i-cache on every op for a case that almost never runs.
    #[cold]
    #[inline(never)]
    fn spill_int(&mut self, i: i64) -> Value {
        Value::int_in(&mut self.heap, i)
    }

    /// Push the program's command-line arguments as an `Array(String)`. The
    /// args live on the shared runtime; the Arc is cloned so the per-arg
    /// allocation can borrow the heap mutably while the source list is read.
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
                // The element is the frozen constant-pool `Str` value
                // itself; storing it is one reference word.
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

/// Total ordering of two same-typed numeric operands. AL floats are
/// canonical finite (no NaN or Infinity in the value space), so
/// `partial_cmp` always succeeds; the `Equal` fallback keeps the function
/// total by construction rather than by `unwrap`.
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
