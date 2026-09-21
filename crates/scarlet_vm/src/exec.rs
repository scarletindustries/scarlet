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

use crate::Stop;
use crate::bigint::{self, Int};
use crate::code::{Body, Code, Func, Instr, Reg};
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
        match self.get(base, r).view() {
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
        show::show(&self.heap, &self.code.types, v, out)
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
