//! Which register each value has.
//!
//! Two values share a register whenever neither is written while the other is
//! held. A frame gives up what its registers hold when it returns, so fewer of
//! them is less work there.
//!
//! A value that a tail call passes takes the register of the parameter it is for,
//! when it can and the frame is no larger for it. The two values of a `Move` are
//! given one register when they can have it, so that a jump to a join has nothing
//! to move. The tail call comes first. `b = if x > best { x } else { best }` and
//! then `go(n - 1, b)` is one move when `b` has the register of `best`, and that
//! is on the way that gives `x`. With `b` in the register of `x` every way has
//! one for the call, and the way that gives `best` one more.
//!
//! Sharing a register changes when a value is released, never whether: the
//! value a register held is released when the next one is written over it,
//! instead of when the frame ends, and by then nothing reads it. A `Drop`
//! names its register, so a value keeps it until its `Drop`, unless that `Drop`
//! does nothing.

use std::collections::HashSet;

use super::ins::{Ins, Lists, Makes, VReg};
use super::live::Held;
use super::translate::{Draft, Step, TailTo};
use super::{ReadsClosure, Reg, Room, Unfit};
use crate::frame::{CLOSURE, FIRST, LINK, WINDOW};
use crate::stop::Limit;

/// A function whose values have registers.
pub(super) struct Assigned {
    pub(super) steps: Vec<Step>,
    pub(super) lists: Lists<VReg>,
    /// Where each label is, as the number of the step it stands at.
    pub(super) labels: Vec<usize>,
    /// The register of each value, by its number, when it has one.
    pub(super) regs: Vec<Option<Reg>>,
    /// [`Draft::kinds`].
    pub(super) kinds: Vec<Option<Makes>>,
    pub(super) params: Vec<VReg>,
    /// How many registers its frame has.
    pub(super) own: Room,
    /// What it reads of the closure it runs as.
    pub(super) closure: ReadsClosure,
}

/// Room for the registers before `end`, or for as many as a frame has.
pub(super) fn room(end: usize) -> Room {
    Room(u16::try_from(end.min(WINDOW)).unwrap_or(u16::MAX))
}

/// The register of parameter `i`.
pub(super) fn param(i: usize) -> Option<Reg> {
    let r = i.checked_add(usize::from(FIRST.0))?;
    u8::try_from(r).ok().map(Reg)
}

/// Give each value of `draft` a register, and take out the instructions that
/// then do nothing.
pub(super) fn allocate(draft: Draft, held: &Held) -> Result<Assigned, Unfit> {
    let clashes = Clashes::new(draft.values as usize, &held.clashes);
    let mut groups = Groups::joined(&draft, &|v| clashes.of(v), moves(&draft));
    let plain = match colour(&draft, held, &clashes, &groups, &[]) {
        Ok(coloured) => coloured,
        Err(_) => {
            groups = Groups::default();
            colour(&draft, held, &clashes, &groups, &[])?
        }
    };
    let (regs, end) = passing(&draft, held, &clashes, &groups, plain.1).unwrap_or(plain);
    let mut assigned = Assigned {
        steps: draft.steps,
        lists: draft.lists,
        labels: draft.labels,
        regs,
        kinds: draft.kinds,
        params: draft.params,
        own: room(end),
        closure: draft.closure,
    };
    let idle = idle(held, &assigned.regs);
    tidy(&mut assigned, &idle);
    Ok(assigned)
}

/// The registers with those that the tail calls would like, in a frame that ends
/// at `end` or before. `groups` is what to keep together when the frame is larger
/// with a tail call's arguments first.
fn passing(
    draft: &Draft,
    held: &Held,
    clashes: &Clashes,
    groups: &Groups,
    end: usize,
) -> Option<Coloured> {
    let passed = passed(draft);
    let wanted = wanted(&passed, end);
    if wanted.is_empty() {
        return None;
    }
    let with = |groups: &Groups| {
        let coloured = colour(draft, held, clashes, groups, &wanted).ok();
        coloured.filter(|(_, larger)| *larger <= end)
    };
    // With no move there is nothing for an argument to go before.
    let first = || {
        moves(draft).next()?;
        let pair = |(v, i): &(VReg, usize)| Some((*draft.params.get(*i)?, *v));
        let pairs = passed.iter().filter_map(pair);
        let pairs = pairs.chain(moves(draft));
        with(&Groups::joined(draft, &|v| clashes.of(v), pairs))
    };
    first().or_else(|| with(groups))
}

/// The two values of each `Move`.
fn moves(draft: &Draft) -> impl Iterator<Item = (VReg, VReg)> {
    draft.steps.iter().filter_map(|step| match step {
        Step::Ins(Ins::Move { dst, src }) => Some((dst.0, src.reg)),
        Step::Ins(_) | Step::TailCall { .. } => None,
    })
}

/// Each value a tail call passes, with its place among the arguments. Those of
/// calls to the running function come first, because they make a loop.
fn passed(draft: &Draft) -> Vec<(VReg, usize)> {
    let (mut own, mut others) = (Vec::new(), Vec::new());
    for step in &draft.steps {
        if let Step::TailCall { to, args } = step {
            let args = args.iter().copied().zip(0..);
            match to {
                TailTo::Own => own.extend(args),
                TailTo::Func(_) => others.extend(args),
            }
        }
    }
    own.extend(others);
    own
}

/// The register of each value that has one, and one past the highest.
type Coloured = (Vec<Option<Reg>>, usize);

/// Whether each step does nothing, once the values have registers.
fn idle(held: &Held, regs: &[Option<Reg>]) -> Vec<bool> {
    let mut idle = held.idle.clone();
    let reg = |v: VReg| regs.get(v.0 as usize).copied().flatten();
    for drop in &held.overwritten {
        if reg(drop.dropped) == reg(drop.written)
            && let Some(idle) = idle.get_mut(drop.step)
        {
            *idle = true;
        }
    }
    idle
}

/// The register of the parameter that each of `passed` is for. A value passed for
/// several is here with each, in the order of `passed`.
///
/// None is wanted from `end` on, where the registers of the frame end without
/// this. A frame's calls start above its registers and its return gives them up,
/// so a larger one costs more than a move.
fn wanted(passed: &[(VReg, usize)], end: usize) -> Vec<(VReg, Reg)> {
    let each = passed.iter().filter_map(|(v, i)| Some((*v, param(*i)?)));
    each.filter(|(_, reg)| usize::from(reg.0) < end).collect()
}

/// A register for each value, and one past the highest.
///
/// A function's arguments arrive in the registers from [`FIRST`] on, in order.
/// Each value in `wanted` takes the first register it has there that none of
/// the values it clashes with has, and each value left over the lowest. The
/// values of a group take theirs together, out of what each of them has there.
fn colour(
    draft: &Draft,
    held: &Held,
    clashes: &Clashes,
    groups: &Groups,
    wanted: &[(VReg, Reg)],
) -> Result<Coloured, Unfit> {
    let mut regs: Vec<Option<Reg>> = vec![None; draft.values as usize];
    let mut end = usize::from(FIRST.0);
    let mut give = |regs: &mut Vec<Option<Reg>>, v: VReg, reg: Reg| {
        for member in groups.members(v) {
            if let Some(of) = regs.get_mut(member.0 as usize) {
                *of = Some(reg);
            }
        }
        end = end.max(usize::from(reg.0).saturating_add(1));
    };
    for (i, v) in draft.params.iter().enumerate() {
        give(&mut regs, *v, param(i).ok_or(TOO_MANY)?);
    }
    let free = |regs: &[Option<Reg>], v: VReg| {
        let unset = regs.get(v.0 as usize) == Some(&None);
        unset.then(|| clashes.taken(groups.members(v), regs))
    };
    let mut wanted = wanted.to_vec();
    wanted.sort_by_key(|(v, _)| groups.group(*v));
    for want in wanted.chunk_by(|(v, _), (w, _)| groups.group(*v) == groups.group(*w)) {
        if let Some((v, _)) = want.first()
            && let Some(taken) = free(&regs, *v)
            && let Some((_, reg)) = want.iter().find(|(_, reg)| !taken.has(*reg))
        {
            give(&mut regs, *v, *reg);
        }
    }
    for (v, named) in (0u32..).map(VReg).zip(&held.named) {
        if *named && let Some(taken) = free(&regs, v) {
            let reg = u8::try_from(taken.lowest_free()).map_err(|_| TOO_MANY)?;
            give(&mut regs, v, Reg(reg));
        }
    }
    Ok((regs, end))
}

/// The values that are to share a register, in groups. A value that is not here is
/// in a group of its own.
#[derive(Default)]
struct Groups {
    /// The group of each value.
    of: Vec<u32>,
    /// The next value of the same group, round to the first again.
    next: Vec<VReg>,
}

impl Groups {
    /// The values of each of `pairs` in one group, where that has no two values
    /// that clash and is of one parameter at most. Parameters never clash, because
    /// nothing writes them. A pair earlier in `pairs` goes before a later one.
    /// `clashes` gives what each value clashes with.
    fn joined<'c>(
        draft: &Draft,
        clashes: &impl Fn(VReg) -> &'c [VReg],
        pairs: impl Iterator<Item = (VReg, VReg)>,
    ) -> Groups {
        let mut pairs = pairs.peekable();
        if pairs.peek().is_none() {
            return Groups::default();
        }
        let mut groups = Groups {
            of: (0..draft.values).collect(),
            next: (0..draft.values).map(VReg).collect(),
        };
        let one = |v: VReg| Whole {
            values: 1,
            clashes: clashes(v).len(),
            arrives: false,
        };
        let mut wholes: Vec<Whole> = (0..draft.values).map(VReg).map(one).collect();
        for v in &draft.params {
            if let Some(whole) = wholes.get_mut(v.0 as usize) {
                whole.arrives = true;
            }
        }
        // Those that may not be one, so that each two are looked at once.
        let mut refused = HashSet::new();
        for (a, b) in pairs {
            let (a, b) = (groups.group(a), groups.group(b));
            if a == b || refused.contains(&(a.min(b), a.max(b))) {
                continue;
            }
            let whole = |group: u32| wholes.get(group as usize).copied().unwrap_or_default();
            let (of_a, of_b) = (whole(a), whole(b));
            // The one with fewer clashes is gone over, so that one value in many
            // pairs is not gone over for each of them.
            let clash = || {
                if of_a.clashes <= of_b.clashes {
                    groups.clash(a, b, clashes)
                } else {
                    groups.clash(b, a, clashes)
                }
            };
            if (of_a.arrives && of_b.arrives) || clash() {
                refused.insert((a.min(b), a.max(b)));
                continue;
            }
            let (to, from) = if of_b.values > of_a.values {
                (b, a)
            } else {
                (a, b)
            };
            if let Some(whole) = wholes.get_mut(to as usize) {
                *whole = Whole {
                    values: of_a.values.saturating_add(of_b.values),
                    clashes: of_a.clashes.saturating_add(of_b.clashes),
                    arrives: of_a.arrives || of_b.arrives,
                };
            }
            groups.join(to, from);
        }
        groups
    }

    fn group(&self, v: VReg) -> u32 {
        self.of.get(v.0 as usize).copied().unwrap_or(v.0)
    }

    /// Every value of the group of `v`, which has the number of one of them.
    fn members(&self, v: VReg) -> impl Iterator<Item = VReg> {
        let next = move |at: &VReg| self.next.get(at.0 as usize).copied();
        std::iter::successors(Some(v), move |at| next(at).filter(|next| *next != v))
    }

    /// Whether a value of `from` clashes with one of `to`.
    fn clash<'c>(&self, from: u32, to: u32, clashes: &impl Fn(VReg) -> &'c [VReg]) -> bool {
        self.members(VReg(from)).any(|v| {
            let mut others = clashes(v).iter();
            others.any(|other| self.group(*other) == to)
        })
    }

    /// Put the values of `from` in `to`.
    fn join(&mut self, to: u32, from: u32) {
        let moving: Vec<VReg> = self.members(VReg(from)).collect();
        for v in moving {
            if let Some(of) = self.of.get_mut(v.0 as usize) {
                *of = to;
            }
        }
        // Two rings become one when one value of each gives its next to the other.
        let (to, from) = (to as usize, from as usize);
        if to != from && to.max(from) < self.next.len() {
            self.next.swap(to, from);
        }
    }
}

/// What [`Groups::joined`] keeps of a group.
#[derive(Clone, Copy, Default)]
struct Whole {
    values: u32,
    /// How many clashes its values have.
    clashes: usize,
    /// Whether one of them is a parameter.
    arrives: bool,
}

const TOO_MANY: Unfit = Unfit::TooMany(Limit::Values);

/// A set of registers, one bit each.
#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct RegSet([u64; 4]);

impl RegSet {
    /// How many there are.
    pub(super) fn len(&self) -> u32 {
        let words = self.0.iter().map(|word| word.count_ones());
        words.fold(0, u32::saturating_add)
    }

    pub(super) fn add(&mut self, Reg(r): Reg) {
        if let Some(word) = self.0.get_mut(usize::from(r / 64)) {
            *word |= 1 << (r % 64);
        }
    }

    pub(super) fn remove(&mut self, Reg(r): Reg) {
        if let Some(word) = self.0.get_mut(usize::from(r / 64)) {
            *word &= !(1 << (r % 64));
        }
    }

    pub(super) fn has(&self, Reg(r): Reg) -> bool {
        let word = self.0.get(usize::from(r / 64));
        word.is_some_and(|word| word & (1 << (r % 64)) != 0)
    }

    /// Add everything in `other`.
    pub(super) fn add_all(&mut self, other: &RegSet) {
        for (word, more) in self.0.iter_mut().zip(other.0) {
            *word |= more;
        }
    }

    /// Take out the highest register here.
    pub(super) fn take_highest(&mut self) -> Option<Reg> {
        let words = self.0.iter_mut().zip([0u8, 64, 128, 192]);
        let (word, below) = words.rev().find(|(word, _)| **word != 0)?;
        let bit = u8::try_from(word.ilog2()).ok()?;
        *word &= !(1 << bit);
        Some(Reg(below | bit))
    }

    /// The lowest register not here, or 256 when every one is.
    fn lowest_free(&self) -> usize {
        self.0
            .iter()
            .zip([0usize, 64, 128, 192])
            .find(|(word, _)| **word != u64::MAX)
            .map_or(256, |(word, below)| {
                below.saturating_add(word.trailing_ones() as usize)
            })
    }
}

/// For each value, the values it clashes with.
struct Clashes {
    /// Where each value's start in `all`, and one past the last one's end.
    starts: Vec<usize>,
    all: Vec<VReg>,
}

impl Clashes {
    fn new(values: usize, pairs: &[(VReg, VReg)]) -> Clashes {
        // First how many each value has, then where they end, counted down as
        // each is put in its place to where they start.
        let mut starts = vec![0usize; values.saturating_add(1)];
        for v in pairs.iter().flat_map(|(a, b)| [a, b]) {
            if let Some(n) = starts.get_mut(v.0 as usize) {
                *n = n.saturating_add(1);
            }
        }
        let mut end = 0usize;
        for n in &mut starts {
            end = end.saturating_add(*n);
            *n = end;
        }
        let mut all = vec![VReg(0); end];
        for (a, b) in pairs {
            for (v, other) in [(a, b), (b, a)] {
                if let Some(at) = starts.get_mut(v.0 as usize) {
                    *at = at.saturating_sub(1);
                    if let Some(slot) = all.get_mut(*at) {
                        *slot = *other;
                    }
                }
            }
        }
        Clashes { starts, all }
    }

    fn of(&self, v: VReg) -> &[VReg] {
        let v = v.0 as usize;
        let start = self.starts.get(v).copied().unwrap_or(0);
        let end = self.starts.get(v.saturating_add(1)).copied().unwrap_or(0);
        self.all.get(start..end).unwrap_or_default()
    }

    /// The registers that none of `values` may have: those of the values they
    /// clash with, and the two that a frame's link and closure have.
    fn taken(&self, values: impl Iterator<Item = VReg>, regs: &[Option<Reg>]) -> RegSet {
        let mut taken = RegSet::default();
        taken.add(LINK);
        taken.add(CLOSURE);
        for other in values.flat_map(|v| self.of(v)) {
            if let Some(Some(reg)) = regs.get(other.0 as usize) {
                taken.add(*reg);
            }
        }
        taken
    }
}

/// Take out of `f` what does nothing: what `idle` says, a `Move` from a
/// register to itself, and a `Jump` to the step that would run next anyway.
fn tidy(f: &mut Assigned, idle: &[bool]) {
    let reg = |v: VReg| f.regs.get(v.0 as usize).copied().flatten();
    // Whether each step stays, and for each place in the function the step
    // that runs there: itself, or the next that stays.
    let mut stays = vec![false; f.steps.len()];
    let mut runs = vec![f.steps.len(); f.steps.len().saturating_add(1)];
    let mut next = f.steps.len();
    for (i, step) in f.steps.iter().enumerate().rev() {
        let nothing = if let Step::Ins(Ins::Move { dst, src }) = step {
            reg(dst.0) == reg(src.reg)
        } else if let Step::Ins(Ins::Jump { to }) = step {
            let place = f.labels.get(to.0 as usize);
            place.and_then(|at| runs.get(*at)) == Some(&next)
        } else {
            idle.get(i) == Some(&true)
        };
        if !nothing {
            next = i;
            if let Some(stays) = stays.get_mut(i) {
                *stays = true;
            }
        }
        if let Some(runs) = runs.get_mut(i) {
            *runs = next;
        }
    }
    // Where each step moves to, and where the next one does for one that goes.
    let mut moves_to = vec![0usize; f.steps.len().saturating_add(1)];
    let mut kept = 0usize;
    for (i, to) in moves_to.iter_mut().enumerate() {
        *to = kept;
        kept = kept.saturating_add(usize::from(stays.get(i) == Some(&true)));
    }
    for at in &mut f.labels {
        *at = moves_to.get(*at).copied().unwrap_or(kept);
    }
    let mut stays = stays.into_iter();
    f.steps.retain(|_| stays.next().unwrap_or(true));
}

#[cfg(test)]
mod tests;
