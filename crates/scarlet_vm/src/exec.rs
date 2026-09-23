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
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use num_bigint::Sign;
use num_traits::{FromPrimitive, ToPrimitive};
use scarlet_ir::core_ir::{FuncIdx, HttpTypes, IoErrors, JsonTypes, VariantRef};
use scarlet_ir::intrinsic::Intrinsic;

use crate::Stop;
use crate::array::{self, End, Seq};
use crate::bigint::{self, Int};
use crate::binary::{self, Bits};
use crate::code::{BitsOp, Body, Code, Func, Instr, IntOp, Reg};
use crate::eq;
use crate::float::{self, NumOp};
use crate::heap::{Cell, Full, Heap, Kind};
use crate::host::Host;
use crate::http;
use crate::json;
use crate::map;
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

pub(crate) struct Machine<'c, 'h, 'o> {
    code: &'c Code,
    host: &'h Host,
    out: &'o mut dyn Write,
    heap: Heap,
    globals: Vec<Value>,
    regs: Vec<Value>,
    /// `os.env` as a map, made the first time it is asked for. The
    /// environment cannot change while the program runs, so every call after
    /// shares this one.
    env: Option<Cell>,
}

impl<'c, 'h, 'o> Machine<'c, 'h, 'o> {
    pub(crate) fn new(code: &'c Code, host: &'h Host, out: &'o mut dyn Write) -> Self {
        Machine {
            code,
            host,
            out,
            heap: Heap::default(),
            globals: vec![Value::NIL; code.globals as usize],
            regs: Vec::new(),
            env: None,
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
                Instr::BinaryConst { dst, bytes, len } => {
                    let cell = binary::make(&mut self.heap, bytes, *len).map_err(full)?;
                    self.set(base, *dst, Value::cell(cell));
                }
                Instr::Bits { dst, op, args } => {
                    let v = self.bits_op(*op, base, args)?;
                    self.set(base, *dst, v);
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
                Instr::Float { dst, op, a, b } => {
                    let (a, b) = (self.float(base, *a)?, self.float(base, *b)?);
                    self.set(base, *dst, float::op(*op, a, b));
                }
                Instr::FloatNeg { dst, a } => {
                    let a = self.float(base, *a)?;
                    self.set(base, *dst, Value::float(-a));
                }
                Instr::Num { dst, op, a, b } => {
                    let v = self.num(*op, self.get(base, *a), self.get(base, *b))?;
                    self.set(base, *dst, v);
                }
                Instr::Neg { dst, a } => {
                    let a = self.get(base, *a);
                    let v = match a.view() {
                        View::Float(f) => Value::float(-f),
                        View::Int(_)
                        | View::Nil
                        | View::Bool(_)
                        | View::Func(_)
                        | View::Cell(_)
                        | View::Nullary(_) => {
                            let a = self.int_of(a)?;
                            bigint::neg(&mut self.heap, a).map_err(full)?
                        }
                    };
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
                Instr::JumpUnlessBinary {
                    src,
                    bytes,
                    len,
                    to,
                } => {
                    let v = self.get(base, *src);
                    let is = match v.as_cell().and_then(|c| binary::bits(&self.heap, c)) {
                        Some(b) => b.len == *len && binary::bytes(&self.heap, b) == bytes.as_ref(),
                        None => false,
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
                    let v = self.builtin(*intrinsic, base, args)?;
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

    /// What built-in `i` gives for the values in `args`, which it only reads.
    /// The loader checked how many there are (`code::built`). The result
    /// holds its own reference.
    fn builtin(&mut self, i: Intrinsic, base: usize, args: &[Reg]) -> Result<Value, Stop> {
        let arg = |m: &Self, k: usize| args.get(k).map_or(Value::NIL, |r| m.get(base, *r));
        let v = arg(self, 0);
        match i {
            Intrinsic::BinaryFromString => {
                let text = self.text_of(v)?;
                let cell = binary::make(&mut self.heap, &text, text.len() as u64 * 8);
                Ok(Value::cell(cell.map_err(full)?))
            }
            Intrinsic::BinaryToString => {
                let b = self.binary(v)?;
                let text = binary::bytes(&self.heap, b);
                match std::str::from_utf8(&text) {
                    Ok(_) if b.len % 8 == 0 => {
                        let s = Value::cell(self.heap.string(&text).map_err(full)?);
                        self.ok(s)
                    }
                    _ => self.err_nil(),
                }
            }
            Intrinsic::BinaryBitSize => {
                let n = self.binary(v)?.len;
                bigint::value(&mut self.heap, n.into()).map_err(full)
            }
            Intrinsic::BinaryByteSize => {
                let n = self.binary(v)?.len.div_ceil(8);
                bigint::value(&mut self.heap, n.into()).map_err(full)
            }
            Intrinsic::BinaryAppend => {
                let (a, b) = (self.binary(v)?, self.binary(arg(self, 1))?);
                let cell = binary::join(&mut self.heap, &[a, b]).map_err(full)?;
                Ok(Value::cell(cell))
            }
            // The ASCII built-ins read whole bytes only, as the old VM's did:
            // a last byte that is not whole is left out.
            Intrinsic::BinaryByteAt => {
                let b = self.binary(v)?;
                let found = match self.int_of(arg(self, 1))? {
                    Int::Small(i) => u64::try_from(i)
                        .ok()
                        .and_then(|i| binary::whole_byte(&self.heap, b, i)),
                    Int::Big(_) => None,
                };
                let n = found.map_or(-1, i64::from);
                bigint::value(&mut self.heap, n.into()).map_err(full)
            }
            // From `from`, clamped to the binary; an empty needle is found
            // where the search starts.
            Intrinsic::BinaryIndexOf => {
                let hay = binary::whole_bytes(&self.heap, self.binary(v)?);
                let needle = binary::whole_bytes(&self.heap, self.binary(arg(self, 1))?);
                let from = match self.int_of(arg(self, 2))? {
                    Int::Small(n) => usize::try_from(n).unwrap_or(0).min(hay.len()),
                    Int::Big(n) if n.sign() == num_bigint::Sign::Minus => 0,
                    Int::Big(_) => hay.len(),
                };
                let found = if needle.is_empty() {
                    Some(from)
                } else {
                    hay.get(from..)
                        .and_then(|rest| rest.windows(needle.len()).position(|w| w == needle))
                        .map(|at| from + at)
                };
                match found {
                    Some(at) => {
                        let at = bigint::value(&mut self.heap, at.into()).map_err(full)?;
                        self.some(at)
                    }
                    None => Ok(Value::nullary(self.code.abi.none)),
                }
            }
            // Digits only, no sign: empty input or any other byte is `None`.
            // An Int has no bounds, so no number is too large.
            Intrinsic::BinaryParseInt => {
                let digits = binary::whole_bytes(&self.heap, self.binary(v)?);
                let base = self.radix(arg(self, 1))?;
                let valid =
                    !digits.is_empty() && digits.iter().all(|c| char::from(*c).is_digit(base));
                let n = if valid {
                    num_bigint::BigInt::parse_bytes(&digits, base)
                } else {
                    None
                };
                match n {
                    Some(n) => {
                        let n = bigint::value(&mut self.heap, n).map_err(full)?;
                        self.some(n)
                    }
                    None => Ok(Value::nullary(self.code.abi.none)),
                }
            }
            Intrinsic::BinaryEqIgnoreAsciiCase => {
                let a = binary::whole_bytes(&self.heap, self.binary(v)?);
                let b = binary::whole_bytes(&self.heap, self.binary(arg(self, 1))?);
                Ok(Value::bool(a.eq_ignore_ascii_case(&b)))
            }
            Intrinsic::BinaryToAsciiLower => {
                let mut bytes = binary::whole_bytes(&self.heap, self.binary(v)?);
                bytes.make_ascii_lowercase();
                let len = bytes.len() as u64 * 8;
                Ok(Value::cell(
                    binary::make(&mut self.heap, &bytes, len).map_err(full)?,
                ))
            }
            // Lowercase hex, and a `-` before a negative number's digits.
            Intrinsic::BinaryFromIntAscii => {
                let n = match self.int_of(v)? {
                    Int::Small(n) => num_bigint::BigInt::from(n),
                    Int::Big(n) => n,
                };
                let text = n.to_str_radix(self.radix(arg(self, 1))?);
                let len = text.len() as u64 * 8;
                let cell = binary::make(&mut self.heap, text.as_bytes(), len);
                Ok(Value::cell(cell.map_err(full)?))
            }
            // A negative bound, or one past the end, is `Err(Nil)`, never a
            // short read (`binary.scrl`).
            Intrinsic::BinarySliceBits => {
                let b = self.binary(v)?;
                let (at, take) = (self.int_of(arg(self, 1))?, self.int_of(arg(self, 2))?);
                let window = match (at, take) {
                    (Int::Small(at), Int::Small(take)) if at >= 0 && take >= 0 => {
                        let (at, take) = (at as u64, take as u64);
                        (at + take <= b.len).then_some((at, take))
                    }
                    _ => None,
                };
                match window {
                    Some((at, take)) => {
                        let cut = binary::slice(&mut self.heap, b, at, take).map_err(full)?;
                        self.ok(Value::cell(cut))
                    }
                    None => self.err_nil(),
                }
            }
            Intrinsic::StringInspect => {
                let mut text = Vec::new();
                self.show(v, &mut text)?;
                Ok(Value::cell(self.heap.string(&text).map_err(full)?))
            }
            // Adjacent delimiters give empty pieces, and an empty `on` gives
            // one piece per Unicode scalar value (`string.scrl`).
            Intrinsic::StringSplit => {
                let s = self.utf8_of(v)?;
                let on = self.utf8_of(arg(self, 1))?;
                let pieces: Vec<String> = if on.is_empty() {
                    s.chars().map(String::from).collect()
                } else {
                    s.split(on.as_str()).map(String::from).collect()
                };
                self.strings(&pieces)
            }
            Intrinsic::StringContains => {
                let (s, needle) = (self.utf8_of(v)?, self.utf8_of(arg(self, 1))?);
                Ok(Value::bool(s.contains(needle.as_str())))
            }
            Intrinsic::StringTrim => {
                let s = self.utf8_of(v)?;
                Ok(Value::cell(
                    self.heap.string(s.trim().as_bytes()).map_err(full)?,
                ))
            }
            Intrinsic::StringToGraphemes => {
                let s = self.utf8_of(v)?;
                let pieces: Vec<String> =
                    unicode_segmentation::UnicodeSegmentation::graphemes(s.as_str(), true)
                        .map(String::from)
                        .collect();
                self.strings(&pieces)
            }
            // An optional sign, then one or more ASCII digits, and nothing
            // else. An Int has no bounds, so no number of digits is too many.
            Intrinsic::IntFromString => {
                let s = self.utf8_of(v)?;
                let digits = s.strip_prefix(['+', '-']).unwrap_or(&s);
                let valid = !digits.is_empty() && digits.bytes().all(|c| c.is_ascii_digit());
                let n = if valid {
                    num_bigint::BigInt::parse_bytes(s.as_bytes(), 10)
                } else {
                    None
                };
                match n {
                    Some(n) => {
                        let n = bigint::value(&mut self.heap, n).map_err(full)?;
                        self.some(n)
                    }
                    None => Ok(Value::nullary(self.code.abi.none)),
                }
            }
            // An Int as an endless row of two's-complement bits (`int.scrl`).
            Intrinsic::IntBitwiseAnd | Intrinsic::IntBitwiseOr | Intrinsic::IntBitwiseXor => {
                let (a, b) = (self.int_of(v)?.big(), self.int_of(arg(self, 1))?.big());
                let n = match i {
                    Intrinsic::IntBitwiseAnd => a & b,
                    Intrinsic::IntBitwiseOr => a | b,
                    _ => a ^ b,
                };
                bigint::value(&mut self.heap, n).map_err(full)
            }
            Intrinsic::IntBitwiseNot => {
                let n = !self.int_of(v)?.big();
                bigint::value(&mut self.heap, n).map_err(full)
            }
            // A negative count shifts the other way.
            Intrinsic::IntBitwiseShiftLeft | Intrinsic::IntBitwiseShiftRight => {
                let x = self.int_of(v)?.big();
                let count = self.int_of(arg(self, 1))?.big();
                let left = (i == Intrinsic::IntBitwiseShiftLeft) != (count.sign() == Sign::Minus);
                let n = shift(x, left, &count.magnitude().clone().into())?;
                bigint::value(&mut self.heap, n).map_err(full)
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
            Intrinsic::TimeMonotonic => {
                let ms = self.host.monotonic_ms();
                bigint::value(&mut self.heap, ms.into()).map_err(full)
            }
            // Negative before 1970: `duration_since` gives how far before as
            // an error, and the sign goes back on.
            Intrinsic::TimeEpochMs => {
                let ms = match SystemTime::now().duration_since(UNIX_EPOCH) {
                    Ok(after) => num_bigint::BigInt::from(after.as_millis()),
                    Err(before) => -num_bigint::BigInt::from(before.duration().as_millis()),
                };
                bigint::value(&mut self.heap, ms).map_err(full)
            }
            // From the OS's own generator, never one of ours, and never
            // zeros or anything else in place of bytes it did not give.
            Intrinsic::CryptoRandomBytes => {
                let n = match self.int_of(v)? {
                    Int::Small(n) => usize::try_from(n).ok(),
                    Int::Big(_) => None,
                };
                let Some(n) = n else {
                    return self.err_nil();
                };
                if n > binary::MAX_BYTES {
                    return Err(Stop::HeapFull);
                }
                let mut bytes = vec![0u8; n];
                if getrandom::fill(&mut bytes).is_err() {
                    return self.err_nil();
                }
                let cell = binary::make(&mut self.heap, &bytes, n as u64 * 8).map_err(full)?;
                self.ok(Value::cell(cell))
            }
            // The digests and the tag are over bytes: a binary that is not
            // whole bytes has none, rather than the digest of some bytes near
            // it, which another binary would share.
            Intrinsic::CryptoSha1 | Intrinsic::CryptoSha256 | Intrinsic::CryptoSha512 => {
                let Some(m) = self.whole_bytes(v)? else {
                    return self.err_nil();
                };
                let alg = match i {
                    Intrinsic::CryptoSha1 => &aws_lc_rs::digest::SHA1_FOR_LEGACY_USE_ONLY,
                    Intrinsic::CryptoSha256 => &aws_lc_rs::digest::SHA256,
                    _ => &aws_lc_rs::digest::SHA512,
                };
                let digest = aws_lc_rs::digest::digest(alg, &m);
                self.ok_bytes(digest.as_ref())
            }
            Intrinsic::CryptoHmacSha256 => {
                let (Some(key), Some(msg)) =
                    (self.whole_bytes(v)?, self.whole_bytes(arg(self, 1))?)
                else {
                    return self.err_nil();
                };
                let key = aws_lc_rs::hmac::Key::new(aws_lc_rs::hmac::HMAC_SHA256, &key);
                let tag = aws_lc_rs::hmac::sign(&key, &msg);
                self.ok_bytes(tag.as_ref())
            }
            // The same bits, compared in time that depends on the lengths
            // alone. The last byte of each is padded with zeros the same way,
            // so equal lengths and equal bytes are equal bits.
            Intrinsic::CryptoConstEq => {
                let (a, b) = (self.binary(v)?, self.binary(arg(self, 1))?);
                let same = a.len == b.len
                    && aws_lc_rs::constant_time::verify_slices_are_equal(
                        &binary::bytes(&self.heap, a),
                        &binary::bytes(&self.heap, b),
                    )
                    .is_ok();
                Ok(Value::bool(same))
            }
            // A key, message or signature of the wrong shape, whole bytes or
            // not, is a signature that does not check out.
            Intrinsic::CryptoP256Verify | Intrinsic::CryptoEd25519Verify => {
                let alg: &'static dyn aws_lc_rs::signature::VerificationAlgorithm = match i {
                    Intrinsic::CryptoP256Verify => &aws_lc_rs::signature::ECDSA_P256_SHA256_ASN1,
                    _ => &aws_lc_rs::signature::ED25519,
                };
                let parts = (
                    self.whole_bytes(v)?,
                    self.whole_bytes(arg(self, 1))?,
                    self.whole_bytes(arg(self, 2))?,
                );
                let valid = match parts {
                    (Some(key), Some(message), Some(sig)) => {
                        aws_lc_rs::signature::UnparsedPublicKey::new(alg, key)
                            .verify(&message, &sig)
                            .is_ok()
                    }
                    _ => false,
                };
                Ok(Value::bool(valid))
            }
            Intrinsic::JsonParseBinary => {
                let src = self.binary(v)?;
                let parsed = if src.len % 8 == 0 {
                    json::parse(&binary::bytes(&self.heap, src))
                } else {
                    Err(json::ParseError {
                        offset: (src.len / 8) as usize,
                        message: "input is not a whole number of bytes".into(),
                    })
                };
                let types = self.json_types()?;
                match parsed {
                    Ok((tape, arena)) => {
                        let arena = binary::make(&mut self.heap, &arena, arena.len() as u64 * 8);
                        let arena = Value::cell(arena.map_err(full)?);
                        let tape = binary::make(&mut self.heap, &tape, tape.len() as u64 * 8);
                        let tape = Value::cell(tape.map_err(full)?);
                        let root = Value::int(0).unwrap_or(Value::NIL);
                        let doc = self
                            .heap
                            .ctor(types.doc, &[arena, tape, root])
                            .map_err(full)?;
                        self.ok(Value::cell(doc))
                    }
                    Err(e) => {
                        let offset =
                            bigint::value(&mut self.heap, e.offset.into()).map_err(full)?;
                        let text = self.heap.string(e.message.as_bytes()).map_err(full)?;
                        let error = self
                            .heap
                            .ctor(types.parse_error, &[offset, Value::cell(text)])
                            .map_err(full)?;
                        self.err(Value::cell(error))
                    }
                }
            }
            // -1 for a node the tape does not have: `scarlet/json` reads it.
            Intrinsic::JsonKind | Intrinsic::JsonLen => {
                let d = self.doc(v)?;
                let t = self.tape(&d)?;
                let n = match i {
                    Intrinsic::JsonKind => t.kind(d.idx),
                    _ => t.len(d.idx).and_then(|n| i64::try_from(n).ok()),
                };
                bigint::value(&mut self.heap, n.unwrap_or(-1).into()).map_err(full)
            }
            Intrinsic::JsonField | Intrinsic::JsonIndex => {
                let d = self.doc(v)?;
                let key = arg(self, 1);
                let found = match i {
                    Intrinsic::JsonField => {
                        let name = self.text_of(key)?;
                        self.tape(&d)?.field(d.idx, &name)
                    }
                    _ => match self.int_of(key)? {
                        Int::Small(n) => usize::try_from(n)
                            .ok()
                            .and_then(|n| self.tape(&d).ok()?.element(d.idx, n)),
                        Int::Big(_) => None,
                    },
                };
                match found {
                    Some(at) => {
                        let doc = self.doc_at(&d, at)?;
                        self.some(doc)
                    }
                    None => Ok(Value::nullary(self.code.abi.none)),
                }
            }
            Intrinsic::JsonEntries => {
                let d = self.doc(v)?;
                let members = self.tape(&d)?.members(d.idx).unwrap_or_default();
                let mut items = Vec::with_capacity(members.len());
                for (key, at) in members {
                    let key = Value::cell(self.heap.string(&key).map_err(full)?);
                    let doc = self.doc_at(&d, at)?;
                    items.push(Value::cell(self.heap.tuple(&[key, doc]).map_err(full)?));
                }
                Ok(Value::cell(
                    array::from_values(&mut self.heap, &items).map_err(full)?,
                ))
            }
            Intrinsic::JsonElements => {
                let d = self.doc(v)?;
                let ats = self.tape(&d)?.elements(d.idx).unwrap_or_default();
                let mut items = Vec::with_capacity(ats.len());
                for at in ats {
                    items.push(self.doc_at(&d, at)?);
                }
                Ok(Value::cell(
                    array::from_values(&mut self.heap, &items).map_err(full)?,
                ))
            }
            Intrinsic::JsonString => {
                let d = self.doc(v)?;
                let text = self.tape(&d)?.string(d.idx);
                match text.filter(|t| std::str::from_utf8(t).is_ok()) {
                    Some(t) => {
                        let s = Value::cell(self.heap.string(&t).map_err(full)?);
                        self.some(s)
                    }
                    None => Ok(Value::nullary(self.code.abi.none)),
                }
            }
            Intrinsic::JsonInt | Intrinsic::JsonIntText => {
                let d = self.doc(v)?;
                match self.tape(&d)?.int(d.idx) {
                    Some(n) => {
                        let found = match i {
                            Intrinsic::JsonInt => bigint::value(&mut self.heap, n).map_err(full)?,
                            _ => Value::cell(
                                self.heap.string(n.to_string().as_bytes()).map_err(full)?,
                            ),
                        };
                        self.some(found)
                    }
                    None => Ok(Value::nullary(self.code.abi.none)),
                }
            }
            Intrinsic::JsonFloat => {
                let d = self.doc(v)?;
                match self.tape(&d)?.float(d.idx) {
                    Some(f) => self.some(Value::float(f)),
                    None => Ok(Value::nullary(self.code.abi.none)),
                }
            }
            Intrinsic::JsonBool => {
                let d = self.doc(v)?;
                match self.tape(&d)?.bool(d.idx) {
                    Some(b) => self.some(Value::bool(b)),
                    None => Ok(Value::nullary(self.code.abi.none)),
                }
            }
            Intrinsic::JsonEncode => {
                let text = self.encode_json(v)?;
                Ok(Value::cell(self.heap.string(&text).map_err(full)?))
            }
            Intrinsic::HttpParseHead | Intrinsic::HttpParseResponseHead => {
                let buf = self.binary(v)?;
                let off = self.offset(arg(self, 1), buf)?;
                // Past the longest head there could be, and its empty lines.
                let window =
                    binary::bytes_from(&self.heap, buf, off as u64, http::MAX_HEAD as u64 + 16);
                match i {
                    Intrinsic::HttpParseHead => self.parsed_request(buf, off, &window),
                    _ => self.parsed_response(buf, off, &window),
                }
            }
            Intrinsic::HttpChunkDecode => {
                let buf = self.binary(v)?;
                let off = self.offset(arg(self, 1), buf)?;
                let max = match self.int_of(arg(self, 2))? {
                    Int::Small(n) => u64::try_from(n).unwrap_or(0),
                    Int::Big(n) if n.sign() == Sign::Minus => 0,
                    Int::Big(_) => u64::MAX,
                };
                let bytes = binary::bytes_from(&self.heap, buf, off as u64, u64::MAX);
                self.chunk_body(buf, off, &bytes, max)
            }
            Intrinsic::HttpFraming => {
                let types = self.http_types()?;
                let (mut te, mut cl) = (Vec::new(), Vec::new());
                for (name, value) in self.headers(v)? {
                    let name = self.bytes_of(name)?;
                    if name.eq_ignore_ascii_case(b"transfer-encoding") {
                        te.push(self.bytes_of(value)?);
                    } else if name.eq_ignore_ascii_case(b"content-length") {
                        cl.push(self.bytes_of(value)?);
                    }
                }
                let te: Vec<&[u8]> = te.iter().map(Vec::as_slice).collect();
                let cl: Vec<&[u8]> = cl.iter().map(Vec::as_slice).collect();
                match http::framing(&te, &cl) {
                    http::Framing::NoBody => Ok(Value::nullary(types.no_body)),
                    http::Framing::Chunked => Ok(Value::nullary(types.chunked)),
                    http::Framing::Length(n) => {
                        let n = bigint::value(&mut self.heap, n.into()).map_err(full)?;
                        Ok(Value::cell(
                            self.heap.ctor(types.length, &[n]).map_err(full)?,
                        ))
                    }
                    http::Framing::Invalid(r) => self.with_status(types.framing_invalid, r),
                }
            }
            Intrinsic::HttpHeaderGet | Intrinsic::HttpHeaderHas => {
                let name = self.bytes_of(arg(self, 1))?;
                let mut found = None;
                for (n, value) in self.headers(v)? {
                    if self.bytes_of(n)?.eq_ignore_ascii_case(&name) {
                        found = Some(value);
                        break;
                    }
                }
                match (i, found) {
                    (Intrinsic::HttpHeaderHas, found) => Ok(Value::bool(found.is_some())),
                    (_, Some(value)) => {
                        let value = self.share(value);
                        self.some(value)
                    }
                    (_, None) => Ok(Value::nullary(self.code.abi.none)),
                }
            }
            Intrinsic::HttpHeadersValid => {
                let mut valid = true;
                for (name, value) in self.headers(v)? {
                    let (name, value) = (self.binary(name)?, self.binary(value)?);
                    valid &= name.len % 8 == 0
                        && value.len % 8 == 0
                        && http::is_token(&binary::bytes(&self.heap, name))
                        && http::is_safe_value(&binary::bytes(&self.heap, value));
                }
                Ok(Value::bool(valid))
            }
            Intrinsic::HttpSerializeHead => {
                let code = self.int_of(v)?.to_string();
                let reason = self.bytes_of(arg(self, 1))?;
                let fields = self.headers(arg(self, 2))?;
                let mut out = Vec::with_capacity(64 + reason.len() + fields.len() * 64);
                out.extend_from_slice(b"HTTP/1.1 ");
                out.extend_from_slice(code.as_bytes());
                out.push(b' ');
                out.extend_from_slice(&reason);
                out.extend_from_slice(b"\r\n");
                for (name, value) in fields {
                    out.extend_from_slice(&self.bytes_of(name)?);
                    out.extend_from_slice(b": ");
                    out.extend_from_slice(&self.bytes_of(value)?);
                    out.extend_from_slice(b"\r\n");
                }
                out.extend_from_slice(b"\r\n");
                let cell =
                    binary::make(&mut self.heap, &out, out.len() as u64 * 8).map_err(full)?;
                Ok(Value::cell(cell))
            }
            Intrinsic::IoReadFile => {
                let path = self.text_of(v)?;
                match std::fs::read(path_of(&path)) {
                    Ok(bytes) if bytes.len() > binary::MAX_BYTES => Err(Stop::HeapFull),
                    Ok(bytes) => {
                        let len = bytes.len() as u64 * 8;
                        let cell = binary::make(&mut self.heap, &bytes, len).map_err(full)?;
                        self.ok(Value::cell(cell))
                    }
                    Err(e) => self.io_error(&e, &path),
                }
            }
            Intrinsic::IoWriteFile => {
                let path = self.text_of(v)?;
                let data = self.binary(arg(self, 1))?;
                if data.len % 8 != 0 {
                    let unaligned = self.io_errors()?.unaligned_binary;
                    return self.err(Value::nullary(unaligned));
                }
                let bytes = binary::bytes(&self.heap, data);
                match std::fs::write(path_of(&path), bytes) {
                    Ok(()) => self.ok(Value::NIL),
                    Err(e) => self.io_error(&e, &path),
                }
            }
            Intrinsic::OsArgv => {
                let mut items = Vec::with_capacity(self.host.argv().len());
                for a in self.host.argv() {
                    items.push(Value::cell(self.heap.string(a.as_bytes()).map_err(full)?));
                }
                Ok(Value::cell(
                    array::from_values(&mut self.heap, &items).map_err(full)?,
                ))
            }
            Intrinsic::OsEnv => {
                let env = match self.env {
                    Some(env) => env,
                    None => {
                        let env = self.env_map().map_err(full)?;
                        self.env = Some(env);
                        env
                    }
                };
                self.heap.share(env);
                Ok(Value::cell(env))
            }
            Intrinsic::MapNew => Ok(Value::cell(map::empty(&mut self.heap).map_err(full)?)),
            Intrinsic::MapSet => {
                let m = self.map(v)?;
                let (key, value) = (arg(self, 1), arg(self, 2));
                let cell = map::set(&mut self.heap, m, key, value).map_err(full)?;
                Ok(Value::cell(cell))
            }
            Intrinsic::MapDelete => {
                let (m, key) = (self.map(v)?, arg(self, 1));
                let cell = map::delete(&mut self.heap, m, key).map_err(full)?;
                Ok(Value::cell(cell))
            }
            Intrinsic::MapGet => {
                let m = self.map(v)?;
                match map::get(&self.heap, m, arg(self, 1)) {
                    Some(found) => {
                        let found = self.share(found);
                        self.some(found)
                    }
                    None => Ok(Value::nullary(self.code.abi.none)),
                }
            }
            Intrinsic::MapHas => {
                let m = self.map(v)?;
                Ok(Value::bool(map::get(&self.heap, m, arg(self, 1)).is_some()))
            }
            Intrinsic::MapSize => {
                let n = map::size(&self.heap, self.map(v)?);
                bigint::value(&mut self.heap, n.into()).map_err(full)
            }
            Intrinsic::MapKeys | Intrinsic::MapValues | Intrinsic::MapToList => {
                let entries = map::entries(&self.heap, self.map(v)?);
                let mut items = Vec::with_capacity(entries.len());
                for (k, v) in entries {
                    let item = match i {
                        Intrinsic::MapKeys => self.share(k),
                        Intrinsic::MapValues => self.share(v),
                        _ => {
                            let pair = [self.share(k), self.share(v)];
                            Value::cell(self.heap.tuple(&pair).map_err(full)?)
                        }
                    };
                    items.push(item);
                }
                Ok(Value::cell(
                    array::from_values(&mut self.heap, &items).map_err(full)?,
                ))
            }
            Intrinsic::FloatFloor
            | Intrinsic::FloatCeil
            | Intrinsic::FloatRound
            | Intrinsic::FloatTruncate => {
                let f = self.float_of(v)?;
                let whole = match i {
                    Intrinsic::FloatFloor => f.floor(),
                    Intrinsic::FloatCeil => f.ceil(),
                    Intrinsic::FloatRound => f.round(),
                    _ => f.trunc(),
                };
                // A Float is finite, so every whole one is an exact Int.
                let n = num_bigint::BigInt::from_f64(whole).unwrap_or_default();
                bigint::value(&mut self.heap, n).map_err(full)
            }
            Intrinsic::FloatFromInt => {
                let f = match self.int_of(v)? {
                    Int::Small(n) => n as f64,
                    Int::Big(n) => n.to_f64().unwrap_or(0.0),
                };
                Ok(Value::float(f))
            }
            Intrinsic::FloatToString => {
                let text = float::text(self.float_of(v)?);
                Ok(Value::cell(
                    self.heap.string(text.as_bytes()).map_err(full)?,
                ))
            }
            Intrinsic::IntToString => {
                let text = self.int_of(v)?.to_string();
                Ok(Value::cell(
                    self.heap.string(text.as_bytes()).map_err(full)?,
                ))
            }
            other => Err(Stop::NotBuiltYet(format!("the built-in {other:?}"))),
        }
    }

    /// A binary operation, with its arguments in the `PrimOp`'s order. A width
    /// or position below zero counts as 0, and a segment wider than the binary
    /// it takes from takes all of it, as in the old VM.
    fn bits_op(&mut self, op: BitsOp, base: usize, args: &[Reg]) -> Result<Value, Stop> {
        let arg = |m: &Self, k: usize| args.get(k).map_or(Value::NIL, |r| m.get(base, *r));
        match op {
            BitsOp::FromInt => {
                let n = match self.int_of(arg(self, 0))? {
                    Int::Small(n) => num_bigint::BigInt::from(n),
                    Int::Big(n) => n,
                };
                let width = self.width(arg(self, 1))?;
                if width > binary::MAX_BITS {
                    return Err(Stop::HeapFull);
                }
                let bytes = binary::from_int(&n, width);
                Ok(Value::cell(
                    binary::make(&mut self.heap, &bytes, width).map_err(full)?,
                ))
            }
            BitsOp::Take => {
                let b = self.binary(arg(self, 0))?;
                let take = self.width(arg(self, 1))?.min(b.len);
                Ok(Value::cell(
                    binary::slice(&mut self.heap, b, 0, take).map_err(full)?,
                ))
            }
            BitsOp::FromString => {
                let text = self.text_of(arg(self, 0))?;
                let cell = binary::make(&mut self.heap, &text, text.len() as u64 * 8);
                Ok(Value::cell(cell.map_err(full)?))
            }
            BitsOp::Concat => {
                let parts = (0..args.len())
                    .map(|k| self.binary(arg(self, k)))
                    .collect::<Result<Vec<Bits>, Stop>>()?;
                Ok(Value::cell(
                    binary::join(&mut self.heap, &parts).map_err(full)?,
                ))
            }
            BitsOp::BitSize => {
                let n = self.binary(arg(self, 0))?.len;
                bigint::value(&mut self.heap, n.into()).map_err(full)
            }
            BitsOp::View => {
                let b = self.binary(arg(self, 0))?;
                let (at, len) = (self.width(arg(self, 1))?, self.width(arg(self, 2))?);
                if at.checked_add(len).is_none_or(|end| end > b.len) {
                    return Err(Stop::BadProgram(format!(
                        "a pattern read bits {at}..{at}+{len} of a binary {} long",
                        b.len
                    )));
                }
                Ok(Value::cell(
                    binary::slice(&mut self.heap, b, at, len).map_err(full)?,
                ))
            }
            BitsOp::MatchPrefix => {
                let b = self.binary(arg(self, 0))?;
                let at = self.width(arg(self, 1))?;
                let prefix = self.binary(arg(self, 2))?;
                Ok(Value::bool(binary::has_at(&self.heap, b, at, prefix)))
            }
            BitsOp::ReadUtf8 => {
                let b = self.binary(arg(self, 0))?;
                let at = self.width(arg(self, 1))?;
                let (cp, took) = binary::read_utf8(&self.heap, b, at).unwrap_or((0, 0));
                let (cp, took) = (i64::from(cp), took as i64);
                let pair = [Value::int(cp), Value::int(took)];
                let [Some(cp), Some(took)] = pair else {
                    return Err(Stop::BadProgram("a code point past a small Int".into()));
                };
                Ok(Value::cell(self.heap.tuple(&[cp, took]).map_err(full)?))
            }
            BitsOp::ReadInt => {
                let b = self.binary(arg(self, 0))?;
                let (at, width) = (self.width(arg(self, 1))?, self.width(arg(self, 2))?);
                if at.checked_add(width).is_none_or(|end| end > b.len) {
                    return Err(Stop::BadProgram(format!(
                        "a pattern read bits {at}..{at}+{width} of a binary {} long",
                        b.len
                    )));
                }
                let n = binary::read_uint(&self.heap, b, at, width);
                bigint::value(&mut self.heap, n).map_err(full)
            }
        }
    }

    /// The binary `v` holds.
    fn binary(&self, v: Value) -> Result<Bits, Stop> {
        v.as_cell()
            .and_then(|cell| binary::bits(&self.heap, cell))
            .ok_or_else(|| Stop::BadProgram(format!("a binary operation on {v:?}")))
    }

    /// The Int `v` as a width or position in bits: 0 below zero, as in the
    /// old VM, and past any binary when it does not fit 64 bits.
    fn width(&self, v: Value) -> Result<u64, Stop> {
        Ok(match self.int_of(v)? {
            Int::Small(n) => u64::try_from(n).unwrap_or(0),
            Int::Big(n) if n.sign() == num_bigint::Sign::Minus => 0,
            Int::Big(_) => u64::MAX,
        })
    }

    /// The string `v`, as text. Every string the VM makes is UTF-8.
    fn utf8_of(&self, v: Value) -> Result<String, Stop> {
        String::from_utf8(self.text_of(v)?)
            .map_err(|_| Stop::BadProgram("a string that is not UTF-8".into()))
    }

    /// A new array of new strings holding `pieces`.
    fn strings(&mut self, pieces: &[String]) -> Result<Value, Stop> {
        let mut items = Vec::with_capacity(pieces.len());
        for p in pieces {
            items.push(Value::cell(self.heap.string(p.as_bytes()).map_err(full)?));
        }
        Ok(Value::cell(
            array::from_values(&mut self.heap, &items).map_err(full)?,
        ))
    }

    /// The bytes of the string `v`.
    fn text_of(&self, v: Value) -> Result<Vec<u8>, Stop> {
        match v.as_cell() {
            Some(cell) if self.is_string(v) => {
                let mut text = Vec::with_capacity(self.heap.string_len(cell));
                self.heap.read_string(cell, &mut text);
                Ok(text)
            }
            _ => Err(Stop::BadProgram(format!("a string operation on {v:?}"))),
        }
    }

    /// The base a `scarlet/binary.Radix` stands for.
    fn radix(&self, v: Value) -> Result<u32, Stop> {
        match (v.view(), self.code.abi.radix) {
            (View::Nullary(r), Some(radix)) if r == radix.dec => Ok(10),
            (View::Nullary(r), Some(radix)) if r == radix.hex => Ok(16),
            _ => Err(Stop::BadProgram(format!("{v:?} as a `binary.Radix`"))),
        }
    }

    /// `Some(v)`, holding `v`'s reference.
    fn some(&mut self, v: Value) -> Result<Value, Stop> {
        let cell = self.heap.ctor(self.code.abi.some, &[v]).map_err(full)?;
        Ok(Value::cell(cell))
    }

    /// `Ok(v)`, holding `v`'s reference.
    fn ok(&mut self, v: Value) -> Result<Value, Stop> {
        let cell = self.heap.ctor(self.code.abi.ok, &[v]).map_err(full)?;
        Ok(Value::cell(cell))
    }

    /// `Err(v)`, holding `v`'s reference.
    fn err(&mut self, v: Value) -> Result<Value, Stop> {
        let cell = self.heap.ctor(self.code.abi.err, &[v]).map_err(full)?;
        Ok(Value::cell(cell))
    }

    fn err_nil(&mut self) -> Result<Value, Stop> {
        let cell = self
            .heap
            .ctor(self.code.abi.err, &[Value::NIL])
            .map_err(full)?;
        Ok(Value::cell(cell))
    }

    /// The environment as a map. When a name is listed twice, the first
    /// wins, as it does for the OS's own `getenv`.
    fn env_map(&mut self) -> Result<Cell, Full> {
        let mut env = map::empty(&mut self.heap)?;
        for (name, value) in self.host.env() {
            let k = Value::cell(self.heap.string(name.as_bytes())?);
            let next = if map::get(&self.heap, env, k).is_some() {
                None
            } else {
                let v = Value::cell(self.heap.string(value.as_bytes())?);
                let next = map::set(&mut self.heap, env, k, v)?;
                self.release(v);
                Some(next)
            };
            self.release(k);
            if let Some(next) = next {
                self.heap.release(env);
                env = next;
            }
        }
        Ok(env)
    }

    fn io_errors(&self) -> Result<IoErrors, Stop> {
        self.code.abi.io.ok_or_else(|| {
            Stop::BadProgram("a file built-in, in a program with no `scarlet/io.IoError`".into())
        })
    }

    /// `Err` of the `IoError` `e` stands for, about `path`.
    fn io_error(&mut self, e: &std::io::Error, path: &[u8]) -> Result<Value, Stop> {
        let io = self.io_errors()?;
        let about_path = match e.raw_os_error() {
            Some(libc::ENOENT) => Some(io.not_found),
            Some(libc::EACCES) => Some(io.permission_denied),
            Some(libc::EEXIST) => Some(io.already_exists),
            Some(libc::ENOTDIR) => Some(io.not_a_directory),
            Some(libc::EISDIR) => Some(io.is_a_directory),
            Some(libc::EROFS) => Some(io.read_only_filesystem),
            Some(libc::ELOOP) => Some(io.filesystem_loop),
            Some(libc::EFBIG) => Some(io.file_too_large),
            _ => None,
        };
        let error = match (about_path, e.raw_os_error()) {
            (Some(variant), _) => {
                let path = Value::cell(self.heap.string(path).map_err(full)?);
                Value::cell(self.heap.ctor(variant, &[path]).map_err(full)?)
            }
            (None, Some(libc::ENOSPC)) => Value::nullary(io.storage_full),
            (None, Some(libc::EDQUOT)) => Value::nullary(io.quota_exceeded),
            // `-1` for an error that did not come from the OS.
            (None, code) => {
                let code =
                    bigint::value(&mut self.heap, code.unwrap_or(-1).into()).map_err(full)?;
                Value::cell(self.heap.ctor(io.errno, &[code]).map_err(full)?)
            }
        };
        self.err(error)
    }

    fn json_types(&self) -> Result<JsonTypes, Stop> {
        self.code.abi.json.ok_or_else(|| {
            Stop::BadProgram("a JSON built-in, in a program with no `scarlet/json` types".into())
        })
    }

    /// The parts of the `Doc` `v`. Only `json.parse` makes one, so anything
    /// else is a program the compiler should not have let through.
    fn doc(&self, v: Value) -> Result<Doc, Stop> {
        let types = self.json_types()?;
        let not_a_doc = || Stop::BadProgram(format!("a JSON read of {v:?}, which is not a `Doc`"));
        let cell = v.as_cell().ok_or_else(not_a_doc)?;
        if self.heap.kind(cell) != Some(Kind::Ctor) || self.heap.variant(cell) != types.doc {
            return Err(not_a_doc());
        }
        let field = |i| self.heap.field(cell, i).ok_or_else(not_a_doc);
        let (arena, tape) = (field(0)?, field(1)?);
        // A node past any tape, for an index no tape has: every read of it is
        // `None`.
        let idx = match self.int_of(field(2)?)? {
            Int::Small(n) => usize::try_from(n).unwrap_or(usize::MAX),
            Int::Big(_) => usize::MAX,
        };
        Ok(Doc { arena, tape, idx })
    }

    fn tape(&self, d: &Doc) -> Result<json::Tape<'_>, Stop> {
        Ok(json::Tape {
            heap: &self.heap,
            tape: self.binary(d.tape)?,
            arena: self.binary(d.arena)?,
        })
    }

    /// A `Doc` over the same document as `d`, at node `idx`.
    fn doc_at(&mut self, d: &Doc, idx: usize) -> Result<Value, Stop> {
        let types = self.json_types()?;
        let idx = bigint::value(&mut self.heap, idx.into()).map_err(full)?;
        let (arena, tape) = (self.share(d.arena), self.share(d.tape));
        let cell = self
            .heap
            .ctor(types.doc, &[arena, tape, idx])
            .map_err(full)?;
        Ok(Value::cell(cell))
    }

    /// The `scarlet/json.Json` tree `root`, as JSON text. The walk is a list
    /// of what is still to write rather than recursion, so a tree nested a
    /// million deep encodes without overflowing the stack.
    fn encode_json(&self, root: Value) -> Result<Vec<u8>, Stop> {
        enum Step {
            Value(Value),
            Key(Value),
            Text(&'static [u8]),
        }
        let types = self.json_types()?;
        let mut out = Vec::new();
        let mut todo = vec![Step::Value(root)];
        while let Some(step) = todo.pop() {
            let v = match step {
                Step::Text(t) => {
                    out.extend_from_slice(t);
                    continue;
                }
                Step::Key(k) => {
                    json::write_string(&mut out, &String::from_utf8_lossy(&self.text_of(k)?));
                    continue;
                }
                Step::Value(v) => v,
            };
            let not_json = || Stop::BadProgram(format!("{v:?} as a `Json` to encode"));
            let cell = match v.view() {
                View::Nullary(n) if n == types.null => {
                    out.extend_from_slice(b"null");
                    continue;
                }
                View::Cell(cell) if self.heap.kind(cell) == Some(Kind::Ctor) => cell,
                View::Nullary(_)
                | View::Cell(_)
                | View::Int(_)
                | View::Float(_)
                | View::Nil
                | View::Bool(_)
                | View::Func(_) => return Err(not_json()),
            };
            let variant = self.heap.variant(cell);
            let inner = self.heap.field(cell, 0).ok_or_else(not_json)?;
            if variant == types.boolean {
                let yes = matches!(inner.view(), View::Bool(true));
                out.extend_from_slice(if yes { b"true" } else { b"false" });
            } else if variant == types.integer {
                let n = self.int_of(inner)?.to_string();
                out.extend_from_slice(n.as_bytes());
            } else if variant == types.real {
                json::write_float(&mut out, self.float_of(inner)?);
            } else if variant == types.str {
                json::write_string(&mut out, &String::from_utf8_lossy(&self.text_of(inner)?));
            } else if variant == types.number {
                // Text that is not a JSON number is `null`, rather than
                // bytes that stop the whole document parsing at the far end.
                let digits = self.text_of(inner)?;
                if json::is_number(&digits) {
                    out.extend_from_slice(&digits);
                } else {
                    out.extend_from_slice(b"null");
                }
            } else if variant == types.list || variant == types.object {
                let Seq::Tree(items) = self.seq(inner)? else {
                    return Err(not_json());
                };
                let items = array::elements(&self.heap, items);
                let object = variant == types.object;
                out.push(if object { b'{' } else { b'[' });
                todo.push(Step::Text(if object { b"}" } else { b"]" }));
                // Pushed last to first, so they come off in order, each
                // after the comma before it.
                for (i, item) in items.into_iter().enumerate().rev() {
                    if object {
                        let pair = item
                            .as_cell()
                            .filter(|&c| self.heap.kind(c) == Some(Kind::Tuple));
                        let pair = pair.ok_or_else(not_json)?;
                        let (Some(k), Some(value)) =
                            (self.heap.element(pair, 0), self.heap.element(pair, 1))
                        else {
                            return Err(not_json());
                        };
                        todo.push(Step::Value(value));
                        todo.push(Step::Text(b":"));
                        todo.push(Step::Key(k));
                    } else {
                        todo.push(Step::Value(item));
                    }
                    if i > 0 {
                        todo.push(Step::Text(b","));
                    }
                }
            } else {
                return Err(not_json());
            }
        }
        Ok(out)
    }

    fn http_types(&self) -> Result<HttpTypes, Stop> {
        self.code.abi.http.ok_or_else(|| {
            Stop::BadProgram(
                "an HTTP built-in, in a program with no `scarlet/http/h1` types".into(),
            )
        })
    }

    /// The bytes of the binary `v`, the last one padded with zero bits.
    fn bytes_of(&self, v: Value) -> Result<Vec<u8>, Stop> {
        Ok(binary::bytes(&self.heap, self.binary(v)?))
    }

    /// The byte offset `v` into `buf`, as `h1`'s offsets are: clamped to the
    /// buffer.
    fn offset(&self, v: Value, buf: Bits) -> Result<usize, Stop> {
        let whole = usize::try_from(buf.len / 8).unwrap_or(usize::MAX);
        Ok(match self.int_of(v)? {
            Int::Small(n) => usize::try_from(n).unwrap_or(0).min(whole),
            Int::Big(n) if n.sign() == Sign::Minus => 0,
            Int::Big(_) => whole,
        })
    }

    /// The name and value of each `Header` in the `Headers` array `v`.
    fn headers(&self, v: Value) -> Result<Vec<(Value, Value)>, Stop> {
        let types = self.http_types()?;
        let not_headers = || Stop::BadProgram(format!("{v:?} as `Headers`"));
        let items = match self.seq(v)? {
            Seq::Tree(t) => array::elements(&self.heap, t),
            Seq::Range { start, end } if start >= end => Vec::new(),
            Seq::Range { .. } => return Err(not_headers()),
        };
        let mut out = Vec::with_capacity(items.len());
        for h in items {
            let cell = h.as_cell().ok_or_else(not_headers)?;
            if self.heap.kind(cell) != Some(Kind::Ctor) || self.heap.variant(cell) != types.header {
                return Err(not_headers());
            }
            let (Some(name), Some(value)) = (self.heap.field(cell, 0), self.heap.field(cell, 1))
            else {
                return Err(not_headers());
            };
            out.push((name, value));
        }
        Ok(out)
    }

    /// Bytes `range` of the head at byte `off` of `buf`, as a slice of it.
    fn view(
        &mut self,
        buf: Bits,
        off: usize,
        range: &std::ops::Range<usize>,
    ) -> Result<Value, Stop> {
        let from = (off + range.start) as u64 * 8;
        let cell =
            binary::slice(&mut self.heap, buf, from, range.len() as u64 * 8).map_err(full)?;
        Ok(Value::cell(cell))
    }

    /// A header block's fields, as `Headers` of slices of `buf`.
    fn fields(&mut self, buf: Bits, off: usize, block: &http::Block) -> Result<Value, Stop> {
        let types = self.http_types()?;
        let mut items = Vec::with_capacity(block.fields.len());
        for f in &block.fields {
            let name = self.view(buf, off, &f.name)?;
            let value = self.view(buf, off, &f.value)?;
            items.push(Value::cell(
                self.heap.ctor(types.header, &[name, value]).map_err(full)?,
            ));
        }
        Ok(Value::cell(
            array::from_values(&mut self.heap, &items).map_err(full)?,
        ))
    }

    /// The `HeadFlags` a block's tokens make.
    fn head_flags(&mut self, flags: http::HeadFlags) -> Result<Value, Stop> {
        let types = self.http_types()?;
        let conn = match flags.conn {
            http::ConnTokens::Neither => types.conn_neither,
            http::ConnTokens::Close => types.conn_close,
            http::ConnTokens::KeepAlive => types.conn_keep_alive,
            http::ConnTokens::Both => types.conn_both,
        };
        let fields = [Value::nullary(conn), Value::bool(flags.expect_100_continue)];
        Ok(Value::cell(
            self.heap.ctor(types.head_flags, &fields).map_err(full)?,
        ))
    }

    /// `variant(status)`: a refusal carrying the HTTP status it answers with.
    fn with_status(&mut self, variant: VariantRef, r: http::Reject) -> Result<Value, Stop> {
        let status = Value::int(r as i64).unwrap_or(Value::NIL);
        Ok(Value::cell(
            self.heap.ctor(variant, &[status]).map_err(full)?,
        ))
    }

    /// The offset `off + n` into `buf`, as an Int.
    fn consumed(&mut self, off: usize, n: usize) -> Result<Value, Stop> {
        bigint::value(&mut self.heap, (off + n).into()).map_err(full)
    }

    /// `h1.Parsed`, for the request head at byte `off` of `buf`, whose bytes
    /// from there are `window`.
    fn parsed_request(&mut self, buf: Bits, off: usize, window: &[u8]) -> Result<Value, Stop> {
        let types = self.http_types()?;
        match http::request(window) {
            http::Request::NeedMore => Ok(Value::nullary(types.parsed_need_more)),
            http::Request::Bad(r) => self.with_status(types.parsed_bad, r),
            http::Request::Done {
                method,
                target,
                http11,
                head,
            } => {
                let method = self.view(buf, off, &method)?;
                let target = self.view(buf, off, &target)?;
                let version = Value::nullary(if http11 { types.http11 } else { types.http10 });
                let headers = self.fields(buf, off, &head)?;
                let flags = self.head_flags(head.flags)?;
                let consumed = self.consumed(off, head.end)?;
                let fields = [method, target, version, headers, flags, consumed];
                Ok(Value::cell(
                    self.heap.ctor(types.parsed_done, &fields).map_err(full)?,
                ))
            }
        }
    }

    /// `h1.ParsedResponse`, as [`Self::parsed_request`].
    fn parsed_response(&mut self, buf: Bits, off: usize, window: &[u8]) -> Result<Value, Stop> {
        let types = self.http_types()?;
        let bad = |why: http::BadResponse| match why {
            http::BadResponse::StatusLine => types.bad_status_line,
            http::BadResponse::Version => types.bad_version,
            http::BadResponse::Field => types.bad_field,
            http::BadResponse::TooLarge => types.head_too_large,
        };
        match http::response(window) {
            http::Response::NeedMore => Ok(Value::nullary(types.response_need_more)),
            http::Response::Bad(why) => {
                let why = Value::nullary(bad(why));
                Ok(Value::cell(
                    self.heap.ctor(types.response_bad, &[why]).map_err(full)?,
                ))
            }
            http::Response::Done {
                http11,
                code,
                reason,
                head,
            } => {
                let version = Value::nullary(if http11 { types.http11 } else { types.http10 });
                let code = Value::int(i64::from(code)).unwrap_or(Value::NIL);
                let reason = self.view(buf, off, &reason)?;
                let headers = self.fields(buf, off, &head)?;
                let flags = self.head_flags(head.flags)?;
                let consumed = self.consumed(off, head.end)?;
                let fields = [version, code, reason, headers, flags, consumed];
                Ok(Value::cell(
                    self.heap.ctor(types.response_done, &fields).map_err(full)?,
                ))
            }
        }
    }

    /// `h1.ChunkBody`, for the chunked body at byte `off` of `buf`, whose
    /// bytes from there are `bytes`. The body is copied out once, when all of
    /// it has arrived.
    fn chunk_body(&mut self, buf: Bits, off: usize, bytes: &[u8], max: u64) -> Result<Value, Stop> {
        let types = self.http_types()?;
        match http::chunked(bytes, max) {
            http::Chunked::NeedMore => Ok(Value::nullary(types.chunked_need_more)),
            http::Chunked::Bad(r) => self.with_status(types.chunked_bad, r),
            http::Chunked::Done { pieces, trailers } => {
                let mut body = Vec::with_capacity(pieces.iter().map(|p| p.len()).sum());
                for p in &pieces {
                    body.extend_from_slice(bytes.get(p.clone()).unwrap_or(&[]));
                }
                let body = binary::make(&mut self.heap, &body, body.len() as u64 * 8);
                let body = Value::cell(body.map_err(full)?);
                let trailer_fields = self.fields(buf, off, &trailers)?;
                let consumed = self.consumed(off, trailers.end)?;
                let fields = [body, trailer_fields, consumed];
                Ok(Value::cell(
                    self.heap.ctor(types.chunked_done, &fields).map_err(full)?,
                ))
            }
        }
    }

    /// The bytes of the binary `v`, or `None` when it is not whole bytes.
    fn whole_bytes(&self, v: Value) -> Result<Option<Vec<u8>>, Stop> {
        let b = self.binary(v)?;
        Ok((b.len % 8 == 0).then(|| binary::bytes(&self.heap, b)))
    }

    /// `Ok` of a new binary holding `bytes`.
    fn ok_bytes(&mut self, bytes: &[u8]) -> Result<Value, Stop> {
        let cell = binary::make(&mut self.heap, bytes, bytes.len() as u64 * 8).map_err(full)?;
        self.ok(Value::cell(cell))
    }

    /// The map `v` is.
    fn map(&self, v: Value) -> Result<Cell, Stop> {
        v.as_cell()
            .filter(|&cell| self.heap.kind(cell) == Some(Kind::Map))
            .ok_or_else(|| {
                Stop::BadProgram(format!("a map operation on {v:?}, which is not a map"))
            })
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

    fn float(&self, base: usize, r: Reg) -> Result<f64, Stop> {
        self.float_of(self.get(base, r))
    }

    fn float_of(&self, v: Value) -> Result<f64, Stop> {
        match v.view() {
            View::Float(f) => Ok(f),
            v @ (View::Int(_)
            | View::Nil
            | View::Bool(_)
            | View::Func(_)
            | View::Cell(_)
            | View::Nullary(_)) => Err(Stop::BadProgram(format!("a Float operation on {v:?}"))),
        }
    }

    /// `a op b` for an operator whose operands are both Ints, both Floats, or,
    /// for `+`, both strings.
    fn num(&mut self, op: NumOp, a: Value, b: Value) -> Result<Value, Stop> {
        if let (View::Float(x), View::Float(y)) = (a.view(), b.view()) {
            return Ok(float::op(op, x, y));
        }
        if op == NumOp::Add && self.is_string(a) && self.is_string(b) {
            let mut text = Vec::new();
            for s in [a, b] {
                if let Some(cell) = s.as_cell() {
                    self.heap.read_string(cell, &mut text);
                }
            }
            return Ok(Value::cell(self.heap.string(&text).map_err(full)?));
        }
        let int = match op {
            NumOp::Add => IntOp::Add,
            NumOp::Sub => IntOp::Sub,
            NumOp::Mul => IntOp::Mul,
            NumOp::Div => IntOp::Div,
            NumOp::Rem => IntOp::Rem,
            NumOp::Lt => IntOp::Lt,
            NumOp::Le => IntOp::Le,
            NumOp::Gt => IntOp::Gt,
            NumOp::Ge => IntOp::Ge,
        };
        let (a, b) = (self.int_of(a)?, self.int_of(b)?);
        bigint::op(&mut self.heap, int, a, b).map_err(full)
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

    /// `v` as `println` shows it, or `None` for `Nil`. Gives up `v`'s
    /// reference.
    pub(crate) fn shown(&mut self, v: Value) -> Result<Option<String>, Stop> {
        if v.bits() == Value::NIL.bits() {
            return Ok(None);
        }
        let mut text = Vec::new();
        let shown = self.show(v, &mut text);
        self.release(v);
        shown?;
        Ok(Some(String::from_utf8_lossy(&text).into_owned()))
    }

    /// Cells not yet freed.
    #[cfg(test)]
    fn live(&self) -> usize {
        self.heap.live()
    }

    /// Release every global, and `os.env`'s map, so a test can see that
    /// nothing else is left.
    #[cfg(test)]
    fn release_globals(&mut self) {
        for v in std::mem::take(&mut self.globals) {
            self.release(v);
        }
        if let Some(env) = self.env.take() {
            self.heap.release(env);
        }
    }
}

/// `x` shifted `count` bits, left (times 2^count, exactly) or right (divided
/// by 2^count, rounded down). A left shift past what an Int can hold is a
/// full heap; a right shift past every bit leaves 0 or -1, by `x`'s sign.
fn shift(
    x: num_bigint::BigInt,
    left: bool,
    count: &num_bigint::BigInt,
) -> Result<num_bigint::BigInt, Stop> {
    if x.sign() == Sign::NoSign {
        return Ok(x);
    }
    let Some(count) = count.to_u64() else {
        return if left {
            Err(Stop::HeapFull)
        } else {
            Ok(if x.sign() == Sign::Minus {
                (-1).into()
            } else {
                0.into()
            })
        };
    };
    if left {
        if x.bits().saturating_add(count) > bigint::MAX_BITS {
            return Err(Stop::HeapFull);
        }
        Ok(x << count)
    } else if count > x.bits() {
        Ok(if x.sign() == Sign::Minus {
            (-1).into()
        } else {
            0.into()
        })
    } else {
        Ok(x >> count)
    }
}

fn ready(f: &Func) -> Result<&Body, Stop> {
    match f {
        Func::Ready(body) => Ok(body),
        Func::NotBuiltYet(what) => Err(Stop::NotBuiltYet(what.clone())),
    }
}

/// A `scarlet/json.Doc`, taken apart: the binaries it holds, borrowed, and
/// the node it points at.
struct Doc {
    arena: Value,
    tape: Value,
    idx: usize,
}

/// The path a string names. A Scarlet string is always UTF-8, so nothing is
/// lost on the way.
fn path_of(text: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(text).into_owned())
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
        cells_left_after_in(&Host::new(Vec::new(), Vec::new()), src)
    }

    fn cells_left_after_in(host: &Host, src: &str) -> (String, usize) {
        let mut scanner = scarlet_core::scanner::new_scanner(src.to_string());
        let parsed = scarlet_core::parser::new_parser(&mut scanner).parse_program();
        let expr = scarlet_core::ast::Expression::BlockExpression(parsed.ast);
        let result = scarlet_core::bytecode::compile(&expr, None);
        assert!(result.success(), "{:?}", result.diagnostics);
        let program = result.into_runnable().expect("a clean compile is runnable");
        let code = crate::code::load(&program);
        let mut out = Vec::new();
        let mut m = Machine::new(&code, host, &mut out);
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

    /// A slice of a binary holds its owner, so the owner lives as long as any
    /// slice of it, and no longer: a binary walked a byte at a time by a
    /// pattern, slices of slices and all, leaves nothing behind.
    #[test]
    fn binaries_and_their_slices_are_all_freed() {
        let (out, left) = cells_left_after(
            "import scarlet/binary\n\
             fn count(b Binary, n Int) Int {\n\
             \tmatch b {\n\
             \t\t<<_, rest:binary>> -> count(rest, n + 1)\n\
             \t\t_ -> n\n\
             \t}\n\
             }\n\
             fn grow(n Int, acc Binary) Binary {\n\
             \tif n == 0 { acc } else { grow(n - 1, <<acc:binary, n:size(8)>>) }\n\
             }\n\
             pub fn main() {\n\
             \tb = grow(1000, <<>>)\n\
             \tprintln(count(b, 0))\n\
             \tkept = binary.slice_bits(b, 8, 16)\n\
             \tprintln(kept)\n\
             }\n",
        );
        assert_eq!(out, "1000\nOk(<<231, 230>>)\n");
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

    /// `os.env`'s map is made once and shared, and what the host built-ins
    /// hand out, errors included, holds its own references. All of it is
    /// freed when the run ends.
    #[test]
    fn what_the_host_built_ins_make_is_all_freed() {
        let env = vec![("A".into(), "1".into()), ("B".into(), "2".into())];
        let host = Host::new(vec!["main.scrl".into(), "x".into()], env);
        let (out, left) = cells_left_after_in(
            &host,
            "import scarlet/os\n\
             import scarlet/io\n\
             import scarlet/crypto\n\
             import scarlet/map\n\
             pub fn main() {\n\
             \tprintln(os.get_env('A'))\n\
             \tprintln(map.size(os.env()))\n\
             \tprintln(os.argv())\n\
             \tprintln(match io.read_file('/no/such/file') {\n\
             \t\tOk(_) -> 'read'\n\
             \t\tErr(_) -> 'failed'\n\
             \t})\n\
             \tprintln(match crypto.random_bytes(40) {\n\
             \t\tOk(_) -> 'random'\n\
             \t\tErr(Nil) -> 'none'\n\
             \t})\n\
             }\n",
        );
        assert_eq!(out, "Some(1)\n2\n[main.scrl, x]\nfailed\nrandom\n");
        assert_eq!(left, 0);
    }

    /// Every `Doc` a read hands out shares its document's tape and strings,
    /// and a parse error holds its own message. When the run ends, all of it
    /// is freed.
    #[test]
    fn what_json_makes_is_all_freed() {
        let (out, left) = cells_left_after(
            "import scarlet/json\n\
             import scarlet/json.{Object, Str, List, Integer}\n\
             pub fn main() {\n\
             \tmatch json.parse('{\"a\": [1, {\"b\": \"deep\"}], \"c\": \"x\"}') {\n\
             \t\tOk(d) -> {\n\
             \t\t\tprintln(json.size(json.field(d, 'a') or d))\n\
             \t\t\t_entries = json.entries(d)\n\
             \t\t\t_elements = json.elements(json.field(d, 'a') or d)\n\
             \t\t\tprintln(json.reencode(d))\n\
             \t\t}\n\
             \t\tErr(_) -> println('no')\n\
             \t}\n\
             \tprintln(match json.parse('[') {\n\
             \t\tOk(_) -> 'parsed'\n\
             \t\tErr(_) -> 'refused'\n\
             \t})\n\
             \tprintln(json.encode(Object([('k', List([Integer(1), Str('v')]))])))\n\
             }\n",
        );
        assert_eq!(
            out,
            "2\n{\"a\":[1,{\"b\":\"deep\"}],\"c\":\"x\"}\nrefused\n{\"k\":[1,\"v\"]}\n"
        );
        assert_eq!(left, 0);
    }

    /// A parsed head's method, target and fields are slices of the buffer
    /// read, sharing it, and a decoded chunked body is one copy. When the run
    /// ends, all of it is freed.
    #[test]
    fn what_http_parsing_makes_is_all_freed() {
        let (out, left) = cells_left_after(
            "import scarlet/http/h1.{Done, ChunkedDone}\n\
             import scarlet/http/h1\n\
             import scarlet/http/headers\n\
             import scarlet/binary\n\
             pub fn main() {\n\
             \treq = <<'POST /x HTTP/1.1\\r\\nHost: a\\r\\nTransfer-Encoding: chunked\\r\\n\\r\\n3\\r\\nabc\\r\\n0\\r\\n\\r\\n'>>\n\
             \tmatch h1.parse_request(req, 0) {\n\
             \t\tDone(method, _, _, hs, _, consumed) -> {\n\
             \t\t\tprintln(binary.to_string(method))\n\
             \t\t\tprintln(headers.get(hs, <<'host'>>))\n\
             \t\t\tprintln(h1.framing(hs))\n\
             \t\t\tprintln(binary.byte_size(h1.serialize_head(200, hs)))\n\
             \t\t\tmatch h1.chunk_decode(req, consumed, 100) {\n\
             \t\t\t\tChunkedDone(body, _, _) -> println(body)\n\
             \t\t\t\t_ -> println('no body')\n\
             \t\t\t}\n\
             \t\t}\n\
             \t\t_ -> println('no head')\n\
             \t}\n\
             }\n",
        );
        assert_eq!(out, "Ok(POST)\nSome(<<97>>)\nChunked\n56\n<<97, 98, 99>>\n");
        assert_eq!(left, 0);
    }

    /// Every version of a map shares nodes with the one it came from, and
    /// what a lookup or a listing hands out holds its own references. When
    /// the run ends, all of it is freed.
    #[test]
    fn maps_and_what_they_hold_are_all_freed() {
        let (out, left) = cells_left_after(
            "import scarlet/map\n\
             fn fill(m map.Map(String, String), n Int) map.Map(String, String) {\n\
             \tif n == 0 { m } else { fill(map.set(m, 'k${n}', 'v${n}'), n - 1) }\n\
             }\n\
             fn drain(m map.Map(String, String), n Int) map.Map(String, String) {\n\
             \tif n == 0 { m } else { drain(map.delete(m, 'k${n}'), n - 2) }\n\
             }\n\
             pub fn main() {\n\
             \tfull = fill(map.new(), 400)\n\
             \thalf = drain(full, 400)\n\
             \tprintln('${map.size(full)} ${map.size(half)}')\n\
             \tprintln(map.get(half, 'k7'))\n\
             \tprintln(map.get(half, 'k8'))\n\
             \t_keys = map.keys(half)\n\
             \t_pairs = map.to_list(map.set(half, 'k7', 'again'))\n\
             \tprintln(full == half)\n\
             \tnested = map.set(map.new(), half, [full])\n\
             \tprintln(map.has(nested, drain(full, 400)))\n\
             }\n",
        );
        assert_eq!(out, "400 200\nSome(v7)\nNone\nFalse\nTrue\n");
        assert_eq!(left, 0);
    }

    /// A field held in different slots is read through a `match` on the
    /// variant. The strings read, the values read from, and the ones a spread
    /// builds are all freed.
    #[test]
    fn fields_read_by_name_are_all_freed() {
        let (out, left) = cells_left_after(
            "type S {\n\tA(name String, n Int, tag String)\n\tB(tag String, name String)\n}\n\
             fn both(s S) String { '${s.name}/${s.tag}' }\n\
             pub fn main() {\n\
             \tb = B(tag: 'x', name: 'bee')\n\
             \ta = A(..b, n: 1)\n\
             \tprintln(both(b))\n\
             \tprintln(both(a))\n\
             \tprintln(both(A(..a, name: 'aye', n: 2)))\n\
             }\n",
        );
        assert_eq!(out, "bee/x\nbee/x\naye/x\n");
        assert_eq!(left, 0);
    }
}
