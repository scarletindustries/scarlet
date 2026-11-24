//! Core IR to instructions, over values that have no register yet.
//!
//! A value is a Core IR local, so `let %2 = IntAdd(%0, %1)` becomes `IntAdd`
//! of values 0 and 1 into 2, and a value no local holds gets a number of its
//! own above them.
//!
//! A `match` becomes one test per arm, in order. Each test jumps to the next
//! arm's test when the value does not fit the arm's pattern. Otherwise it
//! falls into the arm, which reads out the fields the pattern binds. A
//! `LetCont`'s continuation is placed after its body, and a `Goto` is a jump
//! to it. So every jump goes forward.
//!
//! A call names its callee in one of three ways, and each has its own
//! instruction. `Call` is for a function known when the program loads.
//! `CallSelf` is for the function running now, and keeps the closure it runs
//! as, so a lambda calling itself still sees its captures. `CallValue` is for
//! a function value in a register. Each has a tail form that replaces the
//! frame.

use std::collections::{BTreeSet, HashMap};

use scarlet_ir::core_ir::{
    Atom, Callee, Const, CoreExpr, CoreFn, CorePat, FuncIdx, GlobalSlot, JoinId, Load, LocalId,
    MAX_PARAMS, PrimOp, Program,
};
use scarlet_ir::intrinsic::Intrinsic;
use scarlet_ir::tivec::Idx;

use super::ins::{
    Arg, ArgList, Args, BitsOp, CalleeRoom, Dead, Dst, Hit, Ins, KeepList, Kept, Label, Lists,
    LookList, Looks, Newline, Over, OwnRoom, ShareList, Shared, Shares, Src, Taken, VReg, Variant,
};
use super::{Tables, Unfit, Why, add, built};
use crate::ops::{Arith, Compare, NumOp, Order};
use crate::stop::{Arity, Broken, Limit, Missing};
use crate::value::{Float, SmallInt, Value};

/// A function's instructions before its values have registers.
pub(super) struct Draft {
    pub(super) ins: Vec<Ins<VReg>>,
    pub(super) lists: Lists<VReg>,
    /// Where each label is, as the number of the instruction it stands at.
    pub(super) labels: Vec<usize>,
    /// One past the highest value the function names.
    pub(super) values: u32,
    pub(super) params: Vec<VReg>,
    /// The values whose cell the loop's next turn overwrites. A call to self
    /// in tail position leaves them in their registers.
    pub(super) carried: BTreeSet<VReg>,
}

/// The instructions for `core`. `this` is the function's own index, when it
/// has one.
pub(super) fn translate(
    program: &Program,
    core: &CoreFn,
    this: Option<FuncIdx>,
    tables: &mut Tables,
) -> Result<Draft, Unfit> {
    let survey = survey(core);
    let values = match survey.highest {
        None => 0,
        Some(h) => h.checked_add(1).ok_or(Unfit::TooMany(Limit::Values))?,
    };
    let mut t = Translator {
        program,
        this,
        tables,
        ins: Vec::new(),
        lists: Lists::default(),
        labels: Vec::new(),
        landed: None,
        conts: HashMap::new(),
        values,
        kept: survey.reused,
        carried: survey.carried.into_iter().map(of).collect(),
        reads: survey.reads,
        numbers: HashMap::new(),
    };
    t.expr(&core.body, Dest::Return)?;
    let labels: Option<Vec<usize>> = t.labels.into_iter().collect();
    Ok(Draft {
        ins: t.ins,
        lists: t.lists,
        labels: labels.ok_or(Unfit::Broken(Broken::NoPlace))?,
        values: t.values,
        params: core.params.iter().map(|p| of(p.id)).collect(),
        carried: t.carried,
    })
}

fn of(local: LocalId) -> VReg {
    VReg(local.0)
}

fn src(local: LocalId) -> Src<VReg> {
    Src(of(local))
}

/// Where a Core IR expression's value goes.
#[derive(Clone, Copy)]
enum Dest {
    /// Returned from the function, so a `Tail` is a return or a tail call.
    Return,
    /// Written to `reg`, then on to the code after a `LetJoin`. A `Tail` here
    /// is a value, and a call in it is an ordinary call.
    Join { reg: VReg, after: Label },
}

/// A number the program spells, which an operator's instruction can hold.
#[derive(Clone, Copy)]
enum Number {
    Int(SmallInt),
    Float(Float),
}

struct Translator<'c> {
    program: &'c Program,
    this: Option<FuncIdx>,
    tables: &'c mut Tables,
    ins: Vec<Ins<VReg>>,
    lists: Lists<VReg>,
    /// Where each label is, once it is placed.
    labels: Vec<Option<usize>>,
    /// Where the last label was placed.
    landed: Option<usize>,
    /// The label of each `LetCont`'s continuation, for the `Goto`s to it.
    conts: HashMap<JoinId, Label>,
    /// One past the highest value the function names. A value that no local
    /// holds, like a tail expression's, gets a number from here.
    values: u32,
    /// Locals a constructor reuses, so their `Drop` hollows instead of frees.
    kept: BTreeSet<LocalId>,
    /// [`Draft::carried`].
    carried: BTreeSet<VReg>,
    /// How many times each local is read.
    reads: HashMap<LocalId, u32>,
    /// Each local met so far that is a constant number.
    numbers: HashMap<LocalId, Number>,
}

impl Translator<'_> {
    fn small(&self, l: LocalId) -> Option<SmallInt> {
        match self.numbers.get(&l)? {
            Number::Int(n) => Some(*n),
            Number::Float(_) => None,
        }
    }

    fn float(&self, l: LocalId) -> Option<Float> {
        match self.numbers.get(&l)? {
            Number::Float(x) => Some(*x),
            Number::Int(_) => None,
        }
    }

    /// `a op b` on two Ints into `dst`. The instruction holds `b` when it is
    /// a constant, or else `a` when it is one, if there is an instruction that
    /// holds that side.
    fn int_arith(&self, dst: Dst<VReg>, op: Arith, a: LocalId, b: LocalId) -> Ins<VReg> {
        let held = match (self.small(a), self.small(b)) {
            (_, Some(k)) => Some(int_arith_k(op, dst, src(a), k)),
            (Some(k), None) if op.commutes() => Some(int_arith_k(op, dst, src(b), k)),
            (Some(k), None) => int_arith_k_first(op, dst, k, src(b)),
            (None, None) => None,
        };
        held.unwrap_or_else(|| int_arith(op, dst, src(a), src(b)))
    }

    fn int_compare(&self, dst: Dst<VReg>, op: Compare, a: LocalId, b: LocalId) -> Ins<VReg> {
        match (self.small(a), self.small(b)) {
            (_, Some(k)) => int_compare_k(op, dst, src(a), k),
            (Some(k), None) => int_compare_k(op.swapped(), dst, src(b), k),
            (None, None) => int_compare(op, dst, src(a), src(b)),
        }
    }

    /// [`Self::int_arith`], on two Floats.
    fn float_arith(&self, dst: Dst<VReg>, op: Arith, a: LocalId, b: LocalId) -> Ins<VReg> {
        let held = match (self.float(a), self.float(b)) {
            (_, Some(k)) => float_arith_k(op, dst, src(a), k),
            (Some(k), None) if op.commutes() => float_arith_k(op, dst, src(b), k),
            (Some(k), None) => float_arith_k_first(op, dst, k, src(b)),
            (None, None) => None,
        };
        held.unwrap_or_else(|| float_arith(op, dst, src(a), src(b)))
    }

    fn float_order(&self, dst: Dst<VReg>, op: Order, a: LocalId, b: LocalId) -> Ins<VReg> {
        match (self.float(a), self.float(b)) {
            (_, Some(k)) => float_order_k(op, dst, src(a), k),
            (Some(k), None) => float_order_k(op.swapped(), dst, src(b), k),
            (None, None) => float_order(op, dst, src(a), src(b)),
        }
    }

    fn label(&mut self) -> Result<Label, Unfit> {
        add(&mut self.labels, None).map(Label)
    }

    /// Put `l` at the next instruction.
    fn place(&mut self, l: Label) {
        let here = self.ins.len();
        if let Some(at) = self.labels.get_mut(l.0 as usize) {
            *at = Some(here);
        }
        self.landed = Some(here);
    }

    fn value(&mut self) -> Result<VReg, Unfit> {
        let v = VReg(self.values);
        self.values = self
            .values
            .checked_add(1)
            .ok_or(Unfit::TooMany(Limit::Values))?;
        Ok(v)
    }

    fn push(&mut self, ins: Ins<VReg>) {
        self.ins.push(ins);
    }

    fn bad(&mut self, why: Why) -> Result<Ins<VReg>, Unfit> {
        let why = self.tables.why(why)?;
        Ok(Ins::Bad { why })
    }

    fn broken(&mut self, why: Broken) -> Result<Ins<VReg>, Unfit> {
        self.bad(Why::Broken(why))
    }

    /// `locals` held in the instruction, or as a list when they do not fit.
    fn args<const N: usize>(&mut self, locals: &[LocalId]) -> Result<Passed<N>, Unfit> {
        let regs: Vec<VReg> = locals.iter().copied().map(of).collect();
        Ok(match Args::new(&regs) {
            Some(args) => Fit::Inline(args),
            None => Fit::Listed(self.arg_list(&regs)?),
        })
    }

    fn arg_list(&mut self, regs: &[VReg]) -> Result<ArgList, Unfit> {
        let args = regs.iter().copied().map(Arg::shared).collect();
        add(&mut self.lists.args, args).map(ArgList)
    }

    fn reg_list(&mut self, regs: &[VReg]) -> Result<u32, Unfit> {
        add(&mut self.lists.regs, regs.into())
    }

    /// What a call of `func` with `got` arguments is instead, when the program
    /// has no such function or it takes another number of them.
    fn bad_call(&mut self, func: FuncIdx, got: usize) -> Result<Option<Ins<VReg>>, Unfit> {
        let why = match self.program.fns.get(func) {
            None => Why::Missing(Missing::NoFunc(func)),
            Some(f) if f.core.params.len() != got => Why::Broken(Broken::CallArity {
                func: format!("{}.{}", f.module, f.name),
                got,
                want: f.core.params.len(),
            }),
            Some(_) => return Ok(None),
        };
        self.bad(why).map(Some)
    }

    /// The call instruction for `callee`, in tail position or not.
    fn call(
        &mut self,
        dst: Option<VReg>,
        callee: &Callee,
        args: &[LocalId],
    ) -> Result<Ins<VReg>, Unfit> {
        if args.len() > MAX_PARAMS {
            return Err(Unfit::TooMany(Limit::Arguments));
        }
        let own = OwnRoom;
        match *callee {
            Callee::Known(func) => {
                if let Some(bad) = self.bad_call(func, args.len())? {
                    return Ok(bad);
                }
                let (to, needs) = (super::ins::Callee(func), CalleeRoom(func));
                Ok(match (dst.map(Dst), self.args(args)?) {
                    (Some(dst), Fit::Inline(args)) => Ins::Call {
                        dst,
                        to,
                        args,
                        needs,
                        own,
                    },
                    (Some(dst), Fit::Listed(args)) => Ins::CallWide {
                        dst,
                        to,
                        args,
                        needs,
                        own,
                    },
                    (None, Fit::Inline(args)) => Ins::TailCall {
                        to,
                        args,
                        needs,
                        own,
                    },
                    (None, Fit::Listed(args)) => Ins::TailCallWide {
                        to,
                        args,
                        needs,
                        own,
                    },
                })
            }
            Callee::Self_ => {
                let Some(this) = self.this else {
                    return self.broken(Broken::ToplevelCallsSelf);
                };
                if let Some(bad) = self.bad_call(this, args.len())? {
                    return Ok(bad);
                }
                let to = super::ins::Callee(this);
                if dst.is_none() && !self.carried.is_empty() {
                    let args: Vec<VReg> = args.iter().copied().map(of).collect();
                    let keep: Vec<VReg> = self.carried.iter().copied().collect();
                    return Ok(Ins::TailCallSelfKeep {
                        to,
                        args: self.arg_list(&args)?,
                        keep: KeepList(self.reg_list(&keep)?),
                        own,
                    });
                }
                Ok(match (dst.map(Dst), self.args(args)?) {
                    (Some(dst), Fit::Inline(args)) => Ins::CallSelf { dst, to, args, own },
                    (Some(dst), Fit::Listed(args)) => Ins::CallSelfWide { dst, to, args, own },
                    (None, Fit::Inline(args)) => Ins::TailCallSelf { to, args, own },
                    (None, Fit::Listed(args)) => Ins::TailCallSelfWide { to, args, own },
                })
            }
            Callee::Local(f) => {
                let callee = Shared(of(f));
                Ok(match (dst.map(Dst), self.args(args)?) {
                    (Some(dst), Fit::Inline(args)) => Ins::CallValue {
                        dst,
                        callee,
                        args,
                        own,
                    },
                    (Some(dst), Fit::Listed(args)) => Ins::CallValueWide {
                        dst,
                        callee,
                        args,
                        own,
                    },
                    (None, Fit::Inline(args)) => Ins::TailCallValue { callee, args, own },
                    (None, Fit::Listed(args)) => Ins::TailCallValueWide { callee, args, own },
                })
            }
        }
    }

    /// When the last instruction compares two numbers into `cond`, and the
    /// `if` on `cond` is all that reads it, take that instruction back and
    /// give the jump that does it. Constants may be made between the two,
    /// since a comparison reads none that comes after it. Nothing may jump in
    /// between, or that jump would skip the comparison.
    fn take_comparison(&mut self, cond: LocalId, to: Label) -> Option<Ins<VReg>> {
        if self.reads.get(&cond) != Some(&1) {
            return None;
        }
        let between = || self.ins.pop_if(|ins| matches!(ins, Ins::Const { .. }));
        let constants: Vec<Ins<VReg>> = std::iter::from_fn(between).collect();
        let jumped_past = self.landed.is_some_and(|at| at >= self.ins.len());
        let jump = match self.ins.last() {
            Some(compare) if !jumped_past => jump_unless(compare, of(cond), to),
            _ => None,
        };
        if jump.is_some() {
            self.ins.pop();
        }
        self.ins.extend(constants.into_iter().rev());
        jump
    }

    fn expr(&mut self, e: &CoreExpr, dest: Dest) -> Result<(), Unfit> {
        let mut e = e;
        loop {
            match e {
                // A map dropped right after it is set gives its reference to
                // the set.
                CoreExpr::Let {
                    bind,
                    rhs:
                        Atom::Intrinsic {
                            intrinsic: Intrinsic::MapSet,
                            args,
                        },
                    body,
                } if let [m, k, v] = args.as_slice()
                    && k != m
                    && v != m
                    && let CoreExpr::Drop {
                        local, body: rest, ..
                    } = &**body
                    && local == m
                    && !self.kept.contains(m) =>
                {
                    self.push(Ins::MapSetOwned {
                        dst: Dst(of(bind.id)),
                        map: Taken(of(*m)),
                        key: src(*k),
                        value: src(*v),
                    });
                    self.store(bind.id, bind.global);
                    e = rest;
                }
                CoreExpr::Let {
                    bind,
                    rhs:
                        Atom::Intrinsic {
                            intrinsic: Intrinsic::MapGet,
                            args,
                        },
                    body,
                } if let [m, k] = args.as_slice()
                    && bind.global.is_none()
                    && self.reads.get(&bind.id) == Some(&1)
                    && let CoreExpr::Match { scrut, arms, .. } = &**body
                    && *scrut == bind.id
                    && let Some((value, found, missing)) = self.option_arms(arms) =>
                {
                    let to = self.label()?;
                    self.push(Ins::MapFind {
                        dst: Hit(of(value)),
                        map: src(*m),
                        key: src(*k),
                        to,
                    });
                    self.expr(found, dest)?;
                    self.place(to);
                    e = missing;
                }
                CoreExpr::Let {
                    bind,
                    rhs:
                        Atom::Intrinsic {
                            intrinsic: Intrinsic::MapDelete,
                            args,
                        },
                    body,
                } if let [m, k] = args.as_slice()
                    && k != m
                    && let CoreExpr::Drop {
                        local, body: rest, ..
                    } = &**body
                    && local == m
                    && !self.kept.contains(m) =>
                {
                    self.push(Ins::MapDeleteOwned {
                        dst: Dst(of(bind.id)),
                        map: Taken(of(*m)),
                        key: src(*k),
                    });
                    self.store(bind.id, bind.global);
                    e = rest;
                }
                CoreExpr::Let { bind, rhs, body } => {
                    // One that is stored in a global is not held, because the
                    // store reads it.
                    if let Atom::Const(c) = rhs
                        && bind.global.is_none()
                        && let Some(n) = number(self.program.consts.get(c.index()))
                    {
                        self.numbers.insert(bind.id, n);
                    }
                    self.atom(of(bind.id), rhs)?;
                    self.store(bind.id, bind.global);
                    e = body;
                }
                CoreExpr::Drop { local, body, .. } => {
                    let reg = of(*local);
                    self.push(if self.kept.contains(local) {
                        Ins::DropReuse { reg: Kept(reg) }
                    } else {
                        Ins::Drop { reg: Dead(reg) }
                    });
                    e = body;
                }
                CoreExpr::Tail(atom) => return self.tail(atom, dest),
                // The branch's value lands in `bind`, and every path through
                // it jumps to the code after it.
                CoreExpr::LetJoin { bind, join, body } => {
                    let after = self.label()?;
                    let reg = of(bind.id);
                    self.expr(join, Dest::Join { reg, after })?;
                    self.place(after);
                    self.store(bind.id, bind.global);
                    e = body;
                }
                CoreExpr::If {
                    cond, then, els, ..
                } => {
                    let to = self.label()?;
                    let jump = match self.take_comparison(*cond, to) {
                        Some(jump) => jump,
                        None => Ins::JumpIfFalse {
                            cond: src(*cond),
                            to,
                        },
                    };
                    self.push(jump);
                    self.expr(then, dest)?;
                    self.place(to);
                    e = els;
                }
                CoreExpr::Match { scrut, arms, .. } => {
                    for (pat, arm) in arms {
                        let next = self.label()?;
                        self.pattern(*scrut, pat, next)?;
                        self.expr(arm, dest)?;
                        self.place(next);
                    }
                    let no_arm = self.broken(Broken::NoArm)?;
                    self.push(no_arm);
                    return Ok(());
                }
                // Every path through `body` ends in a return or a jump, so
                // the continuation after it is reached only by its `Goto`s.
                CoreExpr::LetCont { id, cont, body } => {
                    let l = self.label()?;
                    self.conts.insert(*id, l);
                    self.expr(body, dest)?;
                    self.place(l);
                    // No `Goto` past here is in the continuation's scope.
                    self.conts.remove(id);
                    e = cont;
                }
                CoreExpr::Goto(id) => {
                    let jump = match self.conts.get(id) {
                        Some(&to) => Ins::Jump { to },
                        None => self.broken(Broken::UndeclaredCase)?,
                    };
                    self.push(jump);
                    return Ok(());
                }
            }
        }
    }

    /// Store `local` in the global it is bound to, if it is bound to one.
    fn store(&mut self, local: LocalId, global: Option<GlobalSlot>) {
        if let Some(slot) = global {
            let src = Shared(of(local));
            self.push(Ins::SetGlobal { slot, src });
        }
    }

    fn tail(&mut self, atom: &Atom, dest: Dest) -> Result<(), Unfit> {
        let ret = |src| Ins::Ret {
            src: Taken(src),
            own: OwnRoom,
        };
        match (dest, atom) {
            (Dest::Return, Atom::Call { callee, args }) => {
                let call = self.call(None, callee, args)?;
                self.push(call);
            }
            (Dest::Return, Atom::Local(local)) => self.push(ret(of(*local))),
            (Dest::Return, atom) => {
                let value = self.value()?;
                self.atom(value, atom)?;
                self.push(ret(value));
            }
            (Dest::Join { reg, after }, atom) => {
                self.atom(reg, atom)?;
                self.push(Ins::Jump { to: after });
            }
        }
        Ok(())
    }

    /// Test `scrut` against `pat`, jumping to `to` when it does not fit, and
    /// read out what the pattern binds when it does.
    fn pattern(&mut self, scrut: LocalId, pat: &CorePat, to: Label) -> Result<(), Unfit> {
        match pat {
            CorePat::Wild => {}
            CorePat::Bind(b) => self.push(Ins::Move {
                dst: Dst(of(b.id)),
                src: Arg::shared(of(scrut)),
            }),
            CorePat::Ctor { variant, fields } => {
                self.push(Ins::JumpUnlessVariant {
                    src: src(scrut),
                    variant: Variant::new(*variant),
                    to,
                });
                for (i, f) in fields.iter().enumerate() {
                    let index = u16::try_from(i).map_err(|_| Missing::TooManyFields)?;
                    self.push(Ins::Field {
                        dst: Dst(of(f.id)),
                        src: src(scrut),
                        index,
                    });
                }
            }
            CorePat::Lit(c) => {
                let src = src(scrut);
                let test = match self.program.consts.get(c.index()) {
                    Some(Const::Int(n)) => Ins::JumpUnlessInt { src, n: *n, to },
                    Some(Const::String(text)) => {
                        let text = self.tables.text(text.as_bytes())?;
                        Ins::JumpUnlessStr { src, text, to }
                    }
                    Some(Const::Float(x)) => Ins::JumpUnlessFloat { src, x: *x, to },
                    Some(Const::Binary { bytes, bit_len }) => {
                        let bits = self.tables.binary(bytes, *bit_len)?;
                        Ins::JumpUnlessBinary { src, bits, to }
                    }
                    None => return Err(Missing::NoConst(c.index()).into()),
                };
                self.push(test);
            }
        }
        Ok(())
    }

    fn atom(&mut self, reg: VReg, atom: &Atom) -> Result<(), Unfit> {
        let dst = Dst(reg);
        let constant = |value| Ins::Const { dst, value };
        let ins = match atom {
            Atom::Local(local) => Ins::Move {
                dst,
                src: Arg::shared(of(*local)),
            },
            Atom::Const(c) => match self.program.consts.get(c.index()) {
                Some(Const::Int(n)) => match Value::int(*n) {
                    Some(value) => constant(value),
                    None => Ins::BigInt { dst, n: *n, id: *c },
                },
                Some(Const::Float(x)) => constant(Value::float(*x)),
                Some(Const::String(text)) => Ins::Str {
                    dst,
                    text: self.tables.text(text.as_bytes())?,
                },
                Some(Const::Binary { bytes, bit_len }) => Ins::BinaryConst {
                    dst,
                    bits: self.tables.binary(bytes, *bit_len)?,
                },
                None => return Err(Missing::NoConst(c.index()).into()),
            },
            Atom::Nil => constant(Value::NIL),
            Atom::Bool(b) => constant(Value::bool(*b)),
            Atom::Load(Load::Global(slot)) => Ins::GetGlobal { dst, slot: *slot },
            Atom::Load(Load::Capture(i)) => match u32::try_from(i.0) {
                Ok(index) => Ins::GetCapture { dst, index },
                Err(_) => self.broken(Broken::NegativeCapture)?,
            },
            Atom::Load(Load::SelfClosure) => match self.this {
                Some(this) => Ins::GetSelf { dst, this },
                None => self.broken(Broken::ToplevelSelf)?,
            },
            Atom::Closure { func_idx, captures } if captures.is_empty() => {
                constant(Value::func(*func_idx))
            }
            Atom::Closure { func_idx, captures } => {
                let func = *func_idx;
                match self.shares(captures.iter().copied())? {
                    Fit::Inline(captures) => Ins::Closure {
                        dst,
                        func,
                        captures,
                    },
                    Fit::Listed(captures) => Ins::ClosureWide {
                        dst,
                        func,
                        captures,
                    },
                }
            }
            Atom::Call { callee, args } => self.call(Some(reg), callee, args)?,
            Atom::PrimOp { op, args } => self.prim(dst, *op, args)?,
            Atom::Intrinsic {
                intrinsic: intrinsic @ (Intrinsic::Print | Intrinsic::Println),
                args,
            } => match args.as_slice() {
                [a] => Ins::Print {
                    dst,
                    a: src(*a),
                    newline: Newline(*intrinsic == Intrinsic::Println),
                },
                _ => return Err(Missing::PrintArity(*intrinsic).into()),
            },
            // `array::put` gives the array back as it was when it has no
            // such index.
            Atom::Intrinsic {
                intrinsic: Intrinsic::ArrayPut,
                args,
            } if let [xs, i, x] = args.as_slice() => {
                let (miss, after) = (self.label()?, self.label()?);
                self.array_set(reg, [*xs, *i, *x], miss);
                self.push(Ins::Jump { to: after });
                self.place(miss);
                self.push(Ins::Move {
                    dst,
                    src: Arg::shared(of(*xs)),
                });
                self.place(after);
                return Ok(());
            }
            // `array::set` says which it was: `Ok` of the new array, or
            // `Err(Nil)`.
            Atom::Intrinsic {
                intrinsic: Intrinsic::ArraySet,
                args,
            } if let [xs, i, x] = args.as_slice() => {
                let (miss, after) = (self.label()?, self.label()?);
                let (made, nil) = (self.value()?, self.value()?);
                self.array_set(made, [*xs, *i, *x], miss);
                let abi = self.program.abi;
                let ok = self.ctor(dst, Variant::new(abi.ok), &[made], None)?;
                self.push(ok);
                self.push(Ins::Jump { to: after });
                self.place(miss);
                self.push(Ins::Const {
                    dst: Dst(nil),
                    value: Value::NIL,
                });
                let err = self.ctor(dst, Variant::new(abi.err), &[nil], None)?;
                self.push(err);
                self.place(after);
                return Ok(());
            }
            Atom::Intrinsic {
                intrinsic: Intrinsic::FloatFromInt,
                args,
            } if let [a] = args.as_slice() => Ins::FloatFromInt { dst, a: src(*a) },
            Atom::Intrinsic { intrinsic, args } => {
                let regs: Vec<VReg> = args.iter().copied().map(of).collect();
                match Looks::new(&regs) {
                    Some(args) if built(*intrinsic, regs.len()) => Ins::Builtin {
                        dst,
                        intrinsic: *intrinsic,
                        args,
                    },
                    _ => return Err(Missing::Intrinsic(*intrinsic).into()),
                }
            }
            // A constructor with no fields is a value word, so there is no
            // allocation for Perceus to reuse.
            Atom::Ctor {
                variant, fields, ..
            } if fields.is_empty() => constant(Value::nullary(*variant)),
            Atom::Ctor {
                variant,
                fields,
                reuse,
            } => {
                let fields: Vec<VReg> = fields.iter().copied().map(of).collect();
                self.ctor(dst, Variant::new(*variant), &fields, reuse.map(of))?
            }
        };
        self.push(ins);
        Ok(())
    }

    fn ctor(
        &mut self,
        dst: Dst<VReg>,
        variant: Variant,
        fields: &[VReg],
        reuse: Option<VReg>,
    ) -> Result<Ins<VReg>, Unfit> {
        if let Some(reuse) = reuse.map(Over) {
            return Ok(match Args::new(fields) {
                Some(fields) => Ins::CtorOver {
                    dst,
                    variant,
                    fields,
                    reuse,
                },
                None => Ins::CtorOverWide {
                    dst,
                    variant,
                    fields: self.arg_list(fields)?,
                    reuse,
                },
            });
        }
        Ok(match Args::new(fields) {
            Some(fields) => Ins::Ctor {
                dst,
                variant,
                fields,
            },
            None => Ins::CtorWide {
                dst,
                variant,
                fields: self.arg_list(fields)?,
            },
        })
    }

    /// The write that `array::put` and `array::set` both are: `xs` with `x`
    /// at index `i` goes to `dst`, and an `i` that `xs` does not have
    /// continues at `miss`.
    fn array_set(&mut self, dst: VReg, [xs, i, x]: [LocalId; 3], miss: Label) {
        self.push(Ins::ArraySet {
            dst: Hit(dst),
            src: Arg::shared(of(xs)),
            index: src(i),
            value: Arg::shared(of(x)),
            miss,
        });
    }

    /// `locals`, for an instruction that shares each.
    fn shares<const N: usize>(
        &mut self,
        locals: impl Iterator<Item = LocalId>,
    ) -> Result<Fit<Shares<VReg, N>, ShareList>, Unfit> {
        let regs: Vec<VReg> = locals.map(of).collect();
        Ok(match Shares::new(&regs) {
            Some(few) => Fit::Inline(few),
            None => Fit::Listed(ShareList(self.reg_list(&regs)?)),
        })
    }

    /// `locals`, for an instruction that looks at each.
    fn looks<const N: usize>(
        &mut self,
        locals: &[LocalId],
    ) -> Result<Fit<Looks<VReg, N>, LookList>, Unfit> {
        let regs: Vec<VReg> = locals.iter().copied().map(of).collect();
        Ok(match Looks::new(&regs) {
            Some(few) => Fit::Inline(few),
            None => Fit::Listed(LookList(self.reg_list(&regs)?)),
        })
    }

    fn bits(&mut self, dst: Dst<VReg>, op: BitsOp, args: &[LocalId]) -> Result<Ins<VReg>, Unfit> {
        Ok(match self.looks(args)? {
            Fit::Inline(args) => Ins::Bits { dst, op, args },
            Fit::Listed(args) => Ins::BitsWide { dst, op, args },
        })
    }

    fn prim(&mut self, dst: Dst<VReg>, op: PrimOp, args: &[LocalId]) -> Result<Ins<VReg>, Unfit> {
        let arity = |want| Unfit::from(Missing::Arity { op, want });
        let one = || match args {
            [a] => Ok(src(*a)),
            _ => Err(arity(Arity::One)),
        };
        let two = || match args {
            [a, b] => Ok((*a, *b)),
            _ => Err(arity(Arity::Two)),
        };
        let both = || two().map(|(a, b)| (src(a), src(b)));
        let three = || match args {
            [a, b, c] => Ok((src(*a), src(*b), src(*c))),
            _ => Err(arity(Arity::Three)),
        };
        let int = |t: &Self, op| two().map(|(a, b)| t.int_arith(dst, op, a, b));
        let int_compare = |t: &Self, op| two().map(|(a, b)| t.int_compare(dst, op, a, b));
        let float = |t: &Self, op| two().map(|(a, b)| t.float_arith(dst, op, a, b));
        let float_order = |t: &Self, op| two().map(|(a, b)| t.float_order(dst, op, a, b));
        let num = |op| both().map(|(a, b)| Ins::Num { dst, op, a, b });
        let bits = |t: &mut Self, op, want: Arity| {
            if want.fits(args.len()) {
                t.bits(dst, op, args)
            } else {
                Err(arity(want))
            }
        };
        match op {
            PrimOp::IntAdd => int(self, Arith::Add),
            PrimOp::IntSub => int(self, Arith::Sub),
            PrimOp::IntMul => int(self, Arith::Mul),
            PrimOp::IntDiv => int(self, Arith::Div),
            PrimOp::IntRem => int(self, Arith::Rem),
            PrimOp::IntEq => int_compare(self, Compare::Eq),
            PrimOp::IntNe => int_compare(self, Compare::Ne),
            PrimOp::IntLt => int_compare(self, Compare::LT),
            PrimOp::IntLe => int_compare(self, Compare::LE),
            PrimOp::IntGt => int_compare(self, Compare::GT),
            PrimOp::IntGe => int_compare(self, Compare::GE),
            PrimOp::FloatAdd => float(self, Arith::Add),
            PrimOp::FloatSub => float(self, Arith::Sub),
            PrimOp::FloatMul => float(self, Arith::Mul),
            PrimOp::FloatDiv => float(self, Arith::Div),
            PrimOp::FloatRem => float(self, Arith::Rem),
            PrimOp::FloatLt => float_order(self, Order::Lt),
            PrimOp::FloatLe => float_order(self, Order::Le),
            PrimOp::FloatGt => float_order(self, Order::Gt),
            PrimOp::FloatGe => float_order(self, Order::Ge),
            PrimOp::Add => num(NumOp::Arith(Arith::Add)),
            PrimOp::Sub => num(NumOp::Arith(Arith::Sub)),
            PrimOp::Mul => num(NumOp::Arith(Arith::Mul)),
            PrimOp::Div => num(NumOp::Arith(Arith::Div)),
            PrimOp::Rem => num(NumOp::Arith(Arith::Rem)),
            PrimOp::Lt => num(NumOp::Order(Order::Lt)),
            PrimOp::Le => num(NumOp::Order(Order::Le)),
            PrimOp::Gt => num(NumOp::Order(Order::Gt)),
            PrimOp::Ge => num(NumOp::Order(Order::Ge)),
            PrimOp::IntNeg => one().map(|a| Ins::IntNeg { dst, a }),
            PrimOp::FloatNeg => one().map(|a| Ins::FloatNeg { dst, a }),
            PrimOp::Neg => one().map(|a| Ins::Neg { dst, a }),
            PrimOp::ToString => one().map(|Src(a)| Ins::ToString { dst, a: Shared(a) }),
            PrimOp::StringConcat | PrimOp::StringConcatMany => Ok(match self.looks(args)? {
                Fit::Inline(parts) => Ins::Concat { dst, parts },
                Fit::Listed(parts) => Ins::ConcatWide { dst, parts },
            }),
            PrimOp::Eq => both().map(|(a, b)| Ins::Equal { dst, a, b }),
            PrimOp::Ne => both().map(|(a, b)| Ins::NotEqual { dst, a, b }),
            PrimOp::Not => one().map(|a| Ins::Not { dst, a }),
            PrimOp::BinaryFromInt => bits(self, BitsOp::FromInt, Arity::Two),
            PrimOp::BinaryTake => bits(self, BitsOp::Take, Arity::Two),
            PrimOp::BinaryFromString => bits(self, BitsOp::FromString, Arity::One),
            PrimOp::BinaryConcatMany => self.bits(dst, BitsOp::Concat, args),
            PrimOp::BinaryBitSize => bits(self, BitsOp::BitSize, Arity::One),
            PrimOp::BinaryView => bits(self, BitsOp::View, Arity::Three),
            PrimOp::BinaryMatchPrefix => bits(self, BitsOp::MatchPrefix, Arity::Three),
            PrimOp::BinaryReadUtf8 => bits(self, BitsOp::ReadUtf8, Arity::Two),
            PrimOp::BinaryReadInt => bits(self, BitsOp::ReadInt, Arity::Three),
            PrimOp::MakeRange => both().map(|(start, end)| Ins::Range { dst, start, end }),
            PrimOp::MakeArray => Ok(match self.shares(args.iter().copied())? {
                Fit::Inline(elements) => Ins::Array { dst, elements },
                Fit::Listed(elements) => Ins::ArrayWide { dst, elements },
            }),
            PrimOp::ArrayLen => one().map(|src| Ins::ArrayLen { dst, src }),
            PrimOp::ArrayElem(index) => one().map(|src| Ins::ArrayElem { dst, src, index }),
            PrimOp::ArrayDrop => both().map(|(Src(src), n)| Ins::ArrayDrop {
                dst,
                src: Arg::shared(src),
                n,
            }),
            PrimOp::ArrayPrepend => {
                let (rest, front) = args.split_last().ok_or(arity(Arity::AtLeastOne))?;
                let rest = Arg::shared(of(*rest));
                Ok(match self.shares(front.iter().copied())? {
                    Fit::Inline(front) => Ins::ArrayPrepend { dst, rest, front },
                    Fit::Listed(front) => Ins::ArrayPrependWide { dst, rest, front },
                })
            }
            PrimOp::ArrayAppend => {
                let (rest, back) = args.split_first().ok_or(arity(Arity::AtLeastOne))?;
                let rest = Arg::shared(of(*rest));
                Ok(match self.shares(back.iter().copied())? {
                    Fit::Inline(back) => Ins::ArrayAppend { dst, rest, back },
                    Fit::Listed(back) => Ins::ArrayAppendWide { dst, rest, back },
                })
            }
            PrimOp::ArrayConcat => both().map(|(a, b)| Ins::ArrayConcat { dst, a, b }),
            PrimOp::ArraySlice => three().map(|(src, start, end)| Ins::ArraySlice {
                dst,
                src,
                start,
                end,
            }),
            PrimOp::ArrayIndex => both().map(|(src, index)| Ins::ArrayIndex { dst, src, index }),
            PrimOp::ArrayIndexOr => three().map(|(src, index, Src(default))| Ins::ArrayIndexOr {
                dst,
                src,
                index,
                default: Shared(default),
            }),
            PrimOp::MakeTuple => Ok(match self.shares(args.iter().copied())? {
                Fit::Inline(elements) => Ins::Tuple { dst, elements },
                Fit::Listed(elements) => Ins::TupleWide { dst, elements },
            }),
            PrimOp::TupleField(index) => one().map(|src| Ins::Element { dst, src, index }),
            PrimOp::Field(index) => one().map(|src| Ins::Field { dst, src, index }),
        }
    }

    /// The arms of a `match` on an `Option` that names both variants and binds
    /// `Some`'s value to a plain name: that name, the `Some` arm and the `None`
    /// arm.
    fn option_arms<'e>(
        &self,
        arms: &'e [(CorePat, CoreExpr)],
    ) -> Option<(LocalId, &'e CoreExpr, &'e CoreExpr)> {
        let abi = &self.program.abi;
        let [(a, x), (b, y)] = arms else {
            return None;
        };
        let some = |p: &CorePat| match p {
            CorePat::Ctor { variant, fields } if *variant == abi.some => match fields.as_slice() {
                [v] if v.global.is_none() => Some(v.id),
                _ => None,
            },
            CorePat::Wild | CorePat::Bind(_) | CorePat::Lit(_) | CorePat::Ctor { .. } => None,
        };
        let none = |p: &CorePat| matches!(p, CorePat::Ctor { variant, fields } if *variant == abi.none && fields.is_empty());
        match (some(a), some(b)) {
            (Some(v), _) if none(b) => Some((v, x, y)),
            (_, Some(v)) if none(a) => Some((v, y, x)),
            _ => None,
        }
    }
}

/// Operands that sit in the instruction, or a list of them.
enum Fit<I, L> {
    Inline(I),
    Listed(L),
}

/// The arguments of a call or a constructor.
type Passed<const N: usize> = Fit<Args<VReg, N>, ArgList>;

/// The number a constant is, when an instruction can hold it.
fn number(c: Option<&Const>) -> Option<Number> {
    match c? {
        Const::Int(n) => SmallInt::new(*n).map(Number::Int),
        Const::Float(x) => Some(Number::Float(Float::new(*x))),
        Const::String(_) | Const::Binary { .. } => None,
    }
}

// The instruction that does `op`, or `None` when there is none that holds the
// constant on that side.

/// The instruction of a family that does `op`, on the operands in braces. A
/// family that has no instruction for some operators says what to do for
/// those.
macro_rules! instr_for {
    ([$op:ident $operands:tt $($rest:pat => $other:expr)?] $($name:ident $does:path),*) => {
        match $op { $($does => Ins::$name $operands,)* $($rest => $other,)? }
    };
    (
        [$op:ident $operands:tt $($rest:pat => $other:expr)?]
        $($name:ident $jump:ident $does:path),*
    ) => {
        match $op { $($does => Ins::$name $operands,)* $($rest => $other,)? }
    };
}

type D = Dst<VReg>;
type S = Src<VReg>;

fn int_arith(op: Arith, dst: D, a: S, b: S) -> Ins<VReg> {
    int_arith!(instr_for op { dst, a, b })
}

fn int_arith_k(op: Arith, dst: D, a: S, k: SmallInt) -> Ins<VReg> {
    int_arith_k!(instr_for op { dst, a, k })
}

fn int_arith_k_first(op: Arith, dst: D, k: SmallInt, a: S) -> Option<Ins<VReg>> {
    Some(int_arith_k_first!(instr_for op { dst, k, a }
        Arith::Add | Arith::Mul | Arith::Div | Arith::Rem => return None))
}

fn int_compare(op: Compare, dst: D, a: S, b: S) -> Ins<VReg> {
    int_compare!(instr_for op { dst, a, b }
        Compare::GT | Compare::GE => int_compare(op.swapped(), dst, b, a))
}

fn int_compare_k(op: Compare, dst: D, a: S, k: SmallInt) -> Ins<VReg> {
    int_compare_k!(instr_for op { dst, a, k })
}

fn float_arith(op: Arith, dst: D, a: S, b: S) -> Ins<VReg> {
    float_arith!(instr_for op { dst, a, b })
}

fn float_arith_k(op: Arith, dst: D, a: S, k: Float) -> Option<Ins<VReg>> {
    Some(float_arith_k!(instr_for op { dst, a, k } Arith::Rem => return None))
}

fn float_arith_k_first(op: Arith, dst: D, k: Float, a: S) -> Option<Ins<VReg>> {
    Some(float_arith_k_first!(instr_for op { dst, k, a }
        Arith::Add | Arith::Mul | Arith::Rem => return None))
}

fn float_order(op: Order, dst: D, a: S, b: S) -> Ins<VReg> {
    float_compare!(instr_for op { dst, a, b }
        Order::Gt | Order::Ge => float_order(op.swapped(), dst, b, a))
}

fn float_order_k(op: Order, dst: D, a: S, k: Float) -> Ins<VReg> {
    float_compare_k!(instr_for op { dst, a, k })
}

/// The jump to `to` unless the comparison holds that `ins` stores in `cond`,
/// or `None` when it stores no comparison of numbers there.
fn jump_unless(ins: &Ins<VReg>, cond: VReg, to: Label) -> Option<Ins<VReg>> {
    macro_rules! of_pairs {
        ([] $($name:ident $jump:ident $op:path),*) => {
            $(if let Ins::$name { dst: Dst(dst), a, b } = *ins && dst == cond {
                return Some(Ins::$jump { a, b, to });
            })*
        };
    }
    macro_rules! with_constants {
        ([] $($name:ident $jump:ident $op:path),*) => {
            $(if let Ins::$name { dst: Dst(dst), a, k } = *ins && dst == cond {
                return Some(Ins::$jump { a, k, to });
            })*
        };
    }
    int_compare!(of_pairs);
    int_compare_k!(with_constants);
    float_compare!(of_pairs);
    float_compare_k!(with_constants);
    None
}

/// What one walk over a function's body finds out.
#[derive(Default)]
struct Survey {
    /// How many times each local is read. A `Drop` counts as a read.
    reads: HashMap<LocalId, u32>,
    /// Every local a constructor overwrites in place. `Atom`'s operand walk
    /// leaves `reuse` out on purpose, because it names a cell to write over,
    /// not a value read.
    reused: BTreeSet<LocalId>,
    /// Those of them that are overwritten by the constructor that binds them
    /// again, which is how a loop takes the cell its last turn gave up
    /// (`scarlet_lower::perceus`). Every other is bound earlier in the turn.
    carried: BTreeSet<LocalId>,
    /// The highest local bound or read.
    highest: Option<u32>,
}

impl Survey {
    fn name(&mut self, l: LocalId) {
        self.highest = self.highest.max(Some(l.0));
    }

    fn read(&mut self, l: LocalId) {
        let n = self.reads.entry(l).or_default();
        *n = n.saturating_add(1);
        self.name(l);
    }

    fn atom(&mut self, atom: &Atom) {
        atom.for_each_operand(|l| self.read(l));
        if let Atom::Ctor {
            reuse: Some(local), ..
        } = atom
        {
            self.reused.insert(*local);
        }
    }
}

fn survey(f: &CoreFn) -> Survey {
    let mut out = Survey::default();
    f.params.iter().for_each(|p| out.name(p.id));
    let mut stack = vec![&f.body];
    while let Some(e) = stack.pop() {
        match e {
            CoreExpr::Let { bind, rhs, body } => {
                out.name(bind.id);
                out.atom(rhs);
                if let Atom::Ctor { reuse, .. } = rhs
                    && *reuse == Some(bind.id)
                {
                    out.carried.insert(bind.id);
                }
                stack.push(body);
            }
            CoreExpr::LetJoin { bind, join, body } => {
                out.name(bind.id);
                stack.push(join);
                stack.push(body);
            }
            CoreExpr::LetCont { cont, body, .. } => {
                stack.push(cont);
                stack.push(body);
            }
            CoreExpr::Drop { local, body, .. } => {
                out.read(*local);
                stack.push(body);
            }
            CoreExpr::Match { scrut, arms, .. } => {
                out.read(*scrut);
                for (pat, arm) in arms {
                    pat.binds().for_each(|b| out.name(b.id));
                    stack.push(arm);
                }
            }
            CoreExpr::If {
                cond, then, els, ..
            } => {
                out.read(*cond);
                stack.push(then);
                stack.push(els);
            }
            CoreExpr::Tail(atom) => out.atom(atom),
            CoreExpr::Goto(_) => {}
        }
    }
    out
}

#[cfg(test)]
mod tests;
