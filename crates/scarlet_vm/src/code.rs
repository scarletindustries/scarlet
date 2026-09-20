//! The VM's instructions, and the loader that makes them from Core IR.
//!
//! A function's Core IR becomes a flat list of register instructions, once,
//! when the program loads. A register is a Core IR local: `let %2 = IntAdd(%0,
//! %1)` becomes `IntAdd { dst: 2, a: 0, b: 1 }`. Each construct maps to one
//! instruction or a few, so the listing reads like the Core IR it came from.
//!
//! The VM is being built one feature at a time. A function that uses
//! something not built yet still loads, marked with what it needs, and only a
//! call to it stops the run. So a program runs as far as the VM has come.

use scarlet_ir::core_ir::{
    Atom, Callee, Const, CoreExpr, CoreFn, FuncIdx, GlobalSlot, Load, LocalId, LoweredFn, PrimOp,
    Program,
};
use scarlet_ir::intrinsic::Intrinsic;
use scarlet_ir::tivec::{Idx, TiVec};

use crate::value::Value;

/// A register: an index into the running function's frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Reg(pub(crate) u32);

impl Reg {
    fn of(local: LocalId) -> Reg {
        Reg(local.0)
    }
}

/// One instruction. `dst` is the register it writes.
#[derive(Debug, Clone)]
pub(crate) enum Instr {
    Const {
        dst: Reg,
        value: Value,
    },
    Move {
        dst: Reg,
        src: Reg,
    },
    GetGlobal {
        dst: Reg,
        slot: GlobalSlot,
    },
    SetGlobal {
        slot: GlobalSlot,
        src: Reg,
    },
    Int {
        dst: Reg,
        op: IntOp,
        a: Reg,
        b: Reg,
    },
    IntNeg {
        dst: Reg,
        a: Reg,
    },
    Println {
        dst: Reg,
        arg: Reg,
    },
    Call {
        dst: Reg,
        func: FuncIdx,
        args: Box<[Reg]>,
    },
    TailCall {
        func: FuncIdx,
        args: Box<[Reg]>,
    },
    Ret {
        src: Reg,
    },
}

/// A two-operand Int operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IntOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// One loaded function.
#[derive(Debug)]
pub(crate) enum Func {
    Ready(Body),
    /// Uses something the VM does not run yet; this says what.
    NotBuiltYet(String),
}

#[derive(Debug)]
pub(crate) struct Body {
    pub(crate) name: String,
    /// Registers a frame for this function needs.
    pub(crate) regs: u32,
    /// Where each argument goes, in order.
    pub(crate) params: Box<[Reg]>,
    pub(crate) instrs: Box<[Instr]>,
}

/// A whole loaded program.
pub(crate) struct Code {
    pub(crate) fns: TiVec<FuncIdx, Func>,
    /// Each module's toplevel, in the order they run, the entry file's last.
    pub(crate) toplevels: Vec<Func>,
    pub(crate) main: Option<FuncIdx>,
    pub(crate) globals: u32,
}

pub(crate) fn load(program: &Program) -> Code {
    let consts = &program.consts;
    let mut fns = TiVec::new();
    for f in &program.fns {
        fns.push(load_fn(f, consts));
    }
    let toplevels = program
        .inits
        .iter()
        .chain([&program.toplevel])
        .map(|f| load_fn(f, consts))
        .collect();
    Code {
        fns,
        toplevels,
        main: program.main,
        globals: program.globals,
    }
}

fn load_fn(f: &LoweredFn, consts: &[Const]) -> Func {
    match Loader::new(consts, &f.core).body(&f.core) {
        Ok((regs, instrs)) => Func::Ready(Body {
            name: format!("{}.{}", f.module, f.name),
            regs,
            params: f.core.params.iter().map(|p| Reg::of(p.id)).collect(),
            instrs: instrs.into_boxed_slice(),
        }),
        Err(what) => Func::NotBuiltYet(what),
    }
}

struct Loader<'c> {
    consts: &'c [Const],
    instrs: Vec<Instr>,
    /// One past the highest register the function names. A value that no
    /// local holds, like a tail expression's, gets a register from here.
    next: u32,
}

impl<'c> Loader<'c> {
    fn new(consts: &'c [Const], f: &CoreFn) -> Self {
        let mut top = 0;
        for p in &f.params {
            top = top.max(p.id.0 + 1);
        }
        top = top.max(highest_local(&f.body));
        Loader {
            consts,
            instrs: Vec::new(),
            next: top,
        }
    }

    fn body(mut self, f: &CoreFn) -> Result<(u32, Vec<Instr>), String> {
        self.expr(&f.body)?;
        Ok((self.next, self.instrs))
    }

    fn scratch(&mut self) -> Reg {
        let r = Reg(self.next);
        self.next += 1;
        r
    }

    fn expr(&mut self, e: &CoreExpr) -> Result<(), String> {
        let mut e = e;
        loop {
            match e {
                CoreExpr::Let { bind, rhs, body } => {
                    let dst = Reg::of(bind.id);
                    self.atom(dst, rhs)?;
                    if let Some(slot) = bind.global {
                        self.instrs.push(Instr::SetGlobal { slot, src: dst });
                    }
                    e = body;
                }
                // Every value so far is a word with nothing to count, so a
                // drop has nothing to release yet.
                CoreExpr::Drop { body, .. } => e = body,
                CoreExpr::Tail(Atom::Call {
                    callee: Callee::Known(func),
                    args,
                }) => {
                    let args = args.iter().copied().map(Reg::of).collect();
                    self.instrs.push(Instr::TailCall { func: *func, args });
                    return Ok(());
                }
                CoreExpr::Tail(atom) => {
                    let src = self.scratch();
                    self.atom(src, atom)?;
                    self.instrs.push(Instr::Ret { src });
                    return Ok(());
                }
                CoreExpr::LetJoin { .. } => return Err("a branch whose value is used".into()),
                CoreExpr::LetCont { .. } | CoreExpr::Goto(_) => {
                    return Err("pattern matching".into());
                }
                CoreExpr::Match { .. } => return Err("match".into()),
                CoreExpr::If { .. } => return Err("if".into()),
            }
        }
    }

    fn atom(&mut self, dst: Reg, atom: &Atom) -> Result<(), String> {
        let instr = match atom {
            Atom::Local(src) => Instr::Move {
                dst,
                src: Reg::of(*src),
            },
            Atom::Const(c) => Instr::Const {
                dst,
                value: self.constant(c.index())?,
            },
            Atom::Nil => Instr::Const {
                dst,
                value: Value::NIL,
            },
            Atom::Bool(b) => Instr::Const {
                dst,
                value: Value::bool(*b),
            },
            Atom::Load(Load::Global(slot)) => Instr::GetGlobal { dst, slot: *slot },
            Atom::Load(_) => return Err("closures that capture".into()),
            Atom::Closure { func_idx, captures } if captures.is_empty() => Instr::Const {
                dst,
                value: Value::func(*func_idx),
            },
            Atom::Closure { .. } => return Err("closures that capture".into()),
            Atom::Call {
                callee: Callee::Known(func),
                args,
            } => Instr::Call {
                dst,
                func: *func,
                args: args.iter().copied().map(Reg::of).collect(),
            },
            Atom::Call { .. } => return Err("calling a function value".into()),
            Atom::PrimOp { op, args } => self.prim(dst, *op, args)?,
            Atom::Intrinsic {
                intrinsic: Intrinsic::Println,
                args,
            } => match args.as_slice() {
                [arg] => Instr::Println {
                    dst,
                    arg: Reg::of(*arg),
                },
                _ => return Err("println with other than one argument".into()),
            },
            Atom::Intrinsic { intrinsic, .. } => return Err(format!("the built-in {intrinsic:?}")),
            Atom::Ctor { .. } => return Err("constructors".into()),
        };
        self.instrs.push(instr);
        Ok(())
    }

    fn prim(&mut self, dst: Reg, op: PrimOp, args: &[LocalId]) -> Result<Instr, String> {
        let int = |op| -> Result<Instr, String> {
            match args {
                [a, b] => Ok(Instr::Int {
                    dst,
                    op,
                    a: Reg::of(*a),
                    b: Reg::of(*b),
                }),
                _ => Err(format!("{op:?} with other than two arguments")),
            }
        };
        match op {
            PrimOp::IntAdd => int(IntOp::Add),
            PrimOp::IntSub => int(IntOp::Sub),
            PrimOp::IntMul => int(IntOp::Mul),
            PrimOp::IntDiv => int(IntOp::Div),
            PrimOp::IntRem => int(IntOp::Rem),
            PrimOp::IntEq => int(IntOp::Eq),
            PrimOp::IntNe => int(IntOp::Ne),
            PrimOp::IntLt => int(IntOp::Lt),
            PrimOp::IntLe => int(IntOp::Le),
            PrimOp::IntGt => int(IntOp::Gt),
            PrimOp::IntGe => int(IntOp::Ge),
            PrimOp::IntNeg => match args {
                [a] => Ok(Instr::IntNeg {
                    dst,
                    a: Reg::of(*a),
                }),
                _ => Err("IntNeg with other than one argument".into()),
            },
            other => Err(format!("the operation {other:?}")),
        }
    }

    fn constant(&self, i: usize) -> Result<Value, String> {
        match self.consts.get(i) {
            Some(Const::Int(n)) => {
                Value::int(*n).ok_or_else(|| "Int beyond 48 bits (big ints)".to_string())
            }
            Some(Const::Float(_)) => Err("Float".into()),
            Some(Const::String(_)) => Err("String".into()),
            Some(Const::Binary { .. }) => Err("Binary".into()),
            None => Err(format!("constant c{i}, which the program does not have")),
        }
    }
}

/// One past the highest local `e` binds or reads.
fn highest_local(e: &CoreExpr) -> u32 {
    let mut top = 0;
    let mut note = |l: LocalId| top = top.max(l.0 + 1);
    let mut stack = vec![e];
    while let Some(e) = stack.pop() {
        match e {
            CoreExpr::Let { bind, rhs, body } => {
                note(bind.id);
                rhs.for_each_operand(&mut note);
                stack.push(body);
            }
            CoreExpr::LetJoin { bind, join, body } => {
                note(bind.id);
                stack.push(join);
                stack.push(body);
            }
            CoreExpr::LetCont { cont, body, .. } => {
                stack.push(cont);
                stack.push(body);
            }
            CoreExpr::Drop { local, body, .. } => {
                note(*local);
                stack.push(body);
            }
            CoreExpr::Match { scrut, arms, .. } => {
                note(*scrut);
                for (pat, arm) in arms {
                    pat.binds().for_each(|b| note(b.id));
                    stack.push(arm);
                }
            }
            CoreExpr::If {
                cond, then, els, ..
            } => {
                note(*cond);
                stack.push(then);
                stack.push(els);
            }
            CoreExpr::Tail(atom) => atom.for_each_operand(&mut note),
            CoreExpr::Goto(_) => {}
        }
    }
    top
}
