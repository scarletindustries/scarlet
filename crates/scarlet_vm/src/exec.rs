//! The interpreter: runs loaded instructions.
//!
//! Frames live on a stack the VM owns, not on Rust's, so a deep recursion in
//! a Scarlet program uses memory and never overflows anything. All frames'
//! registers sit in one vector: a frame is a function, where it is in that
//! function, and where its registers start.

use std::io::Write;

use scarlet_ir::core_ir::FuncIdx;

use crate::Stop;
use crate::code::{Body, Code, Func, Instr, IntOp, Reg};
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
    globals: Vec<Value>,
    regs: Vec<Value>,
}

impl<'c, 'o> Machine<'c, 'o> {
    pub(crate) fn new(code: &'c Code, out: &'o mut dyn Write) -> Self {
        Machine {
            code,
            out,
            globals: vec![Value::NIL; code.globals as usize],
            regs: Vec::new(),
        }
    }

    /// Run every module's toplevel in order, then `main` if the program has
    /// one. The result is the last thing run's value: `main`'s, or the entry
    /// toplevel's for a script.
    pub(crate) fn run(&mut self) -> Result<Value, Stop> {
        let mut last = Value::NIL;
        for top in &self.code.toplevels {
            last = self.call(ready(top)?, &[])?;
        }
        if let Some(main) = self.code.main {
            last = self.call(self.body(main)?, &[])?;
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

    /// Call `body` with `args` and run until it returns.
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
                Instr::Move { dst, src } => {
                    let v = self.get(base, *src);
                    self.set(base, *dst, v);
                }
                Instr::GetGlobal { dst, slot } => {
                    let v = self
                        .globals
                        .get(slot.0 as usize)
                        .copied()
                        .unwrap_or(Value::NIL);
                    self.set(base, *dst, v);
                }
                Instr::SetGlobal { slot, src } => {
                    let v = self.get(base, *src);
                    if let Some(g) = self.globals.get_mut(slot.0 as usize) {
                        *g = v;
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
                    let text = show(self.get(base, *arg))?;
                    if writeln!(self.out, "{text}").is_err() {
                        return Err(Stop::OutputClosed);
                    }
                    self.set(base, *dst, Value::NIL);
                }
                Instr::Call { dst, func, args } => {
                    let callee = self.body(*func)?;
                    let values: Vec<Value> = args.iter().map(|r| self.get(base, *r)).collect();
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
                    let values: Vec<Value> = args.iter().map(|r| self.get(base, *r)).collect();
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
                    v @ (View::Float(_) | View::Int(_) | View::Nil | View::Func(_)) => {
                        return Err(Stop::NotBuiltYet(format!("a branch on {v:?}")));
                    }
                },
                Instr::Ret { src } => {
                    let v = self.get(base, *src);
                    let ret_to = frame.ret_to;
                    frames.pop();
                    self.regs.truncate(base);
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
    fn enter(&mut self, body: &Body, base: usize, args: &[Value]) {
        self.regs.truncate(base);
        self.regs.resize(base + body.regs as usize, Value::NIL);
        for (param, arg) in body.params.iter().zip(args) {
            self.set(base, *param, *arg);
        }
    }

    fn get(&self, base: usize, r: Reg) -> Value {
        self.regs
            .get(base + r.0 as usize)
            .copied()
            .unwrap_or(Value::NIL)
    }

    fn set(&mut self, base: usize, r: Reg, v: Value) {
        if let Some(slot) = self.regs.get_mut(base + r.0 as usize) {
            *slot = v;
        }
    }

    fn int(&self, base: usize, r: Reg) -> Result<i64, Stop> {
        match self.get(base, r).view() {
            View::Int(n) => Ok(n),
            v @ (View::Float(_) | View::Nil | View::Bool(_) | View::Func(_)) => {
                Err(Stop::NotBuiltYet(format!("an Int operation on {v:?}")))
            }
        }
    }
}

fn ready(f: &Func) -> Result<&Body, Stop> {
    match f {
        Func::Ready(body) => Ok(body),
        Func::NotBuiltYet(what) => Err(Stop::NotBuiltYet(what.clone())),
    }
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

/// How `println` shows a value.
fn show(v: Value) -> Result<String, Stop> {
    Ok(match v.view() {
        View::Int(n) => n.to_string(),
        View::Nil => "Nil".into(),
        View::Bool(true) => "True".into(),
        View::Bool(false) => "False".into(),
        View::Float(_) => return Err(Stop::NotBuiltYet("printing a Float".into())),
        View::Func(_) => return Err(Stop::NotBuiltYet("printing a function".into())),
    })
}
