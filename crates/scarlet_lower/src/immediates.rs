//! Bool and Nil are immediates in the Core IR a backend sees, never
//! constructors.
//!
//! The source spells `True`, `False` and `Nil` as the prelude's constructors,
//! and `lower` keeps them that way, while a comparison or `&&` already makes an
//! [`Atom::Bool`] and an empty block an [`Atom::Nil`]. This pass removes the
//! difference: each of those constructors becomes the atom, and a `match` on a
//! Bool becomes an `If`. So a backend never needs to know which `TypeId` is
//! the prelude's `Bool`, and a Bool only ever looks one way.
//!
//! It runs between `lower` and `perceus`, so Perceus never sees a Bool as a
//! constructor.

use super::{Atom, CoreExpr, CoreFn, CorePat, LocalId, VariantRef};

/// The prelude's constructors that are immediates.
#[derive(Clone, Copy)]
pub struct Immediates {
    pub(crate) true_: VariantRef,
    pub(crate) false_: VariantRef,
    pub(crate) nil: VariantRef,
}

/// What a pattern tests a Bool or Nil for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Test {
    /// Matches every value. This is a wildcard, or `Nil`, which has only one
    /// value.
    Any,
    Is(bool),
}

impl Immediates {
    fn immediate(&self, v: VariantRef) -> Option<Atom> {
        if v == self.true_ {
            Some(Atom::Bool(true))
        } else if v == self.false_ {
            Some(Atom::Bool(false))
        } else if v == self.nil {
            Some(Atom::Nil)
        } else {
            None
        }
    }

    /// Returns `None` for any pattern that is not over a Bool or Nil.
    fn test(&self, p: &CorePat) -> Option<Test> {
        match p {
            CorePat::Wild | CorePat::Bind(_) => Some(Test::Any),
            CorePat::Ctor { variant, .. } if *variant == self.true_ => Some(Test::Is(true)),
            CorePat::Ctor { variant, .. } if *variant == self.false_ => Some(Test::Is(false)),
            CorePat::Ctor { variant, .. } if *variant == self.nil => Some(Test::Any),
            CorePat::Ctor { .. } | CorePat::Lit(_) => None,
        }
    }

    /// For a match whose arms all test a Bool or Nil, where at least one names
    /// `True`, `False` or `Nil`, this gives the arm that runs when the value is
    /// `True` and the one that runs when it is `False`. For a match on Nil,
    /// both are the arm that runs. A match of only wildcards and bindings is left to
    /// the backend, which does not need to know what type it is over.
    fn arms_taken(&self, arms: &[(CorePat, CoreExpr)]) -> Option<(usize, usize)> {
        if !arms.iter().any(|(p, _)| matches!(p, CorePat::Ctor { .. })) {
            return None;
        }
        let tests: Vec<Test> = arms
            .iter()
            .map(|(p, _)| self.test(p))
            .collect::<Option<_>>()?;
        let first = |b: bool| {
            tests
                .iter()
                .position(|t| *t == Test::Any || *t == Test::Is(b))
        };
        Some((first(true)?, first(false)?))
    }
}

/// Rewrite `f`'s body.
pub(crate) fn rewrite(f: &mut CoreFn, im: &Immediates) {
    im.expr(&mut f.body);
}

impl Immediates {
    /// The spine is walked in a loop, so a long toplevel costs no stack depth.
    fn expr(&self, mut e: &mut CoreExpr) {
        loop {
            e = match e {
                CoreExpr::Let { rhs, body, .. } => {
                    self.atom(rhs);
                    body
                }
                CoreExpr::Join { cont, body, .. } => {
                    self.expr(body);
                    cont
                }
                CoreExpr::SetGlobal { body, .. } | CoreExpr::Drop { body, .. } => body,
                CoreExpr::Match { scrut, arms } => match self.arms_taken(arms) {
                    Some(taken) => {
                        let branch = branch(*scrut, std::mem::take(arms), taken);
                        *e = branch;
                        e
                    }
                    None => return arms.iter_mut().for_each(|(_, arm)| self.expr(arm)),
                },
                CoreExpr::If { then, els, .. } => {
                    self.expr(then);
                    els
                }
                CoreExpr::Ret(_) | CoreExpr::TailCall { .. } | CoreExpr::Jump { .. } => return,
            };
        }
    }

    fn atom(&self, atom: &mut Atom) {
        if let Atom::Ctor { variant, .. } = atom
            && let Some(immediate) = self.immediate(*variant)
        {
            *atom = immediate;
        }
    }
}

/// What a match on `scrut` is that takes one of two of its arms: that arm when the
/// same one runs whatever the value, or else an `If` with the `True` arm as `then`
/// and the `False` arm as `else`. No other arm can ever run. An arm that binds the
/// value starts by binding it.
fn branch(
    scrut: LocalId,
    mut arms: Vec<(CorePat, CoreExpr)>,
    (on_true, on_false): (usize, usize),
) -> CoreExpr {
    // Both indices came from `arms`, so they are in range. Taking the later one
    // first leaves the earlier one where it was.
    arms.truncate(on_true.max(on_false).saturating_add(1));
    let mut take = |i: usize| {
        let (pat, body) = arms.swap_remove(i);
        match pat {
            CorePat::Bind(bind) => CoreExpr::Let {
                bind,
                rhs: Atom::Local(scrut),
                body: Box::new(body),
            },
            CorePat::Wild | CorePat::Lit(_) | CorePat::Ctor { .. } => body,
        }
    };
    if on_true == on_false {
        return take(on_true);
    }
    let later = Box::new(take(on_true.max(on_false)));
    let earlier = Box::new(take(on_true.min(on_false)));
    let (then, els) = if on_true > on_false {
        (later, earlier)
    } else {
        (earlier, later)
    };
    CoreExpr::If {
        cond: scrut,
        then,
        els,
    }
}

#[cfg(test)]
mod tests;
