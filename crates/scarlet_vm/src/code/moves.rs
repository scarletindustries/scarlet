//! Tail calls as moves and a jump.
//!
//! A call in tail position to a function the loader knows takes over the frame.
//! Its arguments go to the registers of the callee's parameters, every other
//! register that may hold a cell is given up, and the run goes on at the
//! callee's first instruction. Only here, where every value has its register,
//! is it known what that takes. An argument that is in its parameter's register
//! already takes nothing.
//!
//! The moves happen as if all at once, so one is made only when nothing still
//! reads the register it writes. When every register left to write is still
//! read, they are in cycles, and each is turned in one instruction that reads
//! all its registers before it writes one (`Move2` and `Move3`), or in a few
//! when it is long. So no register is ever needed to put one aside.
//!
//! What a register may hold here is found in one walk over the function, which
//! is enough because every jump so far goes forward. The jump back at a tail
//! call to self is made here, after everything that counts on that. What it
//! brings to the start beside the parameters is only big Ints, and a walk of its
//! own finds those ([`left_by_turns`]). The walk also finds the `Drop`s of
//! registers that can hold no cell, which are left out.

use std::collections::BTreeMap;

use super::alloc::{Assigned, RegSet, param, room};
use super::ins::{Arg, Callee, Dead, DeadList, Dst, Ins, Label, Lists, Makes, Role, Taken, VReg};
use super::translate::{Step, TailTo};
use super::{ReadsClosure, Reg, Room, Unfit, WhyId, add};
use crate::frame::{CLOSURE, FIRST, WINDOW};
use crate::stop::{Broken, Stop};

/// A function's instructions, and where its values are.
pub(super) struct Body {
    pub(super) ins: Vec<Ins<VReg>>,
    pub(super) lists: Lists<VReg>,
    /// Where each label is, as the number of the instruction it stands at.
    pub(super) labels: Vec<usize>,
    /// The register of each value, by its number, when it has one.
    pub(super) regs: Vec<Option<Reg>>,
    /// [`Draft::kinds`](super::translate::Draft::kinds).
    pub(super) kinds: Vec<Option<Makes>>,
    pub(super) params: Vec<VReg>,
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
            kinds: Vec::new(),
            params: Vec::new(),
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

/// Make every tail call in `f` the instructions that do it.
pub(super) fn expand(f: Assigned) -> Result<Body, Unfit> {
    let mut at = Walk::new(&f)?;
    at.held.ints.add_all(&left_by_turns(&f)?);
    let mut out = Out {
        ins: Vec::with_capacity(f.steps.len()),
        lists: f.lists,
        regs: f.regs,
        names: [None; WINDOW],
        given_up: BTreeMap::new(),
        from: [None; WINDOW],
        reads: [0; WINDOW],
    };
    let mut labels = vec![0; f.labels.len()];
    let mut start = None;
    for (i, step) in f.steps.into_iter().enumerate() {
        for Label(l) in at.arrive(i) {
            if let Some(place) = labels.get_mut(l as usize) {
                *place = out.ins.len();
            }
        }
        let (to, args) = match step {
            Step::Ins(Ins::Drop { reg }) if at.empty(reg.0, &out.regs) => continue,
            Step::Ins(ins) => {
                at.pass(&ins, &out.lists, &out.regs, &f.kinds)?;
                out.ins.push(ins);
                continue;
            }
            Step::TailCall { to, args } => (to, args),
        };
        let mut gone = at.held.cells;
        match to {
            TailTo::Own => {}
            TailTo::Func(_) => gone.add_all(&at.held.ints),
        }
        out.pass(gone, &args)?;
        match to {
            TailTo::Own => {
                let to = match start {
                    Some(to) => to,
                    None => *start.insert(add(&mut labels, 0).map(Label)?),
                };
                out.ins.push(Ins::Jump { to });
            }
            TailTo::Func(func) => {
                if f.closure.keeps() {
                    out.give_up(CLOSURE)?;
                }
                let to = Callee(func);
                out.ins.push(Ins::Enter { to });
            }
        }
        at.held = Held::default();
    }
    Ok(Body {
        ins: out.ins,
        lists: out.lists,
        labels,
        regs: out.regs,
        kinds: f.kinds,
        params: f.params,
        own: f.own,
        closure: f.closure,
    })
}

/// The registers that a turn of a loop may leave a big Int in, when the loop can
/// end in a tail call to another function.
///
/// A big Int that a turn worked out and does not pass stays in its register
/// until the register is next written or the frame ends, which is an instruction
/// fewer for each number a turn works out and drops. But a frame that another
/// function takes over gives up no more than that function's registers, so every
/// way out to one gives up what any turn left.
fn left_by_turns(f: &Assigned) -> Result<RegSet, Unfit> {
    let (mut turns, mut leaves) = (false, false);
    for step in &f.steps {
        match step {
            Step::TailCall {
                to: TailTo::Own, ..
            } => turns = true,
            Step::TailCall {
                to: TailTo::Func(_),
                ..
            } => leaves = true,
            Step::Ins(_) => {}
        }
    }
    let mut left = RegSet::default();
    if !turns || !leaves {
        return Ok(left);
    }
    // What an earlier turn left is among what those turns left, so one walk
    // from a start where nothing is left finds it all.
    let mut at = Walk::new(f)?;
    for (i, step) in f.steps.iter().enumerate() {
        at.arrive(i);
        match step {
            Step::Ins(ins) => at.pass(ins, &f.lists, &f.regs, &f.kinds)?,
            Step::TailCall { to, args } => {
                if *to == TailTo::Own {
                    let mut ints = at.held.ints;
                    for (i, v) in args.iter().enumerate() {
                        let src = f.regs.get(v.0 as usize).copied().flatten();
                        for r in param(i).into_iter().chain(src) {
                            ints.remove(r);
                        }
                    }
                    left.add_all(&ints);
                }
                at.held = Held::default();
            }
        }
    }
    Ok(left)
}

/// Every label of `labels` with where it stands, the last first.
pub(super) fn labels_by_place(labels: &[usize]) -> Vec<(usize, Label)> {
    let numbered = labels.iter().copied().zip((0..).map(Label));
    let mut all: Vec<(usize, Label)> = numbered.collect();
    all.sort_unstable_by_key(|(at, _)| std::cmp::Reverse(*at));
    all
}

/// What may be in the registers.
#[derive(Clone, Copy, Default)]
struct Held {
    /// Those that may hold a cell, unless the only kind it can be is a big Int.
    cells: RegSet,
    /// Those that may hold a big Int and no other cell.
    ints: RegSet,
}

impl Held {
    fn makes(&self, r: Reg) -> Makes {
        match (self.cells.has(r), self.ints.has(r)) {
            (true, _) => Makes::Any,
            (false, true) => Makes::Int,
            (false, false) => Makes::NoCell,
        }
    }

    fn set(&mut self, r: Reg, makes: Makes) {
        self.cells.remove(r);
        self.ints.remove(r);
        match makes {
            Makes::NoCell => {}
            Makes::Int => self.ints.add(r),
            Makes::Any => self.cells.add(r),
        }
    }

    /// What may be in them at a place reached with `self` and with `other`.
    fn or(&mut self, other: &Held) {
        self.cells.add_all(&other.cells);
        self.ints.add_all(&other.ints);
    }
}

/// The walk over a function, at the step it has reached.
struct Walk {
    held: Held,
    /// What the jumps so far bring to each label.
    brought: Vec<Held>,
    /// Every label with the step it stands at, the first last.
    labels: Vec<(usize, Label)>,
}

impl Walk {
    fn new(f: &Assigned) -> Result<Walk, Unfit> {
        let mut held = Held::default();
        for (i, v) in f.params.iter().enumerate() {
            if let Some(r) = param(i) {
                held.set(r, kind(&f.kinds, *v)?);
            }
        }
        Ok(Walk {
            held,
            brought: vec![Held::default(); f.labels.len()],
            labels: labels_by_place(&f.labels),
        })
    }

    /// Come to step `i`. Gives the labels that stand there.
    fn arrive(&mut self, i: usize) -> Vec<Label> {
        let mut here = Vec::new();
        while let Some((_, label)) = self.labels.pop_if(|(at, _)| *at <= i) {
            if let Some(brought) = self.brought.get(label.0 as usize) {
                self.held.or(brought);
            }
            here.push(label);
        }
        here
    }

    /// Whether no cell can be in the register of `v`.
    fn empty(&self, v: VReg, regs: &[Option<Reg>]) -> bool {
        let reg = regs.get(v.0 as usize).copied().flatten();
        reg.is_some_and(|r| self.held.makes(r) == Makes::NoCell)
    }

    /// Go on past `ins`.
    fn pass(
        &mut self,
        ins: &Ins<VReg>,
        lists: &Lists<VReg>,
        regs: &[Option<Reg>],
        kinds: &[Option<Makes>],
    ) -> Result<(), Unfit> {
        let reg = |v: VReg| regs.get(v.0 as usize).copied().flatten();
        // A jump takes nothing and writes nothing.
        if let Some(Label(l)) = ins.target()
            && let Some(brought) = self.brought.get_mut(l as usize)
        {
            brought.or(&self.held);
        }
        if !ins.falls_through() {
            self.held = Held::default();
            return Ok(());
        }
        // Going on from here, the value is the constructor the test is for.
        if let Ins::JumpUnlessTag { src, tag, .. } = ins
            && !tag.is_cell()
            && let Some(r) = reg(src.0)
        {
            self.held.set(r, Makes::NoCell);
        }
        let makes = match ins {
            Ins::Move { src, .. } => reg(src.reg).map_or(Makes::Any, |r| self.held.makes(r)),
            ins => ins.makes(),
        };
        // It writes after it has taken, so a register may be among both.
        ins.operands(lists, &mut |v, role| match role {
            Role::Take | Role::Pass { moved: true } | Role::Drop => {
                if let Some(r) = reg(v) {
                    self.held.set(r, Makes::NoCell);
                }
            }
            Role::Look
            | Role::Share
            | Role::Pass { moved: false }
            | Role::Write
            | Role::WriteOnHit => {}
        });
        let mut typed = Ok(());
        ins.operands(lists, &mut |v, role| match role {
            Role::Write | Role::WriteOnHit => match (reg(v), kind(kinds, v)) {
                (Some(r), Ok(kind)) => self.held.set(r, makes.min(kind)),
                (None, Ok(_)) => {}
                (_, Err(why)) => typed = Err(why),
            },
            Role::Look | Role::Share | Role::Pass { .. } | Role::Take | Role::Drop => {}
        });
        typed
    }
}

/// What `v` may be by its type.
fn kind(kinds: &[Option<Makes>], v: VReg) -> Result<Makes, Unfit> {
    let kind = kinds.get(v.0 as usize).copied().flatten();
    kind.ok_or(Unfit::Broken(Broken::NoType))
}

/// The instructions made so far.
struct Out {
    ins: Vec<Ins<VReg>>,
    lists: Lists<VReg>,
    /// The register of each value. A register that no value stands for at a
    /// tail call gets a number here, for an instruction to name it by.
    regs: Vec<Option<Reg>>,
    /// The number each register got.
    names: [Option<VReg>; WINDOW],
    /// The list of each set of registers given up at once, so that tail calls
    /// with the same one share it.
    given_up: BTreeMap<RegSet, DeadList>,
    // Space for one call's moves, as they were before it and after.
    /// For each register still to be written, the one it gets.
    from: [Option<Reg>; WINDOW],
    /// How many of those get each register.
    reads: [u16; WINDOW],
}

impl Out {
    /// Put `args` in the callee's parameters, and give up every register in
    /// `gone` that is neither passed nor written.
    fn pass(&mut self, mut gone: RegSet, args: &[VReg]) -> Result<(), Unfit> {
        let mut stays = RegSet::default();
        for (i, v) in args.iter().enumerate() {
            let src = self.regs.get(v.0 as usize).copied().flatten();
            let (Some(dst), Some(src)) = (param(i), src) else {
                return Err(Broken::NoPlace.into());
            };
            gone.remove(dst);
            gone.remove(src);
            if dst == src {
                stays.add(dst);
            } else {
                *slot(&mut self.from, dst)? = Some(src);
                let reads = slot(&mut self.reads, src)?;
                *reads = reads.saturating_add(1);
            }
        }
        let dsts = || (0..args.len()).filter_map(param);
        self.give_up_all(gone)?;
        // Written now are the registers nothing still reads, and then each that
        // was read only for them.
        let mut takes: Vec<(Reg, Reg)> = Vec::new();
        let mut ready: Vec<Reg> = Vec::new();
        for dst in dsts().rev() {
            if *slot(&mut self.reads, dst)? == 0 && slot(&mut self.from, dst)?.is_some() {
                ready.push(dst);
            }
        }
        while let Some(dst) = ready.pop() {
            let Some(src) = slot(&mut self.from, dst)?.take() else {
                continue;
            };
            let reads = slot(&mut self.reads, src)?;
            *reads = reads.saturating_sub(1);
            let last = *reads == 0;
            if last && !stays.has(src) {
                takes.push((dst, src));
            } else {
                self.take(&takes)?;
                takes.clear();
                let (dst, src) = (Dst(self.name(dst)?), Arg::shared(self.name(src)?));
                self.ins.push(Ins::Move { dst, src });
            }
            if last && slot(&mut self.from, src)?.is_some() {
                ready.push(src);
            }
        }
        self.take(&takes)?;
        // What is left is in cycles, each register read for one other.
        for dst in dsts() {
            let mut cycle = Vec::new();
            let mut at = dst;
            while let Some(src) = slot(&mut self.from, at)?.take() {
                *slot(&mut self.reads, src)? = 0;
                cycle.push(at);
                at = src;
            }
            self.turn(&cycle)?;
        }
        Ok(())
    }

    /// Give each register of `cycle` what the next one holds, and the last what
    /// the first does.
    fn turn(&mut self, mut cycle: &[Reg]) -> Result<(), Unfit> {
        while let [a, b, rest @ ..] = cycle {
            let Some(c) = rest.first() else {
                return self.take(&[(*a, *b), (*b, *a)]);
            };
            // The third then has what the first did, and takes its place in the
            // cycle that is left.
            self.take(&[(*a, *b), (*b, *c), (*c, *a)])?;
            cycle = rest;
        }
        Ok(())
    }

    /// Move the second register of each pair to the first, three at a time. The
    /// three are read before one of them is written.
    fn take(&mut self, moves: &[(Reg, Reg)]) -> Result<(), Unfit> {
        for few in moves.chunks(3) {
            let moves = match *few {
                [(a, x)] => Ins::Move {
                    dst: Dst(self.name(a)?),
                    src: Arg::moved(self.name(x)?),
                },
                [(a, x), (b, y)] => Ins::Move2 {
                    dsts: [Dst(self.name(a)?), Dst(self.name(b)?)],
                    srcs: [Taken(self.name(x)?), Taken(self.name(y)?)],
                },
                [(a, x), (b, y), (c, z)] => Ins::Move3 {
                    dsts: [Dst(self.name(a)?), Dst(self.name(b)?), Dst(self.name(c)?)],
                    srcs: [
                        Taken(self.name(x)?),
                        Taken(self.name(y)?),
                        Taken(self.name(z)?),
                    ],
                },
                _ => return Err(Broken::NoPlace.into()),
            };
            self.ins.push(moves);
        }
        Ok(())
    }

    /// Give up every register in `gone`, from the top down.
    fn give_up_all(&mut self, mut gone: RegSet) -> Result<(), Unfit> {
        if gone.len() <= FEW_DROPS {
            while let Some(r) = gone.take_highest() {
                self.give_up(r)?;
            }
            return Ok(());
        }
        let regs = match self.given_up.get(&gone) {
            Some(regs) => *regs,
            None => {
                let of = gone;
                let mut names = Vec::new();
                while let Some(r) = gone.take_highest() {
                    names.push(self.name(r)?);
                }
                let regs = add(&mut self.lists.regs, names.into()).map(DeadList)?;
                self.given_up.insert(of, regs);
                regs
            }
        };
        self.ins.push(Ins::DropWide { regs });
        Ok(())
    }

    fn give_up(&mut self, r: Reg) -> Result<(), Unfit> {
        let reg = Dead(self.name(r)?);
        self.ins.push(Ins::Drop { reg });
        Ok(())
    }

    /// A value whose register is `r`.
    fn name(&mut self, r: Reg) -> Result<VReg, Unfit> {
        let name = slot(&mut self.names, r)?;
        if let Some(name) = name {
            return Ok(*name);
        }
        let v = add(&mut self.regs, Some(r)).map(VReg)?;
        *name = Some(v);
        Ok(v)
    }
}

/// How many registers are given up with a `Drop` each. One instruction for all
/// of them is cheaper from about here on.
const FEW_DROPS: u32 = 2;

/// What `of` has for `r`. It has something for each register.
fn slot<T>(of: &mut [T], r: Reg) -> Result<&mut T, Unfit> {
    let at = of.get_mut(usize::from(r.0));
    at.ok_or(Unfit::Broken(Broken::NoPlace))
}

#[cfg(test)]
mod tests;
