//! Bool and Nil are immediates in the Core IR a backend sees, never
//! constructors.
//!
//! The source spells `True`, `False` and `Nil` as the prelude's constructors,
//! and `lower` keeps them that way, while a comparison or `&&` already makes an
//! [`Atom::Bool`] and an empty block an [`Atom::Nil`]. This pass removes the
//! difference: each of those constructors becomes the atom, and a `match` on a
//! Bool becomes an `If`. So a backend never needs to know which `TypeId` is
//! the prelude's `Bool`, and a Bool only ever looks one way.

use super::{Atom, CoreExpr, CoreFn, CorePat, LocalId, VariantRef};
use crate::typed_ir::RTy;

/// The prelude's constructors that are immediates.
#[derive(Clone, Copy)]
pub(crate) struct Immediates {
    pub(crate) true_: VariantRef,
    pub(crate) false_: VariantRef,
    pub(crate) nil: VariantRef,
}

/// What a pattern tests a Bool or Nil for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Test {
    /// Matches every value: a wildcard, or `Nil`, which has only one.
    Any,
    Is(bool),
}

impl Immediates {
    fn atom(&self, v: VariantRef) -> Option<Atom> {
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

    /// `None` for any pattern that is not over a Bool or Nil, and for a
    /// binding, which would need its local bound to the scrutinee.
    fn test(&self, p: &CorePat) -> Option<Test> {
        match p {
            CorePat::Wild => Some(Test::Any),
            CorePat::Ctor { variant, .. } if *variant == self.true_ => Some(Test::Is(true)),
            CorePat::Ctor { variant, .. } if *variant == self.false_ => Some(Test::Is(false)),
            CorePat::Ctor { variant, .. } if *variant == self.nil => Some(Test::Any),
            CorePat::Ctor { .. } | CorePat::Lit(_) | CorePat::Bind(_) => None,
        }
    }

    /// For a match whose arms all test a Bool or Nil, and at least one names
    /// `True`, `False` or `Nil`: the arm that runs when the value is `True`,
    /// and the one that runs when it is `False`. For a match on Nil, both are
    /// the arm that runs. A match of only wildcards is left to the backend,
    /// which does not need to know what type it is over.
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

/// Rewrite `f` in place.
pub(crate) fn rewrite(f: &mut CoreFn, im: &Immediates) {
    let mut work = vec![&mut f.body];
    while let Some(e) = work.pop() {
        if let CoreExpr::Match { arms, .. } = e
            && let Some(taken) = im.arms_taken(arms)
        {
            let old = std::mem::replace(e, CoreExpr::Tail(Atom::Nil));
            if let CoreExpr::Match { scrut, arms, ty } = old {
                *e = branch(scrut, arms, ty, taken);
            }
            work.push(e);
            continue;
        }
        match e {
            CoreExpr::Let { rhs, body, .. } => {
                immediate(rhs, im);
                work.push(body);
            }
            CoreExpr::Tail(atom) => immediate(atom, im),
            CoreExpr::LetJoin { join, body, .. }
            | CoreExpr::LetCont {
                cont: join, body, ..
            } => {
                work.push(join);
                work.push(body);
            }
            CoreExpr::Drop { body, .. } => work.push(body),
            CoreExpr::Match { arms, .. } => work.extend(arms.iter_mut().map(|(_, arm)| arm)),
            CoreExpr::If { then, els, .. } => {
                work.push(then);
                work.push(els);
            }
            CoreExpr::Goto(_) => {}
        }
    }
}

fn immediate(atom: &mut Atom, im: &Immediates) {
    if let Atom::Ctor { variant, .. } = atom
        && let Some(a) = im.atom(*variant)
    {
        *atom = a;
    }
}

/// The match on `scrut` as the arms it can take: one arm when the same one
/// runs whatever the value, else an `If` from the `True` arm to the `False`
/// one. Every other arm can never run.
fn branch(
    scrut: LocalId,
    arms: Vec<(CorePat, CoreExpr)>,
    ty: RTy,
    (on_true, on_false): (usize, usize),
) -> CoreExpr {
    let mut then = None;
    let mut els = None;
    for (i, (_, body)) in arms.into_iter().enumerate() {
        if i == on_true {
            then = Some(body);
        } else if i == on_false {
            els = Some(body);
        }
    }
    match (then, els) {
        (Some(body), None) if on_true == on_false => body,
        (Some(then), Some(els)) => CoreExpr::If {
            cond: scrut,
            then: Box::new(then),
            els: Box::new(els),
            ty,
        },
        _ => arm_went_missing(on_true, on_false),
    }
}

/// `arms_taken` picked an arm the match does not have. Only a compiler bug can
/// get here.
#[allow(clippy::panic)]
#[cold]
#[inline(never)]
fn arm_went_missing(on_true: usize, on_false: usize) -> ! {
    panic!(
        "internal compiler error: a Bool match lost arm {on_true} or {on_false}. \
         Report this as a compiler bug."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core_ir::testkit::{bind, func, local, vref};

    fn im() -> Immediates {
        Immediates {
            true_: vref(1, 0),
            false_: vref(1, 1),
            nil: vref(2, 0),
        }
    }

    fn ctor(v: VariantRef) -> CorePat {
        CorePat::Ctor {
            variant: v,
            fields: Vec::new(),
        }
    }

    fn ret(n: u32) -> CoreExpr {
        CoreExpr::Tail(Atom::Local(local(n)))
    }

    fn rewritten(body: CoreExpr) -> String {
        let mut f = func(vec![bind(0, RTy(0))], body, RTy(0));
        rewrite(&mut f, &im());
        f.to_string()
    }

    #[test]
    fn true_false_and_nil_become_atoms() {
        let body = |v| CoreExpr::Let {
            bind: bind(1, RTy(0)),
            rhs: Atom::Ctor {
                variant: v,
                fields: Vec::new(),
                reuse: None,
            },
            body: Box::new(ret(1)),
        };
        assert!(rewritten(body(vref(1, 0))).contains("= true"));
        assert!(rewritten(body(vref(1, 1))).contains("= false"));
        assert!(rewritten(body(vref(2, 0))).contains("= nil"));
        assert!(rewritten(body(vref(3, 0))).contains("ctor 3.0()"));
    }

    #[test]
    fn a_match_on_a_bool_becomes_an_if() {
        let m = |arms| CoreExpr::Match {
            scrut: local(0),
            arms,
            ty: RTy(0),
        };
        let both = rewritten(m(vec![
            (ctor(vref(1, 1)), ret(8)),
            (ctor(vref(1, 0)), ret(7)),
        ]));
        assert!(both.contains("if %0"), "{both}");
        let lines: Vec<&str> = both.lines().map(str::trim).collect();
        let at = |s: &str| lines.iter().position(|l| *l == s).expect(s);
        assert!(
            at("ret %7") < at("ret %8"),
            "the True arm is `then`:\n{both}"
        );

        let wild = rewritten(m(vec![(ctor(vref(1, 0)), ret(7)), (CorePat::Wild, ret(8))]));
        assert!(wild.contains("if %0"), "{wild}");
    }

    /// A match on Nil has one value to match, so its arm is all that is left.
    #[test]
    fn a_match_on_nil_is_its_arm() {
        let nil = rewritten(CoreExpr::Match {
            scrut: local(0),
            arms: vec![(ctor(vref(2, 0)), ret(7))],
            ty: RTy(0),
        });
        assert!(!nil.contains("match"), "{nil}");
        assert!(nil.contains("ret %7"), "{nil}");
    }

    #[test]
    fn a_match_on_anything_else_is_left_alone() {
        let other = rewritten(CoreExpr::Match {
            scrut: local(0),
            arms: vec![(ctor(vref(3, 0)), ret(7)), (CorePat::Wild, ret(8))],
            ty: RTy(0),
        });
        assert!(other.contains("match %0"), "{other}");

        let wild = rewritten(CoreExpr::Match {
            scrut: local(0),
            arms: vec![(CorePat::Wild, ret(7))],
            ty: RTy(0),
        });
        assert!(wild.contains("match %0"), "{wild}");
    }
}
