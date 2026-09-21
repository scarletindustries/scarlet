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
//!
//! A `match` becomes one test per arm, in order: each jumps to the next arm's
//! test when its value does not fit the arm's pattern, and otherwise falls
//! into the arm, which reads out the fields the pattern binds. A `LetCont`'s
//! continuation is placed after its body, and a `Goto` is a jump to it.

use std::collections::HashMap;

use scarlet_ir::core_ir::{
    Atom, Callee, Const, CoreExpr, CoreFn, CorePat, FuncIdx, GlobalSlot, JoinId, Load, LocalId,
    LoweredFn, PrimOp, Program, VariantRef,
};
use scarlet_ir::intrinsic::Intrinsic;
use scarlet_ir::tivec::{Idx, TiVec};

use crate::show::Types;
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
    /// A new string holding `text`. Made each time the instruction runs,
    /// until constants get a shared area of their own.
    Str {
        dst: Reg,
        text: Box<[u8]>,
    },
    /// A new big int holding `n`, for a constant past a small Int's range.
    /// Made each time it runs, like [`Instr::Str`].
    BigInt {
        dst: Reg,
        n: i64,
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
    /// The text `${a}` shows.
    ToString {
        dst: Reg,
        a: Reg,
    },
    /// Every string in `parts`, joined in order.
    Concat {
        dst: Reg,
        parts: Box<[Reg]>,
    },
    /// A new cell for constructor `variant`, holding `fields`. A constructor
    /// with no fields is a [`Instr::Const`] instead: it needs no cell.
    Ctor {
        dst: Reg,
        variant: VariantRef,
        fields: Box<[Reg]>,
    },
    /// Perceus's last use of `reg`: give up its reference now rather than at
    /// the frame's end.
    Drop {
        reg: Reg,
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
    /// Continue at instruction `to` of this function.
    Jump {
        to: u32,
    },
    /// Continue at `to` when `cond` is `False`, else at the next instruction.
    JumpIfFalse {
        cond: Reg,
        to: u32,
    },
    /// Continue at `to` unless `src` is constructor `variant`.
    JumpUnlessVariant {
        src: Reg,
        variant: VariantRef,
        to: u32,
    },
    /// Continue at `to` unless `src` is the Int `n`.
    JumpUnlessInt {
        src: Reg,
        n: i64,
        to: u32,
    },
    /// Continue at `to` unless `src` is the string `text`.
    JumpUnlessStr {
        src: Reg,
        text: Box<[u8]>,
        to: u32,
    },
    /// Field `index` of the constructor in `src`.
    Field {
        dst: Reg,
        src: Reg,
        index: u16,
    },
    /// Stop the run: the program broke a promise the compiler makes, and this
    /// says which. Placed where only such a program can reach, like after a
    /// `match`'s last arm.
    Bad {
        why: &'static str,
    },
}

impl Instr {
    /// Where this instruction may jump, for a jump still waiting on its label.
    fn target(&mut self) -> Option<&mut u32> {
        match self {
            Instr::Jump { to }
            | Instr::JumpIfFalse { to, .. }
            | Instr::JumpUnlessVariant { to, .. }
            | Instr::JumpUnlessInt { to, .. }
            | Instr::JumpUnlessStr { to, .. } => Some(to),
            Instr::Const { .. }
            | Instr::Str { .. }
            | Instr::BigInt { .. }
            | Instr::Move { .. }
            | Instr::GetGlobal { .. }
            | Instr::SetGlobal { .. }
            | Instr::Int { .. }
            | Instr::IntNeg { .. }
            | Instr::Println { .. }
            | Instr::ToString { .. }
            | Instr::Concat { .. }
            | Instr::Ctor { .. }
            | Instr::Drop { .. }
            | Instr::Call { .. }
            | Instr::TailCall { .. }
            | Instr::Ret { .. }
            | Instr::Field { .. }
            | Instr::Bad { .. } => None,
        }
    }
}

/// Where a Core IR expression's value goes.
#[derive(Clone, Copy)]
enum Dest {
    /// Returned from the function: a `Tail` is a return or a tail call.
    Return,
    /// Written to `reg`, then on to the code after a `LetJoin`: a `Tail` is
    /// a value, and a call in it is an ordinary call.
    Join { reg: Reg, after: Label },
}

/// A jump target not placed yet. Every jump to it is patched when it is.
#[derive(Clone, Copy)]
struct Label(usize);

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
    pub(crate) types: Types,
}

pub(crate) fn load(program: &Program) -> Code {
    let consts = &program.consts;
    let mut fns = TiVec::new();
    for f in &program.fns {
        let this = fns.next_idx();
        fns.push(load_fn(f, consts, Some(this)));
    }
    let toplevels = program
        .inits
        .iter()
        .chain([&program.toplevel])
        .map(|f| load_fn(f, consts, None))
        .collect();
    Code {
        fns,
        toplevels,
        main: program.main,
        globals: program.globals,
        types: program.types.clone(),
    }
}

/// `this` is the function's own index, which a call to `self` means. A
/// toplevel has none, and cannot call itself.
fn load_fn(f: &LoweredFn, consts: &[Const], this: Option<FuncIdx>) -> Func {
    match Loader::new(consts, &f.core, this).body(&f.core) {
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
    this: Option<FuncIdx>,
    instrs: Vec<Instr>,
    /// Each label's instruction index once placed, and the jumps still
    /// waiting for it.
    labels: Vec<(Option<u32>, Vec<usize>)>,
    /// The label of each `LetCont`'s continuation, for the `Goto`s to it.
    conts: HashMap<JoinId, Label>,
    /// One past the highest register the function names. A value that no
    /// local holds, like a tail expression's, gets a register from here.
    next: u32,
}

impl<'c> Loader<'c> {
    fn new(consts: &'c [Const], f: &CoreFn, this: Option<FuncIdx>) -> Self {
        let mut top = 0;
        for p in &f.params {
            top = top.max(p.id.0 + 1);
        }
        top = top.max(highest_local(&f.body));
        Loader {
            consts,
            this,
            instrs: Vec::new(),
            labels: Vec::new(),
            conts: HashMap::new(),
            next: top,
        }
    }

    fn body(mut self, f: &CoreFn) -> Result<(u32, Vec<Instr>), String> {
        self.expr(&f.body, Dest::Return)?;
        Ok((self.next, self.instrs))
    }

    fn label(&mut self) -> Label {
        self.labels.push((None, Vec::new()));
        Label(self.labels.len() - 1)
    }

    /// Put `l` at the next instruction, and point every jump to it there.
    fn place(&mut self, l: Label) {
        let here = self.instrs.len() as u32;
        let waiting = match self.labels.get_mut(l.0) {
            Some((at, waiting)) => {
                *at = Some(here);
                std::mem::take(waiting)
            }
            None => Vec::new(),
        };
        for i in waiting {
            if let Some(to) = self.instrs.get_mut(i).and_then(Instr::target) {
                *to = here;
            }
        }
    }

    /// Push a jump to `l`, patched when `l` is placed if it is not yet.
    fn jump(&mut self, l: Label, make: impl FnOnce(u32) -> Instr) {
        let at = self.labels.get(l.0).and_then(|(at, _)| *at);
        let i = self.instrs.len();
        self.instrs.push(make(at.unwrap_or(0)));
        if at.is_none()
            && let Some((_, waiting)) = self.labels.get_mut(l.0)
        {
            waiting.push(i);
        }
    }

    /// A known callee, with `self` resolved to this function.
    fn known(&self, callee: &Callee) -> Option<FuncIdx> {
        match callee {
            Callee::Known(f) => Some(*f),
            Callee::Self_ => self.this,
            Callee::Local(_) => None,
        }
    }

    fn scratch(&mut self) -> Reg {
        let r = Reg(self.next);
        self.next += 1;
        r
    }

    fn expr(&mut self, e: &CoreExpr, dest: Dest) -> Result<(), String> {
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
                CoreExpr::Drop { local, body, .. } => {
                    self.instrs.push(Instr::Drop {
                        reg: Reg::of(*local),
                    });
                    e = body;
                }
                CoreExpr::Tail(atom) => {
                    match (dest, atom) {
                        (Dest::Return, Atom::Call { callee, args })
                            if self.known(callee).is_some() =>
                        {
                            let args = args.iter().copied().map(Reg::of).collect();
                            if let Some(func) = self.known(callee) {
                                self.instrs.push(Instr::TailCall { func, args });
                            }
                        }
                        (Dest::Return, atom) => {
                            let src = self.scratch();
                            self.atom(src, atom)?;
                            self.instrs.push(Instr::Ret { src });
                        }
                        (Dest::Join { reg, after }, atom) => {
                            self.atom(reg, atom)?;
                            self.jump(after, |to| Instr::Jump { to });
                        }
                    }
                    return Ok(());
                }
                // The branch's value lands in `bind`, and every path through
                // it jumps to the code after it.
                CoreExpr::LetJoin { bind, join, body } => {
                    let after = self.label();
                    let reg = Reg::of(bind.id);
                    self.expr(join, Dest::Join { reg, after })?;
                    self.place(after);
                    if let Some(slot) = bind.global {
                        self.instrs.push(Instr::SetGlobal { slot, src: reg });
                    }
                    e = body;
                }
                CoreExpr::If {
                    cond, then, els, ..
                } => {
                    let otherwise = self.label();
                    let cond = Reg::of(*cond);
                    self.jump(otherwise, |to| Instr::JumpIfFalse { cond, to });
                    self.expr(then, dest)?;
                    self.place(otherwise);
                    e = els;
                }
                CoreExpr::Match { scrut, arms, .. } => {
                    let src = Reg::of(*scrut);
                    for (pat, arm) in arms {
                        let next = self.label();
                        self.pattern(src, pat, next)?;
                        self.expr(arm, dest)?;
                        self.place(next);
                    }
                    self.instrs.push(Instr::Bad {
                        why: "a match found no arm for its value",
                    });
                    return Ok(());
                }
                // Every path through `body` ends in a return or a jump, so
                // the continuation after it is reached only by its `Goto`s.
                CoreExpr::LetCont { id, cont, body } => {
                    let l = self.label();
                    self.conts.insert(*id, l);
                    self.expr(body, dest)?;
                    self.place(l);
                    e = cont;
                }
                CoreExpr::Goto(id) => {
                    match self.conts.get(id) {
                        Some(&l) => self.jump(l, |to| Instr::Jump { to }),
                        None => self.instrs.push(Instr::Bad {
                            why: "a jump to a case that no match declares",
                        }),
                    }
                    return Ok(());
                }
            }
        }
    }

    /// Test `src` against `pat`, jumping to `next` when it does not fit, and
    /// read out what the pattern binds when it does.
    fn pattern(&mut self, src: Reg, pat: &CorePat, next: Label) -> Result<(), String> {
        match pat {
            CorePat::Wild => {}
            CorePat::Bind(b) => self.instrs.push(Instr::Move {
                dst: Reg::of(b.id),
                src,
            }),
            CorePat::Ctor { variant, fields } => {
                let variant = *variant;
                self.jump(next, |to| Instr::JumpUnlessVariant { src, variant, to });
                for (i, f) in fields.iter().enumerate() {
                    let index = u16::try_from(i)
                        .map_err(|_| "a constructor with more than 65535 fields".to_string())?;
                    self.instrs.push(Instr::Field {
                        dst: Reg::of(f.id),
                        src,
                        index,
                    });
                }
            }
            CorePat::Lit(c) => match self.consts.get(c.index()) {
                Some(Const::Int(n)) => {
                    let n = *n;
                    self.jump(next, |to| Instr::JumpUnlessInt { src, n, to });
                }
                Some(Const::String(text)) => {
                    let text: Box<[u8]> = text.as_bytes().into();
                    self.jump(next, |to| Instr::JumpUnlessStr { src, text, to });
                }
                Some(Const::Float(_)) => return Err("matching a Float".into()),
                Some(Const::Binary { .. }) => return Err("matching a Binary".into()),
                None => {
                    return Err(format!(
                        "constant c{}, which the program does not have",
                        c.index()
                    ));
                }
            },
        }
        Ok(())
    }

    fn atom(&mut self, dst: Reg, atom: &Atom) -> Result<(), String> {
        let instr = match atom {
            Atom::Local(src) => Instr::Move {
                dst,
                src: Reg::of(*src),
            },
            Atom::Const(c) => match self.consts.get(c.index()) {
                Some(Const::String(text)) => Instr::Str {
                    dst,
                    text: text.as_bytes().into(),
                },
                Some(Const::Int(n)) if Value::int(*n).is_none() => Instr::BigInt { dst, n: *n },
                _ => Instr::Const {
                    dst,
                    value: self.constant(c.index())?,
                },
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
            Atom::Call { callee, args } => match self.known(callee) {
                Some(func) => Instr::Call {
                    dst,
                    func,
                    args: args.iter().copied().map(Reg::of).collect(),
                },
                None => return Err("calling a function value".into()),
            },
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
            // Perceus's `reuse` is a hint that `fields` may overwrite a cell
            // just dropped. A fresh cell is always right, so it waits.
            Atom::Ctor {
                variant, fields, ..
            } if fields.is_empty() => Instr::Const {
                dst,
                value: Value::nullary(*variant),
            },
            Atom::Ctor {
                variant, fields, ..
            } => Instr::Ctor {
                dst,
                variant: *variant,
                fields: fields.iter().copied().map(Reg::of).collect(),
            },
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
            PrimOp::ToString => match args {
                [a] => Ok(Instr::ToString {
                    dst,
                    a: Reg::of(*a),
                }),
                _ => Err("ToString with other than one argument".into()),
            },
            PrimOp::StringConcat | PrimOp::StringConcatMany => Ok(Instr::Concat {
                dst,
                parts: args.iter().copied().map(Reg::of).collect(),
            }),
            PrimOp::IntNeg => match args {
                [a] => Ok(Instr::IntNeg {
                    dst,
                    a: Reg::of(*a),
                }),
                _ => Err("IntNeg with other than one argument".into()),
            },
            PrimOp::FieldUnchecked(index) => match args {
                [src] => Ok(Instr::Field {
                    dst,
                    src: Reg::of(*src),
                    index,
                }),
                _ => Err("FieldUnchecked with other than one argument".into()),
            },
            other => Err(format!("the operation {other:?}")),
        }
    }

    fn constant(&self, i: usize) -> Result<Value, String> {
        match self.consts.get(i) {
            Some(Const::Int(n)) => {
                Value::int(*n).ok_or_else(|| format!("the constant {n} as a small Int"))
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
