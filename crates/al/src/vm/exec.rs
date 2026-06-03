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
//! The rooting rule governs every allocating arm, inline or routed: it
//! computes its worst-case word need from [`cost`] BEFORE popping its
//! operands — while they are still on the VM stack, where a collection
//! triggered by `ensure` can see and rewrite them — then pops and
//! allocates collection-free. The typed pop helpers and the
//! `make_*`/`stdlib_*` constructors at the bottom of this file are the
//! shared vocabulary those arms (and the sibling handlers) build on.
//!
//! Value semantics live here too ([`values_equal`], `compare`,
//! `is_truthy`): structural equality with the Range/Array congruence
//! (a lazy range equals the array of its elements) and the hash
//! fast-reject on enums.

use al_core::bytecode::{Op, Value, ValueView, enum_hash_with_payload};
use al_core::static_ir::VariantTemplate;

use super::poll::monotonic_now_ms;
use super::{
    CallFrame, EnumTemplate, IO_REDUCTION_COST, REDUCTION_BUDGET, Step, VM, VmResult, cost,
    enum_template, f64_str, freeze, inspect, range_len, value_type_name,
};

impl VM {
    pub(super) fn execute_slice(&mut self) -> VmResult<Step> {
        // Hoist the active frame's scalar state into locals so the per-instruction
        // path avoids two Vec indexes. Synced back to self.frames on Call/TailCall/Ret.
        // `func_idx` is hoisted for `CallSelf`/`TailCallSelf`, which resolve the
        // callee from the live frame instead of popping a closure.
        let (mut ip, mut code_start, mut base_slot, mut func_idx) = {
            let f = self.frame();
            (f.ip, f.code_start, f.base_slot, f.func_idx)
        };
        // Remaining reduction budget for this slice; one function application
        // costs one reduction. Exhaustion preempts the process.
        let mut reds = REDUCTION_BUDGET;
        // Drop GC debt left over from the previous slice. `GcDebt` is
        // scheduler-wide, so a charge accrued by a process that blocked or
        // finished between its last collection and its next call checkpoint
        // would otherwise be billed to whatever process runs next. GC
        // work shrinks the slice it happens in and never carries
        // across a context switch — the owing process already yielded.
        self.gc.pending_reds = 0;

        loop {
            let addr = code_start + ip;
            if addr as usize >= self.program.code.len() {
                break;
            }

            // SAFETY: bounds checked immediately above.
            let instr = unsafe { al_core::bytecode::fetch(&self.program.code, addr as usize) };
            ip += 1;

            // Debug discipline: every opcode's budget
            // starts at zero, so an allocation without its own `ensure` trips
            // the watermark in `alloc_young` even when the previous opcode
            // left slack behind. Compiled out of release builds.
            if cfg!(debug_assertions) {
                self.heap.note_ensured(0);
            }

            // Typed-op dispatch helpers. Defined here (after `instr`) so the
            // free `self`/`instr`/`ip`/`base_slot`/`code_start` resolve by
            // macro_rules definition-site hygiene. Each expands to exactly the
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
            // Integer-result forms: `push_int` budgets + boxes the rare
            // out-of-range spill itself (operands are immediates).
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
                (|$a:ident, $b:ident| $body:expr) => {{
                    let $a = self.stack[base_slot + instr.a as usize].as_int_typed();
                    let $b = self.program.constants[instr.b as usize].as_int_typed();
                    self.push_int($body);
                }};
            }
            macro_rules! lc_jump {
                (|$a:ident, $b:ident| $cond:expr) => {{
                    let $a = self.stack[base_slot + instr.a as usize].as_int_typed();
                    let $b = self.program.constants[instr.b as usize].as_int_typed();
                    if $cond {
                        ip = instr.operand - code_start;
                    }
                }};
            }

            match instr.op {
                Op::PushConst => {
                    self.stack
                        .push(self.program.constants[instr.operand as usize]);
                }
                Op::PushLocal => {
                    let slot = base_slot + instr.operand as usize;
                    self.stack.push(self.stack[slot]);
                }
                Op::PushGlobal => {
                    // Top-level bindings live in the global (literal) area,
                    // shared by every process on this scheduler.
                    let slot = instr.operand as usize;
                    self.stack.push(self.globals[slot]);
                }
                Op::StoreLocal => {
                    let slot = base_slot + instr.operand as usize;
                    let v = self.pop()?;
                    self.stack[slot] = v;
                    // Top-level bindings (the main process's entry frame) are
                    // the program's globals: freeze the binding's graph into
                    // the program-wide frozen area (the shared `copy_graph`
                    // with the frozen builder as destination) and mirror the
                    // frozen root into the global area. The table holds only
                    // frozen words — never arena pointers — so it is not a GC
                    // root and `PushGlobal` is a zero-copy word push on every
                    // scheduler.
                    //
                    // Why `base_slot == 0 && current_is_main` singles out the
                    // entry frame: spawned processes also run their seed frame
                    // at base_slot 0, hence the is_main guard. Within main no
                    // other frame can sit at base_slot 0. A callee's base is
                    // the operand-stack depth at call time, and main's stack
                    // never drains below the entry frame's locals: `Ret`
                    // truncates only to the returning frame's own base, the
                    // entry frame exits via `Halt`, and the compiler marks
                    // tail position only inside function bodies, so a
                    // module-scope call is never a frame-collapsing
                    // `TailCall`. Those locals are never zero — `__main__`
                    // opens with the precompiled stdlib's binding stores (the
                    // `__pre*` slots seeded by `Compiler::seed_static`) — so
                    // even a zero-arity call at operand depth zero gets a
                    // base_slot of at least the stdlib binding count. Without
                    // that floor, such a call would land a callee at
                    // base_slot 0 and this arm would silently freeze and
                    // publish the callee's locals as globals.
                    //
                    // The publish below is unconditional — there is no
                    // per-slot "already published" filter — so a slot that
                    // is stored more than once is frozen and published more
                    // than once. The contract readers rely on is therefore
                    // not at-most-once publication but publish-before-read:
                    // all of a slot's stores happen inside the single
                    // top-level statement that owns it, before any closure
                    // that could `PushGlobal` it exists (`PushGlobal` is
                    // emitted only for entry-frame slots referenced from
                    // nested functions), so every reader observes exactly
                    // one stable published value per slot. Two compiler
                    // facts uphold that. Sequential rebindings never
                    // re-store: `get_or_create_local` gives every
                    // module-scope binding — including a shadowing rebinding
                    // of an existing name — a fresh entry-frame slot rather
                    // than reusing the old one. And the slots that are
                    // stored repeatedly — binary-pattern cursor temps (one
                    // store per dynamic segment) and or-pattern alternative
                    // bindings (re-stored when a later alternative re-binds
                    // the same name) — finish all their stores while the
                    // owning statement is still executing. A re-store does
                    // re-freeze the slot's graph into the never-collected
                    // frozen area; that waste is bounded by the owning
                    // statement.
                    if base_slot == 0 && self.current_is_main {
                        if slot >= self.globals.len() {
                            self.globals.resize(slot + 1, Value::nil());
                        }
                        let frozen = freeze::freeze_global(
                            &mut self.heap,
                            &mut self.frozen,
                            self.stack[slot],
                        );
                        self.globals[slot] = frozen.value();
                        if let Some(rt) = &self.runtime {
                            rt.publish_global(slot, frozen);
                        }
                    }
                }
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
                    let v = *self.peek()?;
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
                        return Err(format!(
                            "Cannot negate non-numeric value '{}'",
                            value_type_name(&a)
                        ));
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
                Op::AddStr => {
                    // Size the concatenation while both operands are rooted.
                    let need = cost::str(self.peek_str_len(1) + self.peek_str_len(0));
                    self.ensure(need);
                    let b = self.pop()?;
                    let a = self.pop()?;
                    match (a.as_str(), b.as_str()) {
                        (Some(sa), Some(sb)) => {
                            let mut out = String::with_capacity(sa.len() + sb.len());
                            out.push_str(sa);
                            out.push_str(sb);
                            let v = Value::str_in(&mut self.heap, &out);
                            self.stack.push(v);
                        }
                        _ => {
                            debug_assert!(false, "AddStr on non-Str");
                            let v = Value::str_in(&mut self.heap, "");
                            self.stack.push(v);
                        }
                    }
                }

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
                    self.stack.push(Value::bool(!is_truthy(&a)));
                }
                Op::Jump => {
                    ip = instr.operand - code_start;
                }
                Op::JumpIfFalse => {
                    let cond = self.pop()?;
                    if !is_truthy(&cond) {
                        ip = instr.operand - code_start;
                    }
                }
                Op::JumpIfTrue => {
                    let cond = self.pop()?;
                    if is_truthy(&cond) {
                        ip = instr.operand - code_start;
                    }
                }

                // ---- Superinstructions --------------------------------------
                // Peephole-fused (PushLocal a; PushConst b; <op>) sequences;
                // operands packed into Instruction.{a,b}. Zero stack traffic
                // for the compare+branch forms.
                Op::AddIntLC => lc_arith!(|a, b| a.wrapping_add(b)),
                Op::SubIntLC => lc_arith!(|a, b| a.wrapping_sub(b)),
                Op::JumpGeIntLC => lc_jump!(|a, b| a >= b),
                Op::JumpNeIntLC => lc_jump!(|a, b| a != b),
                Op::Nop => {}

                Op::Call | Op::TailCall => {
                    let arity = instr.operand;
                    let callee = self.pop()?;

                    let Some(cl) = callee.as_closure() else {
                        return Err("Cannot call non-function".to_string());
                    };
                    let cl_func_idx = cl.func_idx();
                    let func = &self.program.functions[cl_func_idx as usize];
                    let (func_arity, func_locals, func_code_start) =
                        (func.arity, func.locals, func.code_start);

                    if arity != func_arity {
                        return Err(format!("Expected {} arguments, got {}", func_arity, arity));
                    }

                    let args_start = self.stack.len() - arity as usize;

                    // The callee value itself becomes the frame's `captures`
                    // handle — one word copied, no captures clone.
                    if instr.op == Op::TailCall {
                        self.collapse_tail_frame(base_slot, args_start);
                        let f = self.frame_mut();
                        f.func_idx = cl_func_idx;
                        f.code_start = func_code_start;
                        f.ip = 0;
                        f.captures = callee;
                    } else {
                        self.frame_mut().ip = ip;
                        self.frames.push(CallFrame {
                            func_idx: cl_func_idx,
                            code_start: func_code_start,
                            ip: 0,
                            base_slot: args_start,
                            captures: callee,
                        });
                        base_slot = args_start;
                    }

                    for _ in arity..func_locals {
                        self.stack.push(Value::small_int(0));
                    }

                    ip = 0;
                    code_start = func_code_start;
                    func_idx = cl_func_idx;

                    // One function application = one reduction, plus whatever
                    // collection work accrued since the last checkpoint (GC
                    // charges reductions so GC-heavy processes yield fairly).
                    // The debt is zero on every call that did not collect, so
                    // it is paid behind a test: draining unconditionally puts
                    // a store and saturating-sub chain on every call, which
                    // measured ~1.5x on a tail-recursive loop. `reds` is at
                    // least 1 here (a non-positive value yielded at the last
                    // checkpoint), so the plain decrement cannot wrap.
                    // The callee frame is fully consistent here, so preemption
                    // is a plain return — resume re-hoists from the frame.
                    reds -= 1;
                    if self.gc.pending_reds != 0 {
                        reds = reds.saturating_sub(self.take_gc_reds());
                    }
                    if reds <= 0 {
                        return Ok(Step::Yield);
                    }
                }
                Op::CallSelf | Op::TailCallSelf => {
                    // Self-recursion fast path: callee is the live frame's
                    // function, so we skip the closure pop, the `Value::Closure`
                    // tag match, and the arity check (statically guaranteed by
                    // `compile_call`). `func_idx`/`code_start` are already the
                    // target's, so the only frame work is slot bookkeeping.
                    let arity = instr.operand;
                    let func = &self.program.functions[func_idx as usize];
                    let func_locals = func.locals;
                    let args_start = self.stack.len() - arity as usize;

                    if instr.op == Op::TailCallSelf {
                        // Reuse the frame in place. Captures are already the
                        // current frame's — self-recursion cannot change the
                        // closed-over environment — so no `captures.clone()`.
                        self.collapse_tail_frame(base_slot, args_start);
                        let f = self.frame_mut();
                        f.ip = 0;
                    } else {
                        // Push a child frame. A self-call from inside a
                        // capture-carrying closure must see the same captures,
                        // so the child shares the current frame's closure
                        // handle (a one-word `Value` copy).
                        let captures = self.frame().captures;
                        self.frame_mut().ip = ip;
                        self.frames.push(CallFrame {
                            func_idx,
                            code_start,
                            ip: 0,
                            base_slot: args_start,
                            captures,
                        });
                        base_slot = args_start;
                    }

                    for _ in arity..func_locals {
                        self.stack.push(Value::small_int(0));
                    }
                    ip = 0;

                    // Same checkpoint shape as `Call`: one reduction, GC debt
                    // drained only when a collection actually charged some.
                    reds -= 1;
                    if self.gc.pending_reds != 0 {
                        reds = reds.saturating_sub(self.take_gc_reds());
                    }
                    if reds <= 0 {
                        return Ok(Step::Yield);
                    }
                }
                Op::Ret => {
                    let ret_val = self.pop()?;
                    let Some(old_frame) = self.frames.pop() else {
                        return Err("internal error: return with no active call frame".to_string());
                    };

                    self.stack.truncate(old_frame.base_slot);

                    self.stack.push(ret_val);

                    match self.frames.last() {
                        None => break,
                        Some(f) => {
                            ip = f.ip;
                            code_start = f.code_start;
                            base_slot = f.base_slot;
                            func_idx = f.func_idx;
                        }
                    }
                }
                // Aggregate values (arrays, tuples, ranges, field access):
                // one method per op (see `vm::collections`).
                Op::MakeArray => self.make_array(instr.operand)?,
                Op::MakeTuple => self.make_tuple(instr.operand)?,
                Op::TupleIndex => self.tuple_index(instr.operand)?,
                Op::MakeRange => self.make_range()?,
                Op::Index => self.seq_index()?,
                Op::IndexOrElse => self.seq_index_or_else(instr.operand, &mut ip, code_start)?,
                Op::ElemAt => self.elem_at(instr.operand)?,
                Op::ArrayLen => self.seq_len()?,
                Op::ArraySlice => self.seq_slice()?,
                Op::ArrayConcat => self.seq_concat()?,
                Op::Prepend => self.seq_prepend(instr.operand)?,
                Op::Drop => self.seq_drop()?,
                Op::Append => self.seq_append(instr.operand)?,
                Op::GetField => self.get_field(instr.operand)?,
                Op::MakeClosure => {
                    let func_idx = instr.operand;
                    let cc = self.program.functions[func_idx as usize].capture_count as usize;
                    self.ensure(cost::closure(cc));
                    // The one place captures are materialized: the closure
                    // object holds them inline; later invocations copy the
                    // one-word handle.
                    let captures = self.pop_n(cc)?;
                    let v = Value::closure_in(&mut self.heap, func_idx, &captures);
                    self.stack.push(v);
                }
                Op::PushCapture => {
                    let capture_idx = instr.operand as usize;
                    let v = match self
                        .frame()
                        .captures
                        .as_closure()
                        .and_then(|cl| cl.captures().get(capture_idx).copied())
                    {
                        Some(v) => v,
                        None => {
                            return Err(format!("Capture index out of bounds: {}", capture_idx));
                        }
                    };
                    self.stack.push(v);
                }
                Op::PushSelf => {
                    // The frame's `captures` handle IS the closure being
                    // executed, so pushing self is a one-word clone — no
                    // rebuild, no cache.
                    let val = self.frame().captures;
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
                Op::MakeEnumPayload => {
                    let payload_count = instr.b as usize;
                    // Names and labels are constant-pool references; only the
                    // enum cell and its payload slots are fresh.
                    self.ensure(cost::enum_(payload_count));
                    let payloads = self.pop_n(payload_count)?;

                    let labels_val = self.pop()?;
                    let variant_name_val = self.pop()?;
                    let enum_name_val = self.pop()?;
                    let type_id_val = self.pop()?;

                    let Some(type_id) = type_id_val.as_int() else {
                        return Err("Enum type id must be int".to_string());
                    };
                    let type_id = type_id as i32;

                    if enum_name_val.as_str().is_none() {
                        return Err("Enum name must be string".to_string());
                    }

                    // The field-label array is a per-ctor-site pooled constant
                    // (`emit_construct_header` → a frozen labels array), and
                    // `PushConst` copies the constant word, so the popped
                    // value is pointer-identical to the pool entry and stable
                    // for the program's lifetime. Freeze the labels Tuple
                    // once per site, memoized by that address; later
                    // constructions reuse the one frozen tuple.
                    let labels_key = match (labels_val.as_array(), labels_val.object_addr()) {
                        (Some(_), Some(addr)) => addr,
                        _ => return Err("Field labels must be an array".to_string()),
                    };
                    let field_labels = match self.label_cache.get(&labels_key) {
                        Some(cached) => *cached,
                        None => {
                            // The label strings are frozen constants; build
                            // the canonical labels Tuple in the frozen area
                            // once per ctor site, shared by every instance.
                            let labels = expect_string_array(&labels_val)?;
                            let tuple = self.frozen.tuple(labels);
                            self.label_cache.insert(labels_key, tuple);
                            tuple
                        }
                    };

                    if variant_name_val.as_str().is_none() {
                        return Err("Variant name must be string".to_string());
                    }

                    // `instr.operand` indexes the constant pool at the
                    // compile-time `enum_name_prefix_hash(enum_name,
                    // variant_name)`. Folding only the payload hashes here is
                    // bit-identical to hashing the name bytes then the
                    // payloads in one pass, without re-walking the static
                    // name bytes. The prehash constant is read by reference —
                    // unlike the header it is never `PushConst`-ed, so there is
                    // no extra opcode dispatch, no stack push/pop, and no
                    // per-construction refcount churn on the path.
                    let name_prefix_hash =
                        self.program.constants[instr.operand as usize].as_int_typed() as u64;
                    let hash = enum_hash_with_payload(name_prefix_hash, &payloads);
                    // `enum_name`/`variant_name` are frozen constant-pool
                    // `Str` values — stored as single reference words, never
                    // copied per construction.
                    let v = Value::enum_in(
                        &mut self.heap,
                        type_id,
                        hash,
                        enum_name_val,
                        variant_name_val,
                        field_labels,
                        &payloads,
                    );
                    self.stack.push(v);
                }
                Op::MatchEnum => {
                    let variant_name = self.pop()?;
                    let type_id_val = self.pop()?;
                    let val = self.pop()?;

                    let Some(variant_str) = variant_name.as_str() else {
                        return Err("Enum/variant names must be strings".to_string());
                    };
                    let Some(type_id) = type_id_val.as_int() else {
                        return Err("Enum type id must be int".to_string());
                    };
                    let type_id = type_id as i32;

                    if let Some(ev) = val.as_enum() {
                        self.stack.push(Value::bool(
                            ev.type_id() == type_id && ev.variant_name() == variant_str,
                        ));
                    } else {
                        self.stack.push(Value::bool(false));
                    }
                }
                Op::UnwrapEnum => {
                    let enum_val = self.pop()?;
                    if let Some(ev) = enum_val.as_enum() {
                        for p in ev.payload() {
                            self.stack.push(*p);
                        }
                    } else {
                        return Err("Cannot unwrap non-enum value".to_string());
                    }
                }
                Op::ToString => {
                    let val = self.pop()?;
                    // An already-Str operand is its own string image: push the
                    // same value word back instead of round-tripping through
                    // inspect() plus a fresh arena Str allocation.
                    if val.as_str().is_some() {
                        self.stack.push(val);
                    } else {
                        // Variable-size op: format into host memory first (a
                        // Rust String is not a heap value), which fixes the
                        // real need; `val` is never read past this point, so
                        // nothing unrooted is consulted across the safepoint.
                        let s = inspect(&val, &self.program);
                        self.ensure(cost::str(s.len()));
                        let v = Value::str_in(&mut self.heap, &s);
                        self.stack.push(v);
                    }
                }
                Op::StrConcatN => {
                    let n = instr.operand as usize;
                    let len = self.stack.len();
                    if n > len {
                        return Err("Stack underflow. This is likely a compiler bug.".to_string());
                    }
                    let base = len - n;
                    // The real need is the sum of the operand lengths, summed
                    // while all n operands are still rooted on the stack.
                    let mut total = 0usize;
                    for v in &self.stack[base..] {
                        match v.as_str() {
                            Some(s) => total += s.len(),
                            None => return Err("str_concat requires strings".to_string()),
                        }
                    }
                    self.ensure(cost::str(total));
                    let mut out = String::with_capacity(total);
                    for v in self.stack.drain(base..) {
                        if let Some(s) = v.as_str() {
                            out.push_str(s);
                        }
                    }
                    let v = Value::str_in(&mut self.heap, &out);
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
                Op::FileRead => {
                    if let Some(step) = self.file_read(ip, &mut reds)? {
                        return Ok(step);
                    }
                }
                Op::FileWrite => {
                    if let Some(step) = self.file_write(ip, &mut reds)? {
                        return Ok(step);
                    }
                }
                Op::TcpListen => self.tcp_listen()?,
                Op::TcpAccept => {
                    if let Some(step) = self.tcp_accept(ip, &mut reds)? {
                        return Ok(step);
                    }
                }
                Op::TcpConnect => {
                    if let Some(step) = self.tcp_connect(ip, &mut reds)? {
                        return Ok(step);
                    }
                }
                Op::TcpRead => {
                    if let Some(step) = self.tcp_read(ip, &mut reds)? {
                        return Ok(step);
                    }
                }
                Op::TcpReadUntil => {
                    if let Some(step) = self.tcp_read_until(ip, &mut reds)? {
                        return Ok(step);
                    }
                }
                Op::TcpWrite => {
                    if let Some(step) = self.tcp_write(ip, &mut reds)? {
                        return Ok(step);
                    }
                }
                Op::TcpWriteParts => {
                    if let Some(step) = self.tcp_write_parts(ip, &mut reds)? {
                        return Ok(step);
                    }
                }
                Op::TcpClose => self.tcp_close(&mut reds)?,
                Op::TcpCloseServer => self.tcp_close_server()?,
                Op::TcpLocalAddr => self.tcp_local_addr()?,
                Op::DnsResolve => {
                    if let Some(step) = self.dns_resolve(ip, &mut reds)? {
                        return Ok(step);
                    }
                }
                Op::ProcessSpawn => self.process_spawn(&mut reds)?,
                Op::Sleep => {
                    if let Some(step) = self.sleep(ip)? {
                        return Ok(step);
                    }
                }
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
                    self.ensure(cost::str(24));
                    let f = self.pop_float("float.to_string")?;
                    let s = f64_str(f);
                    let v = Value::str_in(&mut self.heap, &s);
                    self.stack.push(v);
                }
            }
        }

        // Halt, Ret with no caller, or running off the end of the code: the
        // current process is finished and its result is its top-of-stack.
        Ok(Step::Done)
    }

    /// Collapse the active frame for a tail call: drop the slots in
    /// `[base, args_start)` and slide the freshly-pushed argument words sitting
    /// at `[args_start, len)` down to `base`. Behaviourally identical to
    /// `self.stack.drain(base..args_start)`.
    ///
    /// `Vec::drain`'s tail-shift lowers to a `memmove` libcall, and a tail call
    /// moves only `arity` words (≈ 1–4), so the call setup dwarfs the copy — it
    /// was ~9% of `bench_heavy.al`'s pure-recursion time. Moving the words by
    /// hand keeps the shift inline. The destination starts at or below the
    /// source (`base <= args_start`), so even when the argument block overlaps
    /// its destination an ascending copy reads each shared slot before it is
    /// overwritten — exactly the shift `drain` performs.
    #[inline]
    fn collapse_tail_frame(&mut self, base: usize, args_start: usize) {
        let len = self.stack.len();
        debug_assert!(base <= args_start && args_start <= len);
        let n_args = len - args_start;
        // SAFETY: both ranges lie within the live stack. Each discarded slot in
        // `[base, args_start)` is dropped exactly once; the argument words are
        // then bit-copied down over those already-dropped slots (so they are
        // not re-dropped), and `set_len` shrinks the logical length so the
        // moved-from tail is never read or dropped again.
        unsafe {
            let p = self.stack.as_mut_ptr();
            for i in base..args_start {
                std::ptr::drop_in_place(p.add(i));
            }
            for k in 0..n_args {
                let v = p.add(args_start + k).read();
                p.add(base + k).write(v);
            }
            self.stack.set_len(base + n_args);
        }
    }

    pub(super) fn pop(&mut self) -> VmResult<Value> {
        self.stack
            .pop()
            .ok_or_else(|| "Stack underflow. This is likely a compiler bug.".to_string())
    }

    pub(super) fn pop_n(&mut self, n: usize) -> VmResult<Vec<Value>> {
        let len = self.stack.len();
        if n > len {
            return Err("Stack underflow. This is likely a compiler bug.".to_string());
        }
        Ok(self.stack.split_off(len - n))
    }

    fn peek(&self) -> VmResult<&Value> {
        self.stack
            .last()
            .ok_or_else(|| "Stack underflow. This is likely a compiler bug.".to_string())
    }

    // --- Typed pop helpers (stdlib / I/O ops only — not the arithmetic loop) --
    //
    // The popped `Value` is returned whole (a word); callers borrow the
    // arena contents through it (`str_ref`/`bin_ref`). Holding such a
    // borrow across `ensure` would be a rooting-rule bug — every opcode
    // ensures BEFORE popping, and allocation after `ensure` never collects,
    // so borrows held across `*_in` constructors are sound.

    #[inline]
    pub(super) fn pop_str(&mut self, op: &str) -> VmResult<Value> {
        let v = self.pop()?;
        if v.as_str().is_some() {
            Ok(v)
        } else {
            Err(format!("{op} requires a String"))
        }
    }

    #[inline]
    pub(super) fn pop_int(&mut self, op: &str) -> VmResult<i64> {
        match self.pop()?.as_int() {
            Some(n) => Ok(n),
            None => Err(format!("{op} requires an Int")),
        }
    }

    /// Pop a numeric value as `f64`. Accepts Int too (coerced), mirroring the
    /// numeric tolerance of the arithmetic loop (`Op::Neg` etc.).
    #[inline]
    fn pop_float(&mut self, op: &str) -> VmResult<f64> {
        let v = self.pop()?;
        if let Some(f) = v.as_float() {
            Ok(f)
        } else if let Some(n) = v.as_int() {
            Ok(n as f64)
        } else {
            Err(format!("{op} requires a Float"))
        }
    }

    #[inline]
    pub(super) fn pop_binary(&mut self, op: &str) -> VmResult<Value> {
        let v = self.pop()?;
        if v.as_binary().is_some() {
            Ok(v)
        } else {
            Err(format!("{op} requires a Binary"))
        }
    }

    // --- Prelude value constructors ------------------------------------------
    //
    // `make_nil`/`make_none` copy prebuilt frozen-area values and allocate
    // nothing. The payload-carrying constructors build one fresh wrapper
    // enum in the current process arena, against the caller's ensured
    // budget (`cost::WRAP`).

    #[inline]
    pub(super) fn make_nil(&self) -> Value {
        self.templates.nil
    }

    #[inline]
    pub(super) fn make_none(&self) -> Value {
        self.templates.none
    }

    #[inline]
    pub(super) fn make_some(&mut self, v: Value) -> Value {
        let t = self.templates.some;
        t.instantiate(&mut self.heap, &[v])
    }

    #[inline]
    pub(super) fn make_ok(&mut self, v: Value) -> Value {
        let t = self.templates.ok;
        t.instantiate(&mut self.heap, &[v])
    }

    #[inline]
    pub(super) fn make_err(&mut self, v: Value) -> Value {
        let t = self.templates.err;
        t.instantiate(&mut self.heap, &[v])
    }

    /// The frozen [`EnumTemplate`] for a stdlib variant, built on first use
    /// (interned names go into the program's frozen area) and memoized by
    /// template identity, so runtime error construction allocates only the
    /// enum cell itself — never names — exactly as `cost::NET_ERR`/
    /// `cost::io_err` budget.
    pub(super) fn stdlib_template(&mut self, t: &'static VariantTemplate) -> EnumTemplate {
        let key = t as *const VariantTemplate as usize;
        if let Some(tpl) = self.template_cache.get(&key) {
            return *tpl;
        }
        let tpl = enum_template(&mut self.frozen, t);
        self.template_cache.insert(key, tpl);
        tpl
    }

    /// A nullary stdlib enum instance in the current process arena. The
    /// caller has ensured `cost::enum_(0)`.
    pub(super) fn stdlib_enum(&mut self, t: &'static VariantTemplate) -> Value {
        let tpl = self.stdlib_template(t);
        tpl.instantiate(&mut self.heap, &[])
    }

    /// `+` — the one arithmetic op with a non-numeric case: Str + Str
    /// concatenation, whose result is sized and budgeted while both operands
    /// are still rooted on the stack (the rooting rule). Numeric pairs fall
    /// through to [`arith`](Self::arith).
    pub(super) fn add(&mut self) -> VmResult<()> {
        let need = match (
            self.peek_at(1).and_then(|v| v.as_str()),
            self.peek_at(0).and_then(|v| v.as_str()),
        ) {
            (Some(a), Some(b)) => cost::str(a.len() + b.len()),
            _ => 0,
        };
        if need > 0 {
            self.ensure(need);
        }
        let b = self.pop()?;
        let a = self.pop()?;
        if let (Some(sa), Some(sb)) = (a.as_str(), b.as_str()) {
            let mut out = String::with_capacity(sa.len() + sb.len());
            out.push_str(sa);
            out.push_str(sb);
            let v = Value::str_in(&mut self.heap, &out);
            self.stack.push(v);
            return Ok(());
        }
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
    /// pairs use `int_f`, float (or mixed) pairs use `float_f` after int
    /// promotion, anything else is a compiler-bug error. Arithmetic is
    /// TOTAL — every numeric case yields a value: integer overflow wraps
    /// with two's-complement semantics (the spill into a boxed big int
    /// budgets itself in `boxed_int`; the operands are immediates, so
    /// nothing unrooted is held across that safepoint), and `Value::float`
    /// collapses any non-finite float result (overflow to ±Inf, `0.0/0.0`
    /// NaN) to `0.0`.
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
            (ValueView::Int(ai), ValueView::Float(bf)) => Value::float(float_f(ai as f64, bf)),
            (ValueView::Float(af), ValueView::Int(bi)) => Value::float(float_f(af, bi as f64)),
            _ => {
                return Err(format!(
                    "Cannot perform arithmetic on '{}' and '{}'. This is likely a compiler bug.",
                    value_type_name(&a),
                    value_type_name(&b)
                ));
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

    /// An integer result as a `Value`, ensuring + boxing the rare big-int
    /// spill itself. ONLY for standalone integer results: `ensure` replaces
    /// the opcode budget, so this must not run between an opcode's own
    /// `ensure` and its allocations (use `int_value` under an arm budget
    /// that includes `cost::BIG_INT` instead).
    #[inline]
    pub(super) fn boxed_int(&mut self, i: i64) -> Value {
        if Value::fits_small_int(i) {
            Value::small_int(i)
        } else {
            self.spill_int(i)
        }
    }

    /// The out-of-range half of [`boxed_int`](Self::boxed_int). Kept out of
    /// line so the safepoint + allocation never inline into the integer
    /// arithmetic arms — the dispatch loop's hottest code — where they cost
    /// registers and i-cache on every op for a case that almost never runs.
    #[cold]
    #[inline(never)]
    fn spill_int(&mut self, i: i64) -> Value {
        self.ensure(cost::BIG_INT);
        Value::int_in(&mut self.heap, i)
    }

    /// An integer result under the CURRENT opcode budget (which includes
    /// `cost::BIG_INT` when the value may spill). Never collects.
    #[inline]
    pub(super) fn int_value(&mut self, i: i64) -> Value {
        if Value::fits_small_int(i) {
            Value::small_int(i)
        } else {
            Value::int_in(&mut self.heap, i)
        }
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
                    return Err("Field label must be string".to_string());
                }
                out.push(it);
            }
            Ok(out)
        }
        None => Err("Field labels must be an array".to_string()),
    }
}

#[inline]
fn slices_equal(a: &[Value], b: &[Value]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| values_equal(x, y))
}

pub(super) fn values_equal(a: &Value, b: &Value) -> bool {
    match (a.kind(), b.kind()) {
        (ValueView::Int(x), ValueView::Int(y)) => x == y,
        (ValueView::Float(x), ValueView::Float(y)) => x == y,
        (ValueView::Bool(x), ValueView::Bool(y)) => x == y,
        (ValueView::Str(x), ValueView::Str(y)) => x == y,
        (ValueView::Enum(ae), ValueView::Enum(be)) => {
            ae.hash() == be.hash()
                && ae.type_id() == be.type_id()
                && ae.variant_name() == be.variant_name()
                && slices_equal(ae.payload(), be.payload())
        }
        (ValueView::Nil, ValueView::Nil) => true,
        (ValueView::Closure(x), ValueView::Closure(y)) => {
            x.func_idx() == y.func_idx() && slices_equal(x.captures(), y.captures())
        }
        (ValueView::Array(aa), ValueView::Array(ba)) => {
            aa.len() == ba.len() && aa.iter().zip(ba.iter()).all(|(x, y)| values_equal(&x, &y))
        }
        (ValueView::Range(as_, ae), ValueView::Range(bs, be)) => {
            // Normalised: empty ranges compare equal regardless of endpoints.
            let alen = range_len(as_, ae);
            let blen = range_len(bs, be);
            (alen == 0 && blen == 0) || (as_ == bs && ae == be)
        }
        (ValueView::Range(s, e), ValueView::Array(arr))
        | (ValueView::Array(arr), ValueView::Range(s, e)) => {
            let len = range_len(s, e) as usize;
            if arr.len() != len {
                return false;
            }
            for (i, av) in arr.iter().enumerate() {
                let n = match av.as_int() {
                    Some(n) => n,
                    None => return false,
                };
                if n != s + i as i64 {
                    return false;
                }
            }
            true
        }
        (ValueView::Binary(a), ValueView::Binary(b)) => a.bits_eq(&b),
        (ValueView::Tuple(at), ValueView::Tuple(bt)) => slices_equal(at, bt),
        (ValueView::Socket(asv), ValueView::Socket(bsv)) => {
            asv.id == bsv.id && asv.is_listener == bsv.is_listener
        }
        _ => false,
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
        _ => Err(format!(
            "Cannot compare '{}' with '{}'. This is likely a compiler bug.",
            value_type_name(&a),
            value_type_name(&b)
        )),
    }
}

fn is_truthy(v: &Value) -> bool {
    v.as_bool().unwrap_or(false)
}
