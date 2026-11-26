//! What may not be there, as Core IR. It is a [`CoreExpr::Probe`], whose arms are those
//! of the `match` on it or make its value.

use crate::program::InternalLinkError;
use crate::typed_ir::{Gives, RTy, TypedArm, TypedExpr, ValueRef};
use crate::{Atom, CoreBind, CoreExpr, LocalId, ProbeOp, VariantRef};

use super::{Dest, Frame, Lower, R};

/// A probe whose operands are worked out.
pub(super) struct Asked {
    op: ProbeOp,
    args: Vec<LocalId>,
    /// The type of what it finds.
    found: RTy,
    /// The constructors of its value. With none, that is what it finds, or else its
    /// first operand.
    pub(super) made: Option<Made>,
    /// The type of its value.
    pub(super) ty: RTy,
}

/// The constructors of a probe's value.
#[derive(Clone, Copy)]
pub(super) struct Made {
    /// Of what it finds.
    pub(super) hit: VariantRef,
    pub(super) miss: VariantRef,
    /// The type of the `Nil` that `miss` holds, when it holds one.
    pub(super) nil: Option<RTy>,
}

impl Asked {
    /// The probe, which puts what it finds in `found`.
    pub(super) fn probe(&self, found: LocalId, hit: CoreExpr, miss: CoreExpr) -> CoreExpr {
        CoreExpr::Probe {
            op: self.op,
            args: self.args.clone(),
            binds: vec![Some(CoreBind::new(found, self.found))],
            hit: Box::new(hit),
            miss: Box::new(miss),
        }
    }
}

impl Lower<'_> {
    /// The probe that `e` is, when it is one, with its operands on the spine.
    pub(super) fn asked(&mut self, e: &TypedExpr) -> R<Option<Asked>> {
        let TypedExpr::Probe {
            ty,
            op,
            args,
            found,
            gives,
        } = e
        else {
            return Ok(None);
        };
        Ok(Some(Asked {
            op: *op,
            args: self.operands(args)?,
            found: *found,
            made: self.made(*gives),
            ty: *ty,
        }))
    }

    /// What a value that a probe `gives` is made of.
    fn made(&self, gives: Gives) -> Option<Made> {
        let ctors = self.around.ctors;
        match gives {
            Gives::Option => Some(Made {
                hit: ctors.some,
                miss: ctors.none,
                nil: None,
            }),
            Gives::Result { nil } => Some(Made {
                hit: ctors.ok,
                miss: ctors.err,
                nil: Some(nil),
            }),
            Gives::OrFirst => None,
        }
    }

    /// A local for what `asked` finds.
    pub(super) fn found(&mut self, asked: &Asked) -> R<LocalId> {
        self.fresh(asked.found)
    }

    /// The probe that sends its value to `dest`.
    pub(super) fn value_of(&mut self, asked: &Asked, dest: Dest) -> R<CoreExpr> {
        let found = self.found(asked)?;
        let ty = asked.ty;
        let (hit, miss) = match asked.made {
            Some(Made { hit, miss, nil }) => {
                let some = Atom::Ctor {
                    variant: hit,
                    fields: vec![found],
                };
                let hit = self.sealed(|lo| lo.end(some, ty, dest))?;
                let miss = self.sealed(|lo| {
                    let nil = nil.map(|ty| lo.let_(ty, Atom::Nil)).transpose()?;
                    let none = Atom::Ctor {
                        variant: miss,
                        fields: nil.into_iter().collect(),
                    };
                    lo.end(none, ty, dest)
                })?;
                (hit, miss)
            }
            None => {
                let op = asked.op;
                let first = asked.args.first();
                let first = *first.ok_or(InternalLinkError::ProbeOperands(op))?;
                let hit = self.sealed(|lo| lo.end(Atom::Local(found), ty, dest))?;
                let miss = self.sealed(|lo| lo.end(Atom::Local(first), ty, dest))?;
                (hit, miss)
            }
        };
        Ok(asked.probe(found, hit, miss))
    }

    /// The probe alone, on the spine. Whatever comes of it, the same is next.
    pub(super) fn only_ask(&mut self, asked: &Asked) -> R<()> {
        let id = self.fresh_join()?;
        let found = self.found(asked)?;
        let next = || CoreExpr::Jump {
            to: id,
            args: Vec::new(),
        };
        self.spine.push(Frame::Join {
            id,
            params: Vec::new(),
            body: Box::new(asked.probe(found, next(), next())),
        });
        Ok(())
    }

    /// What the `match` that `e` is asks, and its arms, when that is a probe whose value
    /// it need not have. It may be a `Let` of that value and the one `match` that reads
    /// it.
    pub(super) fn matched<'e>(&self, e: &'e TypedExpr) -> Option<(&'e TypedExpr, &'e [TypedArm])> {
        let (value, arms) = if let TypedExpr::Match { scrut, arms, .. } = e {
            (&**scrut, arms)
        } else if let TypedExpr::Let {
            bind, init, body, ..
        } = e
            && let TypedExpr::Match { scrut, arms, .. } = &**body
            && let TypedExpr::Var {
                place: ValueRef::Local(of),
                ..
            } = **scrut
            && bind.id == of
            && bind.global.is_none()
            && self.reads.get(of.0) == Some(&1)
        {
            (&**init, arms)
        } else {
            return None;
        };
        let made = matches!(
            end(value),
            TypedExpr::Probe {
                gives: Gives::Option | Gives::Result { .. },
                ..
            }
        );
        made.then_some((value, arms))
    }
}

/// What the value of `e` is, behind its `Let`s and `Seq`s.
fn end(mut e: &TypedExpr) -> &TypedExpr {
    while let TypedExpr::Let { body, .. } | TypedExpr::Seq { body, .. } = e {
        e = body;
    }
    e
}

/// Whether the value of `e` is a probe's.
pub(super) fn is_probes(e: &TypedExpr) -> bool {
    matches!(end(e), TypedExpr::Probe { .. })
}
