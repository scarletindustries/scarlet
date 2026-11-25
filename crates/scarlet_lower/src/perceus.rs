//! Perceus is the pass that adds reference counting to Core IR. It decides
//! where each heap value is released. This is the dropping half of Lorenzen &
//! Leijen (ICFP'22, Fig. 5), run as a pass from Core IR to Core IR. In ANF,
//! finding a value's last use is one backward scan.
//!
//! A backward liveness walk inserts a `Drop` after each heap bind's last read.
//! Where `If` and `Match` branches split, ownership is evened out so every path
//! releases every owned slot exactly once (Fig. 5's Δ-rule). A return gives up
//! the value it returns, a tail call its arguments, and a jump its arguments
//! and what the continuation it goes to owns.

use std::collections::{BTreeMap, BTreeSet};

use super::spine::{End, Frame, peel};
use super::{Atom, CoreExpr, CoreFn, CorePat, JoinId, LocalId};
use crate::program::InternalLinkError;
use crate::typed_ir::{RTy, ResolvedPool};
use scarlet_ir::tivec::TiVec;

/// Run Perceus on one lowered function. `locals` is the type of every local
/// `lower` made for it, `pool` says which of those types can be cells, and
/// `reached` which joins are [always reached](CoreExpr::always_reached).
pub(crate) fn perceus(
    pool: &ResolvedPool,
    locals: &TiVec<LocalId, RTy>,
    reached: &BTreeSet<JoinId>,
    f: CoreFn,
) -> Result<CoreFn, InternalLinkError> {
    let mut cx = Perceus {
        pool,
        locals,
        reached,
        entered: BTreeMap::new(),
        around: Vec::new(),
        plain: BTreeSet::new(),
    };
    let CoreFn {
        params,
        body,
        ret_ty,
    } = f;
    let (body, live) = cx.drop_pass(body)?;
    // Params are owned on entry. Any the body doesn't read drop at its head,
    // so the frame still releases everything it owns.
    let dead = params.iter().map(|p| p.id);
    let body = cx.wrap_drops(dead.filter(|id| !live.contains(id)), body)?;
    Ok(CoreFn {
        params,
        body,
        ret_ty,
    })
}

/// The locals live at a program point, without those in [`Perceus::around`].
/// It's a `BTreeSet` so the order drops are inserted in, and so the golden
/// snapshots, never changes between runs.
type Live = BTreeSet<LocalId>;

struct Perceus<'p> {
    pool: &'p ResolvedPool,
    locals: &'p TiVec<LocalId, RTy>,
    /// The joins that every way through their body comes to.
    reached: &'p BTreeSet<JoinId>,
    /// What is live where the continuation of each join in scope starts, beside
    /// its parameters and what is in [`Self::around`].
    entered: BTreeMap<JoinId, Live>,
    /// What is live in the continuations of the joins in [`Self::reached`] whose
    /// bodies are being walked, the innermost last. All of it is live on every
    /// way through those bodies, so it is kept here once, and a long chain of
    /// joins does not copy what is live after it into each.
    around: Vec<Live>,
    /// The locals that are a constructor with no fields in the arm being walked,
    /// because its pattern says so.
    plain: BTreeSet<LocalId>,
}

impl Perceus<'_> {
    /// Whether the local gets a drop where it dies.
    fn gets_drop(&self, id: LocalId) -> Result<bool, InternalLinkError> {
        let ty = self.locals.get(id).copied();
        let ty = ty.ok_or(InternalLinkError::UntypedLocal(id))?;
        Ok(self.pool.gets_drop(ty)? && !self.plain.contains(&id))
    }

    fn is_around(&self, id: LocalId) -> bool {
        self.around.iter().any(|live| live.contains(&id))
    }

    /// Whether `id` is live where `live` is.
    fn is_live(&self, live: &Live, id: LocalId) -> bool {
        live.contains(&id) || self.is_around(id)
    }

    /// Make `id` live.
    fn read(&self, live: &mut Live, id: LocalId) {
        if !self.is_around(id) {
            live.insert(id);
        }
    }

    /// Transform `e` and return `(e', live)`, where `live` is the locals from
    /// outer scopes that `e'` reads, which the caller is responsible for.
    /// Drops are inserted for locals that die inside `e`. The spine is walked
    /// in a loop, so a long straight-line body, or a long chain of joins,
    /// doesn't recurse on the Rust stack.
    fn drop_pass(&mut self, mut e: CoreExpr) -> Result<(CoreExpr, Live), InternalLinkError> {
        let mut spine: Vec<Frame> = Vec::new();
        let end = loop {
            match peel(e) {
                Ok((frame, rest)) => {
                    spine.push(frame);
                    e = rest;
                }
                Err(end) => break end,
            }
        };
        let (mut out, mut live) = self.end(end)?;
        let mut reads = Vec::new();
        while let Some(frame) = spine.pop() {
            let frame = match frame {
                // These die here: operands whose last read is this right-hand
                // side, plus the bind itself if nothing reads it. They drop
                // straight after it, as early as Perceus allows.
                Frame::Let { bind, rhs } => {
                    if !live.remove(&bind.id) {
                        out = self.wrap_drops([bind.id], out)?;
                    }
                    operands(&rhs, &mut reads);
                    let dying = reads.iter().copied();
                    let dying = dying.filter(|x| !self.is_live(&live, *x));
                    out = self.wrap_drops(dying, out)?;
                    reads.iter().for_each(|x| self.read(&mut live, *x));
                    Frame::Let { bind, rhs }
                }
                // The slot has a reference of its own.
                Frame::SetGlobal { slot, value } => {
                    if !self.is_live(&live, value) {
                        out = self.wrap_drops([value], out)?;
                        live.insert(value);
                    }
                    Frame::SetGlobal { slot, value }
                }
                Frame::Join { id, params, body } => {
                    let dead = params.iter().map(|p| p.id);
                    out = self.wrap_drops(dead.filter(|p| !live.contains(p)), out)?;
                    params.iter().for_each(|p| {
                        live.remove(&p.id);
                    });
                    let body = Box::new(self.join_body(id, *body, &mut live)?);
                    Frame::Join { id, params, body }
                }
                Frame::Drop { local } => return Err(InternalLinkError::DropsGiven(local)),
            };
            out = frame.wrap(out);
        }
        Ok((out, live))
    }

    /// The body of the join `id`, whose continuation has `live` live where it
    /// starts. Leaves in `live` what is live where the body does.
    fn join_body(
        &mut self,
        id: JoinId,
        body: CoreExpr,
        live: &mut Live,
    ) -> Result<CoreExpr, InternalLinkError> {
        let entered = std::mem::take(live);
        let (body, in_body) = if self.reached.contains(&id) {
            self.entered.insert(id, Live::new());
            self.around.push(entered);
            let body = self.drop_pass(body)?;
            let around = self.around.pop();
            *live = around.ok_or(InternalLinkError::NotAround(id))?;
            body
        } else {
            self.entered.insert(id, entered);
            self.drop_pass(body)?
        };
        self.entered.remove(&id);
        live.extend(in_body);
        Ok(body)
    }

    /// [`Self::drop_pass`], of what a spine ends in.
    fn end(&mut self, end: End) -> Result<(CoreExpr, Live), InternalLinkError> {
        match end {
            End::Ret(value) => {
                let mut live = Live::new();
                self.read(&mut live, value);
                Ok((CoreExpr::Ret(value), live))
            }
            // The call gives up what it passes, so that needs no `Drop`. One
            // here would come before the read.
            End::TailCall { callee, args } => {
                let mut live = Live::new();
                callee
                    .local()
                    .into_iter()
                    .for_each(|f| self.read(&mut live, f));
                args.iter().for_each(|x| self.read(&mut live, *x));
                Ok((CoreExpr::TailCall { callee, args }, live))
            }
            // The jump hands over exactly what is live where the continuation
            // starts, and its arguments. Everything else is dropped before it,
            // so nothing is released both before a jump and after it, or on
            // neither side.
            End::Jump { to, args } => {
                let entered = self.entered.get(&to).cloned();
                let mut live = entered.ok_or(InternalLinkError::UnboundJoin(to))?;
                args.iter().for_each(|x| self.read(&mut live, *x));
                Ok((CoreExpr::Jump { to, args }, live))
            }
            End::If { cond, then, els } => self.drop_if(cond, *then, *els),
            End::Match { scrut, arms } => self.drop_match(scrut, arms),
        }
    }

    fn drop_if(
        &mut self,
        cond: LocalId,
        then: CoreExpr,
        els: CoreExpr,
    ) -> Result<(CoreExpr, Live), InternalLinkError> {
        let (then, mut live) = self.drop_pass(then)?;
        let (els, live_e) = self.drop_pass(els)?;
        // Both branches must release the same set, so the branch that does not
        // need a local drops it at entry.
        let then = self.wrap_drops(without(&live_e, &live), then)?;
        let els = self.wrap_drops(without(&live, &live_e), els)?;
        live.extend(live_e);
        self.read(&mut live, cond);
        let (then, els) = (Box::new(then), Box::new(els));
        Ok((CoreExpr::If { cond, then, els }, live))
    }

    fn drop_match(
        &mut self,
        scrut: LocalId,
        arms: Vec<(CorePat, CoreExpr)>,
    ) -> Result<(CoreExpr, Live), InternalLinkError> {
        struct Arm {
            pat: CorePat,
            body: CoreExpr,
            live: Live,
            dead_binders: Vec<LocalId>,
        }
        // The match reads `scrut`, and then each arm owns it. `scrut` is live
        // on entry to the Match, but the arms don't add it to `live_union`. It
        // drops at the head of each arm unless that arm reads it again.
        let mut live_union = Live::new();
        let mut lowered: Vec<Arm> = Vec::with_capacity(arms.len());
        for (pat, body) in arms {
            let (body, mut live) = self.in_arm(scrut, &pat, |cx| cx.drop_pass(body))?;
            let bound = pat.binds().map(|b| b.id);
            let dead_binders = bound.filter(|b| !live.remove(b)).collect();
            live_union.extend(live.iter().copied());
            lowered.push(Arm {
                pat,
                body,
                live,
                dead_binders,
            });
        }
        let scrut_dies = !self.is_live(&live_union, scrut);
        let arms = lowered
            .into_iter()
            .map(|a| {
                let others = without(&live_union, &a.live);
                let drops = others.chain(scrut_dies.then_some(scrut));
                let drops = drops.chain(a.dead_binders);
                let drops: Vec<LocalId> = drops.collect();
                let body = self.in_arm(scrut, &a.pat, |cx| cx.wrap_drops(drops, a.body))?;
                Ok::<_, InternalLinkError>((a.pat, body))
            })
            .collect::<Result<_, _>>()?;
        let mut live = live_union;
        self.read(&mut live, scrut);
        Ok((CoreExpr::Match { scrut, arms }, live))
    }

    /// Run `f` in an arm for values of `scrut` that fit `pat`.
    fn in_arm<T>(&mut self, scrut: LocalId, pat: &CorePat, f: impl FnOnce(&mut Self) -> T) -> T {
        let plain = matches!(pat, CorePat::Ctor { fields, .. } if fields.is_empty());
        let first = plain && self.plain.insert(scrut);
        let out = f(self);
        if first {
            self.plain.remove(&scrut);
        }
        out
    }

    /// Put `Drop{x}` in front of `body` for each `x` in `dead` that gets one,
    /// going in reverse so the printed order matches `dead`.
    fn wrap_drops(
        &self,
        dead: impl IntoIterator<Item = LocalId, IntoIter: DoubleEndedIterator>,
        mut body: CoreExpr,
    ) -> Result<CoreExpr, InternalLinkError> {
        for local in dead.into_iter().rev() {
            if self.gets_drop(local)? {
                let rest = Box::new(body);
                body = CoreExpr::Drop { local, body: rest };
            }
        }
        Ok(body)
    }
}

/// What is in `a` and not in `b`, in order.
fn without<'l>(a: &'l Live, b: &'l Live) -> impl DoubleEndedIterator<Item = LocalId> {
    a.iter().copied().filter(|x| !b.contains(x))
}

/// Make `reads` the locals that an atom reads, in order. A repeated operand, as in
/// `add(x, x)`, is counted once. The VM takes a reference for each operand it
/// passes on, so the count stays right.
fn operands(a: &Atom, reads: &mut Vec<LocalId>) {
    reads.clear();
    a.for_each_operand(|id| reads.push(id));
    reads.sort_unstable();
    reads.dedup();
}

#[cfg(test)]
mod tests;
