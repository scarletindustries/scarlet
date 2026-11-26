//! The rows a tree is made from, and what is known of the value they are tried on.

use std::collections::BTreeSet;
use std::rc::Rc;

use crate::ConstId;
use crate::typed_ir::{PatRest, TypedBinPatSeg, TypedBind, TypedExpr, TypedPat, each_expr};

use super::tree::{Goal, Key, Length, Place};

/// One row: what has to hold for it, and what it does then.
#[derive(Debug, Clone)]
pub(super) struct Row<'p> {
    pub(super) goal: Goal,
    /// What is left to check, in the order the patterns are written.
    pub(super) checks: Vec<Check<'p>>,
    /// The names bound so far.
    pub(super) binds: Vec<(TypedBind, Place)>,
    /// Whether something in it runs at most once, which is its guard or such a size. The
    /// row is then never made several with nothing to tell them apart.
    pub(super) once: bool,
    /// Whether it reads sizes that are not [`fixed`].
    pub(super) sizes: bool,
    /// Whether some of its checks wait for others ([`Check::first`]).
    pub(super) in_order: bool,
}

#[derive(Debug, Clone)]
pub(super) struct Check<'p> {
    pub(super) on: Place,
    pub(super) what: What<'p>,
    /// Whether the checks behind it in its row wait for it.
    pub(super) first: bool,
}

#[derive(Debug, Clone)]
pub(super) enum What<'p> {
    Pat(&'p TypedPat),
    /// One of several patterns that bind nothing, all of them constructors or all
    /// literals.
    OneOf(Rc<[&'p TypedPat]>),
    Bits(Bits<'p>),
    /// All of the checks of one of several lists, tried in turn.
    Any {
        alts: Rc<[Vec<Check<'p>>]>,
        names: Rc<[TypedBind]>,
    },
}

impl What<'_> {
    /// About how much it is to ask this once.
    pub(super) fn weight(&self) -> usize {
        match self {
            What::Pat(_) | What::Bits(_) => 1,
            What::OneOf(alts) => alts.len(),
            What::Any { alts, .. } => {
                let each = alts.iter().flatten().map(|c| c.what.weight());
                each.fold(alts.len(), usize::saturating_add)
            }
        }
    }
}

/// What is left to read of a binary.
#[derive(Debug, Clone, Copy)]
pub(super) struct Bits<'p> {
    /// Where the next segment starts.
    pub(super) cursor: Place,
    /// How many bits there are.
    pub(super) total: Place,
    /// 0, as the constant.
    pub(super) zero: ConstId,
    pub(super) segs: &'p [TypedBinPatSeg],
    pub(super) rest: PatRest,
}

/// What is known of a pattern at a place.
pub(super) enum Look<'p> {
    Never,
    /// It fits, as long as its parts do.
    Parts(Vec<(Place, &'p TypedPat)>, Option<(TypedBind, Place)>),
    /// It fits when one of these does.
    Or(Vec<&'p TypedPat>),
    Open,
}

/// What is known of a place, on the way through the tree that is being built.
#[derive(Debug, Clone, Default)]
pub(super) struct Fact {
    pub(super) key: Option<Key>,
    /// What it is one of.
    pub(super) one_of: Option<Rc<BTreeSet<Key>>>,
    /// What it is none of.
    pub(super) none_of: Option<Rc<BTreeSet<Key>>>,
    /// The least number of elements it can have, and one past the greatest.
    pub(super) least: usize,
    pub(super) most: Option<usize>,
}

impl Fact {
    /// Whether it is known not to be `key`.
    pub(super) fn excludes(&self, key: Key) -> bool {
        self.key.is_some_and(|known| known != key)
            || self
                .one_of
                .as_ref()
                .is_some_and(|keys| !keys.contains(&key))
            || self
                .none_of
                .as_ref()
                .is_some_and(|keys| keys.contains(&key))
    }
}

#[derive(Debug, Default)]
pub(super) struct Facts {
    /// What is known of each place, by its number, when something is.
    of: Vec<Option<Fact>>,
    /// What each fact was before, to go back to.
    before: Vec<(Place, Option<Fact>)>,
}

impl Facts {
    pub(super) fn get(&self, p: Place) -> Option<&Fact> {
        self.of.get(p.index()).and_then(Option::as_ref)
    }

    pub(super) fn set(&mut self, p: Place, change: impl FnOnce(&mut Fact)) {
        if self.of.len() <= p.index() {
            self.of.resize_with(p.index().saturating_add(1), || None);
        }
        if let Some(known) = self.of.get_mut(p.index()) {
            let before = known.clone();
            change(known.get_or_insert_default());
            self.before.push((p, before));
        }
    }

    pub(super) fn mark(&self) -> usize {
        self.before.len()
    }

    pub(super) fn back_to(&mut self, mark: usize) {
        while self.before.len() > mark {
            if let Some((p, before)) = self.before.pop()
                && let Some(known) = self.of.get_mut(p.index())
            {
                *known = before;
            }
        }
    }
}

/// The checks of `row` that nothing in front of them has to be done for.
pub(super) fn visible<'r, 'p>(row: &'r Row<'p>) -> &'r [Check<'p>] {
    let first = row.checks.iter().position(|c| c.first);
    let end = first.map_or(row.checks.len(), |at| at.saturating_add(1));
    row.checks.get(..end).unwrap_or_default()
}

/// Whether the checks behind `p`, in a row where that matters, wait for it: it reads
/// a size, or which alternative of it fits decides what names are.
pub(super) fn comes_first(p: &TypedPat) -> bool {
    holds(p, &|p| reads_size(p) || or_that_binds(p))
}

/// Whether `p` itself reads a size that is not [`fixed`].
pub(super) fn reads_size(p: &TypedPat) -> bool {
    let reads = |seg: &TypedBinPatSeg| match seg {
        TypedBinPatSeg::Int { bits, .. }
        | TypedBinPatSeg::Binary {
            bits: Some(bits), ..
        } => !fixed(bits),
        TypedBinPatSeg::Utf8Literal { .. }
        | TypedBinPatSeg::Utf8 { .. }
        | TypedBinPatSeg::Binary { bits: None, .. } => false,
    };
    matches!(p, TypedPat::Bin { segs, .. } if segs.iter().any(reads))
}

/// Whether a size is made of constants, so that nothing can tell when it is read.
pub(crate) fn fixed(size: &TypedExpr) -> bool {
    let mut fixed = true;
    each_expr(size, |e| {
        fixed &= matches!(
            e,
            TypedExpr::Const { .. } | TypedExpr::Binary { .. } | TypedExpr::Unary { .. }
        );
    });
    fixed
}

/// Put every place that `checks` ask something of in `out`.
pub(super) fn asked_of(checks: &[Check<'_>], out: &mut Vec<Place>) {
    for check in checks {
        match &check.what {
            What::Pat(_) | What::OneOf(_) | What::Bits(_) => out.push(check.on),
            What::Any { alts, .. } => alts.iter().for_each(|alt| asked_of(alt, out)),
        }
    }
}

impl Length {
    /// Note what an array has that passes.
    pub(super) fn passed(self, fact: &mut Fact) {
        fact.least = fact.least.max(self.n);
        if !self.or_more {
            fact.most = self.n.checked_add(1);
        }
    }

    /// Note what an array has that does not.
    pub(super) fn failed(self, fact: &mut Fact) {
        if self.or_more {
            fact.most = Some(fact.most.map_or(self.n, |most| most.min(self.n)));
        } else if fact.least == self.n {
            fact.least = self.n.saturating_add(1);
        } else if fact.most == self.n.checked_add(1) {
            fact.most = Some(self.n);
        }
    }
}

/// Whether `p` is an or-pattern that binds names.
pub(super) fn or_that_binds(p: &TypedPat) -> bool {
    matches!(p, TypedPat::Or { alts, .. } if alts.iter().any(binds))
}

/// Whether `p` binds a name.
pub(super) fn binds(p: &TypedPat) -> bool {
    holds(p, &|p| match p {
        TypedPat::Bind(_)
        | TypedPat::Array {
            rest: PatRest::Bind(_),
            ..
        }
        | TypedPat::Bin {
            rest: PatRest::Bind(_),
            ..
        } => true,
        TypedPat::Wild { .. }
        | TypedPat::Lit { .. }
        | TypedPat::Range { .. }
        | TypedPat::Ctor { .. }
        | TypedPat::Tuple { .. }
        | TypedPat::Array { .. }
        | TypedPat::Bin { .. }
        | TypedPat::Or { .. } => false,
    })
}

/// Whether `is` holds of `p` or of a pattern in it.
pub(super) fn holds(p: &TypedPat, is: &impl Fn(&TypedPat) -> bool) -> bool {
    is(p) || inner(p).iter().any(|p| holds(p, is))
}

/// The patterns straight in `p`.
fn inner(p: &TypedPat) -> Vec<&TypedPat> {
    match p {
        TypedPat::Wild { .. }
        | TypedPat::Bind(_)
        | TypedPat::Lit { .. }
        | TypedPat::Range { .. } => Vec::new(),
        TypedPat::Ctor { fields: ps, .. }
        | TypedPat::Tuple { elems: ps, .. }
        | TypedPat::Array { prefix: ps, .. }
        | TypedPat::Or { alts: ps, .. } => ps.iter().collect(),
        TypedPat::Bin { segs, .. } => segs.iter().filter_map(value_of).collect(),
    }
}

/// What a segment's value has to fit, when it reads one.
pub(super) fn value_of(seg: &TypedBinPatSeg) -> Option<&TypedPat> {
    match seg {
        TypedBinPatSeg::Utf8Literal { .. } => None,
        TypedBinPatSeg::Utf8 { value }
        | TypedBinPatSeg::Int { value, .. }
        | TypedBinPatSeg::Binary { value, .. } => Some(value),
    }
}

/// How many patterns `p` is, without those that fit everything and bind nothing.
pub(super) fn size_of(p: &TypedPat) -> usize {
    let own = usize::from(!matches!(p, TypedPat::Wild { .. }));
    let parts = inner(p).into_iter().map(size_of);
    parts.fold(own, usize::saturating_add)
}
