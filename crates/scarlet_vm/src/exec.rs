//! The interpreter: runs loaded instructions.
//!
//! Frames live on a stack the VM owns, not on Rust's, so a deep recursion in
//! a Scarlet program uses memory and never overflows anything. All frames'
//! registers sit in one vector: a frame is a function, where it is in that
//! function, and where its registers start.
//!
//! Reference counting follows one rule: every register, global and argument
//! holds one reference to its value. So copying a value into a register adds
//! a reference, and overwriting a register, or leaving a frame, gives one up.
//! Perceus's `Drop` gives one up early. This alone keeps every count right,
//! whatever Perceus did or did not insert (`docs/vm-design.md`, "Memory").

use std::io::Write;

use scarlet_ir::core_ir::{FuncIdx, VariantRef};
use scarlet_ir::intrinsic::Intrinsic;

use crate::Stop;
use crate::array::{self, End, Seq};
use crate::bigint::{self, Int};
use crate::code::{Body, Code, Func, Instr, Reg};
use crate::eq;
use crate::heap::{Full, Heap, Kind};
use crate::show;
use crate::value::{Value, View};

struct Frame<'c> {
    body: &'c Body,
    pc: usize,
    base: usize,
    /// The caller's register the result goes to. `None` for the frame a run
    /// started with.
    ret_to: Option<Reg>,
    /// The closure this frame runs as, holding one reference: a closure cell,
    /// a function value, or `NIL` for a call by name. Its captures are what
    /// `GetCapture` reads.
    env: Value,
}

pub(crate) struct Machine<'c, 'o> {
    code: &'c Code,
    out: &'o mut dyn Write,
    heap: Heap,
    globals: Vec<Value>,
    regs: Vec<Value>,
}

impl<'c, 'o> Machine<'c, 'o> {
    pub(crate) fn new(code: &'c Code, out: &'o mut dyn Write) -> Self {
        Machine {
            code,
            out,
            heap: Heap::default(),
            globals: vec![Value::NIL; code.globals as usize],
            regs: Vec::new(),
        }
    }

    /// Run every module's toplevel in order, then `main` if the program has
    /// one. The result is the last thing run's value: `main`'s, or the entry
    /// toplevel's for a script. The caller holds its reference.
    pub(crate) fn run(&mut self) -> Result<Value, Stop> {
        let mut last = Value::NIL;
        for top in &self.code.toplevels {
            let v = self.call(ready(top)?, &[])?;
            self.release(last);
            last = v;
        }
        if let Some(main) = self.code.main {
            let v = self.call(self.body(main)?, &[])?;
            self.release(last);
            last = v;
        }
        Ok(last)
    }

    fn body(&self, f: FuncIdx) -> Result<&'c Body, Stop> {
        match self.code.fns.get(f) {
            Some(func) => ready(func),
            None => Err(Stop::NotBuiltYet(format!(
                "a call to {f}, which the program does not have"
            ))),
        }
    }

    /// Call `body` with `args` and run until it returns. The arguments'
    /// references pass to the callee; the result's passes to the caller.
    fn call(&mut self, body: &'c Body, args: &[Value]) -> Result<Value, Stop> {
        let base = self.regs.len();
        self.enter(body, base, args);
        let mut frames = vec![Frame {
            body,
            pc: 0,
            base,
            ret_to: None,
            env: Value::NIL,
        }];
        loop {
            let Some(frame) = frames.last_mut() else {
                return Ok(Value::NIL);
            };
            let Some(instr) = frame.body.instrs.get(frame.pc) else {
                return Err(Stop::BadProgram(format!(
                    "{} ran past its last instruction",
                    frame.body.name
                )));
            };
            frame.pc += 1;
            let base = frame.base;
            match instr {
                Instr::Const { dst, value } => self.set(base, *dst, *value),
                Instr::Str { dst, text } => {
                    let cell = self.heap.string(text).map_err(full)?;
                    self.set(base, *dst, Value::cell(cell));
                }
                Instr::BigInt { dst, n } => {
                    let v = bigint::value(&mut self.heap, (*n).into()).map_err(full)?;
                    self.set(base, *dst, v);
                }
                Instr::Move { dst, src } => {
                    let v = self.share(self.get(base, *src));
                    self.set(base, *dst, v);
                }
                Instr::GetGlobal { dst, slot } => {
                    let v = self
                        .globals
                        .get(slot.0 as usize)
                        .copied()
                        .unwrap_or(Value::NIL);
                    let v = self.share(v);
                    self.set(base, *dst, v);
                }
                Instr::SetGlobal { slot, src } => {
                    let v = self.share(self.get(base, *src));
                    if let Some(g) = self.globals.get_mut(slot.0 as usize) {
                        let old = std::mem::replace(g, v);
                        self.release(old);
                    }
                }
                Instr::Int { dst, op, a, b } => {
                    let (a, b) = (self.int(base, *a)?, self.int(base, *b)?);
                    let v = bigint::op(&mut self.heap, *op, a, b).map_err(full)?;
                    self.set(base, *dst, v);
                }
                Instr::IntNeg { dst, a } => {
                    let a = self.int(base, *a)?;
                    let v = bigint::neg(&mut self.heap, a).map_err(full)?;
                    self.set(base, *dst, v);
                }
                Instr::Println { dst, arg } => {
                    let mut text = Vec::new();
                    self.show(self.get(base, *arg), &mut text)?;
                    text.push(b'\n');
                    if self.out.write_all(&text).is_err() {
                        return Err(Stop::OutputClosed);
                    }
                    self.set(base, *dst, Value::NIL);
                }
                Instr::ToString { dst, a } => {
                    let v = self.get(base, *a);
                    let s = if self.is_string(v) {
                        self.share(v)
                    } else {
                        let mut text = Vec::new();
                        self.show(v, &mut text)?;
                        Value::cell(self.heap.string(&text).map_err(full)?)
                    };
                    self.set(base, *dst, s);
                }
                Instr::Concat { dst, parts } => {
                    let mut text = Vec::new();
                    for part in parts {
                        let v = self.get(base, *part);
                        match v.as_cell() {
                            Some(cell) if self.is_string(v) => {
                                self.heap.read_string(cell, &mut text);
                            }
                            _ => return Err(Stop::NotBuiltYet(format!("joining {v:?}"))),
                        }
                    }
                    let s = Value::cell(self.heap.string(&text).map_err(full)?);
                    self.set(base, *dst, s);
                }
                Instr::Ctor {
                    dst,
                    variant,
                    fields,
                } => {
                    let values: Vec<Value> = fields
                        .iter()
                        .map(|r| self.share(self.get(base, *r)))
                        .collect();
                    let cell = self.heap.ctor(*variant, &values).map_err(full)?;
                    self.set(base, *dst, Value::cell(cell));
                }
                Instr::Drop { reg } => self.set(base, *reg, Value::NIL),
                Instr::Call { dst, func, args } => {
                    let callee = self.body(*func)?;
                    let values: Vec<Value> = args
                        .iter()
                        .map(|r| self.share(self.get(base, *r)))
                        .collect();
                    let new_base = base + frame.body.regs as usize;
                    self.enter(callee, new_base, &values);
                    frames.push(Frame {
                        body: callee,
                        pc: 0,
                        base: new_base,
                        ret_to: Some(*dst),
                        env: Value::NIL,
                    });
                }
                Instr::CallSelf { dst, args } => {
                    let values = self.args(base, args);
                    let body = frame.body;
                    let env = self.share(frame.env);
                    let new_base = base + body.regs as usize;
                    self.enter(body, new_base, &values);
                    frames.push(Frame {
                        body,
                        pc: 0,
                        base: new_base,
                        ret_to: Some(*dst),
                        env,
                    });
                }
                Instr::CallValue { dst, callee, args } => {
                    let (body, env) = self.callee(self.get(base, *callee), args.len())?;
                    let values = self.args(base, args);
                    let new_base = base + frame.body.regs as usize;
                    self.enter(body, new_base, &values);
                    frames.push(Frame {
                        body,
                        pc: 0,
                        base: new_base,
                        ret_to: Some(*dst),
                        env,
                    });
                }
                // A tail call's callee takes over this frame, so a loop
                // written as tail recursion runs in constant space.
                Instr::TailCall { func, args } => {
                    let callee = self.body(*func)?;
                    let values = self.args(base, args);
                    self.enter(callee, base, &values);
                    let old = std::mem::replace(&mut frame.env, Value::NIL);
                    self.release(old);
                    frame.body = callee;
                    frame.pc = 0;
                }
                Instr::TailCallSelf { args } => {
                    let values = self.args(base, args);
                    self.enter(frame.body, base, &values);
                    frame.pc = 0;
                }
                // The callee's closure gets its own reference before `enter`
                // releases this frame's registers, one of which may be the
                // only other holder.
                Instr::TailCallValue { callee, args } => {
                    let (body, env) = self.callee(self.get(base, *callee), args.len())?;
                    let values = self.args(base, args);
                    self.enter(body, base, &values);
                    let old = std::mem::replace(&mut frame.env, env);
                    self.release(old);
                    frame.body = body;
                    frame.pc = 0;
                }
                Instr::Closure {
                    dst,
                    func,
                    captures,
                } => {
                    let values = self.args(base, captures);
                    let cell = self.heap.closure(*func, &values).map_err(full)?;
                    self.set(base, *dst, Value::cell(cell));
                }
                Instr::GetCapture { dst, index } => {
                    let capture = match frame.env.as_cell() {
                        Some(cell) if self.heap.kind(cell) == Some(Kind::Closure) => {
                            self.heap.capture(cell, *index as usize)
                        }
                        _ => None,
                    };
                    let Some(v) = capture else {
                        return Err(Stop::BadProgram(format!(
                            "{} read capture {index}, which the closure it runs as does not have",
                            frame.body.name
                        )));
                    };
                    let v = self.share(v);
                    self.set(base, *dst, v);
                }
                Instr::GetSelf { dst, this } => {
                    let v = match frame.env.view() {
                        View::Nil => Value::func(*this),
                        View::Float(_)
                        | View::Int(_)
                        | View::Bool(_)
                        | View::Func(_)
                        | View::Cell(_)
                        | View::Nullary(_) => self.share(frame.env),
                    };
                    self.set(base, *dst, v);
                }
                Instr::Jump { to } => frame.pc = *to as usize,
                Instr::JumpUnlessVariant { src, variant, to } => {
                    if !self.is_variant(self.get(base, *src), *variant) {
                        frame.pc = *to as usize;
                    }
                }
                Instr::JumpUnlessInt { src, n, to } => {
                    if !self.is_int(self.get(base, *src), *n) {
                        frame.pc = *to as usize;
                    }
                }
                Instr::JumpUnlessStr { src, text, to } => {
                    let v = self.get(base, *src);
                    let is = match v.as_cell() {
                        Some(cell) if self.is_string(v) => self.heap.string_is(cell, text),
                        _ => false,
                    };
                    if !is {
                        frame.pc = *to as usize;
                    }
                }
                Instr::Field { dst, src, index } => {
                    let field = match self.get(base, *src).as_cell() {
                        Some(cell) if self.heap.kind(cell) == Some(Kind::Ctor) => {
                            self.heap.field(cell, usize::from(*index))
                        }
                        _ => None,
                    };
                    let Some(field) = field else {
                        return Err(Stop::BadProgram(format!(
                            "field {index} read from a value with no such field"
                        )));
                    };
                    let field = self.share(field);
                    self.set(base, *dst, field);
                }
                Instr::Tuple { dst, elements } => {
                    let values = self.args(base, elements);
                    let cell = self.heap.tuple(&values).map_err(full)?;
                    self.set(base, *dst, Value::cell(cell));
                }
                Instr::Element { dst, src, index } => {
                    let element = match self.get(base, *src).as_cell() {
                        Some(cell) if self.heap.kind(cell) == Some(Kind::Tuple) => {
                            self.heap.element(cell, usize::from(*index))
                        }
                        _ => None,
                    };
                    let Some(element) = element else {
                        return Err(Stop::BadProgram(format!(
                            "element {index} read from a value with no such element"
                        )));
                    };
                    let element = self.share(element);
                    self.set(base, *dst, element);
                }
                Instr::Array { dst, elements } => {
                    let values = self.args(base, elements);
                    let cell = array::from_values(&mut self.heap, &values).map_err(full)?;
                    self.set(base, *dst, Value::cell(cell));
                }
                Instr::Builtin {
                    dst,
                    intrinsic,
                    args,
                } => {
                    let args: Vec<Value> = args.iter().map(|r| self.get(base, *r)).collect();
                    let v = self.builtin(*intrinsic, &args)?;
                    self.set(base, *dst, v);
                }
                Instr::Equal { dst, a, b } => {
                    let same = eq::equal(&self.heap, self.get(base, *a), self.get(base, *b));
                    self.set(base, *dst, Value::bool(same));
                }
                Instr::NotEqual { dst, a, b } => {
                    let same = eq::equal(&self.heap, self.get(base, *a), self.get(base, *b));
                    self.set(base, *dst, Value::bool(!same));
                }
                Instr::Not { dst, a } => {
                    let b = match self.get(base, *a).view() {
                        View::Bool(b) => b,
                        v @ (View::Float(_)
                        | View::Int(_)
                        | View::Nil
                        | View::Func(_)
                        | View::Cell(_)
                        | View::Nullary(_)) => {
                            return Err(Stop::BadProgram(format!("`!` on {v:?}, not a Bool")));
                        }
                    };
                    self.set(base, *dst, Value::bool(!b));
                }
                Instr::Range { dst, start, end } => {
                    let start = self.range_end(self.get(base, *start))?;
                    let end = self.range_end(self.get(base, *end))?;
                    let cell = array::range(&mut self.heap, start, end).map_err(full)?;
                    self.set(base, *dst, Value::cell(cell));
                }
                Instr::ArrayLen { dst, src } => {
                    let n = self.seq(self.get(base, *src))?.len(&self.heap);
                    let v = bigint::value(&mut self.heap, n.into()).map_err(full)?;
                    self.set(base, *dst, v);
                }
                Instr::ArrayElem { dst, src, index } => {
                    let s = self.seq(self.get(base, *src))?;
                    let Some(v) = self.element(s, usize::from(*index))? else {
                        return Err(Stop::BadProgram(format!(
                            "element {index} read from an array with no such element"
                        )));
                    };
                    self.set(base, *dst, v);
                }
                Instr::ArrayDrop { dst, src, n } => {
                    let s = self.seq(self.get(base, *src))?;
                    let n = match self.get(base, *n).view() {
                        View::Int(n) if n >= 0 => n as usize,
                        v @ (View::Int(_)
                        | View::Float(_)
                        | View::Nil
                        | View::Bool(_)
                        | View::Func(_)
                        | View::Cell(_)
                        | View::Nullary(_)) => {
                            return Err(Stop::BadProgram(format!(
                                "an array's first {v:?} elements dropped"
                            )));
                        }
                    };
                    let cell = match s {
                        Seq::Tree(a) => array::skip(&mut self.heap, a, n),
                        Seq::Range { start, end } => {
                            let n = (n as u64).min(array::range_len(start, end));
                            array::range(&mut self.heap, start + n as i64, end)
                        }
                    };
                    self.set(base, *dst, Value::cell(cell.map_err(full)?));
                }
                Instr::ArrayPrepend { dst, front, rest } => {
                    let s = self.seq(self.get(base, *rest))?;
                    let a = self.tree(s)?;
                    let items = self.args(base, front);
                    let cell = self.push_all(a, items.into_iter().rev(), End::Front);
                    self.heap.release(a);
                    self.set(base, *dst, Value::cell(cell?));
                }
                Instr::ArrayAppend { dst, rest, back } => {
                    let s = self.seq(self.get(base, *rest))?;
                    let a = self.tree(s)?;
                    let items = self.args(base, back);
                    let cell = self.push_all(a, items.into_iter(), End::Back);
                    self.heap.release(a);
                    self.set(base, *dst, Value::cell(cell?));
                }
                Instr::ArrayConcat { dst, a, b } => {
                    let (a, b) = (self.seq(self.get(base, *a))?, self.seq(self.get(base, *b))?);
                    let (a, b) = (self.tree(a)?, self.tree(b)?);
                    let cell = array::concat(&mut self.heap, a, b).map_err(full);
                    self.heap.release(a);
                    self.heap.release(b);
                    self.set(base, *dst, Value::cell(cell?));
                }
                Instr::ArraySlice {
                    dst,
                    src,
                    start,
                    end,
                } => {
                    let s = self.seq(self.get(base, *src))?;
                    let bounds = (
                        self.position(self.get(base, *start))?,
                        self.position(self.get(base, *end))?,
                    );
                    let cut = match (s, bounds) {
                        (Seq::Tree(a), (Some(from), Some(to))) => {
                            array::slice(&mut self.heap, a, from, to).map_err(full)?
                        }
                        (Seq::Range { start, end }, (Some(from), Some(to)))
                            if from <= to && to as u64 <= array::range_len(start, end) =>
                        {
                            let (from, to) = (start + from as i64, start + to as i64);
                            Some(array::range(&mut self.heap, from, to).map_err(full)?)
                        }
                        (Seq::Tree(_) | Seq::Range { .. }, _) => None,
                    };
                    let v = match cut {
                        Some(cut) => self.heap.ctor(self.code.abi.ok, &[Value::cell(cut)]),
                        None => self.heap.ctor(self.code.abi.err, &[Value::NIL]),
                    };
                    let v = Value::cell(v.map_err(full)?);
                    self.set(base, *dst, v);
                }
                Instr::ArrayIndex { dst, src, index } => {
                    let found = self.index(self.get(base, *src), self.get(base, *index))?;
                    let v = match found {
                        Some(v) => {
                            let cell = self.heap.ctor(self.code.abi.some, &[v]).map_err(full)?;
                            Value::cell(cell)
                        }
                        None => Value::nullary(self.code.abi.none),
                    };
                    self.set(base, *dst, v);
                }
                Instr::ArrayIndexOr {
                    dst,
                    src,
                    index,
                    default,
                } => {
                    let found = self.index(self.get(base, *src), self.get(base, *index))?;
                    let v = match found {
                        Some(v) => v,
                        None => self.share(self.get(base, *default)),
                    };
                    self.set(base, *dst, v);
                }
                Instr::Bad { why } => return Err(Stop::BadProgram((*why).into())),
                Instr::JumpIfFalse { cond, to } => match self.get(base, *cond).view() {
                    View::Bool(true) => {}
                    View::Bool(false) => frame.pc = *to as usize,
                    v @ (View::Float(_)
                    | View::Int(_)
                    | View::Nil
                    | View::Func(_)
                    | View::Cell(_)
                    | View::Nullary(_)) => {
                        return Err(Stop::NotBuiltYet(format!("a branch on {v:?}")));
                    }
                },
                Instr::Ret { src } => {
                    let v = self.share(self.get(base, *src));
                    let ret_to = frame.ret_to;
                    let env = frame.env;
                    frames.pop();
                    self.leave(base);
                    self.release(env);
                    match (ret_to, frames.last()) {
                        (Some(dst), Some(caller)) => {
                            let caller_base = caller.base;
                            self.set(caller_base, dst, v);
                        }
                        _ => return Ok(v),
                    }
                }
            }
        }
    }

    /// What built-in `i` gives for `args`, which it only reads. The result
    /// holds its own reference.
    fn builtin(&mut self, i: Intrinsic, args: &[Value]) -> Result<Value, Stop> {
        let &[v] = args else {
            return Err(Stop::BadProgram(format!(
                "the built-in {i:?} called with {} arguments",
                args.len()
            )));
        };
        match i {
            Intrinsic::StringInspect => {
                let mut text = Vec::new();
                self.show(v, &mut text)?;
                Ok(Value::cell(self.heap.string(&text).map_err(full)?))
            }
            Intrinsic::StringLength => {
                let Some(cell) = v.as_cell().filter(|_| self.is_string(v)) else {
                    return Err(Stop::BadProgram(format!("`string.length` of {v:?}")));
                };
                let mut bytes = Vec::with_capacity(self.heap.string_len(cell));
                self.heap.read_string(cell, &mut bytes);
                let n = match std::str::from_utf8(&bytes) {
                    Ok(s) => s.chars().count(),
                    Err(_) => {
                        return Err(Stop::BadProgram("a string that is not UTF-8".into()));
                    }
                };
                bigint::value(&mut self.heap, n.into()).map_err(full)
            }
            Intrinsic::ArrayLength => {
                let n = self.seq(v)?.len(&self.heap);
                bigint::value(&mut self.heap, n.into()).map_err(full)
            }
            Intrinsic::IntToString => {
                let text = match self.int_of(v)? {
                    Int::Small(n) => n.to_string(),
                    Int::Big(n) => n.to_string(),
                };
                Ok(Value::cell(
                    self.heap.string(text.as_bytes()).map_err(full)?,
                ))
            }
            other => Err(Stop::NotBuiltYet(format!("the built-in {other:?}"))),
        }
    }

    /// The array `v` holds: a tree or a range.
    fn seq(&self, v: Value) -> Result<Seq, Stop> {
        v.as_cell()
            .and_then(|cell| array::seq(&self.heap, cell))
            .ok_or_else(|| {
                Stop::BadProgram(format!(
                    "an array operation on {v:?}, which is not an array"
                ))
            })
    }

    /// Element `i` of `s`, holding its own reference, or `None` past the end.
    /// A range's element is made from its start.
    fn element(&mut self, s: Seq, i: usize) -> Result<Option<Value>, Stop> {
        match s {
            Seq::Tree(a) => Ok(array::get(&self.heap, a, i).map(|v| self.share(v))),
            Seq::Range { start, end } if (i as u64) < array::range_len(start, end) => {
                let n = i128::from(start) + i as i128;
                Ok(Some(bigint::value(&mut self.heap, n.into()).map_err(full)?))
            }
            Seq::Range { .. } => Ok(None),
        }
    }

    /// `s` as a tree, holding one reference: the tree itself, or a range's
    /// elements built into one.
    fn tree(&mut self, s: Seq) -> Result<crate::heap::Cell, Stop> {
        match s {
            Seq::Tree(a) => {
                self.heap.share(a);
                Ok(a)
            }
            Seq::Range { start, end } => {
                let mut items = Vec::new();
                for n in start..end.max(start) {
                    items.push(bigint::value(&mut self.heap, n.into()).map_err(full)?);
                }
                array::from_values(&mut self.heap, &items).map_err(full)
            }
        }
    }

    /// Element `index` of the array `a`, holding its own reference, or `None`
    /// when it has none.
    fn index(&mut self, a: Value, index: Value) -> Result<Option<Value>, Stop> {
        let s = self.seq(a)?;
        match self.position(index)? {
            Some(i) => self.element(s, i),
            None => Ok(None),
        }
    }

    /// A range's end as an `i64`. A range past 64 bits could not be walked in
    /// any time a program has.
    fn range_end(&self, v: Value) -> Result<i64, Stop> {
        match v.view() {
            View::Int(n) => Ok(n),
            View::Cell(cell) if self.heap.kind(cell) == Some(Kind::BigInt) => {
                num_traits::ToPrimitive::to_i64(&self.heap.read_big_int(cell))
                    .ok_or_else(|| Stop::NotBuiltYet("a range with an end past 64 bits".into()))
            }
            v @ (View::Float(_)
            | View::Nil
            | View::Bool(_)
            | View::Func(_)
            | View::Cell(_)
            | View::Nullary(_)) => Err(Stop::BadProgram(format!(
                "a range whose end is {v:?}, not an Int"
            ))),
        }
    }

    /// The Int `v` as a position in an array, or `None` when it names none:
    /// negative, or a big int, which is past the end of any array.
    fn position(&self, v: Value) -> Result<Option<usize>, Stop> {
        match v.view() {
            View::Int(i) => Ok(usize::try_from(i).ok()),
            View::Cell(cell) if self.heap.kind(cell) == Some(Kind::BigInt) => Ok(None),
            v @ (View::Float(_)
            | View::Nil
            | View::Bool(_)
            | View::Func(_)
            | View::Cell(_)
            | View::Nullary(_)) => Err(Stop::BadProgram(format!(
                "an array position that is {v:?}, not an Int"
            ))),
        }
    }

    /// `a` with each of `items` pushed at `end`, in turn. Each item's
    /// reference passes to the result; `a` is borrowed.
    fn push_all(
        &mut self,
        a: crate::heap::Cell,
        items: impl Iterator<Item = Value>,
        end: End,
    ) -> Result<crate::heap::Cell, Stop> {
        let mut cur = a;
        self.heap.share(cur);
        for x in items {
            let next = array::push(&mut self.heap, cur, x, end).map_err(full)?;
            self.heap.release(cur);
            cur = next;
        }
        Ok(cur)
    }

    /// The values in `regs`, each with one more reference: for passing on.
    fn args(&mut self, base: usize, regs: &[Reg]) -> Vec<Value> {
        regs.iter()
            .map(|r| self.share(self.get(base, *r)))
            .collect()
    }

    /// The body function value `f` runs, and `f` with one more reference, for
    /// the frame that runs it as its closure.
    fn callee(&mut self, f: Value, argc: usize) -> Result<(&'c Body, Value), Stop> {
        let func = match f.view() {
            View::Func(func) => func,
            View::Cell(cell) if self.heap.kind(cell) == Some(Kind::Closure) => {
                self.heap.closure_func(cell)
            }
            v @ (View::Float(_)
            | View::Int(_)
            | View::Nil
            | View::Bool(_)
            | View::Cell(_)
            | View::Nullary(_)) => {
                return Err(Stop::BadProgram(format!(
                    "a call to {v:?}, which is not a function"
                )));
            }
        };
        let body = self.body(func)?;
        if body.params.len() != argc {
            return Err(Stop::BadProgram(format!(
                "{} called with {argc} arguments, not {}",
                body.name,
                body.params.len()
            )));
        }
        Ok((body, self.share(f)))
    }

    /// Give `body` a fresh frame at `base`, with `args` in its parameters.
    /// Whatever held registers from `base` on is released first.
    fn enter(&mut self, body: &Body, base: usize, args: &[Value]) {
        self.leave(base);
        self.regs.resize(base + body.regs as usize, Value::NIL);
        for (param, arg) in body.params.iter().zip(args) {
            self.set(base, *param, *arg);
        }
    }

    /// Release every register from `base` on, and drop them.
    fn leave(&mut self, base: usize) {
        while self.regs.len() > base {
            if let Some(v) = self.regs.pop() {
                self.release(v);
            }
        }
    }

    fn get(&self, base: usize, r: Reg) -> Value {
        self.regs
            .get(base + r.0 as usize)
            .copied()
            .unwrap_or(Value::NIL)
    }

    /// Put `v` in register `r`, whose old value is released. `v`'s reference
    /// passes to the register.
    fn set(&mut self, base: usize, r: Reg, v: Value) {
        if let Some(slot) = self.regs.get_mut(base + r.0 as usize) {
            let old = std::mem::replace(slot, v);
            self.release(old);
        }
    }

    /// `v`, with one more reference: for when it goes somewhere new.
    fn share(&mut self, v: Value) -> Value {
        if let Some(cell) = v.as_cell() {
            self.heap.share(cell);
        }
        v
    }

    fn release(&mut self, v: Value) {
        if let Some(cell) = v.as_cell() {
            self.heap.release(cell);
        }
    }

    fn is_string(&self, v: Value) -> bool {
        v.as_cell()
            .is_some_and(|c| self.heap.kind(c) == Some(Kind::String))
    }

    fn is_variant(&self, v: Value, variant: VariantRef) -> bool {
        match v.view() {
            View::Nullary(n) => n == variant,
            View::Cell(cell) => {
                self.heap.kind(cell) == Some(Kind::Ctor) && self.heap.variant(cell) == variant
            }
            View::Float(_) | View::Int(_) | View::Nil | View::Bool(_) | View::Func(_) => false,
        }
    }

    fn is_int(&self, v: Value, n: i64) -> bool {
        match v.view() {
            View::Int(m) => m == n,
            View::Cell(cell) if self.heap.kind(cell) == Some(Kind::BigInt) => {
                self.heap.read_big_int(cell) == n.into()
            }
            View::Float(_)
            | View::Nil
            | View::Bool(_)
            | View::Func(_)
            | View::Cell(_)
            | View::Nullary(_) => false,
        }
    }

    fn int(&self, base: usize, r: Reg) -> Result<Int, Stop> {
        self.int_of(self.get(base, r))
    }

    fn int_of(&self, v: Value) -> Result<Int, Stop> {
        match v.view() {
            View::Int(n) => Ok(Int::Small(n)),
            View::Cell(cell) if self.heap.kind(cell) == Some(Kind::BigInt) => {
                Ok(Int::Big(self.heap.read_big_int(cell)))
            }
            v @ (View::Float(_)
            | View::Nil
            | View::Bool(_)
            | View::Func(_)
            | View::Cell(_)
            | View::Nullary(_)) => Err(Stop::NotBuiltYet(format!("an Int operation on {v:?}"))),
        }
    }

    /// How `println` and `${x}` show a value.
    fn show(&self, v: Value, out: &mut Vec<u8>) -> Result<(), Stop> {
        show::show(&self.heap, self.code, v, out)
    }

    /// Cells not yet freed.
    #[cfg(test)]
    fn live(&self) -> usize {
        self.heap.live()
    }

    /// Release every global, so a test can see that nothing else is left.
    #[cfg(test)]
    fn release_globals(&mut self) {
        for v in std::mem::take(&mut self.globals) {
            self.release(v);
        }
    }
}

fn ready(f: &Func) -> Result<&Body, Stop> {
    match f {
        Func::Ready(body) => Ok(body),
        Func::NotBuiltYet(what) => Err(Stop::NotBuiltYet(what.clone())),
    }
}

fn full(_: Full) -> Stop {
    Stop::HeapFull
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run `src`, then release everything a finished run still holds: the
    /// result and the globals. What is left is what reference counting lost.
    fn cells_left_after(src: &str) -> (String, usize) {
        let mut scanner = scarlet_core::scanner::new_scanner(src.to_string());
        let parsed = scarlet_core::parser::new_parser(&mut scanner).parse_program();
        let expr = scarlet_core::ast::Expression::BlockExpression(parsed.ast);
        let result = scarlet_core::bytecode::compile(&expr, None);
        assert!(result.success(), "{:?}", result.diagnostics);
        let program = result.into_runnable().expect("a clean compile is runnable");
        let code = crate::code::load(&program);
        let mut out = Vec::new();
        let mut m = Machine::new(&code, &mut out);
        let last = m.run().expect("the program runs");
        m.release(last);
        m.release_globals();
        assert!(m.regs.is_empty(), "a frame's registers outlived it");
        let left = m.live();
        drop(m);
        (String::from_utf8(out).expect("UTF-8"), left)
    }

    #[test]
    fn strings_made_and_joined_are_all_freed() {
        let (out, left) = cells_left_after(
            "fn greet(name String) String { 'hello, ${name}' }\n\
             pub fn main() {\n\
             \tprintln(greet('world'))\n\
             \tprintln('${1 + 1} and ${greet('you')}')\n\
             }\n",
        );
        assert_eq!(out, "hello, world\n2 and hello, you\n");
        assert_eq!(left, 0);
    }

    /// Each step makes a string and drops the last one. If a step leaked, the
    /// heap would hold 10,000 cells at the end.
    #[test]
    fn a_loop_that_makes_strings_does_not_keep_them() {
        let (out, left) = cells_left_after(
            "fn spin(n Int, s String) String {\n\
             \tif n == 0 { s } else { spin(n - 1, '${n}') }\n\
             }\n\
             pub fn main() {\n\
             \tprintln(spin(10000, 'start'))\n\
             }\n",
        );
        assert_eq!(out, "1\n");
        assert_eq!(left, 0);
    }

    /// Each step makes a bigger big int and drops the last, and printing one
    /// makes and frees its text.
    #[test]
    fn big_ints_made_in_a_loop_are_all_freed() {
        let (out, left) = cells_left_after(
            "fn grow(n Int, acc Int) Int {\n\
             \tif n == 0 { acc } else { grow(n - 1, acc * 3) }\n\
             }\n\
             pub fn main() {\n\
             \tbig = grow(200, 1)\n\
             \tprintln(big > 1)\n\
             \tprintln('${big / big}')\n\
             }\n",
        );
        assert_eq!(out, "True\n1\n");
        assert_eq!(left, 0);
    }

    /// Constructors hold references to what is in them: a list's cells, the
    /// strings in them, and a record kept in a global are all freed.
    #[test]
    fn constructors_and_what_they_hold_are_all_freed() {
        let (out, left) = cells_left_after(
            "type L {\n\
             \tCons(h String, t L)\n\
             \tEnd\n\
             }\n\
             type Point {\n\
             \tPoint(x Int, y Int)\n\
             }\n\
             const origin = Point(x: 0, y: 0)\n\
             fn build(n Int, acc L) L {\n\
             \tif n == 0 { acc } else { build(n - 1, Cons('${n}', acc)) }\n\
             }\n\
             pub fn main() {\n\
             \tprintln(build(2, End))\n\
             \t_l = build(10000, End)\n\
             \tprintln(origin)\n\
             \tprintln(Some(origin))\n\
             }\n",
        );
        assert_eq!(
            out,
            "Cons(\n  1,\n  Cons(2, End)\n)\nPoint{ x: 0, y: 0 }\nSome(\n  Point{ x: 0, y: 0 }\n)\n"
        );
        assert_eq!(left, 0);
    }

    /// A match's arms read fields out of the value they match, and each field
    /// read holds its own reference: walking, rebuilding and dropping lists
    /// of strings leaves nothing behind.
    #[test]
    fn what_a_match_reads_out_is_all_freed() {
        let (out, left) = cells_left_after(
            "type L {\n\
             \tCons(h String, t L)\n\
             \tEnd\n\
             }\n\
             fn build(n Int, acc L) L {\n\
             \tif n == 0 { acc } else { build(n - 1, Cons('${n}', acc)) }\n\
             }\n\
             fn rev(l L, acc L) L {\n\
             \tmatch l {\n\
             \t\tCons(h, t) -> rev(t, Cons(h, acc))\n\
             \t\tEnd -> acc\n\
             \t}\n\
             }\n\
             fn first(l L) String {\n\
             \tmatch l {\n\
             \t\tCons(h, _) -> h\n\
             \t\tEnd -> 'empty'\n\
             \t}\n\
             }\n\
             pub fn main() {\n\
             \tl = build(1000, End)\n\
             \tprintln(first(rev(l, End)))\n\
             \tprintln(first(l))\n\
             \tprintln(first(End))\n\
             }\n",
        );
        assert_eq!(out, "1000\n1\nempty\n");
        assert_eq!(left, 0);
    }

    /// A closure holds its captures, and each call to it holds the closure:
    /// closures made in a loop, returned, called and dropped leave nothing.
    #[test]
    fn closures_and_their_captures_are_all_freed() {
        let (out, left) = cells_left_after(
            "type Box {\n\
             \tBox(s String)\n\
             }\n\
             fn greeter(name String) fn(String) String {\n\
             \tb = Box(name)\n\
             \tfn(greeting) {\n\
             \t\tmatch b {\n\
             \t\t\tBox(n) -> '${greeting}, ${n}'\n\
             \t\t}\n\
             \t}\n\
             }\n\
             fn spin(n Int, f fn(String) String) String {\n\
             \tif n == 0 { f('bye') } else { spin(n - 1, greeter('${n}')) }\n\
             }\n\
             pub fn main() {\n\
             \tprintln(greeter('you')('hi'))\n\
             \tprintln(spin(1000, greeter('start')))\n\
             \tk = 'kept'\n\
             \tgo = fn(n) { if n == 0 { k } else { go(n - 1) } }\n\
             \tprintln(go(100))\n\
             }\n",
        );
        assert_eq!(out, "hi, you\nbye, 1\nkept\n");
        assert_eq!(left, 0);
    }

    /// A tuple holds references to its elements, and reading one out takes
    /// its own.
    #[test]
    fn tuples_and_what_they_hold_are_all_freed() {
        let (out, left) = cells_left_after(
            "fn pairs(n Int, acc (String, Int)) (String, Int) {\n\
             \tif n == 0 { acc } else { pairs(n - 1, ('${n}', acc.1 + n)) }\n\
             }\n\
             pub fn main() {\n\
             \tp = pairs(1000, ('start', 0))\n\
             \tprintln(p.0)\n\
             \tprintln(p)\n\
             \tprintln(Some((p, 'x')))\n\
             }\n",
        );
        assert_eq!(
            out,
            "1\n(1, 500500)\nSome(\n  (\n    (1, 500500),\n    x\n  )\n)\n"
        );
        assert_eq!(left, 0);
    }

    /// An array shares its nodes with the arrays it was made from. Building
    /// arrays of strings at both ends, walking them, joining them and keeping
    /// old versions around leaves nothing once they are all dropped.
    #[test]
    fn arrays_and_what_they_share_are_all_freed() {
        let (out, left) = cells_left_after(
            "fn build(n Int, acc Array(String)) Array(String) {\n\
             \tif n == 0 { acc } else { build(n - 1, [..acc, '${n}']) }\n\
             }\n\
             fn front(n Int, acc Array(String)) Array(String) {\n\
             \tif n == 0 { acc } else { front(n - 1, ['${n}', ..acc]) }\n\
             }\n\
             fn count(xs Array(String), n Int) Int {\n\
             \tmatch xs {\n\
             \t\t[] -> n\n\
             \t\t[_, ..t] -> count(t, n + 1)\n\
             \t}\n\
             }\n\
             pub fn main() {\n\
             \ta = build(2000, [])\n\
             \tb = front(2000, a)\n\
             \tc = [..b, ..a]\n\
             \tprintln(count(c, 0))\n\
             \tprintln(c[0] or '?')\n\
             \tprintln(a[0] or '?')\n\
             \tprintln(count([..a, ..[]], 0))\n\
             }\n",
        );
        assert_eq!(out, "6000\n1\n2000\n2000\n");
        assert_eq!(left, 0);
    }

    /// A range holds no references, and one built into an array, big ints and
    /// all, is freed like any array.
    #[test]
    fn ranges_and_the_arrays_made_from_them_are_all_freed() {
        let (out, left) = cells_left_after(
            "fn count(xs Array(Int), n Int) Int {\n\
             \tmatch xs {\n\
             \t\t[] -> n\n\
             \t\t[_, ..t] -> count(t, n + 1)\n\
             \t}\n\
             }\n\
             pub fn main() {\n\
             \tr = 0..1000\n\
             \tprintln(count([..r, 5], 0))\n\
             \tprintln(count([..{ 9223372036854775000..9223372036854775007 }, ..r], 0))\n\
             \tprintln(r[0..3])\n\
             \tprintln(count(r, 0))\n\
             }\n",
        );
        assert_eq!(out, "1001\n1007\nOk(\n  [0, 1, 2]\n)\n1000\n");
        assert_eq!(left, 0);
    }

    /// A string held in a global lives until the run ends, and no longer.
    #[test]
    fn a_string_in_a_global_is_freed_with_the_globals() {
        let (out, left) = cells_left_after(
            "const greeting = 'hi'\n\
             pub fn main() {\n\
             \tprintln(greeting)\n\
             \tprintln(greeting)\n\
             }\n",
        );
        assert_eq!(out, "hi\nhi\n");
        assert_eq!(left, 0);
    }
}
