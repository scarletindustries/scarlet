//! Every function's instructions in one array.
//!
//! A jump and a call by name hold the place they go to, so the interpreter
//! looks no function up to run one it knows. Places are given out in a first
//! pass, because a function may call one that comes after it. A function that
//! reads captures finds them only when it runs as a closure, so a call by name
//! to one is refused here, where what every callee reads is known.

use scarlet_ir::core_ir::FuncIdx;
use scarlet_ir::tivec::TiVec;

use super::ins::{Assign, Ins, Label, Lists, Makes, Places, Role, Src, Taken, Unplaced, VReg};
use super::moves::{Body, Fitted};
use super::{BlockId, Entry, EntryPc, Op, Pc, Reg, Room};
use crate::Audit;
use crate::frame::RESULT;
use crate::stop::{Broken, Stop};

/// The instructions a load puts in that the program does not have.
pub(super) struct Extra<'a> {
    /// Where to list the blocks, for a load that counts them. Each starts
    /// with an [`Ins::Count`].
    pub(super) count: Option<&'a mut Vec<Box<[Op]>>>,
    /// When the run is to audit its heap. A load for one that is to do it
    /// before every instruction has an [`Ins::Audit`] in front of each, the
    /// `Count`s among them, and an [`Ins::AuditMade`] for each value whose type
    /// says what it may be: after the instruction that writes it, and where
    /// the function starts for a parameter.
    pub(super) audit: Audit,
}

impl Extra<'_> {
    fn audits_each(&self) -> bool {
        match self.audit {
            Audit::EachInstruction => true,
            Audit::Never | Audit::AtEnd => false,
        }
    }
}

/// The program's instructions, and where each function's are.
pub(super) struct Placed {
    pub(super) ins: Vec<Ins<Reg>>,
    pub(super) lists: Lists<Reg>,
    pub(super) funcs: TiVec<FuncIdx, Entry>,
    pub(super) toplevels: Vec<Entry>,
}

pub(super) fn emit(
    funcs: &TiVec<FuncIdx, Fitted>,
    toplevels: &[Fitted],
    mut extra: Extra<'_>,
) -> Result<Placed, Stop> {
    let mut ins = vec![Ins::Halt { src: Taken(RESULT) }];
    let mut end = ins.len();
    let mut blocks = 0usize;
    // First where each function starts and where in it each instruction goes.
    let mut plan = |f: &Fitted| {
        let plan = Plan::new(&f.body, end, &mut blocks, &extra)?;
        end = plan.end;
        Ok::<Plan, Stop>(plan)
    };
    let mut func_plans = TiVec::new();
    for f in funcs {
        let full = |_| Stop::broken(Broken::TooManyFunctions);
        func_plans.push(plan(f)?).map_err(full)?;
    }
    let mut top_plans = Vec::new();
    for f in toplevels {
        top_plans.push(plan(f)?);
    }
    let mut out = Out {
        funcs,
        plans: &func_plans,
        lists: Lists::default(),
        extra: &mut extra,
    };
    ins.reserve_exact(end.saturating_sub(ins.len()));
    for (f, plan) in funcs.into_iter().zip(&func_plans) {
        out.write(&mut ins, &f.body, plan)?;
    }
    for (f, plan) in toplevels.iter().zip(&top_plans) {
        out.write(&mut ins, &f.body, plan)?;
    }
    let lists = out.lists;
    let entry = |(f, plan): (&Fitted, &Plan)| {
        Ok::<Entry, Stop>(Entry {
            name: f.name.clone(),
            pc: EntryPc(position(plan.start)?),
            end: Pc(position(plan.end)?),
            params: f.params,
            own: f.body.own,
            closure: f.body.closure,
        })
    };
    Ok(Placed {
        ins,
        lists,
        funcs: funcs
            .into_iter()
            .zip(&func_plans)
            .map(entry)
            .collect::<Result<_, _>>()?,
        toplevels: toplevels
            .iter()
            .zip(&top_plans)
            .map(entry)
            .collect::<Result<_, _>>()?,
    })
}

/// `at` as an instruction's place, when a place can name one so far on.
fn position(at: usize) -> Result<u32, Stop> {
    u32::try_from(at).map_err(|_| Stop::ProgramTooLong)
}

/// Where one function's instructions go.
struct Plan {
    start: usize,
    end: usize,
    /// Where each instruction starts, which is at the first of what was put in
    /// front of it. A jump to it goes there.
    places: Vec<u32>,
    /// The block each instruction is the first of, for a load that counts
    /// them.
    heads: Vec<Option<BlockId>>,
}

impl Plan {
    fn new(body: &Body, start: usize, blocks: &mut usize, extra: &Extra<'_>) -> Result<Plan, Stop> {
        let mut heads = vec![None; body.ins.len()];
        if extra.count.is_some() {
            for (head, starts) in heads.iter_mut().zip(block_heads(body)) {
                if starts {
                    *head = Some(BlockId(position(*blocks)?));
                    *blocks = blocks.saturating_add(1);
                }
            }
        }
        // An audit goes in front of a count as well as of the instruction.
        let (each, with_count) = if extra.audits_each() { (2, 4) } else { (1, 2) };
        let mut end = start;
        let mut places = Vec::with_capacity(body.ins.len());
        for (i, (ins, head)) in body.ins.iter().zip(&heads).enumerate() {
            places.push(position(end)?);
            end = end.saturating_add(if head.is_some() { with_count } else { each });
            if extra.audits_each() {
                let params = if i == 0 { audits(body, None).len() } else { 0 };
                end = end.saturating_add(params);
                end = end.saturating_add(audits(body, Some(ins)).len());
            }
        }
        position(end)?;
        Ok(Plan {
            start,
            end,
            places,
            heads,
        })
    }
}

/// Whether each instruction of `body` is the first of a basic block.
///
/// A block starts at the function's first instruction, at every instruction a
/// jump names, and after every instruction that may continue anywhere but at
/// the next one. So an instruction runs only when its block's head has, and
/// when it has run, the rest of the block does, unless the run stops.
fn block_heads(body: &Body) -> Vec<bool> {
    let mut heads = vec![false; body.ins.len()];
    let mut head = |i: usize| {
        if let Some(h) = heads.get_mut(i) {
            *h = true;
        }
    };
    head(0);
    for (i, ins) in body.ins.iter().enumerate() {
        let to = ins.target().and_then(|l| body.labels.get(l.0 as usize));
        if let Some(to) = to {
            head(*to);
        }
        if to.is_some() || !ins.falls_through() {
            head(i.saturating_add(1));
        }
    }
    heads
}

/// The instructions so far, and everything that says what a name in one of
/// them means.
struct Out<'a, 'x> {
    funcs: &'a TiVec<FuncIdx, Fitted>,
    plans: &'a TiVec<FuncIdx, Plan>,
    lists: Lists<Reg>,
    extra: &'a mut Extra<'x>,
}

impl Out<'_, '_> {
    fn write(&mut self, out: &mut Vec<Ins<Reg>>, body: &Body, plan: &Plan) -> Result<(), Stop> {
        let at = At {
            funcs: self.funcs,
            plans: self.plans,
            body,
            plan,
            start: EntryPc(position(plan.start)?),
            regs: position(self.lists.regs.len())?,
            args: position(self.lists.args.len())?,
            dsts: position(self.lists.dsts.len())?,
            sets: position(self.lists.sets.len())?,
        };
        let stop = |why| unplaced(at.funcs, why);
        for list in &body.lists.regs {
            let regs: Result<Box<[Reg]>, _> = list.iter().map(|r| at.reg(*r)).collect();
            self.lists.regs.push(regs.map_err(stop)?);
        }
        for list in &body.lists.args {
            let args = list.iter().map(|a| super::ins::Place::place(*a, &at));
            let args: Result<Box<[_]>, _> = args.collect();
            self.lists.args.push(args.map_err(stop)?);
        }
        for list in &body.lists.dsts {
            let dsts = list.iter().map(|d| super::ins::Place::place(*d, &at));
            let dsts: Result<Box<[_]>, _> = dsts.collect();
            self.lists.dsts.push(dsts.map_err(stop)?);
        }
        for list in &body.lists.sets {
            let sets = list.iter().map(|s| {
                Ok(Assign {
                    field: s.field,
                    value: super::ins::Place::place(s.value, &at)?,
                })
            });
            let sets: Result<Box<[_]>, _> = sets.collect();
            self.lists.sets.push(sets.map_err(stop)?);
        }
        let each = self.extra.audits_each();
        let audit = each.then_some(Ins::Audit);
        let made = |out: &mut Vec<Ins<Reg>>, ins| {
            if each {
                for check in audits(body, ins) {
                    out.push(check.place(&at).map_err(stop)?);
                }
            }
            Ok::<(), Stop>(())
        };
        made(out, None)?;
        let mut block: Vec<Op> = Vec::new();
        for (ins, head) in body.ins.iter().zip(&plan.heads) {
            if let Some(number) = head {
                self.close(&mut block);
                out.extend(audit);
                out.push(Ins::Count { block: *number });
            }
            if self.extra.count.is_some() {
                block.push(ins.op());
            }
            out.extend(audit);
            out.push(ins.place(&at).map_err(stop)?);
            made(out, Some(ins))?;
        }
        self.close(&mut block);
        Ok(())
    }

    /// Put `block` on the list of blocks, for a load that has one.
    fn close(&mut self, block: &mut Vec<Op>) {
        if let Some(blocks) = &mut self.extra.count
            && !block.is_empty()
        {
            blocks.push(std::mem::take(block).into_boxed_slice());
        }
    }
}

/// The checks of what `ins` writes, or of the parameters of `body`.
fn audits(body: &Body, ins: Option<&Ins<VReg>>) -> Vec<Ins<VReg>> {
    let mut checks = Vec::new();
    let mut check = |v: VReg, made: Makes| {
        // A register that a tail call moves to has a number no value has.
        let kind = body.kinds.get(v.0 as usize).copied().flatten();
        let may = made.min(kind.unwrap_or(Makes::Any));
        if may != Makes::Any {
            checks.push(Ins::AuditMade { src: Src(v), may });
        }
    };
    match ins {
        None => body.params.iter().for_each(|p| check(*p, Makes::Any)),
        Some(ins) => ins.operands(&body.lists, &mut |v, role| match role {
            Role::Write | Role::WriteOnHit => check(v, ins.makes()),
            Role::Look | Role::Share | Role::Pass { .. } | Role::Take | Role::Drop => {}
        }),
    }
    checks
}

/// How the load stops for an instruction that gets no place.
fn unplaced(funcs: &TiVec<FuncIdx, Fitted>, why: Unplaced) -> Stop {
    match why {
        Unplaced::NoPlace => Stop::broken(Broken::NoPlace),
        Unplaced::NeedsClosure(func) => match funcs.get(func) {
            Some(f) => f.called_by_name(),
            None => Stop::broken(Broken::NoPlace),
        },
    }
}

/// What the names in one function's instructions mean.
struct At<'a> {
    funcs: &'a TiVec<FuncIdx, Fitted>,
    plans: &'a TiVec<FuncIdx, Plan>,
    body: &'a Body,
    plan: &'a Plan,
    start: EntryPc,
    /// How many lists the functions before this one have.
    regs: u32,
    args: u32,
    dsts: u32,
    sets: u32,
}

impl Places for At<'_> {
    fn reg(&self, r: VReg) -> Result<Reg, Unplaced> {
        let reg = self.body.regs.get(r.0 as usize).copied().flatten();
        reg.ok_or(Unplaced::NoPlace)
    }

    fn label(&self, l: Label) -> Result<Pc, Unplaced> {
        let i = self.body.labels.get(l.0 as usize);
        let place = i.and_then(|i| self.plan.places.get(*i));
        place.copied().map(Pc).ok_or(Unplaced::NoPlace)
    }

    fn entry(&self, f: FuncIdx) -> Result<EntryPc, Unplaced> {
        let (Some(callee), Some(plan)) = (self.funcs.get(f), self.plans.get(f)) else {
            return Err(Unplaced::NoPlace);
        };
        if callee.body.closure.needs_one() {
            return Err(Unplaced::NeedsClosure(f));
        }
        let start = u32::try_from(plan.start);
        start.map(EntryPc).map_err(|_| Unplaced::NoPlace)
    }

    fn own_entry(&self) -> EntryPc {
        self.start
    }

    fn own(&self) -> Room {
        self.body.own
    }

    fn reg_list(&self, i: u32) -> Result<u32, Unplaced> {
        list(self.body.lists.regs.get(i as usize), self.regs, i)
    }

    fn arg_list(&self, i: u32) -> Result<u32, Unplaced> {
        list(self.body.lists.args.get(i as usize), self.args, i)
    }

    fn dst_list(&self, i: u32) -> Result<u32, Unplaced> {
        list(self.body.lists.dsts.get(i as usize), self.dsts, i)
    }

    fn set_list(&self, i: u32) -> Result<u32, Unplaced> {
        list(self.body.lists.sets.get(i as usize), self.sets, i)
    }
}

/// The number in the whole program of a function's own list `i`, which is
/// `found`, with `before` of them in the functions before it.
fn list<L>(found: Option<&L>, before: u32, i: u32) -> Result<u32, Unplaced> {
    let number = found.and_then(|_| before.checked_add(i));
    number.ok_or(Unplaced::NoPlace)
}

#[cfg(test)]
mod tests;
