//! The order that a tail call's arguments are computed in.
//!
//! A loop written as a tail call computes each parameter's next value and then
//! starts again. When the next value is computed after the last read of the
//! one it replaces, the two can share a register and the call moves nothing.
//! The source says the order, so `go(n - 1, acc + n)` computes `n - 1` first,
//! while `acc + n` still reads `n`.
//!
//! This pass moves such an argument down, to just after the last read of what
//! it replaces. It moves only what nothing else in the run can notice: a
//! constant, or an Int or Float operation, which is total, has no effect and
//! reads nothing but numbers. So no call, print or allocation the program wrote
//! changes places with another, and no array or handle is held longer. The one
//! way such an operation ends a run is a big Int the heap has no room for.
//!
//! When two arguments read what the other replaces, one of them needs a move
//! whatever the order, and they stay as written. So does everything before a
//! call when the new order would hold so many values at once that the function
//! might no longer fit the VM's registers.
//!
//! It runs after `perceus`. What moves is never dropped and reads nothing that
//! is, so no drop has to be found again. The drops behind a line are of what
//! that line was the last to read, and they stay behind it.

use std::collections::{BTreeMap, BTreeSet};

use scarlet_ir::rty::{Prim, ResolvedPool};

use super::spine::{Frame, peel};
use super::{Atom, Callee, CoreBind, CoreExpr, CoreFn, JoinId, LocalId, PrimOp};
use crate::program::InternalLinkError;

/// Put the arguments of each of `f`'s tail calls in order.
/// `reached` says which joins are [always reached](CoreExpr::always_reached).
pub(crate) fn schedule(
    pool: &ResolvedPool,
    reached: &BTreeSet<JoinId>,
    mut f: CoreFn,
) -> Result<CoreFn, InternalLinkError> {
    let params: Vec<LocalId> = f.params.iter().map(|p| p.id).collect();
    if params.is_empty() {
        return Ok(f);
    }
    let places = params.iter().copied().zip(0..).collect();
    let scheduler = Scheduler {
        pool,
        params,
        places,
        reached,
    };
    scheduler.expr(&mut f.body)?;
    Ok(f)
}

struct Scheduler<'p> {
    pool: &'p ResolvedPool,
    params: Vec<LocalId>,
    /// Which parameter each local is, of those that are one.
    places: BTreeMap<LocalId, usize>,
    /// The joins that every way through their body comes to.
    reached: &'p BTreeSet<JoinId>,
}

/// One node of a spine that always goes on.
#[derive(Clone, Copy)]
enum Line<'e> {
    Let {
        bind: &'e CoreBind,
        rhs: &'e Atom,
    },
    /// A join that every way through its body comes to.
    Join {
        params: &'e [CoreBind],
        body: &'e CoreExpr,
    },
    Drop,
}

impl<'e> Line<'e> {
    fn makes(self) -> &'e [CoreBind] {
        match self {
            Line::Let { bind, .. } => std::slice::from_ref(bind),
            Line::Join { params, .. } => params,
            Line::Drop => &[],
        }
    }

    /// Call `f` with each local the line reads. A drop reads none: what it
    /// releases has been read.
    fn for_each_read(self, f: impl FnMut(LocalId)) {
        match self {
            Line::Let { rhs, .. } => rhs.for_each_operand(f),
            Line::Join { body, .. } => reads(body).into_iter().for_each(f),
            Line::Drop => {}
        }
    }
}

/// What `e` reads of the locals bound outside it.
fn reads(e: &CoreExpr) -> BTreeSet<LocalId> {
    let (mut read, mut bound) = (BTreeSet::new(), BTreeSet::new());
    let mut work = vec![e];
    while let Some(e) = work.pop() {
        match e {
            CoreExpr::Let { bind, rhs, body } => {
                bound.insert(bind.id);
                rhs.for_each_operand(|l| {
                    read.insert(l);
                });
                work.push(body);
            }
            CoreExpr::SetGlobal { value, body, .. } => {
                read.insert(*value);
                work.push(body);
            }
            CoreExpr::Join {
                params, cont, body, ..
            } => {
                bound.extend(params.iter().map(|p| p.id));
                work.push(cont);
                work.push(body);
            }
            CoreExpr::Drop { body, .. } => work.push(body),
            CoreExpr::Open {
                cell, fields, body, ..
            } => {
                read.insert(*cell);
                bound.extend(fields.iter().map(|(_, b)| b.id));
                work.push(body);
            }
            CoreExpr::Switch {
                scrut,
                cases,
                default,
            } => {
                read.insert(*scrut);
                work.extend(cases.arms());
                work.extend(default.as_deref());
            }
            CoreExpr::NoArm => {}
            CoreExpr::If { cond, then, els } => {
                read.insert(*cond);
                work.push(then);
                work.push(els);
            }
            CoreExpr::Ret(value) => {
                read.insert(*value);
            }
            CoreExpr::TailCall { callee, args } => {
                read.extend(callee.local());
                read.extend(args);
            }
            CoreExpr::Jump { args, .. } => read.extend(args),
        }
    }
    // Each local is bound in one place.
    read.retain(|l| !bound.contains(l));
    read
}

/// What comes after the first `n` nodes of the spine `e`.
fn after(mut e: &mut CoreExpr, n: usize) -> &mut CoreExpr {
    for _ in 0..n {
        e = match e {
            CoreExpr::Let { body, .. }
            | CoreExpr::SetGlobal { body, .. }
            | CoreExpr::Join { cont: body, .. }
            | CoreExpr::Open { body, .. }
            | CoreExpr::Drop { body, .. } => body,
            CoreExpr::Switch { .. }
            | CoreExpr::NoArm
            | CoreExpr::If { .. }
            | CoreExpr::Ret(_)
            | CoreExpr::TailCall { .. }
            | CoreExpr::Jump { .. } => e,
        };
    }
    e
}

impl Scheduler<'_> {
    /// The node `e` is and what comes after it, when it is a line.
    fn line<'e>(&self, e: &'e CoreExpr) -> Option<(Line<'e>, &'e CoreExpr)> {
        match e {
            CoreExpr::Let { bind, rhs, body } => Some((Line::Let { bind, rhs }, body)),
            CoreExpr::Join {
                id,
                params,
                cont,
                body,
            } if self.reached.contains(id) => Some((Line::Join { params, body }, cont)),
            CoreExpr::Drop { body, .. } => Some((Line::Drop, body)),
            CoreExpr::SetGlobal { .. }
            | CoreExpr::Join { .. }
            | CoreExpr::Open { .. }
            | CoreExpr::Switch { .. }
            | CoreExpr::NoArm
            | CoreExpr::If { .. }
            | CoreExpr::Ret(_)
            | CoreExpr::TailCall { .. }
            | CoreExpr::Jump { .. } => None,
        }
    }

    /// The spine is walked in a loop, so a long body costs no stack depth. The
    /// body of a join that it always comes to is left alone, because no call in
    /// it is a tail call.
    fn expr(&self, mut e: &mut CoreExpr) -> Result<(), InternalLinkError> {
        loop {
            let mut end: &CoreExpr = e;
            let mut lines = 0usize;
            while let Some((_, rest)) = self.line(end) {
                lines = lines.saturating_add(1);
                end = rest;
            }
            if let CoreExpr::TailCall {
                callee: Callee::Known(_) | Callee::Self_,
                args,
            } = end
            {
                if let Some(order) = self.order(e, args)? {
                    put_in_order(e, &order);
                }
                return Ok(());
            }
            e = match after(e, lines) {
                CoreExpr::Join { cont, body, .. } => {
                    self.expr(body)?;
                    cont
                }
                CoreExpr::SetGlobal { body, .. } | CoreExpr::Open { body, .. } => body,
                CoreExpr::Switch { cases, default, .. } => {
                    for arm in cases.arms_mut() {
                        self.expr(arm)?;
                    }
                    match default {
                        Some(default) => default,
                        None => return Ok(()),
                    }
                }
                CoreExpr::If { then, els, .. } => {
                    self.expr(then)?;
                    self.expr(els)?;
                    return Ok(());
                }
                CoreExpr::Let { .. }
                | CoreExpr::Drop { .. }
                | CoreExpr::Ret(_)
                | CoreExpr::TailCall { .. }
                | CoreExpr::Jump { .. }
                | CoreExpr::NoArm => return Ok(()),
            };
        }
    }

    /// The order to put the lines of `e` in, which end in a tail call that
    /// passes `args`, when another is better.
    fn order(
        &self,
        e: &CoreExpr,
        args: &[LocalId],
    ) -> Result<Option<Vec<usize>>, InternalLinkError> {
        let all = std::iter::successors(self.line(e), |(_, rest)| self.line(rest));
        let lines: Vec<Line<'_>> = all.map(|(line, _)| line).collect();
        let mut passed: Vec<(LocalId, usize)> = args.iter().copied().zip(0..).collect();
        passed.sort_unstable();
        if !self.may_gain(&lines, &passed) {
            return Ok(None);
        }
        let reads = Reads::new(self, &lines, &passed)?;
        let order = Arguments::new(self, &lines, args, &reads).order(&lines);
        Ok(order.filter(|order| reads.fits(self.params.len(), &lines, order)))
    }

    /// Whether a line reads a parameter after another has computed its argument,
    /// without which no order is better than the one there is. `passed` is what
    /// the call passes, with the places it has, in order.
    fn may_gain(&self, lines: &[Line<'_>], passed: &[(LocalId, usize)]) -> bool {
        // Whether the parameter at each place is replaced so far.
        let mut replaced: Vec<bool> = Vec::new();
        for line in lines {
            let mut reads = false;
            if !replaced.is_empty() {
                line.for_each_read(|l| {
                    let place = self.places.get(&l);
                    reads |= place.is_some_and(|i| replaced.get(*i) == Some(&true));
                });
            }
            if reads {
                return true;
            }
            if let Line::Let { bind, rhs } = line
                && computes(rhs)
                && let Some(place) = place_of(passed, bind.id)
            {
                replaced.resize(passed.len(), false);
                if let Some(replaced) = replaced.get_mut(place) {
                    *replaced = true;
                }
            }
        }
        false
    }

    /// Whether `bind` is a constant number.
    fn is_number(&self, bind: &CoreBind, rhs: &Atom) -> Result<bool, InternalLinkError> {
        if !matches!(rhs, Atom::Const(_)) {
            return Ok(false);
        }
        Ok(match self.pool.prim_of(bind.ty)? {
            Some(Prim::Int | Prim::Float) => true,
            Some(Prim::String) | None => false,
        })
    }
}

/// The first place that `l` has in `passed`, which is in order.
fn place_of(passed: &[(LocalId, usize)], l: LocalId) -> Option<usize> {
    let at = passed.partition_point(|(p, _)| *p < l);
    let (p, place) = passed.get(at)?;
    (*p == l).then_some(*place)
}

/// How many lines come first in `order` as they are.
fn unmoved(order: &[usize]) -> usize {
    let same = |(i, at): &(usize, &usize)| i == *at;
    order.iter().enumerate().take_while(same).count()
}

/// Put the lines of `e` in `order`.
fn put_in_order(e: &mut CoreExpr, order: &[usize]) {
    let first = unmoved(order);
    let e = after(e, first);
    let order = order.get(first..).unwrap_or_default();
    // Something has to be there while it is taken apart.
    let mut rest = std::mem::replace(e, CoreExpr::Ret(LocalId(0)));
    let mut lines: Vec<Option<Frame>> = Vec::with_capacity(order.len());
    for _ in order {
        rest = match peel(rest) {
            Ok((line, rest)) => {
                lines.push(Some(line));
                rest
            }
            Err(end) => end.expr(),
        };
    }
    for at in order.iter().rev() {
        let line = at.checked_sub(first).and_then(|at| lines.get_mut(at));
        if let Some(line) = line.and_then(Option::take) {
            rest = line.wrap(rest);
        }
    }
    *e = rest;
}

/// What moving an argument takes. What computes one reads two locals at most.
#[derive(Clone, Copy)]
struct Movable {
    /// The lines of the constants that nothing else reads, which go with it.
    with: [Option<usize>; 2],
    /// The places of the parameters it reads.
    reads: [Option<usize>; 2],
}

/// One argument of a tail call.
struct Argument {
    /// The line that makes it, when one does.
    made: Option<usize>,
    /// What moving it takes, when that can be done.
    movable: Option<Movable>,
    /// Whether it gains by being made later: something reads the parameter it
    /// is for after it is made.
    gains: bool,
    /// Whether it may be made later, once that has been asked.
    may: Option<bool>,
    /// Whether it is made later.
    sinks: bool,
    /// The line it goes after, when it sinks.
    after: Option<usize>,
}

impl Argument {
    fn reads(&self) -> impl Iterator<Item = usize> + use<> {
        let reads = self.movable.map(|m| m.reads);
        reads.into_iter().flatten().flatten()
    }

    /// The lines that make it, in order, when it can be made later.
    fn lines(&self) -> impl Iterator<Item = usize> + use<> {
        let with = self.movable.map(|m| m.with);
        let made = self.made.filter(|_| self.movable.is_some());
        with.into_iter().flatten().flatten().chain(made)
    }
}

/// The arguments of one tail call.
struct Arguments<'r> {
    all: Vec<Argument>,
    reads: &'r Reads,
    /// Those that may be made later, each after every one that has to come
    /// after it.
    backwards: Vec<usize>,
}

impl Arguments<'_> {
    fn new<'r>(
        of: &Scheduler<'_>,
        lines: &[Line<'_>],
        args: &[LocalId],
        reads: &'r Reads,
    ) -> Arguments<'r> {
        let all = args.iter().enumerate().map(|(i, v)| {
            let made = reads.local(*v).and_then(|v| v.made);
            let last = reads.readers(i).last().map(|(_, line)| *line);
            let read_after = last.filter(|last| made.is_some_and(|at| *last > at));
            let passed = of.params.get(i).is_some_and(|p| reads.passes(*p));
            Argument {
                made,
                movable: made.and_then(|at| reads.movable(of, lines, *v, at)),
                gains: read_after.is_some_and(|last| !passed && !reads.takes_all(lines, last)),
                may: None,
                sinks: false,
                after: made,
            }
        });
        let mut out = Arguments {
            all: all.collect(),
            reads,
            backwards: Vec::new(),
        };
        for i in 0..args.len() {
            out.ask(i);
        }
        out
    }

    /// Find out whether the argument at place `i` may be made later.
    ///
    /// It may not when it reads a parameter whose own argument is made after it
    /// and stays there: it could then be read after it is replaced, where it
    /// was not before. Each that may be made later too, and that replaces
    /// something it reads, comes after it. An argument that is asked about
    /// while it is being found out is in a cycle, which stays as written.
    fn ask(&mut self, i: usize) -> bool {
        let Some(arg) = self.all.get_mut(i) else {
            return false;
        };
        if let Some(known) = arg.may {
            return known;
        }
        arg.may = Some(false);
        let (made, reads) = (arg.made, arg.reads());
        let mut may = arg.movable.is_some();
        for j in reads.filter(|j| *j != i) {
            let follows = self.ask(j);
            if !follows && self.all.get(j).and_then(|j| j.made) > made {
                may = false;
            }
        }
        if may {
            self.backwards.push(i);
        }
        if let Some(arg) = self.all.get_mut(i) {
            arg.may = Some(may);
        }
        may
    }

    /// The lines in their new order, when any gains by being made later. Those
    /// that then have to come after one of these are made later too.
    fn order(mut self, lines: &[Line<'_>]) -> Option<Vec<usize>> {
        let gains = |(i, arg): (usize, &Argument)| arg.gains.then_some(i);
        let mut work: Vec<usize> = self.all.iter().enumerate().filter_map(gains).collect();
        while let Some(i) = work.pop() {
            if let Some(arg) = self.all.get_mut(i)
                && arg.may == Some(true)
                && !std::mem::replace(&mut arg.sinks, true)
            {
                work.extend(arg.reads());
            }
        }
        // Where each line goes: after which that stays, and how many after it.
        let mut keys: Vec<(usize, usize, usize)> = (0..lines.len()).map(|at| (at, 0, at)).collect();
        let mut moves = false;
        for at in self.sinking().flat_map(Argument::lines) {
            moves = true;
            if let Some((_, nth, _)) = keys.get_mut(at) {
                *nth = usize::MAX;
            }
        }
        if !moves {
            return None;
        }
        // Each goes after the last line that reads what it replaces and stays,
        // with the drops behind that line, and after each of these that reads
        // it, which comes first here.
        let stays = |(_, at): &&(usize, usize)| keys.get(*at).is_some_and(|key| key.1 == 0);
        let dropped = |at: &usize| matches!(lines.get(*at), Some(Line::Drop));
        for (i, arg) in self.all.iter_mut().enumerate() {
            if arg.sinks
                && let Some((_, last)) = self.reads.readers(i).iter().rev().find(stays)
            {
                let drops = (last.saturating_add(1)..).take_while(dropped);
                arg.after = drops.last().or(Some(*last)).max(arg.after);
            }
        }
        let mut nth = 0usize;
        for i in self.backwards.iter().rev() {
            let Some(arg) = self.all.get(*i).filter(|arg| arg.sinks) else {
                continue;
            };
            let after = arg.after;
            for at in arg.lines() {
                nth = nth.saturating_add(1);
                if let (Some(key), Some(after)) = (keys.get_mut(at), after) {
                    *key = (after, nth, at);
                }
            }
            for j in arg.reads().filter(|j| j != i) {
                if let Some(arg) = self.all.get_mut(j) {
                    arg.after = arg.after.max(after);
                }
            }
        }
        keys.sort_unstable();
        Some(keys.into_iter().map(|(_, _, at)| at).collect())
    }

    fn sinking(&self) -> impl Iterator<Item = &Argument> {
        self.all.iter().filter(|arg| arg.sinks)
    }
}

/// What the lines in front of a tail call do with the locals that matter to it:
/// the parameters, what it passes, and what that is computed from.
struct Reads {
    /// Those locals, in order, and what is known of each.
    locals: Vec<(LocalId, Local)>,
    /// Each read of a parameter, as its place and the line that reads it, in
    /// order.
    of_params: Vec<(usize, usize)>,
    /// What the call passes, in order.
    passed: Vec<LocalId>,
}

/// What is held around one line.
#[derive(Default)]
struct Around {
    /// How many values are held before it, with what it reads, or after it, with
    /// what it makes.
    most: usize,
    /// How many are held from before it to after it and not read in it.
    past: usize,
    /// What a join reads that is read after it too.
    again: BTreeSet<LocalId>,
}

impl Around {
    /// Whether no more is held at any point inside a join than where it has
    /// `was` around it. Whatever of its reads are still to come there, they are
    /// held both ways, so each in `again` may be the one that is held for no
    /// other reason.
    fn around_no_more_than(&self, was: &Around) -> bool {
        let more = self.again.difference(&was.again).count();
        self.past.saturating_add(more) <= was.past
    }
}

#[derive(Default)]
struct Local {
    /// The line that makes it.
    made: Option<usize>,
    /// How many times the lines and the call read it.
    times: usize,
    /// Whether it is a constant number.
    number: bool,
}

impl Reads {
    fn new(
        of: &Scheduler<'_>,
        lines: &[Line<'_>],
        passed: &[(LocalId, usize)],
    ) -> Result<Reads, InternalLinkError> {
        let passed: Vec<LocalId> = passed.iter().map(|(l, _)| *l).collect();
        let mut names = passed.clone();
        for line in lines {
            if let Line::Let { bind, rhs } = line
                && computes(rhs)
                && passed.binary_search(&bind.id).is_ok()
            {
                rhs.for_each_operand(|l| names.push(l));
            }
        }
        names.sort_unstable();
        names.dedup();
        let mut out = Reads {
            locals: names.into_iter().map(|l| (l, Local::default())).collect(),
            of_params: Vec::new(),
            passed: Vec::new(),
        };
        for (at, line) in lines.iter().enumerate() {
            line.for_each_read(|l| {
                if let Some(place) = of.places.get(&l) {
                    out.of_params.push((*place, at));
                }
                out.count(l);
            });
            for bind in line.makes() {
                if let Some(local) = out.local_mut(bind.id) {
                    local.made = Some(at);
                    if let Line::Let { rhs, .. } = line {
                        local.number = of.is_number(bind, rhs)?;
                    }
                }
            }
        }
        passed.iter().for_each(|l| out.count(*l));
        out.of_params.sort_unstable();
        out.passed = passed;
        Ok(out)
    }

    fn count(&mut self, l: LocalId) {
        if let Some(local) = self.local_mut(l) {
            local.times = local.times.saturating_add(1);
        }
    }

    fn local(&self, l: LocalId) -> Option<&Local> {
        let at = self.locals.binary_search_by_key(&l, |(l, _)| *l).ok()?;
        self.locals.get(at).map(|(_, local)| local)
    }

    fn local_mut(&mut self, l: LocalId) -> Option<&mut Local> {
        let at = self.locals.binary_search_by_key(&l, |(l, _)| *l).ok()?;
        self.locals.get_mut(at).map(|(_, local)| local)
    }

    /// The reads of the parameter at place `i`.
    fn readers(&self, i: usize) -> &[(usize, usize)] {
        let of = |from: usize| self.of_params.partition_point(|(place, _)| *place < from);
        let range = of(i)..of(i.saturating_add(1));
        self.of_params.get(range).unwrap_or_default()
    }

    fn passes(&self, l: LocalId) -> bool {
        self.passed.binary_search(&l).is_ok()
    }

    /// Whether a function that fits the registers with `lines` as they are still
    /// does with them in `order`.
    ///
    /// How many values are held inside a join is not known here, so no more may
    /// be held around one at any point of it. Anywhere else there may be more,
    /// as long as there are few. Around the lines before the first that moves
    /// nothing changes.
    fn fits(&self, params: usize, lines: &[Line<'_>], order: &[usize]) -> bool {
        let joins = lines.iter().any(|line| matches!(line, Line::Join { .. }));
        if !joins && lines.len().saturating_add(params) <= FEW {
            return true;
        }
        let as_written: Vec<usize> = (0..lines.len()).collect();
        let first = unmoved(order);
        let was = self.held(lines, &as_written, first);
        let now = self.held(lines, order, first);
        let most = was.iter().map(|held| held.most).max().unwrap_or(0).max(FEW);
        let mut pairs = lines.iter().zip(was.iter().zip(&now)).skip(first);
        pairs.all(|(line, (was, now))| match line {
            Line::Join { .. } => now.around_no_more_than(was),
            // What may move takes no register but for what it reads and makes.
            Line::Let { rhs, .. } if computes(rhs) => now.most <= most,
            Line::Let { .. } | Line::Drop => now.most <= was.most.max(FEW),
        })
    }

    /// What is held around each line from the `first` in `order` on.
    fn held(&self, lines: &[Line<'_>], order: &[usize], first: usize) -> Vec<Around> {
        let mut held: BTreeSet<LocalId> = self.passed.iter().copied().collect();
        let mut out: Vec<Around> = lines.iter().map(|_| Around::default()).collect();
        for at in order.iter().skip(first).rev() {
            let Some(line) = lines.get(*at) else {
                continue;
            };
            let makes = || line.makes().iter().map(|bind| bind.id);
            held.extend(makes());
            let after = held.len();
            makes().for_each(|l| {
                held.remove(&l);
            });
            let past = held.len();
            let mut again = BTreeSet::new();
            line.for_each_read(|l| {
                if !held.insert(l) && matches!(line, Line::Join { .. }) {
                    again.insert(l);
                }
            });
            if let Some(out) = out.get_mut(*at) {
                *out = Around {
                    most: after.max(held.len()),
                    past: past.saturating_sub(again.len()),
                    again,
                };
            }
        }
        out
    }

    /// Whether line `at` makes a constructor, or updates one, and is the last to
    /// read each of its operands. The VM then moves them all, with an instruction
    /// that has nothing to share. One more that is read again would cost more
    /// than the move it saves.
    fn takes_all(&self, lines: &[Line<'_>], at: usize) -> bool {
        let Some(Line::Let {
            rhs: rhs @ (Atom::Ctor { .. } | Atom::Update { .. }),
            ..
        }) = lines.get(at)
        else {
            return false;
        };
        let mut operands = Vec::new();
        rhs.for_each_operand(|l| operands.push(l));
        let mut again = operands.iter().any(|l| self.passes(*l));
        for line in lines.iter().skip(at.saturating_add(1)) {
            line.for_each_read(|l| again |= operands.contains(&l));
        }
        !again
    }

    /// What moving `v`, which line `at` makes, takes, when it can move.
    fn movable(
        &self,
        of: &Scheduler<'_>,
        lines: &[Line<'_>],
        v: LocalId,
        at: usize,
    ) -> Option<Movable> {
        let Some(Line::Let { rhs, .. }) = lines.get(at) else {
            return None;
        };
        let made = self.local(v)?;
        let constant = matches!(rhs, Atom::Const(_)) && !made.number;
        if constant || !computes(rhs) || made.times != 1 {
            return None;
        }
        let (mut with, mut reads) = ([None; 2], [None; 2]);
        let mut n = 0usize;
        rhs.for_each_operand(|l| {
            let alone = |operand: &&Local| operand.number && operand.times == 1;
            if let (Some(with), Some(reads)) = (with.get_mut(n), reads.get_mut(n)) {
                *with = self.local(l).filter(alone).and_then(|l| l.made);
                *reads = of.places.get(&l).copied();
            }
            n = n.saturating_add(1);
        });
        (n <= 2).then_some(Movable { with, reads })
    }
}

/// So few values that no function comes near the VM's registers with them.
const FEW: usize = 64;

/// Whether `atom` is an Int or Float operation, which has an answer for any
/// numbers, or a constant.
fn computes(atom: &Atom) -> bool {
    match atom {
        Atom::Const(_) | Atom::Nil | Atom::Bool(_) => true,
        Atom::Ctor { fields, .. } => fields.is_empty(),
        Atom::PrimOp { op, .. } => matches!(
            op,
            PrimOp::IntAdd
                | PrimOp::IntSub
                | PrimOp::IntMul
                | PrimOp::IntDiv
                | PrimOp::IntRem
                | PrimOp::IntNeg
                | PrimOp::IntEq
                | PrimOp::IntNe
                | PrimOp::IntLt
                | PrimOp::IntLe
                | PrimOp::IntGt
                | PrimOp::IntGe
                | PrimOp::FloatAdd
                | PrimOp::FloatSub
                | PrimOp::FloatMul
                | PrimOp::FloatDiv
                | PrimOp::FloatRem
                | PrimOp::FloatNeg
                | PrimOp::FloatLt
                | PrimOp::FloatLe
                | PrimOp::FloatGt
                | PrimOp::FloatGe
        ),
        Atom::Local(_)
        | Atom::Load(_)
        | Atom::Update { .. }
        | Atom::Call { .. }
        | Atom::Intrinsic { .. }
        | Atom::Closure { .. } => false,
    }
}

#[cfg(test)]
mod tests;
