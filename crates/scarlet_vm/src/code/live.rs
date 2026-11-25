//! What is still to be read after each instruction.
//!
//! Every jump goes forward, so one walk from a function's end to its start
//! meets each instruction after everything that can run after it. The walk
//! keeps the values held at the point it has reached, and what it kept at each
//! label for the jumps there.
//!
//! It answers two questions at once. One is whether an instruction that hands
//! a value on may hand over the register's reference: it may when nothing the
//! frame runs later reads the register. A callee, a cell or another register
//! then holds the reference the register held, so a value nothing else holds
//! still has a count of 1, which is what lets it be changed in place or its
//! cell be taken apart with no count changed. A plain `Drop` is not a read
//! here: by the time it runs on a register that was taken it does nothing, and
//! on a path where nothing took it, it still finds the value and releases it.
//!
//! The other is which values are held at once and so need a register each.
//! For that a `Drop` is a read, because it names the register. But one whose
//! value every way there has taken does nothing. It is left out, and holds
//! nothing.

use super::Unfit;
use super::ins::{Dst, Ins, Label, Lists, Role, VReg};
use super::moves::labels_by_place;
use super::translate::{Draft, Step};
use crate::frame::MAX_VALUES;
use crate::stop::{Broken, Limit};

/// What [`settle`] found out about a function's values.
pub(super) struct Held {
    /// Each pair of values that a register cannot hold both of: one was
    /// written while the other was still held. One that was held only for
    /// `Drop`s that do nothing does not count, unless the function holds
    /// something that nothing drops ([`Draft::undropped`]): when that is
    /// released depends on which values share registers, so there it counts.
    pub(super) clashes: Vec<(VReg, VReg)>,
    /// Whether the instructions name each value, by its number.
    pub(super) named: Vec<bool>,
    /// Whether each step does nothing: a constant that nothing names, or a
    /// `Drop` of what was taken on every way to it.
    pub(super) idle: Vec<bool>,
}

/// The values held where a label stands.
#[derive(Clone, Default)]
struct AtLabel {
    held: Box<[VReg]>,
    read: Box<[VReg]>,
    /// Those that are held and not read, with the `Drop`s each is held for.
    dropped: Box<[(VReg, Drops)]>,
}

/// Some of the `Drop`s of one value, by one of them. Either each finds the
/// value taken or each may find it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Drops(usize);

/// Go over `draft` once from its end. Each register an instruction hands on is
/// made one it takes where it may be.
///
/// Fails when a path reads a value that nothing on it wrote, or when the
/// function holds more values at once than there are registers.
pub(super) fn settle(draft: &mut Draft) -> Result<Held, Unfit> {
    // A function's code is followed by the next one's, so nothing may go on
    // past its end, and no jump may go there.
    let end = draft.steps.len();
    let ends = draft.steps.last().is_some_and(|s| !s.falls_through());
    if !ends || draft.labels.iter().any(|at| *at >= end) {
        return Err(Unfit::Broken(Broken::RunsOff));
    }
    let values = draft.values as usize;
    let mut walk = Walk::new(values, draft.labels.len(), draft.steps.len());
    let mut labels = labels_by_place(&draft.labels).into_iter().peekable();
    for (at, step) in draft.steps.iter_mut().enumerate().rev() {
        walk.step(at, step, &mut draft.lists)?;
        walk.stand(&mut labels, at);
    }
    // Where the function starts, only its parameters have values.
    if !walk.held.items.iter().all(|v| draft.params.contains(v)) {
        return Err(Unfit::Broken(Broken::ReadBeforeWritten));
    }
    Ok(walk.done(draft.undropped))
}

/// The walk, at the point it has reached.
struct Walk {
    /// The values that are read or dropped from here on before anything
    /// writes them again.
    held: Set,
    /// The same, with a plain `Drop` left out.
    read: Set,
    /// What [`Self::held`] and [`Self::read`] were at each label passed.
    labels: Vec<Option<AtLabel>>,
    /// For each value that is held and not read, the `Drop`s it is held for.
    pending: Vec<Drops>,
    /// Pairs like [`Held::clashes`], where the second value is held for these
    /// alone.
    while_pending: Vec<(VReg, VReg, Drops)>,
    drops: AllDrops,
    out: Held,
    // Space for one instruction's operands, kept between instructions.
    operands: Vec<(VReg, Role)>,
    taken: Vec<bool>,
    handed: Set,
    gone: Vec<VReg>,
}

impl Walk {
    /// The walk of a function of so many values, labels and steps, at
    /// its end.
    fn new(values: usize, labels: usize, steps: usize) -> Walk {
        Walk {
            held: Set::new(values),
            read: Set::new(values),
            labels: vec![None; labels],
            operands: Vec::new(),
            taken: Vec::new(),
            handed: Set::new(values),
            gone: Vec::new(),
            pending: vec![Drops(0); values],
            while_pending: Vec::new(),
            drops: AllDrops::default(),
            out: Held {
                clashes: Vec::new(),
                named: vec![false; values],
                idle: vec![false; steps],
            },
        }
    }

    /// What the walk found, once it is at the start. A value that is held there
    /// to be dropped was not taken.
    fn done(mut self, undropped: bool) -> Held {
        for v in &self.held.items {
            if !self.read.has(*v) {
                self.drops.find(pending(&self.pending, *v));
            }
        }
        for (v, h, drops) in self.while_pending {
            if undropped || self.drops.find_it(drops) {
                self.out.clashes.push((v, h));
            }
        }
        for (step, taken) in self.drops.steps() {
            if let Some(idle) = self.out.idle.get_mut(step) {
                *idle = taken;
            }
        }
        self.out
    }

    /// Whether some value is held and not read. Every value that is read is held.
    fn any_pending(&self) -> bool {
        self.held.items.len() > self.read.items.len()
    }

    /// The `Drop`s that `v` is held for, when it is held and not read.
    fn held_for(&self, v: VReg) -> Option<Drops> {
        let held = self.held.has(v) && !self.read.has(v);
        held.then(|| pending(&self.pending, v))
    }

    /// Keep what is held at `at` for each label that stands there.
    fn stand(
        &mut self,
        labels: &mut std::iter::Peekable<impl Iterator<Item = (usize, Label)>>,
        at: usize,
    ) {
        while let Some((_, label)) = labels.next_if(|(place, _)| *place == at) {
            let held = &self.held.items;
            let mut dropped = Vec::new();
            if self.any_pending() {
                let all = held.iter().filter(|v| !self.read.has(**v));
                dropped.extend(all.map(|v| (*v, pending(&self.pending, *v))));
            }
            let here = AtLabel {
                held: held.as_slice().into(),
                read: self.read.items.as_slice().into(),
                dropped: dropped.into(),
            };
            if let Some(kept) = self.labels.get_mut(label.0 as usize) {
                *kept = Some(here);
            }
        }
    }

    /// Go back over `step`, which is step `at`.
    fn step(&mut self, at: usize, step: &mut Step, lists: &mut Lists<VReg>) -> Result<(), Unfit> {
        // Every instruction that names a constant comes after it, so the walk
        // has met them all. One that names none is the case of a number that
        // each instruction reading it holds.
        if let Step::Ins(Ins::Const {
            dst: Dst(VReg(v)), ..
        }) = step
            && self.out.named.get(*v as usize) == Some(&false)
        {
            if let Some(unread) = self.out.idle.get_mut(at) {
                *unread = true;
            }
            return Ok(());
        }
        if !step.falls_through() {
            self.held.clear();
            self.read.clear();
        }
        self.operands.clear();
        step.operands(lists, &mut |v, role| self.operands.push((v, role)));
        // A jump that is not taken has read nothing and taken nothing, so what
        // counts is what is read after it is. The one kind of jump that hands
        // anything on is one that writes when it does not jump. A tail call
        // takes each of its arguments, and `moves` says which way.
        if let Step::Ins(ins) = step {
            self.decide(ins, lists);
        }
        let target = step.target();
        if let Some(Label(l)) = target {
            let kept = self.labels.get(l as usize).and_then(Option::as_ref);
            let landing = kept.ok_or(Unfit::Broken(Broken::JumpsBack))?;
            // A value that one way reads and the other drops is not taken on
            // the way to the `Drop`, unless this takes it on that way. And one
            // that the jump leaves alone and that is dropped where it lands is
            // not taken either.
            if self.any_pending() {
                for v in &landing.read {
                    if let Some(drops) = self.held_for(*v)
                        && !self.gone.contains(v)
                    {
                        self.drops.find(drops);
                    }
                }
            }
            for (v, drops) in &landing.dropped {
                if self.read.has(*v) || self.operands.iter().any(|(of, _)| of == v) {
                    self.drops.find(*drops);
                    // Where one it writes on a hit is dropped after a miss, it
                    // is held for that before it.
                    if let Some(pending) = self.pending.get_mut(v.0 as usize) {
                        *pending = *drops;
                    }
                } else if self.held.has(*v) {
                    self.drops.join(pending(&self.pending, *v), *drops);
                } else if let Some(pending) = self.pending.get_mut(v.0 as usize) {
                    *pending = *drops;
                }
            }
            landing.held.iter().for_each(|v| self.held.add(*v));
            landing.read.iter().for_each(|v| self.read.add(*v));
        }
        self.clash();
        for (v, role) in &self.operands {
            if let Some(named) = self.out.named.get_mut(v.0 as usize) {
                *named = true;
            }
            match role {
                Role::Write => {
                    self.held.remove(*v);
                    self.read.remove(*v);
                }
                // A jump writes nothing, so a value held where it lands is
                // held here too, even the one that is written otherwise.
                Role::WriteOnHit => {
                    let landing = target.and_then(|Label(l)| self.labels.get(l as usize)?.as_ref());
                    if !landing.is_some_and(|l| l.held.contains(v)) {
                        self.held.remove(*v);
                    }
                    if !landing.is_some_and(|l| l.read.contains(v)) {
                        self.read.remove(*v);
                    }
                }
                Role::Drop => {
                    let drops = self.drops.add(at);
                    if self.read.has(*v) {
                        self.drops.find(drops);
                    } else if self.held.has(*v) {
                        self.drops.join(pending(&self.pending, *v), drops);
                    } else if let Some(pending) = self.pending.get_mut(v.0 as usize) {
                        *pending = drops;
                    }
                    self.held.add(*v);
                }
                Role::Look | Role::Share | Role::Pass { .. } | Role::Take => {}
            }
        }
        for (v, role) in &self.operands {
            match role {
                Role::Look | Role::Share | Role::Pass { .. } | Role::Take => {
                    self.read.add(*v);
                    self.held.add(*v);
                }
                Role::Write | Role::WriteOnHit | Role::Drop => {}
            }
        }
        if self.held.items.len() > MAX_VALUES {
            return Err(Unfit::TooMany(Limit::Values));
        }
        Ok(())
    }

    /// Note what the instruction's written values clash with: everything held
    /// after it, and each other, whether or not they are read.
    fn clash(&mut self) {
        let mut written: &[(VReg, Role)] = &self.operands;
        while let Some(((v, role), rest)) = written.split_first() {
            written = rest;
            if writes(*role) {
                for h in self.held.items.iter().filter(|h| *h != v) {
                    if self.read.has(*h) {
                        self.out.clashes.push((*v, *h));
                    } else {
                        let drops = pending(&self.pending, *h);
                        self.while_pending.push((*v, *h, drops));
                    }
                }
                let others = rest.iter().filter(|(w, role)| writes(*role) && w != v);
                self.out.clashes.extend(others.map(|(w, _)| (*v, *w)));
            }
        }
    }

    /// Make each register `ins` hands on one it takes, when nothing after it
    /// reads the register. Of the operands that name one register, only the
    /// last may take it, because the ones before still read it.
    fn decide(&mut self, ins: &mut Ins<VReg>, lists: &mut Lists<VReg>) {
        self.handed.clear();
        self.taken.clear();
        self.gone.clear();
        for (v, role) in self.operands.iter().rev() {
            match role {
                Role::Pass { .. } => {
                    let takes = !self.read.has(*v) && !self.handed.has(*v);
                    self.taken.push(takes);
                    self.handed.add(*v);
                    if takes {
                        self.gone.push(*v);
                    }
                }
                Role::Take => {
                    self.handed.add(*v);
                    self.gone.push(*v);
                }
                Role::Look | Role::Share => self.handed.add(*v),
                Role::Write | Role::WriteOnHit | Role::Drop => {}
            }
        }
        // What a `Drop` after this is of, and this leaves in its register.
        for (v, role) in &self.operands {
            if *role != Role::Drop
                && let Some(drops) = self.held_for(*v)
                && !self.gone.contains(v)
            {
                self.drops.find(drops);
            }
        }
        if !self.taken.is_empty() {
            ins.decide(lists, &mut |_| self.taken.pop().unwrap_or(false));
            if let Some(taking) = ins.taking() {
                *ins = taking;
            }
        }
    }
}

/// The `Drop`s that `of` says `v` is held for.
fn pending(of: &[Drops], v: VReg) -> Drops {
    of.get(v.0 as usize).copied().unwrap_or(Drops(0))
}

/// Every `Drop` met, in groups that are found or not together.
#[derive(Default)]
struct AllDrops(Vec<Met>);

/// One `Drop` met.
struct Met {
    /// The step it is.
    step: usize,
    /// Another of its group, or itself when it stands for the group.
    of: usize,
    /// Whether one of the group it stands for may find its value.
    found: bool,
}

impl AllDrops {
    /// The `Drop` at `step`, by itself.
    fn add(&mut self, step: usize) -> Drops {
        let of = self.0.len();
        let found = false;
        self.0.push(Met { step, of, found });
        Drops(of)
    }

    /// The one that stands for the group of `drops`.
    fn group(&mut self, Drops(mut i): Drops) -> usize {
        while let Some(up) = self.0.get(i).map(|met| met.of)
            && up != i
        {
            // Each is put halfway up on the way, which keeps the ways short.
            let half = self.0.get(up).map_or(up, |met| met.of);
            if let Some(met) = self.0.get_mut(i) {
                met.of = half;
            }
            i = half;
        }
        i
    }

    /// Note that one of `drops` may find its value.
    fn find(&mut self, drops: Drops) {
        let group = self.group(drops);
        if let Some(met) = self.0.get_mut(group) {
            met.found = true;
        }
    }

    fn find_it(&mut self, drops: Drops) -> bool {
        let group = self.group(drops);
        self.0.get(group).is_none_or(|met| met.found)
    }

    /// Make one group of two.
    fn join(&mut self, a: Drops, b: Drops) {
        let (a, b) = (self.group(a), self.group(b));
        if a != b
            && let Some(met) = self.0.get_mut(b)
        {
            met.of = a;
            if met.found {
                self.find(Drops(a));
            }
        }
    }

    /// The step each is, with whether its group's values are all taken.
    fn steps(&mut self) -> Vec<(usize, bool)> {
        let all = 0..self.0.len();
        let one = |i| {
            let taken = !self.find_it(Drops(i));
            Some((self.0.get(i)?.step, taken))
        };
        all.filter_map(one).collect()
    }
}

/// Whether an operand with `role` is written.
fn writes(role: Role) -> bool {
    match role {
        Role::Write | Role::WriteOnHit => true,
        Role::Look | Role::Share | Role::Pass { .. } | Role::Take | Role::Drop => false,
    }
}

/// A set of values. Asking, adding and removing take no longer for a large
/// one, and neither does going over a small one out of many values.
struct Set {
    items: Vec<VReg>,
    /// One past where each value is in `items`, or 0.
    at: Vec<u32>,
}

impl Set {
    fn new(values: usize) -> Set {
        Set {
            items: Vec::new(),
            at: vec![0; values],
        }
    }

    fn has(&self, v: VReg) -> bool {
        self.at.get(v.0 as usize).is_some_and(|at| *at != 0)
    }

    fn add(&mut self, v: VReg) {
        if let Some(at) = self.at.get_mut(v.0 as usize)
            && *at == 0
            && let Ok(end) = u32::try_from(self.items.len())
        {
            *at = end.saturating_add(1);
            self.items.push(v);
        }
    }

    fn remove(&mut self, v: VReg) {
        let Some(at) = self.at.get_mut(v.0 as usize) else {
            return;
        };
        let Some(i) = std::mem::take(at).checked_sub(1) else {
            return;
        };
        // The last item goes where this one was, unless it is this one.
        if let Some(last) = self.items.pop()
            && let Some(hole) = self.items.get_mut(i as usize)
        {
            *hole = last;
            if let Some(at) = self.at.get_mut(last.0 as usize) {
                *at = i.saturating_add(1);
            }
        }
    }

    fn clear(&mut self) {
        for v in self.items.drain(..) {
            if let Some(at) = self.at.get_mut(v.0 as usize) {
                *at = 0;
            }
        }
    }
}

#[cfg(test)]
mod tests;
