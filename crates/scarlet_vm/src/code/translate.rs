//! Core IR to instructions, over values that have no register yet.
//!
//! A value is a Core IR local, so `let %2 = IntAdd(%0, %1)` becomes `IntAdd`
//! of values 0 and 1 into 2.
//!
//! A `switch` with two outcomes becomes a test that jumps to the second, and one
//! with more becomes a jump by a table when that is not much longer than the cases
//! are many. Otherwise it is a test for each value, in order. A probe is one
//! instruction that goes on with the hit or jumps to the miss behind it, which
//! [`layout`](super::layout) may move out of the hit's way. A join's continuation is placed after its body, and a jump to it is a `Move` for each
//! parameter and a `Jump`. So every jump goes forward.
//!
//! A call names its callee in one of three ways. A function known when the
//! program loads is called by name, and that has an instruction for each number
//! of arguments up to three (`Call0` to `Call3`) and `Call` for more.
//! `CallSelf` is for a function that keeps the closure it runs as, and passes
//! it on, so a lambda calling itself still sees its captures. Any other
//! function calls itself by name. `CallValue` is for a function value in a
//! register. A call in tail position replaces the frame. One of a function
//! value is `TailCallValue`, and any other is [`Step::TailCall`], which is no
//! instruction: what it takes to make it depends on where its arguments are
//! ([`super::moves`]).

use std::collections::HashMap;

use scarlet_ir::TypeId;
use scarlet_ir::core_ir::{
    Arms, Atom, Callee, Cases, Const, ConstId, CoreBind, CoreExpr, CoreFn, FloatKey, FuncIdx,
    JoinId, Load, LocalId, MAX_PARAMS, PrimOp, ProbeOp, VariantRef,
};
use scarlet_ir::intrinsic::Intrinsic;
use scarlet_ir::rty::{ForeignRTy, Prim, RTy, Repr, ResolvedPool};
use scarlet_ir::tivec::Idx;

use super::forms::Form;
use super::ins::{
    Arg, ArgList, Args, Assign, AssignList, BitsOp, Dead, Dst, DstList, Hit, HitList, Hits, Ins,
    IntTable, Label, LastReg, Length, Lists, LookList, Looks, Makes, MaybeDst, MaybeHit, Near,
    Newline, OwnEntry, OwnRoom, PlaceList, Role, Sets, ShareList, Shared, Shares, Split, SplitWide,
    Src, Table, Taken, VReg,
};
use super::{ReadsClosure, Source, Tables, Unfit, Why, add, built};
use crate::array::End;
use crate::heap::{Ctor, Layout};
use crate::ops::{Arith, Compare, NumOp, Order};
use crate::stop::{Arity, Broken, Limit, Missing};
use crate::value::{Float, SmallInt, Value};

/// One thing a function does, before its values have registers.
#[derive(Debug)]
pub(super) enum Step {
    Ins(Ins<VReg>),
    /// A call in tail position to a function known when the program loads. It
    /// takes its arguments and runs nothing after it.
    TailCall {
        to: TailTo,
        args: Box<[VReg]>,
    },
}

/// The function that a [`Step::TailCall`] calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TailTo {
    /// The running one, as the same closure.
    Own,
    /// This one, by name.
    Func(FuncIdx),
}

impl Step {
    /// [`Ins::falls_through`].
    pub(super) fn falls_through(&self) -> bool {
        match self {
            Step::Ins(ins) => ins.falls_through(),
            Step::TailCall { .. } => false,
        }
    }

    /// [`Ins::operands`].
    pub(super) fn operands(&self, lists: &Lists<VReg>, f: &mut impl FnMut(VReg, Role)) {
        match self {
            Step::Ins(ins) => ins.operands(lists, f),
            Step::TailCall { args, .. } => args.iter().for_each(|a| f(*a, Role::Take)),
        }
    }

    /// [`Ins::targets`].
    pub(super) fn targets(&self, lists: &Lists<VReg>, f: &mut impl FnMut(Label)) {
        match self {
            Step::Ins(ins) => ins.targets(lists, f),
            Step::TailCall { .. } => {}
        }
    }
}

/// What a function does before its values have registers.
pub(super) struct Draft {
    pub(super) steps: Vec<Step>,
    pub(super) lists: Lists<VReg>,
    /// Where each label is, as the number of the step it stands at.
    pub(super) labels: Vec<usize>,
    /// One past the highest value the function names.
    pub(super) values: u32,
    pub(super) params: Vec<VReg>,
    /// Each label that only a switch jumps to, and only for constructors with no
    /// fields, with the value it is on. That holds no cell there.
    pub(super) words: Vec<(Label, VReg)>,
    /// What each value may be by its type, by the value's number. A number
    /// that no value has says nothing.
    pub(super) kinds: Vec<Option<Makes>>,
    /// What it reads of the closure it runs as.
    pub(super) closure: ReadsClosure,
    /// The misses of its probes that have the steps of a join or of another miss
    /// behind them.
    pub(super) misses: Vec<Miss>,
}

/// The steps of a probe's miss. Nothing but the probe goes to them.
#[derive(Debug, Clone, Copy)]
pub(super) struct Miss {
    pub(super) from: Label,
    /// The first that is not one of them.
    pub(super) to: Label,
}

/// The instructions for `core`. `this` is the function's own index, when it
/// has one.
pub(super) fn translate<'c>(
    source: &'c Source<'_>,
    core: &'c CoreFn,
    this: Option<FuncIdx>,
    tables: &'c mut Tables,
) -> Result<Draft, Unfit> {
    let survey = survey(core);
    let values = match survey.highest {
        None => 0,
        Some(h) => h.checked_add(1).ok_or(Unfit::TooMany(Limit::Values))?,
    };
    let kinds = survey.kinds(&source.program.pool, values)?;
    let mut t = Translator {
        source,
        this,
        tables,
        steps: Vec::new(),
        lists: Lists::default(),
        labels: Vec::new(),
        landed: None,
        joins: HashMap::new(),
        words: Vec::new(),
        closure: survey.closure,
        reads: survey.reads,
        numbers: HashMap::new(),
        misses: Vec::new(),
        no_arm: None,
    };
    t.expr(&core.body, None)?;
    if let Some(no_arm) = t.no_arm {
        t.place(no_arm);
        let bad = t.broken(Broken::NoArm)?;
        t.push(bad);
    }
    let labels: Option<Vec<usize>> = t.labels.into_iter().collect();
    Ok(Draft {
        steps: t.steps,
        lists: t.lists,
        labels: labels.ok_or(Unfit::Broken(Broken::NoPlace))?,
        values,
        params: core.params.iter().map(|p| of(p.id)).collect(),
        words: t.words,
        kinds,
        closure: t.closure,
        misses: t.misses,
    })
}

fn of(local: LocalId) -> VReg {
    VReg(local.0)
}

fn src(local: LocalId) -> Src<VReg> {
    Src(of(local))
}

/// A number the program spells, which an operator's instruction can hold.
#[derive(Clone, Copy)]
enum Number {
    Int(SmallInt),
    Float(Float),
}

/// A join, for the jumps to it.
#[derive(Clone, Copy)]
struct Join<'c> {
    /// Where its continuation is.
    to: Label,
    params: &'c [CoreBind],
}

struct Translator<'c> {
    source: &'c Source<'c>,
    this: Option<FuncIdx>,
    tables: &'c mut Tables,
    steps: Vec<Step>,
    lists: Lists<VReg>,
    /// Where each label is, once it is placed.
    labels: Vec<Option<usize>>,
    /// Where the last label was placed.
    landed: Option<usize>,
    /// The joins in scope, for the jumps to them.
    joins: HashMap<JoinId, Join<'c>>,
    /// [`Draft::words`].
    words: Vec<(Label, VReg)>,
    /// [`Draft::closure`].
    closure: ReadsClosure,
    /// How many times each local is read.
    reads: HashMap<LocalId, u32>,
    /// Each local met so far that is a constant number.
    numbers: HashMap<LocalId, Number>,
    /// [`Draft::misses`].
    misses: Vec<Miss>,
    /// Where the probes that cannot miss go when they do, behind everything else.
    no_arm: Option<Label>,
}

impl<'c> Translator<'c> {
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

    /// [`Self::no_arm`].
    fn no_arm(&mut self) -> Result<Label, Unfit> {
        if let Some(to) = self.no_arm {
            return Ok(to);
        }
        let to = self.label()?;
        self.no_arm = Some(to);
        Ok(to)
    }

    /// Put `l` at the next step.
    fn place(&mut self, l: Label) {
        let here = self.steps.len();
        if let Some(at) = self.labels.get_mut(l.0 as usize) {
            *at = Some(here);
        }
        self.landed = Some(here);
    }

    fn push(&mut self, ins: Ins<VReg>) {
        self.steps.push(Step::Ins(ins));
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
        let why = match self.source.program.fns.get(func) {
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

    /// The call of `callee` that puts its result in `dst`.
    fn call(
        &mut self,
        dst: Dst<VReg>,
        callee: &Callee,
        args: &[LocalId],
    ) -> Result<Ins<VReg>, Unfit> {
        let own = OwnRoom;
        Ok(match self.calls(callee, args.len())? {
            Calls::Func(func) => self.call_by_name(dst, func, args)?,
            Calls::Own(this) if !self.closure.keeps() => self.call_by_name(dst, this, args)?,
            Calls::Own(_) => {
                let to = OwnEntry;
                match self.args(args)? {
                    Fit::Inline(args) => Ins::CallSelf { dst, own, to, args },
                    Fit::Listed(args) => Ins::CallSelfWide { dst, own, to, args },
                }
            }
            Calls::Value(callee) => match self.args(args)? {
                Fit::Inline(args) => Ins::CallValue {
                    dst,
                    callee,
                    args,
                    own,
                },
                Fit::Listed(args) => Ins::CallValueWide {
                    dst,
                    callee,
                    args,
                    own,
                },
            },
            Calls::Bad(bad) => bad,
        })
    }

    /// The call of `callee` that takes over the frame.
    fn tail_call(&mut self, callee: &Callee, args: &[LocalId]) -> Result<Step, Unfit> {
        let own = OwnRoom;
        let to = match self.calls(callee, args.len())? {
            Calls::Func(func) => TailTo::Func(func),
            Calls::Own(_) => TailTo::Own,
            Calls::Value(callee) => {
                return Ok(Step::Ins(match self.args(args)? {
                    Fit::Inline(args) => Ins::TailCallValue { callee, args, own },
                    Fit::Listed(args) => Ins::TailCallValueWide { callee, args, own },
                }));
            }
            Calls::Bad(bad) => return Ok(Step::Ins(bad)),
        };
        let args = args.iter().copied().map(of).collect();
        Ok(Step::TailCall { to, args })
    }

    /// What a call of `callee` with `got` arguments calls.
    fn calls(&mut self, callee: &Callee, got: usize) -> Result<Calls, Unfit> {
        if got > MAX_PARAMS {
            return Err(Unfit::TooMany(Limit::Arguments));
        }
        let (func, calls) = match (*callee, self.this) {
            (Callee::Known(func), _) => (func, Calls::Func(func)),
            (Callee::Self_, Some(this)) => (this, Calls::Own(this)),
            (Callee::Self_, None) => {
                return self.broken(Broken::ToplevelCallsSelf).map(Calls::Bad);
            }
            (Callee::Local(f), _) => return Ok(Calls::Value(Shared(of(f)))),
        };
        let bad = self.bad_call(func, got)?;
        Ok(bad.map_or(calls, Calls::Bad))
    }

    /// The call to `func` that puts its result in `dst`.
    fn call_by_name(
        &mut self,
        dst: Dst<VReg>,
        func: FuncIdx,
        args: &[LocalId],
    ) -> Result<Ins<VReg>, Unfit> {
        let (own, to) = (OwnRoom, super::ins::Callee(func));
        let arg = |l: LocalId| Arg::shared(of(l));
        Ok(match *args {
            [] => Ins::Call0 { dst, own, to },
            [a] => {
                let a = arg(a);
                Ins::Call1Share { dst, own, to, a }
            }
            [a, b] => {
                let (a, b) = (arg(a), arg(b));
                Ins::Call2Share { dst, own, to, a, b }
            }
            [a, b, c] => {
                let (a, b, c) = (arg(a), arg(b), arg(c));
                Ins::Call3Share {
                    dst,
                    own,
                    to,
                    a,
                    b,
                    c,
                }
            }
            _ => match self.args(args)? {
                Fit::Inline(args) => Ins::Call { dst, own, to, args },
                Fit::Listed(args) => Ins::CallWide { dst, own, to, args },
            },
        })
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
        let constant = |step: &mut Step| matches!(step, Step::Ins(Ins::Const { .. }));
        let between = || self.steps.pop_if(constant);
        let constants: Vec<Step> = std::iter::from_fn(between).collect();
        let jumped_past = self.landed.is_some_and(|at| at >= self.steps.len());
        let jump = match self.steps.last() {
            Some(Step::Ins(compare)) if !jumped_past => jump_unless(compare, of(cond), to),
            Some(Step::Ins(_) | Step::TailCall { .. }) | None => None,
        };
        if jump.is_some() {
            self.steps.pop();
        }
        self.steps.extend(constants.into_iter().rev());
        jump
    }

    /// Write `e`. The steps of a join or of a miss come behind it, at `behind`, when it
    /// says where.
    fn expr(&mut self, e: &'c CoreExpr, behind: Option<Label>) -> Result<(), Unfit> {
        let mut e = e;
        loop {
            match e {
                // A map given up right after it is set gives its reference to
                // the set. What else is given up there goes after it.
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
                    && let Some(after) = giving_up(*m, body) =>
                {
                    self.push(Ins::MapSetOwned {
                        dst: Dst(of(bind.id)),
                        map: Taken(of(*m)),
                        key: src(*k),
                        value: src(*v),
                    });
                    self.give_up_all(&after.drops);
                    e = after.rest;
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
                    && let Some(after) = giving_up(*m, body) =>
                {
                    self.push(Ins::MapDeleteOwned {
                        dst: Dst(of(bind.id)),
                        map: Taken(of(*m)),
                        key: src(*k),
                    });
                    self.give_up_all(&after.drops);
                    e = after.rest;
                }
                CoreExpr::Let { bind, rhs, body } => {
                    if let Atom::Const(c) = rhs
                        && let Some(n) = number(self.source.program.consts.get(c.index()))
                    {
                        self.numbers.insert(bind.id, n);
                    }
                    self.atom(of(bind.id), rhs)?;
                    e = body;
                }
                CoreExpr::SetGlobal { slot, value, body } => {
                    let src = Shared(of(*value));
                    self.push(Ins::SetGlobal { slot: *slot, src });
                    e = body;
                }
                CoreExpr::Drop { local, body } => {
                    self.give_up(*local);
                    e = body;
                }
                // Every path through `body` ends in a return or a jump, so
                // the continuation after it is reached only by the jumps to it.
                CoreExpr::Join {
                    id,
                    params,
                    cont,
                    body,
                } => {
                    let to = self.label()?;
                    self.joins.insert(*id, Join { to, params });
                    self.expr(body, Some(to))?;
                    self.place(to);
                    // It is in scope only in the body.
                    self.joins.remove(id);
                    e = cont;
                }
                CoreExpr::If { cond, then, els } => {
                    let to = self.label()?;
                    let jump = match self.take_comparison(*cond, to) {
                        Some(jump) => jump,
                        None => Ins::JumpIfFalse {
                            cond: src(*cond),
                            to,
                        },
                    };
                    self.push(jump);
                    self.expr(then, None)?;
                    self.place(to);
                    e = els;
                }
                CoreExpr::Probe {
                    op,
                    args,
                    binds,
                    hit,
                    miss,
                } => {
                    // Where it cannot miss, the hit has nothing behind it.
                    if matches!(**miss, CoreExpr::NoArm) {
                        let to = self.no_arm()?;
                        let probe = self.probe(*op, args, binds, to)?;
                        self.push(probe);
                        e = hit;
                        continue;
                    }
                    let to = self.label()?;
                    let probe = self.probe(*op, args, binds, to)?;
                    self.push(probe);
                    self.expr(hit, Some(to))?;
                    self.place(to);
                    if let Some(behind) = behind {
                        self.misses.push(Miss {
                            from: to,
                            to: behind,
                        });
                    }
                    e = miss;
                }
                CoreExpr::Open {
                    cell,
                    variant,
                    fields,
                    body,
                } => {
                    self.open(*cell, *variant, fields)?;
                    e = body;
                }
                CoreExpr::Switch {
                    scrut,
                    cases,
                    default,
                } => {
                    let default = default.as_deref();
                    e = match cases {
                        Cases::Variant { ty, arms } => {
                            once_each(arms)?;
                            self.by_variant(*scrut, *ty, arms, default)?
                        }
                        Cases::Int(arms) => {
                            let numbers = once_each(arms)?;
                            self.by_int(*scrut, arms, &numbers, default)?
                        }
                        Cases::Float(arms) => {
                            once_each(arms)?;
                            self.each_test(*scrut, arms, default, &mut Self::is_float)?
                        }
                        Cases::Text(arms) => {
                            once_each(arms)?;
                            self.each_test(*scrut, arms, default, &mut Self::is_text)?
                        }
                    };
                }
                CoreExpr::NoArm => {
                    let no_arm = self.broken(Broken::NoArm)?;
                    self.push(no_arm);
                    return Ok(());
                }
                CoreExpr::Ret(value) => {
                    let (src, last) = (Taken(of(*value)), LastReg);
                    self.push(if self.closure.keeps() {
                        Ins::RetClosure { src, last }
                    } else {
                        Ins::Ret { src, last }
                    });
                    return Ok(());
                }
                CoreExpr::TailCall { callee, args } => {
                    let call = self.tail_call(callee, args)?;
                    self.steps.push(call);
                    return Ok(());
                }
                CoreExpr::Jump { to, args } => return self.jump(*to, args),
            }
        }
    }

    /// Give a join's parameters their values and go on with it. No parameter is
    /// among `args`, because a join's own are not in scope where it is jumped to.
    fn jump(&mut self, to: JoinId, args: &[LocalId]) -> Result<(), Unfit> {
        let Some(Join { to, params }) = self.joins.get(&to).copied() else {
            let bad = self.broken(Broken::UndeclaredJoin)?;
            self.push(bad);
            return Ok(());
        };
        if params.len() != args.len() {
            let (got, want) = (args.len(), params.len());
            let bad = self.broken(Broken::JumpArity { got, want })?;
            self.push(bad);
            return Ok(());
        }
        for (param, arg) in params.iter().zip(args) {
            self.push(Ins::Move {
                dst: Dst(of(param.id)),
                src: Arg::shared(of(*arg)),
            });
        }
        self.push(Ins::Jump { to });
        Ok(())
    }

    fn give_up_all(&mut self, locals: &[LocalId]) {
        locals.iter().for_each(|l| self.give_up(*l));
    }

    fn give_up(&mut self, local: LocalId) {
        let reg = Dead(of(local));
        self.push(Ins::Drop { reg });
    }

    /// The instruction that asks for `op` of `args`, and goes on at `miss` where it is
    /// not there.
    fn probe(
        &mut self,
        op: ProbeOp,
        args: &[LocalId],
        binds: &[Option<CoreBind>],
        miss: Label,
    ) -> Result<Ins<VReg>, Unfit> {
        let odd = Broken::Probe {
            op,
            args: args.len(),
            binds: binds.len(),
        };
        let to = |bind: &Option<CoreBind>| bind.as_ref().map(|bind| of(bind.id));
        Ok(match (op, args, binds) {
            (ProbeOp::ArraySlice, [xs, start, end], [bind]) => Ins::ArraySlice {
                dst: MaybeHit::new(to(bind)),
                src: src(*xs),
                start: src(*start),
                end: src(*end),
                miss,
            },
            (ProbeOp::ArraySet, [xs, i, x], [Some(bind)]) => Ins::ArraySet {
                dst: Hit(of(bind.id)),
                src: Arg::shared(of(*xs)),
                index: src(*i),
                value: Arg::shared(of(*x)),
                miss,
            },
            // A write that nothing reads asks whether the array has the index.
            (ProbeOp::ArrayGet, [xs, i], [bind]) | (ProbeOp::ArraySet, [xs, i, _], [bind]) => {
                Ins::ArrayGet {
                    dst: MaybeHit::new(to(bind)),
                    src: src(*xs),
                    index: src(*i),
                    miss,
                }
            }
            (ProbeOp::MapGet, [m, k], [bind]) => Ins::MapGet {
                dst: MaybeHit::new(to(bind)),
                map: src(*m),
                key: src(*k),
                miss,
            },
            (ProbeOp::ArrayUncons, [xs], [head, Some(tail)]) => Ins::ArrayUncons {
                head: MaybeHit::new(to(head)),
                tail: Hit(of(tail.id)),
                src: Arg::shared(of(*xs)),
                miss,
            },
            (ProbeOp::ArrayUncons, [xs], [head, None]) => {
                let length = Length { len: 1, more: true };
                let split = Split { length, from: 0 };
                self.split(*xs, split, std::slice::from_ref(head), None, miss)?
            }
            (ProbeOp::ArraySplit { len, more, from }, [xs], _) => {
                let (tail, elements) = match (more, binds.split_last()) {
                    (false, _) => (None, binds),
                    (true, Some((tail, elements))) => (to(tail), elements),
                    (true, None) => return Err(odd.into()),
                };
                let found = u32::try_from(elements.len()).ok();
                let end = found.and_then(|n| from.checked_add(n));
                let cons = (len, more, from) == (1, true, 0) && !elements.is_empty();
                if cons || end.is_none_or(|end| end > len) {
                    return Err(odd.into());
                }
                let length = Length { len, more };
                let split = Split { length, from };
                self.split(*xs, split, elements, tail, miss)?
            }
            (
                ProbeOp::ArrayGet
                | ProbeOp::ArraySlice
                | ProbeOp::ArraySet
                | ProbeOp::ArrayUncons
                | ProbeOp::ArraySplit { .. }
                | ProbeOp::MapGet,
                ..,
            ) => return Err(odd.into()),
        })
    }

    /// The instruction that takes `xs` apart as `split` says, each of `elements` to its
    /// local.
    fn split(
        &mut self,
        xs: LocalId,
        split: Split,
        elements: &[Option<CoreBind>],
        tail: Option<VReg>,
        miss: Label,
    ) -> Result<Ins<VReg>, Unfit> {
        let (src_of, tail) = (src(xs), MaybeHit::new(tail));
        let src = Arg::shared(of(xs));
        let hit = |bind: &Option<CoreBind>| MaybeHit::new(bind.as_ref().map(|b| of(b.id)));
        // What nothing reads is not found.
        let used = elements.iter().rposition(Option::is_some);
        let used = used.map_or(0, |last| last.saturating_add(1));
        let elements = elements.iter().take(used).map(hit);
        let elements: Box<[MaybeHit<VReg>]> = elements.collect();
        if elements.is_empty() && tail.hit().is_none() {
            return Ok(Ins::JumpUnlessLength {
                src: src_of,
                length: split.length,
                to: miss,
            });
        }
        if let (Some(split), Some(elements)) = (Near::new(split), Hits::new(&elements)) {
            return Ok(Ins::ArraySplit {
                elements,
                tail,
                src,
                split,
                miss,
            });
        }
        let wide = SplitWide { split, elements };
        Ok(Ins::ArraySplitWide {
            elements: add(&mut self.lists.splits, wide).map(HitList)?,
            tail,
            src,
            miss,
        })
    }

    /// Take the `variant` in `cell` apart, each of `fields` to its local. Any other
    /// field goes nowhere.
    fn open(
        &mut self,
        cell: LocalId,
        variant: VariantRef,
        fields: &[(u16, CoreBind)],
    ) -> Result<(), Unfit> {
        let odd = || Unfit::Broken(Broken::OpenFields(variant));
        let Form::Fields { ctor, fields: n } = self.source.forms.get(variant)? else {
            return Err(odd());
        };
        if fields.is_empty() || !fields.is_sorted_by(|(a, _), (b, _)| a < b) {
            return Err(odd());
        }
        let mut dsts = vec![MaybeDst::new(None); n];
        for (field, bind) in fields {
            let dst = dsts.get_mut(usize::from(*field)).ok_or_else(odd)?;
            *dst = MaybeDst::new(Some(of(bind.id)));
        }
        let src = Taken(of(cell));
        // The form with as many fields.
        macro_rules! one_of {
            ($($name:ident)*) => {
                None$(.or_else(|| {
                    let dsts = dsts.as_slice().try_into().ok()?;
                    Some(Ins::$name { dsts, src, ctor })
                }))*
            };
        }
        let open = match one_of!(Open1 Open2 Open3 Open4 Open5 Open6 Open7 Open8) {
            Some(open) => open,
            None => Ins::OpenWide {
                dsts: add(&mut self.lists.dsts, dsts.into()).map(DstList)?,
                src,
                ctor,
            },
        };
        self.push(open);
        Ok(())
    }

    /// Write each of `arms` but the last of them all, which it gives back, with a label
    /// in front of each.
    fn arms(&mut self, mut arms: Vec<(Label, &'c CoreExpr)>) -> Result<&'c CoreExpr, Unfit> {
        let Some((to, last)) = arms.pop() else {
            return Ok(&CoreExpr::NoArm);
        };
        for (to, arm) in arms {
            self.place(to);
            self.expr(arm, None)?;
        }
        self.place(to);
        Ok(last)
    }

    /// Go on by which variant of `ty` `scrut` is. Gives the arm that is written last.
    fn by_variant(
        &mut self,
        scrut: LocalId,
        ty: TypeId,
        arms: &'c Arms<u16>,
        default: Option<&'c CoreExpr>,
    ) -> Result<&'c CoreExpr, Unfit> {
        let source = self.source;
        let variant = |v| VariantRef {
            type_id: ty,
            variant_idx: v,
        };
        // Which arm each variant has, of those that a case names.
        let mut arm_of: HashMap<u16, usize> = HashMap::new();
        let mut outcomes: Vec<Outcome<'c>> = Vec::with_capacity(arms.len().saturating_add(1));
        let mut with_fields = 0usize;
        for (i, (variants, e)) in arms.iter().enumerate() {
            let mut plain = true;
            for v in variants {
                let has_fields = match source.forms.get(variant(*v))? {
                    Form::Plain(_) => false,
                    Form::Fields { .. } => true,
                };
                plain &= !has_fields;
                with_fields = with_fields.saturating_add(usize::from(has_fields));
                arm_of.insert(*v, i);
            }
            let only = match variants.as_slice() {
                [v] => Some(*v),
                _ => None,
            };
            outcomes.push(Outcome { e, plain, only });
        }
        let (len, named) = match source.forms.all(ty) {
            Some(all) => (all.len(), arm_of.len()),
            None => (0, 0),
        };
        if named < len {
            let mut left = (0..=u16::MAX).filter(|v| !arm_of.contains_key(v));
            let only = match len.saturating_sub(named) {
                1 => left.next(),
                _ => None,
            };
            let plain = source.forms.all(ty);
            outcomes.push(Outcome {
                e: default.unwrap_or(&CoreExpr::NoArm),
                plain: plain.is_some_and(|all| all.with_fields() == with_fields),
                only,
            });
        }
        if let [one] = outcomes.as_slice() {
            return Ok(one.e);
        }
        if outcomes.is_empty() {
            return Ok(default.unwrap_or(&CoreExpr::NoArm));
        }
        if let [a, b] = outcomes.as_slice() {
            // The one that a test can be for goes first.
            let tested = a.only.map(|v| (v, a, b));
            if let Some((v, yes, no)) = tested.or(b.only.map(|v| (v, b, a))) {
                let to = self.test_variant(scrut, ty, v)?;
                self.expr(yes.e, None)?;
                self.place(to);
                if no.plain {
                    self.words.push((to, of(scrut)));
                }
                return Ok(no.e);
            }
        }
        let dense = named.saturating_mul(TABLE_FILL).saturating_add(TABLE_ANY);
        if len <= dense
            && let Some(last) = len.checked_sub(1)
            && let Ok(last) = u16::try_from(last)
        {
            let mut all = Vec::with_capacity(outcomes.len());
            for outcome in &outcomes {
                let to = self.label()?;
                if outcome.plain {
                    self.words.push((to, of(scrut)));
                }
                all.push((to, outcome.e));
            }
            let first = u32::try_from(self.lists.tables.len()).map_err(|_| Unfit::TooLong)?;
            for v in 0..=last {
                let arm = arm_of.get(&v).copied().unwrap_or(arms.len());
                let to = all.get(arm).map(|(to, _)| *to);
                self.lists.tables.push(to.ok_or(Broken::NoPlace)?);
            }
            let (src, table) = (src(scrut), Table { first, last });
            self.push(Ins::Switch { src, table });
            return self.arms(all);
        }
        self.each_test(scrut, arms, default, &mut |t, scrut, v| {
            t.test_variant(scrut, ty, *v)
        })
    }

    /// A test of whether `scrut` is variant `v` of `ty`, and the label that it jumps to
    /// when it is not.
    fn test_variant(&mut self, scrut: LocalId, ty: TypeId, v: u16) -> Result<Label, Unfit> {
        let form = self.source.forms.get(VariantRef {
            type_id: ty,
            variant_idx: v,
        })?;
        let to = self.label()?;
        self.push(Ins::JumpUnlessTag {
            src: src(scrut),
            tag: form.tag(),
            to,
        });
        Ok(to)
    }

    /// Go on by the Int in `scrut`, where `numbers` are those that `arms` name, in
    /// order. Gives the arm that is written last.
    fn by_int(
        &mut self,
        scrut: LocalId,
        arms: &'c Arms<i64>,
        numbers: &[i64],
        default: Option<&'c CoreExpr>,
    ) -> Result<&'c CoreExpr, Unfit> {
        // Those near each other are found by a table.
        let run = numbers.get(near(numbers)).unwrap_or_default();
        let run = match run {
            [first, .., last] if run.len() > FEW_INTS => {
                let first = i32::try_from(*first).ok();
                first.map(|first| (first, i64::from(first)..=*last, run.len()))
            }
            [] | [_] | [_, ..] => None,
        };
        let in_table = |n: &i64| run.as_ref().is_some_and(|(_, run, _)| run.contains(n));
        let in_run = run.as_ref().map_or(0, |(_, _, len)| *len);
        // The others are tested one by one when they are few.
        let few = numbers.len().saturating_sub(in_run) <= FEW_FAR;
        let mut arms = arms.as_slice();
        // Those written in front of any in the table are tested in front of it. So an arm
        // comes later than by a test of each only by what the table takes longer than the
        // one test it stands for.
        while few
            && let [(keys, arm), rest @ ..] = arms
            && !keys.iter().any(in_table)
        {
            self.tested(scrut, keys, arm, &mut Self::is_int)?;
            arms = rest;
        }
        if arms.is_empty() {
            return Ok(default.unwrap_or(&CoreExpr::NoArm));
        }
        let mut table: Vec<(i64, Label)> = Vec::with_capacity(in_run);
        let mut far: Vec<(i64, Label)> = Vec::new();
        // The label of each arm that is written behind them all.
        let mut to: Vec<Option<Label>> = Vec::with_capacity(arms.len());
        let mut left = Vec::with_capacity(arms.len().saturating_add(1));
        for (keys, arm) in arms {
            if few && !keys.iter().any(in_table) {
                to.push(None);
                continue;
            }
            let label = self.label()?;
            for n in keys {
                let into = if in_table(n) { &mut table } else { &mut far };
                into.push((*n, label));
            }
            to.push(Some(label));
            left.push((label, arm));
        }
        let other = self.label()?;
        left.push((other, default.unwrap_or(&CoreExpr::NoArm)));
        let any_far = arms.iter().any(|(keys, _)| !keys.iter().all(in_table));
        if let Some((first, ..)) = run {
            let past = if any_far { self.label()? } else { other };
            table.sort_unstable_by_key(|(n, _)| *n);
            let table = self.table(&table, first, past)?;
            let (src, table) = (src(scrut), IntTable { table, first });
            self.push(Ins::SwitchInt { src, table });
            if any_far {
                self.place(past);
            }
        }
        if !few {
            far.sort_unstable_by_key(|(n, _)| *n);
            let places = add(&mut self.lists.places, far.into()).map(PlaceList)?;
            let (src, to) = (src(scrut), other);
            self.push(Ins::SwitchIntFar { src, places, to });
            return self.arms(left);
        }
        for ((keys, arm), to) in arms.iter().zip(to) {
            let Some(to) = to else {
                self.tested(scrut, keys, arm, &mut Self::is_int)?;
                continue;
            };
            for n in keys.iter().filter(|n| !in_table(n)) {
                let next = self.is_int(scrut, n)?;
                self.push(Ins::Jump { to });
                self.place(next);
            }
        }
        if any_far {
            self.push(Ins::Jump { to: other });
        }
        self.arms(left)
    }

    /// A table of where each number from `first` on goes, with `past` for those that
    /// `run` leaves out and for any other.
    fn table(&mut self, run: &[(i64, Label)], first: i32, past: Label) -> Result<Table, Unfit> {
        let start = u32::try_from(self.lists.tables.len()).map_err(|_| Unfit::TooLong)?;
        let mut last = 0u16;
        for (n, to) in run {
            while i64::from(first).checked_add(i64::from(last)) != Some(*n) {
                self.lists.tables.push(past);
                last = last.checked_add(1).ok_or(Unfit::TooLong)?;
            }
            self.lists.tables.push(*to);
            last = last.checked_add(1).ok_or(Unfit::TooLong)?;
        }
        self.lists.tables.push(past);
        Ok(Table { first: start, last })
    }

    /// Go on by a test of `scrut` for each value that has an arm, in order. `test`
    /// gives the label that a test jumps to when it fails.
    fn each_test<K>(
        &mut self,
        scrut: LocalId,
        arms: &'c Arms<K>,
        default: Option<&'c CoreExpr>,
        test: &mut impl FnMut(&mut Self, LocalId, &K) -> Result<Label, Unfit>,
    ) -> Result<&'c CoreExpr, Unfit> {
        for (keys, arm) in arms {
            self.tested(scrut, keys, arm, test)?;
        }
        Ok(default.unwrap_or(&CoreExpr::NoArm))
    }

    /// A test of `scrut` for each of `keys`, and `arm` behind them, which they go to.
    /// What is none of them comes to the next step.
    fn tested<K>(
        &mut self,
        scrut: LocalId,
        keys: &[K],
        arm: &'c CoreExpr,
        test: &mut impl FnMut(&mut Self, LocalId, &K) -> Result<Label, Unfit>,
    ) -> Result<(), Unfit> {
        let Some((last, others)) = keys.split_last() else {
            return Ok(());
        };
        let here = self.label()?;
        for key in others {
            let next = test(self, scrut, key)?;
            self.push(Ins::Jump { to: here });
            self.place(next);
        }
        let next = test(self, scrut, last)?;
        self.place(here);
        self.expr(arm, None)?;
        self.place(next);
        Ok(())
    }

    fn is_int(&mut self, scrut: LocalId, n: &i64) -> Result<Label, Unfit> {
        let to = self.label()?;
        let (src, n) = (src(scrut), *n);
        self.push(Ins::JumpUnlessInt { src, n, to });
        Ok(to)
    }

    fn is_float(&mut self, scrut: LocalId, x: &FloatKey) -> Result<Label, Unfit> {
        let to = self.label()?;
        let (src, x) = (src(scrut), x.get());
        self.push(Ins::JumpUnlessFloat { src, x, to });
        Ok(to)
    }

    fn is_text(&mut self, scrut: LocalId, c: &ConstId) -> Result<Label, Unfit> {
        let to = self.label()?;
        let text = match self.source.program.consts.get(c.index()) {
            Some(Const::String(text)) => text,
            Some(Const::Int(_) | Const::Float(_) | Const::Binary { .. }) => {
                return Err(Broken::TextCase(*c).into());
            }
            None => return Err(Missing::NoConst(c.index()).into()),
        };
        let (src, text) = (src(scrut), self.tables.text(text.as_bytes())?);
        self.push(Ins::JumpUnlessStr { src, text, to });
        Ok(to)
    }

    fn atom(&mut self, reg: VReg, atom: &Atom) -> Result<(), Unfit> {
        let dst = Dst(reg);
        let constant = |value| Ins::Const { dst, value };
        let ins = match atom {
            Atom::Local(local) => Ins::Move {
                dst,
                src: Arg::shared(of(*local)),
            },
            Atom::Const(c) => match self.source.program.consts.get(c.index()) {
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
            Atom::Load(Load::Capture(i)) => match (self.this, u32::try_from(i.0)) {
                (Some(_), Ok(index)) => Ins::GetCapture { dst, index },
                (Some(_), Err(_)) => self.broken(Broken::NegativeCapture)?,
                (None, _) => self.broken(Broken::ToplevelCapture)?,
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
            Atom::Call { callee, args } => self.call(dst, callee, args)?,
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
            Atom::Intrinsic {
                intrinsic: Intrinsic::FloatFromInt,
                args,
            } if let [a] = args.as_slice() => Ins::FloatFromInt { dst, a: src(*a) },
            Atom::Intrinsic {
                intrinsic: Intrinsic::InternalStackDepth,
                args,
            } if args.is_empty() => Ins::StackDepth { dst },
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
            Atom::Ctor { variant, fields } => {
                match self.source.forms.of(*variant, fields.len())? {
                    Form::Plain(variant) => constant(Value::nullary(variant)),
                    Form::Fields { ctor, .. } => {
                        let fields: Vec<VReg> = fields.iter().copied().map(of).collect();
                        self.ctor(dst, ctor, &fields)?
                    }
                }
            }
            Atom::Update {
                base,
                variant,
                sets,
            } => self.update(dst, *base, *variant, sets)?,
        };
        self.push(ins);
        Ok(())
    }

    fn ctor(&mut self, dst: Dst<VReg>, ctor: Ctor, fields: &[VReg]) -> Result<Ins<VReg>, Unfit> {
        Ok(match Args::new(fields) {
            Some(fields) => Ins::Ctor { dst, ctor, fields },
            None => Ins::CtorWide {
                dst,
                ctor,
                fields: self.arg_list(fields)?,
            },
        })
    }

    fn update(
        &mut self,
        dst: Dst<VReg>,
        base: LocalId,
        variant: VariantRef,
        sets: &[(u16, LocalId)],
    ) -> Result<Ins<VReg>, Unfit> {
        let odd = || Unfit::Broken(Broken::UpdateFields(variant));
        let Form::Fields { ctor, fields } = self.source.forms.get(variant)? else {
            return Err(odd());
        };
        let in_order = sets.is_sorted_by(|(a, _), (b, _)| a < b);
        let last = sets.last().ok_or_else(odd)?;
        if !in_order || usize::from(last.0) >= fields {
            return Err(odd());
        }
        let sets = sets.iter().map(|(field, local)| Assign {
            field: *field,
            value: Arg::shared(of(*local)),
        });
        let sets: Box<[_]> = sets.collect();
        let (base, layout) = (of(base), Layout::new(ctor, fields));
        // The form with as many fields.
        macro_rules! one_of {
            ($($name:ident)*) => {
                None$(.or_else(|| {
                    let sets = Sets::new(base, layout, &sets)?;
                    Some(Ins::$name { dst, sets })
                }))*
            };
        }
        Ok(match one_of!(Update1 Update2 Update3) {
            Some(update) => update,
            None => Ins::UpdateWide {
                dst,
                base: Arg::shared(base),
                layout,
                sets: add(&mut self.lists.sets, sets).map(AssignList)?,
            },
        })
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

    /// The instruction that puts `xs` with `items` at `end` in `dst`.
    fn pushes(
        &mut self,
        dst: Dst<VReg>,
        xs: LocalId,
        items: &[LocalId],
        end: End,
    ) -> Result<Ins<VReg>, Unfit> {
        let src = Arg::shared(of(xs));
        if let [item] = items {
            let item = Arg::shared(of(*item));
            return Ok(Ins::ArrayPush {
                dst,
                src,
                item,
                end,
            });
        }
        let items: Vec<VReg> = items.iter().copied().map(of).collect();
        Ok(Ins::ArrayExtend {
            dst,
            src,
            items: ShareList(self.reg_list(&items)?),
            end,
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
            PrimOp::MakeArray if args.is_empty() => Ok(Ins::Const {
                dst,
                value: Value::EMPTY_ARRAY,
            }),
            PrimOp::MakeArray => Ok(match self.shares(args.iter().copied())? {
                Fit::Inline(elements) => Ins::Array { dst, elements },
                Fit::Listed(elements) => Ins::ArrayWide { dst, elements },
            }),
            PrimOp::ArrayLen => one().map(|src| Ins::ArrayLen { dst, src }),
            PrimOp::ArrayPrepend => {
                let (xs, items) = args.split_last().ok_or(arity(Arity::AtLeastOne))?;
                self.pushes(dst, *xs, items, End::Front)
            }
            PrimOp::ArrayAppend => {
                let (xs, items) = args.split_first().ok_or(arity(Arity::AtLeastOne))?;
                self.pushes(dst, *xs, items, End::Back)
            }
            PrimOp::ArrayConcat => both().map(|(a, b)| Ins::ArrayConcat { dst, a, b }),
            PrimOp::MakeTuple => Ok(match *args {
                [a, b] => Ins::Tuple2 {
                    dst,
                    elements: [a, b].map(of).map(Shared),
                },
                [a, b, c] => Ins::Tuple3 {
                    dst,
                    elements: [a, b, c].map(of).map(Shared),
                },
                _ => match self.shares(args.iter().copied())? {
                    Fit::Inline(elements) => Ins::Tuple { dst, elements },
                    Fit::Listed(elements) => Ins::TupleWide { dst, elements },
                },
            }),
            PrimOp::TupleField(index) | PrimOp::Field(index) => {
                one().map(|src| Ins::Field { dst, src, index })
            }
        }
    }
}

/// How many entries a table of variants may have for each one that has an arm, and
/// how many in any case. A type may have thousands that no arm names.
const TABLE_FILL: usize = 4;
const TABLE_ANY: usize = 8;

/// So few Ints that a test for each is no dearer than a table.
const FEW_INTS: usize = 2;

/// So few that are far from each other that a test for each is no dearer than to look
/// for one among them.
const FEW_FAR: usize = 8;

/// Where in `numbers`, which are in order, the longest run is of those that are near
/// each other. A table of them has a place for at least one entry in four, and for
/// the rest of an instruction to name how long it is.
fn near(numbers: &[i64]) -> std::ops::Range<usize> {
    let (mut best, mut start) = (0..0, 0usize);
    for (i, n) in numbers.iter().enumerate() {
        let first = numbers.get(start).copied().unwrap_or(*n);
        let len = i.saturating_sub(start).saturating_add(1);
        let room = len.saturating_mul(TABLE_FILL).min(MAX_TABLE);
        let room = i64::try_from(room).unwrap_or(i64::MAX);
        if n.checked_sub(first).is_none_or(|span| span >= room) {
            start = i;
        }
        let end = i.saturating_add(1);
        if end.saturating_sub(start) > best.len() {
            best = start..end;
        }
    }
    best
}

/// Every value that `arms` name, in order, each of which has to be there once, and
/// each arm has to be for one.
fn once_each<K: Copy + Ord>(arms: &Arms<K>) -> Result<Vec<K>, Broken> {
    let mut keys: Vec<K> = arms.iter().flat_map(|(keys, _)| keys).copied().collect();
    keys.sort_unstable();
    let empty = arms.iter().any(|(keys, _)| keys.is_empty());
    match !empty && keys.is_sorted_by(|a, b| a < b) {
        true => Ok(keys),
        false => Err(Broken::Cases),
    }
}

/// How many numbers a table is for at the most.
const MAX_TABLE: usize = 60_000;

/// One way a switch on a variant can go.
struct Outcome<'c> {
    e: &'c CoreExpr,
    /// Whether it is for constructors with no fields.
    plain: bool,
    /// The one variant it is for, when there is one.
    only: Option<u16>,
}

/// What a call calls.
enum Calls {
    /// This function, by name.
    Func(FuncIdx),
    /// The running function, which is this one, as the same closure.
    Own(FuncIdx),
    /// The function value here.
    Value(Shared<VReg>),
    /// Nothing. The call cannot be right, and this stops the run in its place.
    Bad(Ins<VReg>),
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

/// The drops that `e` starts with, and what comes after them.
struct Head<'e> {
    drops: Vec<LocalId>,
    rest: &'e CoreExpr,
}

fn head(mut e: &CoreExpr) -> Head<'_> {
    let mut drops = Vec::new();
    while let CoreExpr::Drop { local, body } = e {
        drops.push(*local);
        e = body;
    }
    Head { drops, rest: e }
}

/// What `e` starts with beside a drop of `local`, when it has one there.
fn giving_up(local: LocalId, e: &CoreExpr) -> Option<Head<'_>> {
    let Head { mut drops, rest } = head(e);
    let all = drops.len();
    drops.retain(|l| *l != local);
    (drops.len() < all).then_some(Head { drops, rest })
}

/// What a value of type `ty` may be.
fn kind_of(pool: &ResolvedPool, ty: RTy) -> Result<Makes, ForeignRTy> {
    Ok(match (pool.repr(ty)?, pool.prim_of(ty)?) {
        (Repr::Word, _) => Makes::NoCell,
        (Repr::Counted, Some(Prim::Int)) => Makes::Int,
        (Repr::Counted, Some(Prim::Float | Prim::String) | None) => Makes::Any,
    })
}

/// What one walk over a function's body finds out.
#[derive(Default)]
struct Survey<'e> {
    /// How many times each local is read. A `Drop` counts as a read.
    reads: HashMap<LocalId, u32>,
    /// The highest local bound or read.
    highest: Option<u32>,
    /// Every place that binds a local.
    binds: Vec<&'e CoreBind>,
    /// [`Draft::closure`].
    closure: ReadsClosure,
}

impl<'e> Survey<'e> {
    fn name(&mut self, l: LocalId) {
        self.highest = self.highest.max(Some(l.0));
    }

    fn bind(&mut self, b: &'e CoreBind) {
        self.name(b.id);
        self.binds.push(b);
    }

    /// [`Draft::kinds`] of a function whose locals are below `values`.
    fn kinds(&self, pool: &ResolvedPool, values: u32) -> Result<Vec<Option<Makes>>, Unfit> {
        let mut kinds = vec![None; values as usize];
        for b in &self.binds {
            if let Some(kind) = kinds.get_mut(b.id.0 as usize) {
                *kind = Some(kind_of(pool, b.ty).map_err(|_| Broken::NoType)?);
            }
        }
        Ok(kinds)
    }

    fn read(&mut self, l: LocalId) {
        let n = self.reads.entry(l).or_default();
        *n = n.saturating_add(1);
        self.name(l);
    }

    fn atom(&mut self, atom: &Atom) {
        atom.for_each_operand(|l| self.read(l));
        if let Atom::Load(load) = atom {
            self.closure = self.closure.max(match load {
                Load::Global(_) => ReadsClosure::Nothing,
                Load::SelfClosure => ReadsClosure::Itself,
                Load::Capture(_) => ReadsClosure::Captures,
            });
        }
    }
}

fn survey(f: &CoreFn) -> Survey<'_> {
    let mut out = Survey::default();
    f.params.iter().for_each(|p| out.bind(p));
    let mut stack = vec![&f.body];
    while let Some(e) = stack.pop() {
        match e {
            CoreExpr::Let { bind, rhs, body } => {
                out.bind(bind);
                out.atom(rhs);
                stack.push(body);
            }
            CoreExpr::SetGlobal { value, body, .. } => {
                out.read(*value);
                stack.push(body);
            }
            CoreExpr::Join {
                params, cont, body, ..
            } => {
                params.iter().for_each(|p| out.bind(p));
                stack.push(cont);
                stack.push(body);
            }
            CoreExpr::Drop { local, body } => {
                out.read(*local);
                stack.push(body);
            }
            CoreExpr::Open {
                cell, fields, body, ..
            } => {
                out.read(*cell);
                fields.iter().for_each(|(_, b)| out.bind(b));
                stack.push(body);
            }
            CoreExpr::Switch {
                scrut,
                cases,
                default,
            } => {
                out.read(*scrut);
                stack.extend(cases.arms());
                stack.extend(default.as_deref());
            }
            CoreExpr::NoArm => {}
            CoreExpr::Probe {
                args,
                binds,
                hit,
                miss,
                ..
            } => {
                args.iter().for_each(|x| out.read(*x));
                binds.iter().flatten().for_each(|b| out.bind(b));
                stack.push(hit);
                stack.push(miss);
            }
            CoreExpr::If { cond, then, els } => {
                out.read(*cond);
                stack.push(then);
                stack.push(els);
            }
            CoreExpr::Ret(value) => out.read(*value),
            CoreExpr::TailCall { callee, args } => {
                callee.local().into_iter().for_each(|f| out.read(f));
                args.iter().for_each(|x| out.read(*x));
            }
            CoreExpr::Jump { args, .. } => args.iter().for_each(|x| out.read(*x)),
        }
    }
    out
}

#[cfg(test)]
mod tests;
