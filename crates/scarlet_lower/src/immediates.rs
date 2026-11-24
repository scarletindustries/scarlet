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
//! constructor and never marks one's drop as a cell to reuse.

use super::{Atom, CoreBind, CoreExpr, CoreFn, CorePat, JoinId, LocalId, ReuseShape, VariantRef};
use crate::typed_ir::RTy;

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
pub(crate) fn rewrite(f: CoreFn, im: &Immediates) -> CoreFn {
    CoreFn {
        body: im.expr(f.body),
        ..f
    }
}

/// One node peeled off a spine, waiting for its rewritten body.
enum Frame {
    Let {
        bind: CoreBind,
        rhs: Atom,
    },
    Join {
        bind: CoreBind,
        join: CoreExpr,
    },
    Cont {
        id: JoinId,
        cont: CoreExpr,
    },
    Drop {
        local: LocalId,
        shape: Option<ReuseShape>,
    },
}

impl Frame {
    fn wrap(self, body: CoreExpr) -> CoreExpr {
        let body = Box::new(body);
        match self {
            Frame::Let { bind, rhs } => CoreExpr::Let { bind, rhs, body },
            Frame::Join { bind, join } => CoreExpr::LetJoin {
                bind,
                join: Box::new(join),
                body,
            },
            Frame::Cont { id, cont } => CoreExpr::LetCont {
                id,
                cont: Box::new(cont),
                body,
            },
            Frame::Drop { local, shape } => CoreExpr::Drop { local, shape, body },
        }
    }
}

impl Immediates {
    /// The spine is walked in a loop, so a long toplevel costs no stack depth.
    fn expr(&self, mut e: CoreExpr) -> CoreExpr {
        let mut spine: Vec<Frame> = Vec::new();
        let tail = loop {
            e = match e {
                CoreExpr::Let { bind, rhs, body } => {
                    spine.push(Frame::Let {
                        bind,
                        rhs: self.atom(rhs),
                    });
                    *body
                }
                CoreExpr::LetJoin { bind, join, body } => {
                    spine.push(Frame::Join {
                        bind,
                        join: self.expr(*join),
                    });
                    *body
                }
                CoreExpr::LetCont { id, cont, body } => {
                    spine.push(Frame::Cont {
                        id,
                        cont: self.expr(*cont),
                    });
                    *body
                }
                CoreExpr::Drop { local, shape, body } => {
                    spine.push(Frame::Drop { local, shape });
                    *body
                }
                // A match on a Bool or Nil becomes the expression that
                // replaces it, which the loop then carries on into.
                CoreExpr::Match { scrut, arms, ty } => match self.branch(scrut, arms, ty) {
                    Ok(e) => e,
                    Err(arms) => {
                        break CoreExpr::Match {
                            scrut,
                            arms: arms
                                .into_iter()
                                .map(|(pat, body)| (pat, self.expr(body)))
                                .collect(),
                            ty,
                        };
                    }
                },
                CoreExpr::If {
                    cond,
                    then,
                    els,
                    ty,
                } => {
                    break CoreExpr::If {
                        cond,
                        then: Box::new(self.expr(*then)),
                        els: Box::new(self.expr(*els)),
                        ty,
                    };
                }
                CoreExpr::Tail(atom) => break CoreExpr::Tail(self.atom(atom)),
                CoreExpr::Goto(j) => break CoreExpr::Goto(j),
            };
        };
        spine.into_iter().rev().fold(tail, |body, f| f.wrap(body))
    }

    fn atom(&self, atom: Atom) -> Atom {
        match atom {
            Atom::Ctor { variant, .. } if let Some(a) = self.immediate(variant) => a,
            other => other,
        }
    }

    /// Rebuild a match on `scrut` from the arms it can take. That is one arm
    /// when the same one runs whatever the value, or else an `If` with the
    /// `True` arm as `then` and the `False` arm as `else`. No other arm can
    /// ever run. An arm that binds the value starts by binding it. A match
    /// that isn't over a Bool or Nil comes back as its arms.
    fn branch(
        &self,
        scrut: LocalId,
        mut arms: Vec<(CorePat, CoreExpr)>,
        ty: RTy,
    ) -> Result<CoreExpr, Vec<(CorePat, CoreExpr)>> {
        let Some((on_true, on_false)) = self.arms_taken(&arms) else {
            return Err(arms);
        };
        // Both indices came from `arms`, so they are in range. Taking the
        // later one first leaves the earlier one where it was.
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
            return Ok(take(on_true));
        }
        let later = take(on_true.max(on_false));
        let earlier = take(on_true.min(on_false));
        let (then, els) = if on_true > on_false {
            (later, earlier)
        } else {
            (earlier, later)
        };
        Ok(CoreExpr::If {
            cond: scrut,
            then: Box::new(then),
            els: Box::new(els),
            ty,
        })
    }
}

#[cfg(test)]
mod tests;
