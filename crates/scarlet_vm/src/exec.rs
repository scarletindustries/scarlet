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

use scarlet_ir::core_ir::FuncIdx;

use crate::Stop;
use crate::code::{Body, Code, Func, Instr, IntOp, Reg};
use crate::heap::{Full, Heap, Kind};
use crate::value::{Value, View};

struct Frame<'c> {
    body: &'c Body,
    pc: usize,
    base: usize,
    /// The caller's register the result goes to. `None` for the frame a run
    /// started with.
    ret_to: Option<Reg>,
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
        }];
        loop {
            let Some(frame) = frames.last_mut() else {
                return Ok(Value::NIL);
            };
            let Some(instr) = frame.body.instrs.get(frame.pc) else {
                return Err(Stop::NotBuiltYet(format!(
                    "{} running past its end",
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
                    let v = int_op(*op, self.int(base, *a)?, self.int(base, *b)?)?;
                    self.set(base, *dst, v);
                }
                Instr::IntNeg { dst, a } => {
                    let v = small(self.int(base, *a)?.checked_neg())?;
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
                Instr::Drop { reg } => self.set(base, *reg, Value::NIL),
                Instr::Call { dst, func, args } => {
                    let callee = self.body(*func)?;
                    let values: Vec<Value> = args
                        .iter()
                        .map(|r| self.share(self.get(base, *r)))
                        .collect();
                    let new_base = base + frame.body.regs as usize;
                    let ret_to = Some(*dst);
                    self.enter(callee, new_base, &values);
                    frames.push(Frame {
                        body: callee,
                        pc: 0,
                        base: new_base,
                        ret_to,
                    });
                }
                Instr::TailCall { func, args } => {
                    let callee = self.body(*func)?;
                    let values: Vec<Value> = args
                        .iter()
                        .map(|r| self.share(self.get(base, *r)))
                        .collect();
                    // The callee takes over this frame, so a loop written as
                    // tail recursion runs in constant space.
                    self.enter(callee, base, &values);
                    frame.body = callee;
                    frame.pc = 0;
                }
                Instr::Jump { to } => frame.pc = *to as usize,
                Instr::JumpIfFalse { cond, to } => match self.get(base, *cond).view() {
                    View::Bool(true) => {}
                    View::Bool(false) => frame.pc = *to as usize,
                    v @ (View::Float(_)
                    | View::Int(_)
                    | View::Nil
                    | View::Func(_)
                    | View::Cell(_)) => {
                        return Err(Stop::NotBuiltYet(format!("a branch on {v:?}")));
                    }
                },
                Instr::Ret { src } => {
                    let v = self.share(self.get(base, *src));
                    let ret_to = frame.ret_to;
                    frames.pop();
                    self.leave(base);
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

    fn int(&self, base: usize, r: Reg) -> Result<i64, Stop> {
        match self.get(base, r).view() {
            View::Int(n) => Ok(n),
            v @ (View::Float(_) | View::Nil | View::Bool(_) | View::Func(_) | View::Cell(_)) => {
                Err(Stop::NotBuiltYet(format!("an Int operation on {v:?}")))
            }
        }
    }

    /// How `println` and `${x}` show a value.
    fn show(&self, v: Value, out: &mut Vec<u8>) -> Result<(), Stop> {
        match v.view() {
            View::Int(n) => out.extend_from_slice(n.to_string().as_bytes()),
            View::Nil => out.extend_from_slice(b"Nil"),
            View::Bool(true) => out.extend_from_slice(b"True"),
            View::Bool(false) => out.extend_from_slice(b"False"),
            View::Cell(cell) if self.is_string(v) => self.heap.read_string(cell, out),
            View::Cell(_) => return Err(Stop::NotBuiltYet("printing this value".into())),
            View::Float(_) => return Err(Stop::NotBuiltYet("printing a Float".into())),
            View::Func(_) => return Err(Stop::NotBuiltYet("printing a function".into())),
        }
        Ok(())
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

/// `docs/semantics.md`'s Int rules: `/` truncates toward zero and `x / 0` is
/// 0; `%` takes the dividend's sign and `x % 0` is `x`.
fn int_op(op: IntOp, a: i64, b: i64) -> Result<Value, Stop> {
    Ok(match op {
        IntOp::Add => small(a.checked_add(b))?,
        IntOp::Sub => small(a.checked_sub(b))?,
        IntOp::Mul => small(a.checked_mul(b))?,
        IntOp::Div => small(if b == 0 { Some(0) } else { a.checked_div(b) })?,
        IntOp::Rem => small(if b == 0 { Some(a) } else { a.checked_rem(b) })?,
        IntOp::Eq => Value::bool(a == b),
        IntOp::Ne => Value::bool(a != b),
        IntOp::Lt => Value::bool(a < b),
        IntOp::Le => Value::bool(a <= b),
        IntOp::Gt => Value::bool(a > b),
        IntOp::Ge => Value::bool(a >= b),
    })
}

/// An Int result as a value. Past a small Int's range, the exact answer needs
/// a big int, which the VM does not have yet. Stopping says so; wrapping would
/// print a wrong number.
fn small(n: Option<i64>) -> Result<Value, Stop> {
    n.and_then(Value::int)
        .ok_or_else(|| Stop::NotBuiltYet("Int beyond 48 bits (big ints)".into()))
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
