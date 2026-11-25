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
//! For that a `Drop` is a read, because it names the register.

use super::Unfit;
use super::ins::{Dst, Ins, Label, Lists, Role, VReg};
use super::translate::Draft;
use crate::frame::MAX_VALUES;
use crate::stop::{Broken, Limit};

/// What [`settle`] found out about a function's values.
pub(super) struct Held {
    /// Each pair of values that a register cannot hold both of: one was
    /// written while the other was still held.
    pub(super) clashes: Vec<(VReg, VReg)>,
    /// Whether the instructions name each value, by its number.
    pub(super) named: Vec<bool>,
    /// Whether each instruction is a constant that nothing names.
    pub(super) unread: Vec<bool>,
}

/// The values held where a label stands.
#[derive(Clone, Default)]
struct AtLabel {
    held: Box<[VReg]>,
    read: Box<[VReg]>,
}

/// Go over `draft` once from its end. Each register an instruction hands on is
/// made one it takes where it may be.
///
/// Fails when a path reads a value that nothing on it wrote, or when the
/// function holds more values at once than there are registers.
pub(super) fn settle(draft: &mut Draft) -> Result<Held, Unfit> {
    // A function's code is followed by the next one's, so nothing may go on
    // past its end, and no jump may go there.
    let end = draft.ins.len();
    let ends = draft.ins.last().is_some_and(|ins| !ins.falls_through());
    if !ends || draft.labels.iter().any(|at| *at >= end) {
        return Err(Unfit::Broken(Broken::RunsOff));
    }
    let values = draft.values as usize;
    let mut walk = Walk::new(values, draft.labels.len(), draft.ins.len());
    let mut labels = labels_by_place(draft).into_iter().peekable();
    for (at, ins) in draft.ins.iter_mut().enumerate().rev() {
        walk.step(at, ins, &mut draft.lists)?;
        walk.stand(&mut labels, at);
    }
    // Where the function starts, only its parameters have values.
    if !walk.held.items.iter().all(|v| draft.params.contains(v)) {
        return Err(Unfit::Broken(Broken::ReadBeforeWritten));
    }
    Ok(walk.out)
}

/// Every label with where it stands, the last first.
fn labels_by_place(draft: &Draft) -> Vec<(usize, Label)> {
    let mut all: Vec<(usize, Label)> = draft
        .labels
        .iter()
        .zip(0..)
        .map(|(at, l)| (*at, Label(l)))
        .collect();
    all.sort_unstable_by_key(|(at, _)| std::cmp::Reverse(*at));
    all
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
    out: Held,
    // Space for one instruction's operands, kept between instructions.
    operands: Vec<(VReg, Role)>,
    taken: Vec<bool>,
    handed: Set,
}

impl Walk {
    /// The walk of a function of so many values, labels and instructions, at
    /// its end.
    fn new(values: usize, labels: usize, ins: usize) -> Walk {
        Walk {
            held: Set::new(values),
            read: Set::new(values),
            labels: vec![None; labels],
            operands: Vec::new(),
            taken: Vec::new(),
            handed: Set::new(values),
            out: Held {
                clashes: Vec::new(),
                named: vec![false; values],
                unread: vec![false; ins],
            },
        }
    }

    /// Keep what is held at `at` for each label that stands there.
    fn stand(
        &mut self,
        labels: &mut std::iter::Peekable<impl Iterator<Item = (usize, Label)>>,
        at: usize,
    ) {
        while let Some((_, label)) = labels.next_if(|(place, _)| *place == at) {
            if let Some(kept) = self.labels.get_mut(label.0 as usize) {
                *kept = Some(AtLabel {
                    held: self.held.items.as_slice().into(),
                    read: self.read.items.as_slice().into(),
                });
            }
        }
    }

    /// Go back over `ins`, which is instruction `at`.
    fn step(
        &mut self,
        at: usize,
        ins: &mut Ins<VReg>,
        lists: &mut Lists<VReg>,
    ) -> Result<(), Unfit> {
        // Every instruction that names a constant comes after it, so the walk
        // has met them all. One that names none is the case of a number that
        // each instruction reading it holds.
        if let Ins::Const {
            dst: Dst(VReg(v)), ..
        } = ins
            && self.out.named.get(*v as usize) == Some(&false)
        {
            if let Some(unread) = self.out.unread.get_mut(at) {
                *unread = true;
            }
            return Ok(());
        }
        if !ins.falls_through() {
            self.held.clear();
            self.read.clear();
        }
        self.operands.clear();
        ins.operands(lists, &mut |v, role| self.operands.push((v, role)));
        // A jump that is not taken has read nothing and taken nothing, so what
        // counts is what is read after it is. The one kind of jump that hands
        // anything on is one that writes when it does not jump.
        self.decide(ins, lists);
        let target = ins.target();
        if let Some(Label(l)) = target {
            let kept = self.labels.get(l as usize).and_then(Option::as_ref);
            let landing = kept.ok_or(Unfit::Broken(Broken::JumpsBack))?;
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
                Role::Drop => self.held.add(*v),
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
                let others = self.held.items.iter().filter(|h| *h != v);
                self.out.clashes.extend(others.map(|h| (*v, *h)));
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
        for (v, role) in self.operands.iter().rev() {
            match role {
                Role::Pass { .. } => {
                    self.taken.push(!self.read.has(*v) && !self.handed.has(*v));
                    self.handed.add(*v);
                }
                Role::Look | Role::Share | Role::Take => self.handed.add(*v),
                Role::Write | Role::WriteOnHit | Role::Drop => {}
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
