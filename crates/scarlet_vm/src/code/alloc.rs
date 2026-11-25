//! Which register each value has.
//!
//! Two values share a register whenever neither is written while the other is
//! held. A frame gives up what its registers hold when it returns, and a loop
//! when it goes round, so fewer of them is less work at each.
//!
//! Sharing a register changes when a value is released, never whether: the
//! value a register held is released when the next one is written over it,
//! instead of when the frame ends, and by then nothing reads it. A `Drop`
//! names its register, so a value keeps it until its `Drop`.

use super::ins::{Ins, Lists, VReg};
use super::live::Held;
use super::translate::Draft;
use super::{ReadsClosure, Reg, Room, Unfit, WhyId};
use crate::frame::{CLOSURE, FIRST, LINK, WINDOW};
use crate::stop::{Broken, Limit, Stop};

/// A function whose values have registers.
pub(super) struct Body {
    pub(super) ins: Vec<Ins<VReg>>,
    pub(super) lists: Lists<VReg>,
    /// Where each label is, as the number of the instruction it stands at.
    pub(super) labels: Vec<usize>,
    /// The register of each value, by its number, when it has one.
    pub(super) regs: Vec<Option<Reg>>,
    /// How many registers its frame has.
    pub(super) own: Room,
    /// What it reads of the closure it runs as.
    pub(super) closure: ReadsClosure,
}

/// A function ready to be given its place among the others.
pub(super) struct Fitted {
    pub(super) name: String,
    /// How many arguments it takes.
    pub(super) params: usize,
    pub(super) body: Body,
}

impl Fitted {
    pub(super) fn new(name: String, params: usize, body: Body) -> Fitted {
        Fitted { name, params, body }
    }

    /// A function that stops with `why` when it is called. Its arguments still
    /// arrive in its registers.
    pub(super) fn bad(name: String, params: usize, why: WhyId) -> Fitted {
        let body = Body {
            ins: vec![Ins::Bad { why }],
            lists: Lists::default(),
            labels: Vec::new(),
            regs: Vec::new(),
            own: room(params.saturating_add(usize::from(FIRST.0))),
            closure: ReadsClosure::Nothing,
        };
        Fitted { name, params, body }
    }

    /// The stop for a call by name to this function, which needs a closure.
    pub(super) fn called_by_name(&self) -> Stop {
        let func = self.name.clone();
        Stop::broken(Broken::CalledByName { func })
    }
}

/// Room for the registers before `end`, or for as many as a frame has.
fn room(end: usize) -> Room {
    Room(u16::try_from(end.min(WINDOW)).unwrap_or(u16::MAX))
}

/// Give each value of `draft` a register, and take out the instructions that
/// then do nothing.
///
/// A function's arguments arrive in the registers from [`FIRST`] on, in order.
/// Each other value takes the lowest register none of the values it clashes
/// with has.
pub(super) fn allocate(draft: Draft, held: &Held) -> Result<Body, Unfit> {
    let values = draft.values as usize;
    let mut regs: Vec<Option<Reg>> = vec![None; values];
    // No value is in the registers that a frame's link and closure have.
    let mut frames = RegSet::default();
    frames.add(LINK);
    frames.add(CLOSURE);
    let mut end = usize::from(FIRST.0);
    let give = |regs: &mut Vec<Option<Reg>>, v: VReg, r: usize| {
        let reg = u8::try_from(r).map(Reg);
        let reg = reg.map_err(|_| Unfit::TooMany(Limit::Values))?;
        if let Some(of) = regs.get_mut(v.0 as usize) {
            *of = Some(reg);
        }
        Ok::<(), Unfit>(())
    };
    for param in &draft.params {
        give(&mut regs, *param, end)?;
        end = end.saturating_add(1);
    }
    let clashes = Clashes::new(values, &held.clashes);
    for (v, named) in (0u32..).map(VReg).zip(&held.named) {
        let i = v.0 as usize;
        if !named || regs.get(i).is_none_or(Option::is_some) {
            continue;
        }
        let mut taken = frames.clone();
        for other in clashes.of(i) {
            if let Some(Some(reg)) = regs.get(other.0 as usize) {
                taken.add(*reg);
            }
        }
        let r = taken.lowest_free();
        give(&mut regs, v, r)?;
        end = end.max(r.saturating_add(1));
    }
    let mut body = Body {
        ins: draft.ins,
        lists: draft.lists,
        labels: draft.labels,
        regs,
        own: room(end),
        closure: draft.closure,
    };
    tidy(&mut body, &held.unread);
    Ok(body)
}

/// A set of registers, one bit each.
#[derive(Clone, Default)]
struct RegSet([u64; 4]);

impl RegSet {
    fn add(&mut self, Reg(r): Reg) {
        if let Some(word) = self.0.get_mut(usize::from(r / 64)) {
            *word |= 1 << (r % 64);
        }
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
}

/// Take out of `body` what does nothing: a constant no instruction reads, as
/// `unread` says, a `Move` from a register to itself, and a `Jump` to the
/// instruction that would run next anyway.
fn tidy(body: &mut Body, unread: &[bool]) {
    let reg = |v: VReg| body.regs.get(v.0 as usize).copied().flatten();
    // Whether each instruction stays, and for each place in the function the
    // instruction that runs there: itself, or the next that stays.
    let mut stays = vec![false; body.ins.len()];
    let mut runs = vec![body.ins.len(); body.ins.len().saturating_add(1)];
    let mut next = body.ins.len();
    for (i, ins) in body.ins.iter().enumerate().rev() {
        let nothing = if let Ins::Move { dst, src } = ins {
            reg(dst.0) == reg(src.reg)
        } else if let Ins::Jump { to } = ins {
            let place = body.labels.get(to.0 as usize);
            place.and_then(|at| runs.get(*at)) == Some(&next)
        } else {
            unread.get(i) == Some(&true)
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
    // Where each instruction moves to, and where the next one does for one
    // that goes.
    let mut moves_to = vec![0usize; body.ins.len().saturating_add(1)];
    let mut kept = 0usize;
    for (i, to) in moves_to.iter_mut().enumerate() {
        *to = kept;
        kept = kept.saturating_add(usize::from(stays.get(i) == Some(&true)));
    }
    for at in &mut body.labels {
        *at = moves_to.get(*at).copied().unwrap_or(kept);
    }
    let mut stays = stays.into_iter();
    body.ins.retain(|_| stays.next().unwrap_or(true));
}

#[cfg(test)]
mod tests;
