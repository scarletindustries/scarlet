//! Writes the decision tree of a `match` as Core IR.
//!
//! A value is read out of the matched one where a way through the tree first needs
//! it, and whatever comes after that on the way reads the same local. When an arm is
//! found, what its guard and its body read is read there, before anything else runs.
//! So what the matched value is in is never held for longer than the names are. An
//! arm that several ways come to is a join around the tree that takes its names, and
//! no body is lowered twice.
//!
//! Where the matched value is a probe's, the tree begins with the probe, and what it
//! finds is where the tree would read a field. The value is made only on a way that
//! reads it whole. A tree that does not ask for the constructor first has the value
//! made in front of it, or nothing, where no way through it reads any.

use std::collections::HashMap;

use crate::matching::{
    self, Branch, Cut, Fail, Given, Goal, Key, Leaf, Length, Node, On, Part, Piece, Place, Places,
    ROOT, Segment, Source, Tree, fixed,
};
use crate::program::InternalLinkError;
use crate::spine::Frame;
use crate::typed_ir::{
    RTy, TypedArm, TypedBinPatSeg, TypedBind, TypedExpr, TypedPat, each_read, pat_binds,
};
use crate::{Atom, Cases, CoreBind, CoreExpr, JoinId, LocalId, PrimOp, ProbeOp, VariantRef};

use super::probes::{Asked, Made};
use super::{Dest, Lower, R, Scrut, SpinePos};

/// Lower `arms` against `scrut`.
pub(super) fn lower(
    lo: &mut Lower<'_>,
    scrut: Scrut,
    arms: &[TypedArm],
    dest: Dest,
) -> R<CoreExpr> {
    let on = match &scrut {
        Scrut::Local { local, alone, name } => On {
            ty: lo.bind_ty(*local)?,
            alone: *alone,
            name: *name,
        },
        Scrut::Probe(asked) => On {
            ty: asked.ty,
            alone: true,
            name: None,
        },
    };
    let ty = on.ty;
    let given = Given {
        shapes: lo.around.shapes,
        consts: lo.around.consts,
        temps: lo.around.temps,
        reads: &lo.reads,
        allowance: lo.around.allowance,
    };
    let Tree {
        root,
        places,
        leaves,
        rests,
        anys,
    } = matching::tree(&given, on, arms)?;
    let (local, probe) = match scrut {
        Scrut::Local { local, .. } => (Some(local), None),
        Scrut::Probe(asked) => match asked.outcomes(&root) {
            Some(outcomes) => (None, Some((asked, outcomes))),
            None if root.reads() => {
                let made = lo.joined(ty, |lo, dest| lo.value_of(&asked, dest))?;
                (Some(made), None)
            }
            None => {
                lo.only_ask(&asked)?;
                (None, None)
            }
        },
    };
    let mut shared = Vec::new();
    let mut bodies = Vec::with_capacity(arms.len());
    for (arm, ways) in arms.iter().zip(&leaves) {
        bodies.push(match ways {
            0 | 1 => Body::Here,
            _ => {
                let (names, join) = share(lo, arm, local, dest)?;
                let id = join.id;
                shared.push(join);
                Body::Shared(id, names)
            }
        });
    }
    let mut locals = vec![None; places.len()];
    let root_local = locals.get_mut(ROOT.index());
    *root_local.ok_or(InternalLinkError::NoPlace(ROOT.index()))? = local;
    let mut write = Write {
        lo,
        arms,
        dest,
        places,
        locals,
        set: Vec::new(),
        bodies,
        after: vec![None; rests.len()],
        ways: rests,
        anys: vec![None; anys],
        whole: None,
    };
    let tree = write.branch(|w| match probe {
        None => w.node(&root),
        Some((asked, outcomes)) => w.probe(&asked, outcomes),
    })?;
    // Each is around all of the tree, so it is in scope wherever an arm is found.
    Ok(shared
        .into_iter()
        .fold(tree, |tree, shared| CoreExpr::Join {
            id: shared.id,
            params: shared.params,
            cont: Box::new(shared.body),
            body: Box::new(tree),
        }))
}

/// Where the body of an arm is.
enum Body {
    /// Where the one way through the tree comes to the arm.
    Here,
    /// In a join, which takes these names.
    Shared(JoinId, Vec<TypedBind>),
}

/// A join that is the body of an arm.
struct Shared {
    id: JoinId,
    params: Vec<CoreBind>,
    body: CoreExpr,
}

/// Lower the body of `arm` as a join, which takes the names that the body reads.
/// `scrut` is the matched value, when it is in scope there.
fn share(
    lo: &mut Lower<'_>,
    arm: &TypedArm,
    scrut: Option<LocalId>,
    dest: Dest,
) -> R<(Vec<TypedBind>, Shared)> {
    let id = lo.fresh_join()?;
    let mut in_guard: HashMap<usize, u32> = HashMap::new();
    if let Some(guard) = &arm.guard {
        each_read(guard, |b| {
            let n = in_guard.entry(b.0).or_default();
            *n = n.saturating_add(1);
        });
    }
    let mut names = pat_binds(&arm.pat);
    let guarded = |b: &TypedBind| in_guard.get(&b.id.0).copied().unwrap_or(0);
    names.retain(|b| lo.reads.get(b.id.0).is_none_or(|n| *n > guarded(b)));
    // A name for the matched value itself is in scope as it is.
    let whole = match &arm.pat {
        TypedPat::Bind(b) => scrut.map(|scrut| (*b, scrut)),
        TypedPat::Wild { .. }
        | TypedPat::Lit { .. }
        | TypedPat::Ctor { .. }
        | TypedPat::Tuple { .. }
        | TypedPat::Array { .. }
        | TypedPat::Bin { .. }
        | TypedPat::Or { .. }
        | TypedPat::Range { .. } => None,
    };
    names.retain(|b| whole.is_none_or(|(w, _)| w.id != b.id));
    let mut params = Vec::with_capacity(names.len());
    let body = lo.sealed(|lo| {
        for b in &names {
            let param = lo.fresh(b.ty)?;
            lo.bind(b.id, param)?;
            params.push(CoreBind::new(param, b.ty));
        }
        if let Some((b, scrut)) = whole {
            lo.bind(b.id, scrut)?;
        }
        lo.expr_tail(&arm.body, dest, SpinePos::Nested)
    })?;
    Ok((names, Shared { id, params, body }))
}

/// What a way through the tree does where nothing on it fits.
#[derive(Clone, Copy)]
enum After {
    Join(JoinId),
    Nothing,
}

/// The tree, as far as it is written.
struct Write<'w, 'a, 'p> {
    lo: &'w mut Lower<'a>,
    arms: &'p [TypedArm],
    dest: Dest,
    places: Places,
    /// The local that each place was read into, on this way through the tree.
    locals: Vec<Option<LocalId>>,
    /// Which of those were read, the last one last.
    set: Vec<Place>,
    bodies: Vec<Body>,
    /// Where the rows are that come after others.
    after: Vec<Option<After>>,
    /// How many places go on at each of those.
    ways: Vec<u32>,
    /// The join where each or-pattern's alternatives come together, and the names it
    /// takes.
    anys: Vec<Option<(JoinId, Vec<TypedBind>)>>,
    /// What the matched value is on this way, when it is a probe's and is not made.
    whole: Option<Whole>,
}

/// A value that a probe has come out as.
#[derive(Clone, Copy)]
struct Whole {
    variant: VariantRef,
    field: Held,
}

/// The one field of a [`Whole`].
#[derive(Clone, Copy)]
enum Held {
    Found(LocalId),
    Nil(RTy),
    Nothing,
}

/// What the tree does where a probe finds something, and where it does not.
struct Outcomes<'t, 'p> {
    hit: (VariantRef, Option<&'t Branch<'p>>),
    miss: (VariantRef, Option<&'t Branch<'p>>),
    /// Where either goes on that has no branch.
    default: Option<&'t Node<'p>>,
    nil: Option<RTy>,
}

impl Asked {
    /// What `root` does for each outcome, when it goes on by which of the two it is.
    fn outcomes<'t, 'p>(&self, root: &'t Node<'p>) -> Option<Outcomes<'t, 'p>> {
        let Made { hit, miss, nil } = self.made?;
        let Node::Switch {
            on,
            branches,
            default,
        } = root
        else {
            return None;
        };
        let of = |v| branches.iter().find(|b| b.keys.contains(&Key::Variant(v)));
        let apart = branches.iter().all(|b| b.keys.len() == 1);
        (*on == ROOT && apart).then_some(Outcomes {
            hit: (hit, of(hit)),
            miss: (miss, of(miss)),
            default: default.as_deref(),
            nil,
        })
    }
}

impl<'p> Write<'_, '_, 'p> {
    /// What `f` writes, as a branch of its own.
    fn branch(&mut self, f: impl FnOnce(&mut Self) -> R<CoreExpr>) -> R<CoreExpr> {
        let mark = self.set.len();
        let outer = self.lo.enter();
        let e = f(self);
        let segment = self.lo.leave(outer);
        // What was read in it is in scope no further.
        while self.set.len() > mark {
            let local = self.set.pop().and_then(|p| self.locals.get_mut(p.index()));
            if let Some(local) = local {
                *local = None;
            }
        }
        Ok(Frame::wrap_all(segment, e?))
    }

    fn give(&mut self, p: Place, local: LocalId) -> R<()> {
        let slot = self.locals.get_mut(p.index());
        *slot.ok_or(InternalLinkError::NoPlace(p.index()))? = Some(local);
        self.set.push(p);
        Ok(())
    }

    /// The local that the value of `p` is in, which is read here when nothing on this
    /// way has read it.
    fn place(&mut self, p: Place) -> R<LocalId> {
        if let Some(local) = self.locals.get(p.index()).copied().flatten() {
            return Ok(local);
        }
        let nowhere = InternalLinkError::NoPlace(p.index());
        let (source, ty) = self.known(p)?;
        if let Some(whole) = self.whole {
            let local = match source {
                Source::Given if p == ROOT => Some(self.made(whole, ty)?),
                Source::Part { of: ROOT, part } => {
                    let held = self.part(whole, part)?;
                    Some(held.ok_or(nowhere.clone())?)
                }
                Source::Given | Source::Part { .. } | Source::Found { .. } => None,
            };
            if let Some(local) = local {
                self.give(p, local)?;
                return Ok(local);
            }
        }
        let Source::Part { of, part } = source else {
            return Err(nowhere.into());
        };
        let whole = self.place(of)?;
        let rhs = match part {
            Part::Field { index, .. } => Atom::prim(PrimOp::Field(index), vec![whole]),
            Part::Element(i) => Atom::prim(PrimOp::TupleField(i), vec![whole]),
            Part::Length => Atom::prim(PrimOp::ArrayLen, vec![whole]),
            Part::Bits => Atom::prim(PrimOp::BinaryBitSize, vec![whole]),
            Part::Start(zero) => Atom::Const(zero),
            Part::From { cursor, total } => {
                let (cursor, total) = (self.place(cursor)?, self.place(total)?);
                let int_t = self.lo.around.temps.int;
                let left = Atom::prim(PrimOp::IntSub, vec![total, cursor]);
                let left = self.lo.let_(int_t, left)?;
                Atom::prim(PrimOp::BinaryView, vec![whole, cursor, left])
            }
        };
        let local = self.lo.let_(ty, rhs)?;
        self.give(p, local)?;
        Ok(local)
    }

    /// The value that a probe has come out as, of type `ty`.
    fn made(&mut self, whole: Whole, ty: RTy) -> R<LocalId> {
        let fields = self.held(whole)?.into_iter().collect();
        let variant = whole.variant;
        self.lo.let_(ty, Atom::Ctor { variant, fields })
    }

    /// What `part` of that value is, when it has one like that.
    fn part(&mut self, whole: Whole, part: Part) -> R<Option<LocalId>> {
        let field = Part::Field {
            variant: whole.variant.variant_idx,
            index: 0,
        };
        if part == field {
            self.held(whole)
        } else {
            Ok(None)
        }
    }

    /// The field of that value, when it has one.
    fn held(&mut self, whole: Whole) -> R<Option<LocalId>> {
        Ok(match whole.field {
            Held::Found(found) => Some(found),
            Held::Nil(ty) => Some(self.lo.let_(ty, Atom::Nil)?),
            Held::Nothing => None,
        })
    }

    /// The probe, and what comes after it in each case.
    fn probe(&mut self, asked: &Asked, outcomes: Outcomes<'_, 'p>) -> R<CoreExpr> {
        let Outcomes {
            hit,
            miss,
            default,
            nil,
        } = outcomes;
        let found = self.lo.found(asked)?;
        let held = nil.map_or(Held::Nothing, Held::Nil);
        let hit = self.outcome(hit, Held::Found(found), default)?;
        let miss = self.outcome(miss, held, default)?;
        self.whole = None;
        Ok(asked.probe(found, hit, miss))
    }

    /// What is done where the probe has come out as `variant`, which `branch` is for,
    /// or else `default`.
    fn outcome(
        &mut self,
        (variant, branch): (VariantRef, Option<&Branch<'p>>),
        field: Held,
        default: Option<&Node<'p>>,
    ) -> R<CoreExpr> {
        self.whole = Some(Whole { variant, field });
        self.branch(|w| match (branch, default) {
            (Some(branch), _) => w.case(branch),
            (None, Some(node)) => w.node(node),
            (None, None) => Ok(CoreExpr::NoArm),
        })
    }

    /// Bind each name to where its value is.
    fn names(&mut self, binds: &[(TypedBind, Place)]) -> R<()> {
        for (b, p) in binds {
            let local = self.place(*p)?;
            self.lo.bind(b.id, local)?;
        }
        Ok(())
    }

    /// Ask for pieces of an array, and go on with `hit` or with `miss`.
    fn cut(
        &mut self,
        cut: &Cut,
        hit: impl FnOnce(&mut Self) -> R<CoreExpr>,
        miss: impl FnOnce(&mut Self) -> R<CoreExpr>,
    ) -> R<CoreExpr> {
        for p in &cut.before {
            self.place(*p)?;
        }
        let xs = self.place(cut.on)?;
        let Length { n, or_more: more } = cut.length;
        let mut places = cut.elements.clone();
        let op = if (n, more) == (1, true) {
            places.resize(1, None);
            ProbeOp::ArrayUncons
        } else {
            ProbeOp::ArraySplit {
                len: n,
                more,
                from: cut.from,
            }
        };
        if more {
            places.push(cut.tail);
        }
        let mut binds = Vec::with_capacity(places.len());
        let mut found = Vec::new();
        for p in places {
            let bind = match p {
                Some(p) => {
                    let (_, ty) = self.known(p)?;
                    let local = self.lo.fresh(ty)?;
                    found.push((p, local));
                    Some(CoreBind::new(local, ty))
                }
                None => None,
            };
            binds.push(bind);
        }
        let hit = self.branch(|w| {
            for (p, local) in found {
                w.give(p, local)?;
            }
            hit(w)
        })?;
        let miss = self.branch(miss)?;
        Ok(CoreExpr::Probe {
            op,
            args: vec![xs],
            binds,
            hit: Box::new(hit),
            miss: Box::new(miss),
        })
    }

    fn known(&self, p: Place) -> R<(Source, RTy)> {
        let known = self.places.get(p);
        Ok(known.ok_or(InternalLinkError::NoPlace(p.index()))?)
    }

    /// What is done where nothing fits.
    fn fail(&self, fail: Fail) -> R<CoreExpr> {
        let Some(id) = fail else {
            return Ok(CoreExpr::NoArm);
        };
        let after = self.after.get(id.index()).copied().flatten();
        Ok(match after {
            Some(After::Join(to)) => CoreExpr::Jump {
                to,
                args: Vec::new(),
            },
            Some(After::Nothing) => CoreExpr::NoArm,
            None => return Err(InternalLinkError::NoRest(id.index()).into()),
        })
    }

    fn node(&mut self, node: &Node<'p>) -> R<CoreExpr> {
        match node {
            Node::Nothing => Ok(CoreExpr::NoArm),
            Node::Rest(id) => self.fail(Some(*id)),
            Node::Leaf(leaf) => self.leaf(leaf),
            Node::Switch {
                on,
                branches,
                default,
            } => self.switch(*on, branches, default.as_deref()),
            Node::Cut { cut, yes, no } => self.cut(cut, |w| w.node(yes), |w| w.node(no)),
            Node::Measure { length, then } => {
                self.place(*length)?;
                self.node(then)
            }
            Node::Within {
                on,
                range: [lo, hi],
                yes,
                fail,
            } => {
                let on = self.place(*on)?;
                let lo = self.lo.int_const(*lo)?;
                let above = self.bool(PrimOp::IntGe, on, lo)?;
                let then = self.branch(|w| {
                    let hi = w.lo.int_const(*hi)?;
                    let below = w.bool(PrimOp::IntLt, on, hi)?;
                    let yes = w.branch(|w| w.node(yes))?;
                    let no = w.fail(*fail)?;
                    Ok(check(below, yes, no))
                })?;
                let no = self.fail(*fail)?;
                Ok(check(above, then, no))
            }
            Node::Segment(segment) => self.segment(segment),
            Node::Ends {
                total,
                cursor,
                yes,
                fail,
            } => {
                let (cursor, total) = (self.place(*cursor)?, self.place(*total)?);
                let ends = self.bool(PrimOp::IntEq, cursor, total)?;
                let yes = self.branch(|w| w.node(yes))?;
                let no = self.fail(*fail)?;
                Ok(check(ends, yes, no))
            }
            Node::Any {
                id,
                binds,
                names,
                first,
                then,
            } => {
                self.names(binds)?;
                let join = self.lo.fresh_join()?;
                let any = self.anys.get_mut(id.index());
                *any.ok_or(InternalLinkError::NoAny(id.index()))? =
                    Some((join, names.iter().map(|(b, _)| *b).collect()));
                let body = Box::new(self.branch(|w| w.node(first))?);
                let mut params = Vec::with_capacity(names.len());
                for (b, p) in names {
                    let param = self.lo.fresh(b.ty)?;
                    params.push(CoreBind::new(param, b.ty));
                    self.give(*p, param)?;
                }
                self.lo.spine.push(Frame::Join {
                    id: join,
                    params,
                    body,
                });
                self.node(then)
            }
            Node::Chain { links, last } => self.chain(links, last),
        }
    }

    /// `op` of two Ints, which is a Bool.
    fn bool(&mut self, op: PrimOp, a: LocalId, b: LocalId) -> R<LocalId> {
        let ty = self.lo.around.temps.bool;
        self.lo.let_(ty, Atom::prim(op, vec![a, b]))
    }

    fn int(&mut self, op: PrimOp, a: LocalId, b: LocalId) -> R<LocalId> {
        let ty = self.lo.around.temps.int;
        self.lo.let_(ty, Atom::prim(op, vec![a, b]))
    }

    /// Each link is the body of a join, and what comes after it is the continuation.
    fn chain(&mut self, links: &[(matching::RestId, Node<'p>)], last: &Node<'p>) -> R<CoreExpr> {
        for (i, (id, node)) in links.iter().enumerate() {
            if self.ways.get(id.index()) == Some(&0) {
                return self.node(node);
            }
            // Where nothing is left to try there is nothing to declare.
            let at_end = i.saturating_add(1) == links.len();
            let passed_on = match last {
                Node::Nothing if at_end => Some(After::Nothing),
                Node::Rest(on) if at_end => self.after.get(on.index()).copied().flatten(),
                Node::Nothing
                | Node::Rest(_)
                | Node::Leaf(_)
                | Node::Switch { .. }
                | Node::Cut { .. }
                | Node::Measure { .. }
                | Node::Within { .. }
                | Node::Segment(_)
                | Node::Ends { .. }
                | Node::Any { .. }
                | Node::Chain { .. } => None,
            };
            if passed_on.is_some() {
                self.goes_on(*id, passed_on)?;
                return self.node(node);
            }
            // What every way through the link reads first is read in front of it, so
            // that what comes after it can read that too.
            match node {
                Node::Leaf(leaf) => self.names(&leaf.binds)?,
                Node::Switch { on: p, .. }
                | Node::Within { on: p, .. }
                | Node::Measure { length: p, .. } => {
                    self.place(*p)?;
                }
                Node::Cut { cut, .. } => {
                    self.place(cut.on)?;
                }
                Node::Nothing
                | Node::Rest(_)
                | Node::Segment(_)
                | Node::Ends { .. }
                | Node::Any { .. }
                | Node::Chain { .. } => {}
            }
            let join = self.lo.fresh_join()?;
            self.goes_on(*id, Some(After::Join(join)))?;
            let body = Box::new(self.branch(|w| w.node(node))?);
            self.lo.spine.push(Frame::Join {
                id: join,
                params: Vec::new(),
                body,
            });
        }
        self.node(last)
    }

    fn goes_on(&mut self, id: matching::RestId, after: Option<After>) -> R<()> {
        let slot = self.after.get_mut(id.index());
        *slot.ok_or(InternalLinkError::NoRest(id.index()))? = after;
        Ok(())
    }

    fn leaf(&mut self, leaf: &Leaf) -> R<CoreExpr> {
        self.names(&leaf.binds)?;
        let i = match leaf.goal {
            Goal::Arm(i) => i,
            Goal::Any(id) => {
                let join = self.anys.get(id.index()).and_then(Option::as_ref);
                let (to, names) = join.ok_or(InternalLinkError::NoAny(id.index()))?;
                return self.jump(*to, names);
            }
        };
        let arms = self.arms;
        let arm = arms.get(i).ok_or(InternalLinkError::NoMatchArm(i))?;
        let Some(guard) = &arm.guard else {
            return self.body(i, &arm.body);
        };
        let holds = self.lo.operand(guard)?;
        let body = self.branch(|w| w.tails(&leaf.late, i, &arm.body))?;
        let no = self.fail(leaf.fail)?;
        Ok(check(holds, body, no))
    }

    /// The body of arm `i`, once the tails in `late` are taken.
    fn tails(&mut self, late: &[(TypedBind, Place)], i: usize, body: &TypedExpr) -> R<CoreExpr> {
        let Some(((b, p), late)) = late.split_first() else {
            return self.body(i, body);
        };
        let (
            Source::Found {
                of,
                piece: Piece::Tail(n),
            },
            _,
        ) = self.known(*p)?
        else {
            return Err(InternalLinkError::NoPlace(p.index()).into());
        };
        let cut = Cut {
            on: of,
            before: Vec::new(),
            length: Length { n, or_more: true },
            from: 0,
            elements: Vec::new(),
            tail: Some(*p),
        };
        let hit = |w: &mut Self| {
            let local = w.place(*p)?;
            w.lo.bind(b.id, local)?;
            w.tails(late, i, body)
        };
        self.cut(&cut, hit, |_| Ok(CoreExpr::NoArm))
    }

    /// The body of arm `i`, where its pattern has matched and its guard held.
    fn body(&mut self, i: usize, body: &TypedExpr) -> R<CoreExpr> {
        match self.bodies.get(i) {
            Some(Body::Shared(to, names)) => self.jump(*to, names),
            Some(Body::Here) => self.lo.expr_tail(body, self.dest, SpinePos::Nested),
            None => Err(InternalLinkError::NoMatchArm(i).into()),
        }
    }

    /// Go on at a join that takes `names`.
    fn jump(&self, to: JoinId, names: &[TypedBind]) -> R<CoreExpr> {
        let args = names.iter().map(|b| self.lo.local(b.id));
        let args = args.collect::<R<_>>()?;
        Ok(CoreExpr::Jump { to, args })
    }

    fn switch(
        &mut self,
        on: Place,
        branches: &[Branch<'p>],
        default: Option<&Node<'p>>,
    ) -> R<CoreExpr> {
        let scrut = self.place(on)?;
        let mut arms = Vec::with_capacity(branches.len());
        for branch in branches {
            let body = self.branch(|w| w.case(branch))?;
            arms.push((branch.keys.as_slice(), body));
        }
        let default = match default {
            Some(node) => Some(Box::new(self.branch(|w| w.node(node))?)),
            None => None,
        };
        Ok(CoreExpr::Switch {
            scrut,
            cases: cases(arms)?,
            default,
        })
    }

    fn case(&mut self, branch: &Branch<'p>) -> R<CoreExpr> {
        for p in &branch.apart {
            self.place(*p)?;
        }
        self.node(&branch.node)
    }

    fn segment(&mut self, segment: &Segment<'p>) -> R<CoreExpr> {
        let Segment {
            on,
            cursor,
            total,
            zero,
            seg,
            value,
            next,
            yes,
            fail,
            ..
        } = segment;
        let bin = self.place(*on)?;
        let at = self.place(*cursor)?;
        let ty = self.lo.bind_ty(bin)?;
        let int_t = self.lo.around.temps.int;
        match seg {
            // A `<<'literal'>>` segment matches its UTF-8 bytes as a prefix.
            TypedBinPatSeg::Utf8Literal { bytes, bits } => {
                let prefix = self.lo.let_(ty, Atom::Const(*bytes))?;
                let bool_t = self.lo.around.temps.bool;
                let matched = Atom::prim(PrimOp::BinaryMatchPrefix, vec![bin, at, prefix]);
                let matched = self.lo.let_(bool_t, matched)?;
                let yes = self.branch(|w| {
                    let width = w.lo.int_const(*bits)?;
                    let end = w.int(PrimOp::IntAdd, at, width)?;
                    w.give(*next, end)?;
                    w.node(yes)
                })?;
                let no = self.fail(*fail)?;
                Ok(check(matched, yes, no))
            }
            // A `:utf8` codepoint, whose width varies. `BinaryReadUtf8` gives back
            // `(codepoint, nbits)`, where `nbits == 0` means decoding failed.
            TypedBinPatSeg::Utf8 { .. } => {
                let pair_ty = self.lo.around.temps.int_pair;
                let read = Atom::prim(PrimOp::BinaryReadUtf8, vec![bin, at]);
                let pair = self.lo.let_(pair_ty, read)?;
                let code = Atom::prim(PrimOp::TupleField(0), vec![pair]);
                let code = self.lo.let_(int_t, code)?;
                let width = Atom::prim(PrimOp::TupleField(1), vec![pair]);
                let width = self.lo.let_(int_t, width)?;
                let none = self.lo.int_const(*zero)?;
                let decoded = self.bool(PrimOp::IntGt, width, none)?;
                let yes = self.branch(|w| {
                    let end = w.int(PrimOp::IntAdd, at, width)?;
                    w.give(*next, end)?;
                    if let Some(value) = value {
                        w.give(*value, code)?;
                    }
                    w.node(yes)
                })?;
                let no = self.fail(*fail)?;
                Ok(check(decoded, yes, no))
            }
            // An Int or Binary segment, whose width is the size expression in
            // bits. A `:binary` segment with no size takes the rest.
            TypedBinPatSeg::Int { bits, .. } => {
                let read = |w: &mut Self, width, end| {
                    w.read(segment, PrimOp::BinaryReadInt, int_t, width, end)
                };
                self.sized(segment, bits, at, read)
            }
            TypedBinPatSeg::Binary {
                bits: Some(bits), ..
            } => {
                let read =
                    |w: &mut Self, width, end| w.read(segment, PrimOp::BinaryView, ty, width, end);
                self.sized(segment, bits, at, read)
            }
            TypedBinPatSeg::Binary { bits: None, .. } => {
                let total = self.place(*total)?;
                let width = self.int(PrimOp::IntSub, total, at)?;
                let end = self.int(PrimOp::IntAdd, at, width)?;
                self.read(segment, PrimOp::BinaryView, ty, width, end)
            }
        }
    }

    /// Work out how wide a segment at `at` is, check that the binary has as much, and
    /// go on with `read`. A negative width matches nothing, like a read past the end.
    fn sized(
        &mut self,
        segment: &Segment<'p>,
        bits: &TypedExpr,
        at: LocalId,
        read: impl FnOnce(&mut Self, LocalId, LocalId) -> R<CoreExpr>,
    ) -> R<CoreExpr> {
        let Segment {
            total,
            zero,
            binds,
            fail,
            ..
        } = segment;
        // Where the size is worked out, every name in front of it is bound. What they
        // are read out of is then given up before anything is called.
        if !fixed(bits) {
            self.names(binds)?;
        }
        let total = self.place(*total)?;
        let width = self.lo.operand(bits)?;
        let none = self.lo.int_const(*zero)?;
        let sized = self.bool(PrimOp::IntGe, width, none)?;
        let then = self.branch(|w| {
            let end = w.int(PrimOp::IntAdd, at, width)?;
            let fits = w.bool(PrimOp::IntLe, end, total)?;
            let yes = w.branch(|w| read(w, width, end))?;
            let no = w.fail(*fail)?;
            Ok(check(fits, yes, no))
        })?;
        let no = self.fail(*fail)?;
        Ok(check(sized, then, no))
    }

    /// Read what a segment is `width` bits of, as `op` does, and go on from `end`.
    fn read(
        &mut self,
        segment: &Segment<'p>,
        op: PrimOp,
        ty: RTy,
        width: LocalId,
        end: LocalId,
    ) -> R<CoreExpr> {
        let (bin, at) = (self.place(segment.on)?, self.place(segment.cursor)?);
        self.give(segment.next, end)?;
        if let Some(value) = segment.value {
            let read = self.lo.let_(ty, Atom::prim(op, vec![bin, at, width]))?;
            self.give(value, read)?;
        }
        self.node(&segment.yes)
    }
}

/// `if cond { then } else { .. }`, where the else is for what does not fit.
fn check(cond: LocalId, then: CoreExpr, els: CoreExpr) -> CoreExpr {
    CoreExpr::If {
        cond,
        then: Box::new(then),
        els: Box::new(els),
    }
}

/// The cases that `arms` are, whose keys are all of one kind.
fn cases(arms: Vec<(&[Key], CoreExpr)>) -> R<Cases> {
    let first = arms.first().and_then(|(keys, _)| keys.first()).copied();
    Ok(match first {
        Some(Key::Variant(v)) => Cases::Variant {
            ty: v.type_id,
            arms: keyed(arms, |key| match key {
                Key::Variant(of) if of.type_id == v.type_id => Some(of.variant_idx),
                Key::Variant(_) | Key::Int(_) | Key::Float(_) | Key::Text(_) => None,
            })?,
        },
        Some(Key::Int(_)) => Cases::Int(keyed(arms, |key| match key {
            Key::Int(n) => Some(n),
            Key::Variant(_) | Key::Float(_) | Key::Text(_) => None,
        })?),
        Some(Key::Float(_)) => Cases::Float(keyed(arms, |key| match key {
            Key::Float(x) => Some(x),
            Key::Variant(_) | Key::Int(_) | Key::Text(_) => None,
        })?),
        Some(Key::Text(_)) => Cases::Text(keyed(arms, |key| match key {
            Key::Text(c) => Some(c),
            Key::Variant(_) | Key::Int(_) | Key::Float(_) => None,
        })?),
        None => return Err(InternalLinkError::OddRow.into()),
    })
}

/// `arms`, with each key as `of` has it.
fn keyed<K>(
    arms: Vec<(&[Key], CoreExpr)>,
    of: impl Fn(Key) -> Option<K>,
) -> R<Vec<(Vec<K>, CoreExpr)>> {
    let mut out = Vec::with_capacity(arms.len());
    for (keys, body) in arms {
        let keys: Option<Vec<K>> = keys.iter().map(|key| of(*key)).collect();
        out.push((keys.ok_or(InternalLinkError::OddPattern)?, body));
    }
    Ok(out)
}
