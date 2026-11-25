//! Which register each value has.
//!
//! Two values share a register whenever neither is written while the other is
//! held. A frame gives up what its registers hold when it returns, so fewer of
//! them is less work there.
//!
//! A value that a tail call passes takes the register of the parameter it is
//! for, when it can and the frame is no larger for it: the call then has nothing
//! to move.
//!
//! Sharing a register changes when a value is released, never whether: the
//! value a register held is released when the next one is written over it,
//! instead of when the frame ends, and by then nothing reads it. A `Drop`
//! names its register, so a value keeps it until its `Drop`, unless that `Drop`
//! does nothing.

use super::ins::{Ins, Lists, Makes, VReg};
use super::live::Held;
use super::translate::{Draft, Step};
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
    let (regs, end) = colour(&draft, held, &clashes, &[])?;
    let wanted = wanted(&draft, end);
    let (regs, end) = if wanted.is_empty() {
        (regs, end)
    } else {
        match colour(&draft, held, &clashes, &wanted) {
            Ok(better) if better.1 <= end => better,
            Ok(_) | Err(_) => (regs, end),
        }
    };
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
    tidy(&mut assigned, &held.idle);
    Ok(assigned)
}

/// Each value a tail call passes, with the register of the parameter it is
/// passed for, by the values' numbers. A value passed for several is here with
/// each, in the order of the calls. A function that holds something nothing
/// drops wants nothing: the registers say when that is released, and a loop is
/// not to hold it longer.
///
/// None is wanted from `end` on, where the registers of the frame end without
/// this. A frame's calls start above its registers and its return gives them up,
/// so a larger one costs more than a move.
fn wanted(draft: &Draft, end: usize) -> Vec<(VReg, Reg)> {
    let mut all = Vec::new();
    if draft.undropped {
        return all;
    }
    for step in &draft.steps {
        if let Step::TailCall { args, .. } = step {
            let each = args.iter().enumerate();
            let each = each.filter_map(|(i, v)| Some((*v, param(i)?)));
            all.extend(each.filter(|(_, reg)| usize::from(reg.0) < end));
        }
    }
    all.sort_by_key(|(v, _)| *v);
    all
}

/// A register for each value, and one past the highest.
///
/// A function's arguments arrive in the registers from [`FIRST`] on, in order.
/// Each value in `wanted` takes the first register it has there that none of
/// the values it clashes with has, and each value left over the lowest.
fn colour(
    draft: &Draft,
    held: &Held,
    clashes: &Clashes,
    wanted: &[(VReg, Reg)],
) -> Result<(Vec<Option<Reg>>, usize), Unfit> {
    let mut regs: Vec<Option<Reg>> = vec![None; draft.values as usize];
    let mut end = usize::from(FIRST.0);
    let mut give = |regs: &mut Vec<Option<Reg>>, v: VReg, reg: Reg| {
        if let Some(of) = regs.get_mut(v.0 as usize) {
            *of = Some(reg);
        }
        end = end.max(usize::from(reg.0).saturating_add(1));
    };
    for (i, v) in draft.params.iter().enumerate() {
        give(&mut regs, *v, param(i).ok_or(TOO_MANY)?);
    }
    let free = |regs: &[Option<Reg>], v: VReg| {
        let i = v.0 as usize;
        (regs.get(i) == Some(&None)).then(|| clashes.taken(i, regs))
    };
    for want in wanted.chunk_by(|(v, _), (w, _)| v == w) {
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

    fn of(&self, v: usize) -> &[VReg] {
        let start = self.starts.get(v).copied().unwrap_or(0);
        let end = self.starts.get(v.saturating_add(1)).copied().unwrap_or(0);
        self.all.get(start..end).unwrap_or_default()
    }

    /// The registers value `v` may not have: those of the values it clashes
    /// with, and the two that a frame's link and closure have.
    fn taken(&self, v: usize, regs: &[Option<Reg>]) -> RegSet {
        let mut taken = RegSet::default();
        taken.add(LINK);
        taken.add(CLOSURE);
        for other in self.of(v) {
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
