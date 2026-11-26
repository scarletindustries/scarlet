//! What a match holds while a guard or a size runs, as what it was lowered to says. Nothing that a
//! program prints depends on it, but when a handle closes does, and whether a tail is
//! taken in place.

use std::collections::{BTreeMap, BTreeSet};

use scarlet_ir::core_ir::{
    Atom, Callee, CoreExpr, CoreFn, FuncIdx, JoinId, LocalId, PrimOp, ProbeOp,
};

use super::oracle::{BODIES, SIZES};

type Live = BTreeSet<LocalId>;

/// What is wrong with how a match holds its arrays.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct Wrong {
    /// A tail that a guard does not read was taken in front of it, and where the guard
    /// fails the array is read again.
    pub(super) early: usize,
    /// An array was held while a guard ran, for nothing but its tail.
    pub(super) late: usize,
    /// An array was held while a guard ran, for nothing but its length.
    pub(super) length: usize,
    /// How many guards there were behind a question of an array.
    pub(super) guards: usize,
}

/// What each join's continuation reads.
type Joins = BTreeMap<JoinId, Live>;

/// What `e` reads that is bound in front of it.
fn live(e: &CoreExpr, joins: &Joins) -> Live {
    let mut out = Live::new();
    match e {
        CoreExpr::Let { bind, rhs, body } => {
            out = live(body, joins);
            out.remove(&bind.id);
            rhs.for_each_operand(|l| {
                out.insert(l);
            });
        }
        CoreExpr::Join {
            id,
            params,
            cont,
            body,
        } => {
            let mut there = live(cont, joins);
            params.iter().for_each(|p| {
                there.remove(&p.id);
            });
            let mut joins = joins.clone();
            joins.insert(*id, there);
            out = live(body, &joins);
        }
        CoreExpr::If { cond, then, els } => {
            out = live(then, joins);
            out.extend(live(els, joins));
            out.insert(*cond);
        }
        CoreExpr::Probe {
            args,
            binds,
            hit,
            miss,
            ..
        } => {
            out = live(hit, joins);
            binds.iter().flatten().for_each(|b| {
                out.remove(&b.id);
            });
            out.extend(live(miss, joins));
            out.extend(args);
        }
        CoreExpr::Switch {
            scrut,
            cases,
            default,
        } => {
            for arm in cases.arms().into_iter().chain(default.as_deref()) {
                out.extend(live(arm, joins));
            }
            out.insert(*scrut);
        }
        CoreExpr::Jump { to, args } => {
            out = joins.get(to).cloned().unwrap_or_default();
            out.extend(args);
        }
        CoreExpr::TailCall { args, .. } => out.extend(args),
        CoreExpr::NoArm => {}
        CoreExpr::Ret(v) => {
            out.insert(*v);
        }
        CoreExpr::Drop { .. } | CoreExpr::Open { .. } | CoreExpr::SetGlobal { .. } => {
            panic!("{e} is of a later pass")
        }
    }
    out
}

/// The walk, at the point it has reached.
#[derive(Clone, Default)]
struct At {
    joins: Joins,
    /// Each tail that was taken and that nothing has read since, with its array.
    tails: BTreeMap<LocalId, LocalId>,
    /// The arrays that something has been asked of.
    asked: Live,
    /// What each local was read out of.
    out_of: BTreeMap<LocalId, LocalId>,
}

impl At {
    /// Whether `l`, or something that it was read out of, is one of `all`.
    fn held(&self, mut l: LocalId, all: &Live) -> bool {
        loop {
            if all.contains(&l) {
                return true;
            }
            match self.out_of.get(&l) {
                Some(of) => l = *of,
                None => return false,
            }
        }
    }
}

/// Whether `binds` of `op` are nothing, and whether they are a tail alone.
fn finds(
    op: ProbeOp,
    binds: &[Option<scarlet_ir::core_ir::CoreBind>],
) -> Option<(bool, Option<LocalId>)> {
    let more = match op {
        ProbeOp::ArrayUncons => true,
        ProbeOp::ArraySplit { more, .. } => more,
        ProbeOp::ArrayGet | ProbeOp::ArraySlice | ProbeOp::ArraySet | ProbeOp::MapGet => {
            return None;
        }
    };
    let tail = match (more, binds.last()) {
        (true, Some(Some(tail))) => Some(tail.id),
        _ => None,
    };
    let elements = binds.iter().flatten().count() - usize::from(tail.is_some());
    Some((elements > 0, tail))
}

/// How the first thing that reads `xs` on each way through `e` reads it.
#[derive(Debug, Default, Clone, Copy)]
struct Reads {
    tail: bool,
    length: bool,
    other: bool,
}

impl Reads {
    fn or(self, b: Reads) -> Reads {
        Reads {
            tail: self.tail || b.tail,
            length: self.length || b.length,
            other: self.other || b.other,
        }
    }
}

/// How `xs` is read in `e`, whatever else is. A length is asked for by a probe that finds
/// nothing, and what comes behind that counts too.
fn reads(e: &CoreExpr, xs: LocalId, joins: &BTreeMap<JoinId, Reads>) -> Reads {
    let mut out = Reads::default();
    let mut e = e;
    let mut joins = joins.clone();
    loop {
        match e {
            CoreExpr::Let { rhs, body, .. } => {
                let mut any = false;
                rhs.for_each_operand(|l| any |= l == xs);
                match rhs {
                    Atom::PrimOp {
                        op: PrimOp::ArrayLen,
                        ..
                    } => out.length |= any,
                    _ => out.other |= any,
                }
                e = body;
            }
            CoreExpr::Join { id, cont, body, .. } => {
                let there = reads(cont, xs, &joins);
                joins.insert(*id, there);
                e = body;
            }
            CoreExpr::If { then, els, .. } => {
                return out.or(reads(then, xs, &joins)).or(reads(els, xs, &joins));
            }
            CoreExpr::Probe {
                op,
                args,
                binds,
                hit,
                miss,
            } => {
                if args.contains(&xs) {
                    match finds(*op, binds) {
                        Some((false, Some(_))) => out.tail = true,
                        Some((false, None)) => out.length = true,
                        Some((true, _)) | None => out.other = true,
                    }
                }
                return out.or(reads(hit, xs, &joins)).or(reads(miss, xs, &joins));
            }
            CoreExpr::Switch {
                scrut,
                cases,
                default,
            } => {
                out.other |= *scrut == xs;
                for arm in cases.arms().into_iter().chain(default.as_deref()) {
                    out = out.or(reads(arm, xs, &joins));
                }
                return out;
            }
            CoreExpr::Jump { to, args } => {
                out.other |= args.contains(&xs);
                return out.or(joins.get(to).copied().unwrap_or_default());
            }
            CoreExpr::TailCall { args, .. } => {
                out.other |= args.contains(&xs);
                return out;
            }
            CoreExpr::Ret(v) => {
                out.other |= *v == xs;
                return out;
            }
            CoreExpr::NoArm
            | CoreExpr::Drop { .. }
            | CoreExpr::Open { .. }
            | CoreExpr::SetGlobal { .. } => return out,
        }
    }
}

/// What is wrong with `f`, which is a match that is lowered and has no drops yet.
pub(super) fn wrong(f: &CoreFn) -> Wrong {
    let mut out = Wrong::default();
    walk(&f.body, &mut At::default(), &BTreeMap::new(), &mut out);
    out
}

/// Where it goes on behind a call of `f`, which gave `given`, when that is a guard or a
/// size: where the guard holds or the size is one that a binary may have, and where not.
fn goes_on(f: u32, given: LocalId, mut e: &CoreExpr) -> Option<(&CoreExpr, &CoreExpr)> {
    if f >= BODIES {
        return None;
    }
    // A size is looked at first.
    while f >= SIZES
        && let CoreExpr::Let { body, .. } = e
    {
        e = body;
    }
    match e {
        CoreExpr::If { cond, then, els } if f >= SIZES || *cond == given => Some((then, els)),
        _ => None,
    }
}

fn walk(e: &CoreExpr, at: &mut At, conts: &BTreeMap<JoinId, (CoreExpr, Joins)>, out: &mut Wrong) {
    match e {
        CoreExpr::Let { bind, rhs, body } => {
            if let Atom::Call {
                callee: Callee::Known(FuncIdx(f)),
                args,
            } = rhs
                && let Some((then, els)) = goes_on(*f, bind.id, body)
            {
                guard(args, then, els, at, conts, out);
            }
            if let Atom::PrimOp {
                op: PrimOp::Field(_) | PrimOp::TupleField(_),
                args,
            } = rhs
            {
                at.out_of.insert(bind.id, args[0]);
            }
            rhs.for_each_operand(|l| {
                at.tails.remove(&l);
                if matches!(
                    rhs,
                    Atom::PrimOp {
                        op: PrimOp::ArrayLen,
                        ..
                    }
                ) {
                    at.asked.insert(l);
                }
            });
            walk(body, at, conts, out);
        }
        CoreExpr::Join {
            id,
            params,
            cont,
            body,
        } => {
            let mut there = live(cont, &at.joins);
            params.iter().for_each(|p| {
                there.remove(&p.id);
            });
            let mut inner = at.clone();
            inner.joins.insert(*id, there);
            let mut conts_in = conts.clone();
            conts_in.insert(*id, ((**cont).clone(), at.joins.clone()));
            walk(body, &mut inner, &conts_in, out);
            // What the body has read was taken for that.
            for l in live(body, &inner.joins) {
                at.tails.remove(&l);
            }
            walk(cont, at, conts, out);
        }
        CoreExpr::If { then, els, .. } => {
            walk(then, &mut at.clone(), conts, out);
            walk(els, at, conts, out);
        }
        CoreExpr::Probe {
            op,
            args,
            binds,
            hit,
            miss,
        } => {
            let mut inner = at.clone();
            if let (Some((_, tail)), [xs]) = (finds(*op, binds), args.as_slice()) {
                at.asked.insert(*xs);
                inner.asked.insert(*xs);
                for bind in binds.iter().flatten() {
                    inner.out_of.insert(bind.id, *xs);
                }
                if let Some(tail) = tail {
                    inner.out_of.remove(&tail);
                    inner.tails.insert(tail, *xs);
                }
            }
            walk(hit, &mut inner, conts, out);
            walk(miss, at, conts, out);
        }
        CoreExpr::Switch { cases, default, .. } => {
            for arm in cases.arms().into_iter().chain(default.as_deref()) {
                walk(arm, &mut at.clone(), conts, out);
            }
        }
        CoreExpr::Jump { .. }
        | CoreExpr::TailCall { .. }
        | CoreExpr::Ret(_)
        | CoreExpr::NoArm
        | CoreExpr::Drop { .. }
        | CoreExpr::Open { .. }
        | CoreExpr::SetGlobal { .. } => {}
    }
}

/// Look at a guard that is given `args` and goes on with `then` or with `els`.
fn guard(
    args: &[LocalId],
    then: &CoreExpr,
    els: &CoreExpr,
    at: &At,
    conts: &BTreeMap<JoinId, (CoreExpr, Joins)>,
    out: &mut Wrong,
) {
    if at.asked.is_empty() {
        return;
    }
    out.guards += 1;
    let (holds, fails) = (live(then, &at.joins), live(els, &at.joins));
    for (tail, xs) in &at.tails {
        let waits = holds.contains(tail) && !fails.contains(tail) && !args.contains(tail);
        out.early += usize::from(waits && at.held(*xs, &fails));
    }
    let mut joined = BTreeMap::new();
    for xs in &at.asked {
        joined.clear();
        let mut ids: Vec<&JoinId> = conts.keys().collect();
        // A continuation may go on at one that is further out, which is declared first.
        ids.sort();
        for id in ids {
            let reads = reads(&conts[id].0, *xs, &joined);
            joined.insert(*id, reads);
        }
        let (then, els) = (reads(then, *xs, &joined), reads(els, *xs, &joined));
        let read = |r: Reads| r.tail || r.length || r.other;
        out.late += usize::from(then.tail && !then.other && !at.held(*xs, &fails));
        // It is held by itself, and not in something else that is read.
        let mut others = holds.clone();
        others.extend(&fails);
        others.remove(xs);
        let only_length = els.length && !els.tail && !els.other && !read(then);
        out.length += usize::from(only_length && !at.held(*xs, &others));
    }
}
