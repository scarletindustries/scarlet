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
//! A call names its callee in one of three ways. A function known when the
//! program loads is called by name, and that has an instruction for each number
//! of arguments up to three (`Call0` to `Call3`) and `Call` for more.
//! `CallSelf` is for a function that keeps the closure it runs as, and passes
//! it on, so a lambda calling itself still sees its captures. Any other
//! function calls itself by name. `CallValue` is for a function value in a
//! register. A call in tail position is one of three instructions that replace
//! the frame: `TailCall`, `TailCallSelf` and `TailCallValue`.

use std::collections::{HashMap, HashSet};

use scarlet_ir::core_ir::{
    Atom, Callee, Const, CoreBind, CoreExpr, CoreFn, CorePat, FuncIdx, GlobalSlot, JoinId, Load,
    LocalId, MAX_PARAMS, PrimOp,
};
use scarlet_ir::intrinsic::Intrinsic;
use scarlet_ir::tivec::Idx;

use super::forms::{CtorOf, Form};
use super::ins::{
    Arg, ArgList, Args, BitsOp, Dead, Dst, DstList, Hit, Ins, Label, LastReg, Lists, LookList,
    Looks, MaybeDst, Newline, OwnEntry, OwnRoom, ShareList, Shared, Shares, Src, Taken, VReg,
};
use super::{ReadsClosure, Source, Tables, Unfit, Why, add, built};
use crate::heap::Ctor;
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
    /// What it reads of the closure it runs as.
    pub(super) closure: ReadsClosure,
}

/// The instructions for `core`. `this` is the function's own index, when it
/// has one.
pub(super) fn translate(
    source: &Source<'_>,
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
        source,
        this,
        tables,
        ins: Vec::new(),
        lists: Lists::default(),
        labels: Vec::new(),
        landed: None,
        conts: HashMap::new(),
        values,
        closure: survey.closure,
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
        closure: t.closure,
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
    source: &'c Source<'c>,
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
    /// [`Draft::closure`].
    closure: ReadsClosure,
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
                if let Some(dst) = dst {
                    return self.call_by_name(Dst(dst), func, args);
                }
                let to = super::ins::Callee(func);
                Ok(match self.args(args)? {
                    Fit::Inline(args) => Ins::TailCall { to, args, own },
                    Fit::Listed(args) => Ins::TailCallWide { to, args, own },
                })
            }
            Callee::Self_ => {
                let Some(this) = self.this else {
                    return self.broken(Broken::ToplevelCallsSelf);
                };
                if let Some(bad) = self.bad_call(this, args.len())? {
                    return Ok(bad);
                }
                if let Some(dst) = dst
                    && !self.closure.keeps()
                {
                    return self.call_by_name(Dst(dst), this, args);
                }
                let to = OwnEntry;
                Ok(match (dst.map(Dst), self.args(args)?) {
                    (Some(dst), Fit::Inline(args)) => Ins::CallSelf { dst, own, to, args },
                    (Some(dst), Fit::Listed(args)) => Ins::CallSelfWide { dst, own, to, args },
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
                    && local == m =>
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
                    && local == m =>
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
                        && let Some(n) = number(self.source.program.consts.get(c.index()))
                    {
                        self.numbers.insert(bind.id, n);
                    }
                    self.atom(of(bind.id), rhs)?;
                    self.store(bind.id, bind.global);
                    e = body;
                }
                CoreExpr::Drop { local, body, .. } => {
                    self.give_up(*local);
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
                        let arm = self.pattern(*scrut, pat, arm, next)?;
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
        let keeps = self.closure.keeps();
        let ret = |src| {
            let (src, last) = (Taken(src), LastReg);
            if keeps {
                Ins::RetClosure { src, last }
            } else {
                Ins::Ret { src, last }
            }
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

    fn give_up(&mut self, local: LocalId) {
        let reg = Dead(of(local));
        self.push(Ins::Drop { reg });
    }

    /// Test `scrut` against `pat`, jumping to `to` when it does not fit, and
    /// read out what the pattern binds when it does. Gives what is left of
    /// `arm`, which runs next.
    fn pattern<'e>(
        &mut self,
        scrut: LocalId,
        pat: &CorePat,
        arm: &'e CoreExpr,
        to: Label,
    ) -> Result<&'e CoreExpr, Unfit> {
        match pat {
            CorePat::Wild => {}
            CorePat::Bind(b) => self.push(Ins::Move {
                dst: Dst(of(b.id)),
                src: Arg::shared(of(scrut)),
            }),
            CorePat::Ctor { variant, fields } => {
                let form = self.source.forms.of(*variant, fields.len())?;
                self.push(Ins::JumpUnlessTag {
                    src: src(scrut),
                    tag: form.tag(),
                    to,
                });
                if let Form::Fields { ctor, .. } = form
                    && let Some(opened) = self.opened(scrut, fields, arm)
                {
                    opened.before.iter().for_each(|l| self.give_up(*l));
                    self.open(scrut, ctor, &opened.dsts)?;
                    opened.after.iter().for_each(|l| self.give_up(*l));
                    return Ok(opened.rest);
                }
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
                let test = match self.source.program.consts.get(c.index()) {
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
        Ok(arm)
    }

    /// What an arm begins with when it begins by giving up its scrutinee, a
    /// constructor whose fields the pattern binds to `fields`.
    ///
    /// Perceus puts what an arm has no use for at its start, so an arm that
    /// does not need the value it matched begins with a run of drops and the
    /// value's is one of them. The cell can then be taken apart there
    /// ([`Ins::Open`]), where otherwise each field is read out with a reference
    /// of its own and the cell is freed after. Nothing jumps into the run,
    /// because a `Goto` only goes to the start of a continuation. A drop in
    /// front of the value's stays in front, so it may be of no field. A field
    /// that is dropped after it and read nowhere else is one nothing reads.
    fn opened<'e>(
        &self,
        scrut: LocalId,
        fields: &[CoreBind],
        arm: &'e CoreExpr,
    ) -> Option<Opened<'e>> {
        let (mut before, mut after) = (Vec::new(), Vec::new());
        let mut found = false;
        let mut rest = arm;
        while let CoreExpr::Drop { local, body } = rest {
            if *local == scrut {
                found = true;
            } else if found {
                after.push(*local);
            } else {
                before.push(*local);
            }
            rest = body;
        }
        let in_front: HashSet<LocalId> = before.iter().copied().collect();
        if !found
            || fields
                .iter()
                .any(|f| f.id == scrut || in_front.contains(&f.id))
        {
            return None;
        }
        let dropped: HashSet<LocalId> = after.iter().copied().collect();
        let mut unread = HashSet::new();
        let mut dsts = Vec::with_capacity(fields.len());
        for f in fields {
            let read = match self.reads.get(&f.id) {
                None => false,
                Some(times) => *times > 1 || !dropped.contains(&f.id),
            };
            if !read {
                unread.insert(f.id);
            }
            dsts.push(MaybeDst::new(read.then(|| of(f.id))));
        }
        after.retain(|l| !unread.contains(l));
        Some(Opened {
            before,
            dsts,
            after,
            rest,
        })
    }

    /// Take the `ctor` in `scrut` apart, each field to its place in `dsts`. One
    /// with nothing to keep is only given up.
    fn open(&mut self, scrut: LocalId, ctor: Ctor, dsts: &[MaybeDst<VReg>]) -> Result<(), Unfit> {
        if dsts.iter().all(|dst| dst.dst().is_none()) {
            self.give_up(scrut);
            return Ok(());
        }
        let src = Taken(of(scrut));
        // The form with as many fields.
        macro_rules! one_of {
            ($($name:ident)*) => {
                None$(.or_else(|| {
                    let dsts = dsts.try_into().ok()?;
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
                let abi = self.source.abi;
                let ok = self.build(dst, abi.ok, [made])?;
                self.push(ok);
                self.push(Ins::Jump { to: after });
                self.place(miss);
                self.push(Ins::Const {
                    dst: Dst(nil),
                    value: Value::NIL,
                });
                let err = self.build(dst, abi.err, [nil])?;
                self.push(err);
                self.place(after);
                return Ok(());
            }
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
        };
        self.push(ins);
        Ok(())
    }

    /// A new cell for a constructor the VM builds on its own.
    fn build<const N: usize>(
        &mut self,
        dst: Dst<VReg>,
        ctor: CtorOf<N>,
        fields: [VReg; N],
    ) -> Result<Ins<VReg>, Unfit> {
        self.ctor(dst, ctor.ctor(), &fields)
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

    /// The arms of a `match` on an `Option` that names both variants and binds
    /// `Some`'s value to a plain name: that name, the `Some` arm and the `None`
    /// arm.
    fn option_arms<'e>(
        &self,
        arms: &'e [(CorePat, CoreExpr)],
    ) -> Option<(LocalId, &'e CoreExpr, &'e CoreExpr)> {
        let abi = &self.source.program.abi;
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

/// What [`Translator::opened`] found at the start of an arm.
struct Opened<'e> {
    /// What is given up before the scrutinee is.
    before: Vec<LocalId>,
    /// Where each field goes, when something reads it.
    dsts: Vec<MaybeDst<VReg>>,
    /// What is given up after the scrutinee.
    after: Vec<LocalId>,
    rest: &'e CoreExpr,
}

/// What one walk over a function's body finds out.
#[derive(Default)]
struct Survey {
    /// How many times each local is read. A `Drop` counts as a read.
    reads: HashMap<LocalId, u32>,
    /// The highest local bound or read.
    highest: Option<u32>,
    /// [`Draft::closure`].
    closure: ReadsClosure,
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
        if let Atom::Load(load) = atom {
            self.closure = self.closure.max(match load {
                Load::Global(_) => ReadsClosure::Nothing,
                Load::SelfClosure => ReadsClosure::Itself,
                Load::Capture(_) => ReadsClosure::Captures,
            });
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
