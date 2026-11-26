//! Bool and Nil are immediates in the Core IR a backend sees, never
//! constructors.
//!
//! The source spells `True`, `False` and `Nil` as the prelude's constructors,
//! and `lower` keeps them that way, while a comparison or `&&` already makes an
//! [`Atom::Bool`] and an empty block an [`Atom::Nil`]. This pass removes the
//! difference: each of those constructors becomes the atom, and a `Switch` on a
//! Bool becomes an `If`. So a backend never needs to know which `TypeId` is
//! the prelude's `Bool`, and a Bool only ever looks one way.
//!
//! It runs between `lower` and `perceus`, so Perceus never sees a Bool as a
//! constructor.

use scarlet_ir::TypeId;

use super::{Atom, Cases, CoreExpr, CoreFn, LocalId, VariantRef};

/// The prelude's constructors that are immediates.
#[derive(Clone, Copy)]
pub struct Immediates {
    pub(crate) true_: VariantRef,
    pub(crate) false_: VariantRef,
    pub(crate) nil: VariantRef,
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

    /// Whether a value of `ty` is an immediate, so that no backend switches on one.
    fn are(&self, ty: TypeId) -> bool {
        ty == self.true_.type_id || ty == self.nil.type_id
    }

    /// What a switch is on the Bool or the Nil in `scrut`, whose type is `ty`: what
    /// it goes on with, when only that can be, or else an `If`.
    fn branch(
        &self,
        scrut: LocalId,
        ty: TypeId,
        arms: Vec<(Vec<u16>, CoreExpr)>,
        default: Option<Box<CoreExpr>>,
    ) -> CoreExpr {
        let (mut on_true, mut on_false, mut always) = (None, None, default);
        for (variants, body) in arms {
            let is = |v: VariantRef| v.type_id == ty && variants.contains(&v.variant_idx);
            let body = Box::new(body);
            match (is(self.true_), is(self.false_)) {
                (true, false) => on_true = Some(body),
                (false, true) => on_false = Some(body),
                (true, true) | (false, false) => always = Some(body),
            }
        }
        match (on_true, on_false, always) {
            (Some(then), Some(els), _)
            | (Some(then), None, Some(els))
            | (None, Some(els), Some(then)) => CoreExpr::If {
                cond: scrut,
                then,
                els,
            },
            (Some(only), None, None) | (None, Some(only), None) | (None, None, Some(only)) => *only,
            (None, None, None) => CoreExpr::NoArm,
        }
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
                CoreExpr::SetGlobal { body, .. }
                | CoreExpr::Drop { body, .. }
                | CoreExpr::Open { body, .. } => body,
                CoreExpr::Switch {
                    scrut,
                    cases: Cases::Variant { ty, arms },
                    default,
                } if self.are(*ty) => {
                    let (arms, default) = (std::mem::take(arms), default.take());
                    *e = self.branch(*scrut, *ty, arms, default);
                    e
                }
                CoreExpr::Switch { cases, default, .. } => {
                    for arm in cases.arms_mut() {
                        self.expr(arm);
                    }
                    match default {
                        Some(default) => default,
                        None => return,
                    }
                }
                CoreExpr::If { then, els, .. } => {
                    self.expr(then);
                    els
                }
                CoreExpr::Ret(_)
                | CoreExpr::TailCall { .. }
                | CoreExpr::Jump { .. }
                | CoreExpr::NoArm => return,
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

#[cfg(test)]
mod tests;
