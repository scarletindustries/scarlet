//! Perceus is the pass that adds reference counting to Core IR. It decides
//! where each heap value is released. This is the dropping half of Lorenzen &
//! Leijen (ICFP'22, Fig. 5), run as a pass from Core IR to Core IR. In ANF,
//! finding a value's last use is one backward scan.
//!
//! A backward liveness walk inserts a `Drop` after each heap bind's last read.
//! Where `If` and `Switch` branches split, ownership is evened out so every path
//! releases every owned slot exactly once (Fig. 5's Δ-rule). A return gives up
//! the value it returns, a tail call its arguments, and a jump its arguments
//! and what the continuation it goes to owns.
//!
//! Where a run of field reads is the last use of a constructor, and the case it
//! is in says which constructor that is, the reads and the drop behind them are
//! one [`CoreExpr::Open`]. A backend then need not count a reference for each
//! field that the cell is about to give up.

use std::collections::{BTreeMap, BTreeSet};

use super::spine::{End, Frame, peel};
use super::{Atom, Cases, CoreBind, CoreExpr, CoreFn, JoinId, LocalId, PrimOp, VariantRef};
use crate::program::InternalLinkError;
use crate::typed_ir::{RTy, ResolvedPool, Shape, Shapes};
use scarlet_ir::tivec::TiVec;

/// Run Perceus on one lowered function. `locals` is the type of every local
/// `lower` made for it, `pool` says which of those types can be cells, `shapes`
/// which of their constructors are, and `reached` which joins are
/// [always reached](CoreExpr::always_reached).
pub(crate) fn perceus(
    pool: &ResolvedPool,
    shapes: &Shapes,
    locals: &TiVec<LocalId, RTy>,
    reached: &BTreeSet<JoinId>,
    f: CoreFn,
) -> Result<CoreFn, InternalLinkError> {
    let mut cx = Perceus {
        pool,
        shapes,
        locals,
        reached,
        entered: BTreeMap::new(),
        around: Vec::new(),
        plain: BTreeSet::new(),
        known: BTreeMap::new(),
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
    shapes: &'p Shapes,
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
    /// The locals that are a constructor with no fields in the case being walked.
    plain: BTreeSet<LocalId>,
    /// The locals that are this constructor there, which has fields.
    known: BTreeMap<LocalId, VariantRef>,
}

/// What a case says of the value it is for.
#[derive(Clone, Copy)]
enum Says {
    /// It is a constructor with no fields.
    Plain,
    /// It is this one, which has fields.
    Is(VariantRef),
    Nothing,
}

/// A read of a field of a cell whose constructor is known.
struct Read {
    bind: CoreBind,
    cell: LocalId,
    variant: VariantRef,
    field: u16,
}

impl Read {
    fn frame(self) -> Frame {
        Frame::Let {
            bind: self.bind,
            rhs: Atom::prim(PrimOp::Field(self.field), vec![self.cell]),
        }
    }
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
            let run = match self.part_read(frame) {
                Ok(last) => self.opens(last, &mut spine, &live)?,
                Err(frame) => vec![frame],
            };
            for frame in run.into_iter().rev() {
                out = self.frame(frame, out, &mut live, &mut reads)?;
            }
        }
        Ok((out, live))
    }

    /// `frame`, in front of `out`, where `live` is live. Leaves in `live` what is
    /// live in front of it. `reads` is space to work in.
    fn frame(
        &mut self,
        frame: Frame,
        mut out: CoreExpr,
        live: &mut Live,
        reads: &mut Vec<LocalId>,
    ) -> Result<CoreExpr, InternalLinkError> {
        let frame = match frame {
            // These die here: operands whose last read is this right-hand
            // side, plus the bind itself if nothing reads it. They drop
            // straight after it, as early as Perceus allows.
            Frame::Let { bind, rhs } => {
                if !live.remove(&bind.id) {
                    out = self.wrap_drops([bind.id], out)?;
                }
                operands(&rhs, reads);
                let dying = reads.iter().copied();
                let dying = dying.filter(|x| !self.is_live(live, *x));
                out = self.wrap_drops(dying, out)?;
                reads.iter().for_each(|x| self.read(live, *x));
                Frame::Let { bind, rhs }
            }
            // The slot has a reference of its own.
            Frame::SetGlobal { slot, value } => {
                if !self.is_live(live, value) {
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
                let body = Box::new(self.join_body(id, *body, live)?);
                Frame::Join { id, params, body }
            }
            // A field that nothing reads goes with the cell.
            Frame::Open {
                cell,
                variant,
                mut fields,
            } => {
                fields.retain(|(_, bind)| live.remove(&bind.id));
                self.read(live, cell);
                if fields.is_empty() {
                    return self.wrap_drops([cell], out);
                }
                Frame::Open {
                    cell,
                    variant,
                    fields,
                }
            }
            Frame::Drop { local } => return Err(InternalLinkError::DropsGiven(local)),
        };
        Ok(frame.wrap(out))
    }

    /// The read of a field that `frame` is, when it is one of a cell whose
    /// constructor is known.
    fn part_read(&self, frame: Frame) -> Result<Read, Frame> {
        if let Frame::Let {
            bind,
            rhs:
                Atom::PrimOp {
                    op: PrimOp::Field(field),
                    ref args,
                },
        } = frame
            && let [cell] = args.as_slice()
            && let Some(variant) = self.known.get(cell)
        {
            return Ok(Read {
                bind,
                cell: *cell,
                variant: *variant,
                field,
            });
        }
        Err(frame)
    }

    /// The run of such reads that ends in `last` and that `spine` ends in, with one
    /// [`Frame::Open`] for those of each cell that dies in it. What is live after
    /// it is in `live`. The reads of a cell come together where its first is, which
    /// nothing can tell, because reads are all there is in the run.
    fn opens(
        &self,
        last: Read,
        spine: &mut Vec<Frame>,
        live: &Live,
    ) -> Result<Vec<Frame>, InternalLinkError> {
        let mut run = vec![last];
        while let Some(frame) = spine.pop() {
            match self.part_read(frame) {
                Ok(read) => run.push(read),
                Err(frame) => {
                    spine.push(frame);
                    break;
                }
            }
        }
        let mut frames: Vec<Frame> = Vec::with_capacity(run.len());
        // Where the frame is that opens each cell that dies.
        let mut opened: BTreeMap<LocalId, usize> = BTreeMap::new();
        for read in run.into_iter().rev() {
            let Read {
                bind,
                cell,
                variant,
                field,
            } = read;
            if self.is_live(live, cell) {
                frames.push(read.frame());
                continue;
            }
            let at = *opened.entry(cell).or_insert(frames.len());
            let Some(Frame::Open { fields, .. }) = frames.get_mut(at) else {
                frames.push(Frame::Open {
                    cell,
                    variant,
                    fields: vec![(field, bind)],
                });
                continue;
            };
            let place = fields.partition_point(|(f, _)| *f < field);
            match fields.get(place) {
                // A second read of a field is another name for it.
                Some((f, first)) if *f == field => {
                    let rhs = Atom::Local(first.id);
                    frames.push(Frame::Let { bind, rhs });
                }
                Some(_) | None => fields.insert(place, (field, bind)),
            }
        }
        Ok(frames)
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
            End::Switch {
                scrut,
                cases,
                default,
            } => self.drop_switch(scrut, cases, default),
            End::NoArm => Ok((CoreExpr::NoArm, Live::new())),
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
        let then = self.drops_at_head(without(&live_e, &live), then)?;
        let els = self.drops_at_head(without(&live, &live_e), els)?;
        live.extend(live_e);
        self.read(&mut live, cond);
        let (then, els) = (Box::new(then), Box::new(els));
        Ok((CoreExpr::If { cond, then, els }, live))
    }

    fn drop_switch(
        &mut self,
        scrut: LocalId,
        mut cases: Cases,
        mut default: Option<Box<CoreExpr>>,
    ) -> Result<(CoreExpr, Live), InternalLinkError> {
        struct Case {
            says: Says,
            body: CoreExpr,
            live: Live,
        }
        // The switch reads `scrut`, and then each case owns it. One that does not
        // read it again drops it at its head.
        let says = self.says(&cases);
        let bodies = cases.arms_mut().into_iter().chain(default.as_deref_mut());
        let bodies = bodies.map(|body| std::mem::replace(body, CoreExpr::NoArm));
        let bodies: Vec<CoreExpr> = bodies.collect();
        let mut live_union = Live::new();
        let mut walked = Vec::with_capacity(bodies.len());
        for (body, says) in bodies.into_iter().zip(says) {
            let (body, live) = self.in_case(scrut, says, |cx| cx.drop_pass(body))?;
            live_union.extend(live.iter().copied());
            walked.push(Case { says, body, live });
        }
        let scrut_dies = !self.is_live(&live_union, scrut);
        let arms = cases.arms_mut().into_iter().chain(default.as_deref_mut());
        for (arm, case) in arms.zip(walked) {
            let Case { says, body, live } = case;
            let drops = without(&live_union, &live).chain(scrut_dies.then_some(scrut));
            *arm = self.in_case(scrut, says, |cx| cx.drops_at_head(drops, body))?;
        }
        let mut live = live_union;
        self.read(&mut live, scrut);
        let switch = CoreExpr::Switch {
            scrut,
            cases,
            default,
        };
        Ok((switch, live))
    }

    /// What each case says of the value it is for, and last what there being no
    /// case for it says.
    fn says(&self, cases: &Cases) -> Vec<Says> {
        let (Cases::Variant { ty, arms }, Some(shape)) = (cases, self.shape(cases)) else {
            let nothing = |_| Says::Nothing;
            return (0..=cases.arms().len()).map(nothing).collect();
        };
        let mut left = shape.with_fields();
        let mut all = Vec::with_capacity(arms.len().saturating_add(1));
        for (variants, _) in arms {
            let has_fields = |v: &&u16| shape.fields(**v).is_none_or(|n| n > 0);
            let fielded = variants.iter().filter(has_fields).count();
            left = left.saturating_sub(fielded);
            all.push(match variants.as_slice() {
                [v] if fielded == 1 => Says::Is(VariantRef {
                    type_id: *ty,
                    variant_idx: *v,
                }),
                _ if fielded == 0 => Says::Plain,
                _ => Says::Nothing,
            });
        }
        all.push(if left == 0 {
            Says::Plain
        } else {
            Says::Nothing
        });
        all
    }

    /// The shape of the type that `cases` are of, when they are of constructors.
    fn shape(&self, cases: &Cases) -> Option<&Shape> {
        match cases {
            Cases::Variant { ty, .. } => self.shapes.of(*ty),
            Cases::Int(_) | Cases::Float(_) | Cases::Text(_) => None,
        }
    }

    /// Run `f` in a case for a value that is `scrut`, of which the case says `says`.
    fn in_case<T>(&mut self, scrut: LocalId, says: Says, f: impl FnOnce(&mut Self) -> T) -> T {
        let outer = (self.plain.contains(&scrut), self.known.get(&scrut).copied());
        match says {
            Says::Plain => self.say(scrut, true, None),
            Says::Is(variant) => self.say(scrut, false, Some(variant)),
            Says::Nothing => {}
        }
        let out = f(self);
        let (plain, known) = outer;
        self.say(scrut, plain, known);
        out
    }

    /// Note whether `l` is a constructor with no fields, and which with fields.
    fn say(&mut self, l: LocalId, plain: bool, known: Option<VariantRef>) {
        match plain {
            true => self.plain.insert(l),
            false => self.plain.remove(&l),
        };
        match known {
            Some(variant) => self.known.insert(l, variant),
            None => self.known.remove(&l),
        };
    }

    /// [`Self::wrap_drops`], at the head of a branch. One that stops the run gives
    /// up nothing.
    fn drops_at_head(
        &self,
        dead: impl IntoIterator<Item = LocalId, IntoIter: DoubleEndedIterator>,
        body: CoreExpr,
    ) -> Result<CoreExpr, InternalLinkError> {
        if matches!(body, CoreExpr::NoArm) {
            return Ok(body);
        }
        self.wrap_drops(dead, body)
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
