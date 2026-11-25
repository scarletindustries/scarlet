//! Long literals made a piece at a time, for a function that has more values
//! at once than a frame has registers.
//!
//! Core IR works out every element of an array literal and then makes the
//! array, so each element needs a register until the last is ready. A table
//! of 300 numbers would not fit. Here the array is made of its first few
//! elements as soon as those are ready, and the rest are put on its end as
//! they come. That is how Lua builds a long table. The elements after a spread
//! are put on its end a piece at a time too, and those before one are made into
//! an array of their own, which is then joined to it. An interpolation and a
//! binary literal are joined a piece at a time in the same way.

use std::collections::HashMap;

use scarlet_ir::core_ir::{Atom, CoreBind, CoreExpr, CoreFn, CorePat, JoinId, LocalId, PrimOp};
use scarlet_ir::rty::RTy;

/// How many elements a piece has. It is what one leaf of an array holds.
const PIECE: usize = 32;

/// `f` with every literal of more than one piece made in pieces, or `None`
/// when it has none.
pub(super) fn in_pieces(f: &CoreFn) -> Option<CoreFn> {
    let mut names = Locals::default();
    f.params.iter().for_each(|p| names.name(p.id));
    names.walk(&f.body);
    let mut cut = Cut { names, pieces: 0 };
    let body = cut.expr(&f.body, f.ret_ty);
    (cut.pieces > 0).then(|| CoreFn {
        params: f.params.clone(),
        body,
        ret_ty: f.ret_ty,
    })
}

/// A literal of more than one piece.
struct Literal<'a> {
    /// What its elements go on the end of, when it spells that.
    onto: Option<LocalId>,
    elements: &'a [LocalId],
    /// What comes after its elements, when it spells that.
    before: Option<LocalId>,
    /// What makes the first piece, when there is nothing to put it on.
    first: PrimOp,
    /// What puts a piece on the end of those before it.
    next: PrimOp,
}

impl<'a> Literal<'a> {
    fn of(rhs: &'a Atom) -> Option<Literal<'a>> {
        let Atom::PrimOp { op, args } = rhs else {
            return None;
        };
        let array = |onto, elements, before| Literal {
            onto,
            elements,
            before,
            first: PrimOp::MakeArray,
            next: PrimOp::ArrayAppend,
        };
        let literal = if *op == PrimOp::MakeArray {
            array(None, args, None)
        } else if *op == PrimOp::ArrayAppend {
            let (onto, elements) = args.split_first()?;
            array(Some(*onto), elements, None)
        } else if *op == PrimOp::ArrayPrepend {
            let (before, elements) = args.split_last()?;
            array(None, elements, Some(*before))
        } else if *op == PrimOp::StringConcatMany || *op == PrimOp::BinaryConcatMany {
            Literal {
                onto: None,
                elements: args,
                before: None,
                first: *op,
                next: *op,
            }
        } else {
            return None;
        };
        (literal.elements.len() > PIECE).then_some(literal)
    }

    /// Whether putting a piece on the end copies those before it. An array's
    /// become the new one instead.
    fn copies(&self) -> bool {
        self.next != PrimOp::ArrayAppend
    }

    /// What makes `piece`, on the end of `so_far`.
    fn piece(&self, so_far: Option<LocalId>, piece: &[LocalId]) -> Atom {
        match so_far {
            None => Atom::prim(self.first, piece.to_vec()),
            Some(l) => {
                let args = std::iter::once(l).chain(piece.iter().copied());
                Atom::prim(self.next, args.collect())
            }
        }
    }
}

/// Every local a function names, and how often.
#[derive(Default)]
struct Locals {
    highest: u32,
    /// How often each is read or dropped.
    times: HashMap<LocalId, u32>,
}

impl Locals {
    fn name(&mut self, l: LocalId) {
        self.highest = self.highest.max(l.0);
    }

    fn read(&mut self, l: LocalId) {
        let n = self.times.entry(l).or_default();
        *n = n.saturating_add(1);
    }

    fn atom(&mut self, atom: &Atom) {
        atom.for_each_operand(|l| self.read(l));
    }

    fn walk(&mut self, e: &CoreExpr) {
        let mut todo = vec![e];
        while let Some(e) = todo.pop() {
            match e {
                CoreExpr::Let { bind, rhs, body } => {
                    self.name(bind.id);
                    self.atom(rhs);
                    todo.push(body);
                }
                CoreExpr::LetJoin { bind, join, body } => {
                    self.name(bind.id);
                    todo.extend([&**join, &**body]);
                }
                CoreExpr::LetCont { cont, body, .. } => todo.extend([&**cont, &**body]),
                CoreExpr::Drop { local, body, .. } => {
                    self.read(*local);
                    todo.push(body);
                }
                CoreExpr::Match { scrut, arms, .. } => {
                    self.read(*scrut);
                    for (pat, arm) in arms {
                        pat.binds().for_each(|b| self.name(b.id));
                        todo.push(arm);
                    }
                }
                CoreExpr::If {
                    cond, then, els, ..
                } => {
                    self.read(*cond);
                    todo.extend([&**then, &**els]);
                }
                CoreExpr::Tail(atom) => self.atom(atom),
                CoreExpr::Goto(_) => {}
            }
        }
    }
}

/// One step of a straight run of code.
enum Step {
    Let(CoreBind, Atom),
    Join(CoreBind, CoreExpr),
    Cont(JoinId, CoreExpr),
    Drop(LocalId),
}

impl Step {
    fn before(self, body: CoreExpr) -> CoreExpr {
        let body = Box::new(body);
        match self {
            Step::Let(bind, rhs) => CoreExpr::Let { bind, rhs, body },
            Step::Join(bind, join) => CoreExpr::LetJoin {
                bind,
                join: Box::new(join),
                body,
            },
            Step::Cont(id, cont) => CoreExpr::LetCont {
                id,
                cont: Box::new(cont),
                body,
            },
            Step::Drop(local) => CoreExpr::Drop { local, body },
        }
    }

    /// The local this gives a value.
    fn binds(&self) -> Option<LocalId> {
        match self {
            Step::Let(bind, _) | Step::Join(bind, _) => Some(bind.id),
            Step::Cont(..) | Step::Drop(_) => None,
        }
    }
}

struct Cut {
    names: Locals,
    /// How many pieces were made.
    pieces: usize,
}

impl Cut {
    fn fresh(&mut self, ty: RTy) -> Option<CoreBind> {
        self.names.highest = self.names.highest.checked_add(1)?;
        Some(CoreBind {
            id: LocalId(self.names.highest),
            ty,
            global: None,
        })
    }

    /// `e`, whose value has type `ty`, with its literals in pieces.
    fn expr(&mut self, e: &CoreExpr, ty: RTy) -> CoreExpr {
        let mut steps = Vec::new();
        let mut e = e;
        let end = loop {
            match e {
                CoreExpr::Let { bind, rhs, body } => {
                    steps.push(Step::Let(bind.clone(), rhs.clone()));
                    e = body;
                }
                CoreExpr::LetJoin { bind, join, body } => {
                    steps.push(Step::Join(bind.clone(), self.expr(join, bind.ty)));
                    e = body;
                }
                CoreExpr::LetCont { id, cont, body } => {
                    steps.push(Step::Cont(*id, self.expr(cont, ty)));
                    e = body;
                }
                CoreExpr::Drop { local, body } => {
                    steps.push(Step::Drop(*local));
                    e = body;
                }
                // A literal that is returned is given a name, and what is
                // returned is the name.
                CoreExpr::Tail(atom) => {
                    if Literal::of(atom).is_some()
                        && let Some(bind) = self.fresh(ty)
                    {
                        let named = Atom::Local(bind.id);
                        steps.push(Step::Let(bind, atom.clone()));
                        break CoreExpr::Tail(named);
                    }
                    break CoreExpr::Tail(atom.clone());
                }
                CoreExpr::If {
                    cond,
                    then,
                    els,
                    ty: t,
                } => {
                    break CoreExpr::If {
                        cond: *cond,
                        then: Box::new(self.expr(then, ty)),
                        els: Box::new(self.expr(els, ty)),
                        ty: *t,
                    };
                }
                CoreExpr::Match { scrut, arms, ty: t } => {
                    let arm = |(pat, arm): &(CorePat, CoreExpr)| (pat.clone(), self.expr(arm, ty));
                    break CoreExpr::Match {
                        scrut: *scrut,
                        arms: arms.iter().map(arm).collect(),
                        ty: *t,
                    };
                }
                CoreExpr::Goto(id) => break CoreExpr::Goto(*id),
            }
        };
        let steps = self.run(steps);
        steps.into_iter().rev().fold(end, |body, s| s.before(body))
    }

    /// A straight run of code with its literals in pieces. Each piece goes as
    /// early as it can, straight after the last of its elements.
    fn run(&mut self, steps: Vec<Step>) -> Vec<Step> {
        // The step that gives each local its value, and the one that drops it.
        let mut bound = HashMap::new();
        let mut dropped = HashMap::new();
        for (i, step) in steps.iter().enumerate() {
            bound.extend(step.binds().map(|l| (l, i)));
            if let Step::Drop(l) = step {
                dropped.insert(*l, i);
            }
        }
        // What goes after each step, with what goes before the first in front.
        let mut after: Vec<Vec<Step>> = Vec::new();
        after.resize_with(steps.len().saturating_add(1), Vec::new);
        // The drops that were moved, and what each literal became.
        let mut moved = vec![false; steps.len()];
        let mut became: HashMap<usize, Atom> = HashMap::new();
        for (i, step) in steps.iter().enumerate() {
            let Step::Let(bind, rhs) = step else { continue };
            let Some(literal) = Literal::of(rhs) else {
                continue;
            };
            let mut pieces = literal.elements.chunks(PIECE).peekable();
            let mut place = 0;
            let mut so_far = literal.onto;
            // The piece before, when it is copied into this one and is then not
            // needed.
            let mut spent: Option<LocalId> = None;
            while let Some(piece) = pieces.next() {
                let made = literal.piece(so_far, piece);
                let dead = spent.map(Step::Drop);
                // The last piece is the literal, unless something comes after
                // its elements.
                if pieces.peek().is_none() && literal.before.is_none() {
                    became.insert(i, made);
                    if let Some(here) = after.get_mut(i.saturating_add(1)) {
                        here.extend(dead);
                    }
                    continue;
                }
                let Some(part) = self.fresh(bind.ty) else {
                    return steps;
                };
                let needed = literal.onto.iter().chain(piece);
                let ready = needed.filter_map(|l| bound.get(l));
                place = ready.fold(place, |p, at| p.max(at.saturating_add(1)));
                let Some(here) = after.get_mut(place) else {
                    return steps;
                };
                so_far = Some(part.id);
                spent = so_far.filter(|_| literal.copies());
                here.push(Step::Let(part, made));
                here.extend(dead);
                // An element that is only here and then dropped can be given
                // up now, and its register with it.
                for l in piece {
                    if self.names.times.get(l) == Some(&2)
                        && let Some(d) = dropped.get(l).filter(|d| **d > i)
                        && let Some(Step::Drop(l)) = steps.get(*d)
                        && let Some(moved) = moved.get_mut(*d)
                        && !*moved
                    {
                        *moved = true;
                        here.push(Step::Drop(*l));
                    }
                }
                self.pieces = self.pieces.saturating_add(1);
            }
            // The elements are an array of their own by now.
            if let (Some(front), Some(back)) = (so_far, literal.before) {
                became.insert(i, Atom::prim(PrimOp::ArrayConcat, vec![front, back]));
                if let Some(here) = after.get_mut(i.saturating_add(1)) {
                    here.push(Step::Drop(front));
                }
            }
        }
        let mut out = Vec::new();
        let mut after = after.into_iter();
        out.extend(after.next().unwrap_or_default());
        for (i, (step, then)) in steps.into_iter().zip(after).enumerate() {
            if let Some(made) = became.remove(&i)
                && let Step::Let(bind, _) = step
            {
                out.push(Step::Let(bind, made));
            } else if moved.get(i) != Some(&true) {
                out.push(step);
            }
            out.extend(then);
        }
        out
    }
}

#[cfg(test)]
mod tests;
