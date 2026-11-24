//! Every function's instructions in one array.
//!
//! A jump and a call by name hold the place they go to, so the interpreter
//! looks no function up to run one it knows. Places are given out in a first
//! pass, because a function may call one that comes after it.

use scarlet_ir::core_ir::FuncIdx;
use scarlet_ir::tivec::TiVec;

use super::alloc::{Body, Fitted};
use super::ins::{Ins, Label, Lists, Places, Taken, VReg};
use super::{BlockId, Entry, EntryPc, Op, Pc, Reg, Room};
use crate::Audit;
use crate::stop::{Broken, Stop};

/// The instructions a load puts in that the program does not have.
pub(super) struct Extra<'a> {
    /// Where to list the blocks, for a load that counts them. Each starts
    /// with an [`Ins::Count`].
    pub(super) count: Option<&'a mut Vec<Box<[Op]>>>,
    /// When the run is to audit its heap. A load for one that is to do it
    /// before every instruction has an [`Ins::Audit`] in front of each, the
    /// `Count`s among them.
    pub(super) audit: Audit,
}

/// The program's instructions, and where each function's are.
pub(super) struct Placed {
    pub(super) ins: Vec<Ins<Reg>>,
    pub(super) lists: Lists<Reg>,
    pub(super) funcs: TiVec<FuncIdx, Entry>,
    pub(super) toplevels: Vec<Entry>,
}

/// Where a function's result goes when nothing called it: the first register,
/// where the [`Ins::Halt`] it returns to looks.
const RESULT: Reg = Reg(0);

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
            needs: f.body.needs,
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
        let (each, with_count) = match extra.audit {
            Audit::EachInstruction => (2, 4),
            Audit::Never | Audit::AtEnd => (1, 2),
        };
        let mut end = start;
        let mut places = Vec::with_capacity(body.ins.len());
        for head in &heads {
            places.push(position(end)?);
            end = end.saturating_add(if head.is_some() { with_count } else { each });
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
            regs: position(self.lists.regs.len())?,
            args: position(self.lists.args.len())?,
        };
        let lost = || Stop::broken(Broken::NoPlace);
        for list in &body.lists.regs {
            let regs: Option<Box<[Reg]>> = list.iter().map(|r| at.reg(*r)).collect();
            self.lists.regs.push(regs.ok_or_else(lost)?);
        }
        for list in &body.lists.args {
            let args = list.iter().map(|a| super::ins::Place::place(*a, &at));
            let args: Option<Box<[_]>> = args.collect();
            self.lists.args.push(args.ok_or_else(lost)?);
        }
        let audit = match self.extra.audit {
            Audit::EachInstruction => Some(Ins::Audit),
            Audit::Never | Audit::AtEnd => None,
        };
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
            out.push(ins.place(&at).ok_or_else(lost)?);
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

/// What the names in one function's instructions mean.
struct At<'a> {
    funcs: &'a TiVec<FuncIdx, Fitted>,
    plans: &'a TiVec<FuncIdx, Plan>,
    body: &'a Body,
    plan: &'a Plan,
    /// How many lists the functions before this one have.
    regs: u32,
    args: u32,
}

impl Places for At<'_> {
    fn reg(&self, r: VReg) -> Option<Reg> {
        self.body.regs.get(r.0 as usize).copied().flatten()
    }

    fn label(&self, l: Label) -> Option<Pc> {
        let i = self.body.labels.get(l.0 as usize)?;
        self.plan.places.get(*i).copied().map(Pc)
    }

    fn entry(&self, f: FuncIdx) -> Option<EntryPc> {
        u32::try_from(self.plans.get(f)?.start).ok().map(EntryPc)
    }

    fn needs(&self, f: FuncIdx) -> Option<Room> {
        Some(self.funcs.get(f)?.body.needs)
    }

    fn own(&self) -> Room {
        self.body.needs
    }

    fn reg_list(&self, i: u32) -> Option<u32> {
        self.body.lists.regs.get(i as usize)?;
        self.regs.checked_add(i)
    }

    fn arg_list(&self, i: u32) -> Option<u32> {
        self.body.lists.args.get(i as usize)?;
        self.args.checked_add(i)
    }
}

#[cfg(test)]
mod tests;
