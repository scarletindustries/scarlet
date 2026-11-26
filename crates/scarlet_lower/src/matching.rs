//! A match as a decision tree.
//!
//! Each arm is a row of checks, which are patterns that something in the matched
//! value has to fit, and the arms are tried from the first to the last. The tree
//! asks one thing at a time, and it asks what the first row still has to know.
//! Every row that asks the same is then split by the answer, so nothing is asked
//! twice on one way through the tree. This is the algorithm of Jules Jacobs' "How
//! to compile pattern matching", which Gleam's compiler uses too.
//!
//! What the tree asks of a value is always there to be asked, stops nothing and
//! calls nothing, so the order it asks in cannot be seen. Four things can be, and
//! keep their order. A guard runs where all its row's checks hold, and the rows
//! below are only tried when it has failed. A size in a `<<>>` pattern is an
//! expression of any kind, so unless it is made of constants the checks behind it in
//! its row wait until it has been read. An or-pattern that binds names tries each
//! alternative with the rest of the row and its guard, and one that binds none tries
//! the rest once. So a row is never two rows with one guard or one size between them.
//!
//! ## How large a tree gets
//!
//! A row that does not ask what a question answers goes on in every branch of it.
//! That is what makes a decision tree grow, in the worst case without bound. Here
//! each match has an allowance for such copies, in proportion to its size
//! ([`Allowance`]). Once it is spent, the rows in front of one that would have to
//! be copied are a tree by themselves, and where none of them fits, the rest are
//! tried, in one place however many ways lead there ([`Node::Chain`]). That is how
//! the rows read, and takes no more code than they do.
//!
//! One thing is outside the allowance. An or-pattern that binds names, in a row with
//! a guard or with a size that is not made of constants, tries each alternative with
//! everything that comes after it. Where it is not plain that only one can fit, that
//! is a row for each, and several of them beside each other are a row for each
//! combination.
//!
//! Only the bodies of the arms are in one place in any case, and it is `lower`
//! that sees to it.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::rc::Rc;

use smallvec::{SmallVec, smallvec};

use super::{Const, ConstId, FloatKey, VariantRef};
use crate::program::InternalLinkError;
use crate::typed_ir::{
    PatRest, RTy, Shapes, TempTys, TypedArm, TypedBind, TypedPat, each_expr, pat_binds,
};

mod rows;
mod tree;

pub(crate) use rows::fixed;
pub(crate) use tree::{
    AnyId, Branch, Fail, Goal, Key, Leaf, Length, Node, Part, Place, Places, ROOT, RestId, Segment,
    Source, Tree,
};

use rows::{
    Bits, Check, Fact, Facts, Look, Row, What, asked_of, binds, comes_first, holds, or_that_binds,
    reads_size, size_of, value_of, visible,
};
use tree::Ways;

type R<T> = Result<T, InternalLinkError>;

/// What is left of a row, which is nearly always one row.
type Settled<'p> = SmallVec<[Row<'p>; 1]>;

/// The patterns of one check that a value may fit.
type Pats<'p> = SmallVec<[&'p TypedPat; 1]>;

/// How much the trees of a match may copy.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Allowance {
    /// As a multiple of how large the match is.
    times: usize,
    /// However small it is.
    any: usize,
}

impl Allowance {
    pub(crate) const USUAL: Allowance = Allowance { times: 2, any: 64 };
}

/// How deep the branches for what no case is for may be in one another. Past that
/// each is tried behind the one before, so that a long row of them takes no stack.
const DEEP: usize = 24;

/// How many rows are looked at to choose what to ask.
const LOOKED_AT: usize = 16;

/// What a tree is made of, beside the match.
pub(crate) struct Given<'a> {
    pub(crate) shapes: &'a Shapes,
    pub(crate) consts: &'a [Const],
    pub(crate) temps: TempTys,
    /// How many times the function reads each binding.
    pub(crate) reads: &'a [u32],
    pub(crate) allowance: Allowance,
}

/// The tree that finds the arm for a value of type `ty`. Nothing but the match
/// reads that value when it is `alone`.
pub(crate) fn tree<'p>(
    given: &Given<'_>,
    ty: RTy,
    alone: bool,
    arms: &'p [TypedArm],
) -> R<Tree<'p>> {
    let mut build = Build {
        given,
        alone,
        arms: Vec::with_capacity(arms.len()),
        places: Places::default(),
        facts: Facts::default(),
        rests: 0,
        anys: 0,
        allowance: given.allowance.any,
        depth: 0,
    };
    let root = build.places.add(Source::Given, ty)?;
    let mut rows = VecDeque::with_capacity(arms.len());
    for (i, arm) in arms.iter().enumerate() {
        let mut guard = 0usize;
        if let Some(e) = &arm.guard {
            guard = 1;
            each_expr(e, |_| guard = guard.saturating_add(1));
        }
        build.arms.push(guard);
        let size = size_of(&arm.pat).saturating_add(guard);
        let more = size.saturating_mul(given.allowance.times);
        build.allowance = build.allowance.saturating_add(more);
        let sizes = holds(&arm.pat, &reads_size);
        let once = guard > 0 || sizes;
        let in_order = sizes || (guard > 0 && holds(&arm.pat, &or_that_binds));
        let mut row = Row {
            goal: Goal::Arm(i),
            checks: Vec::new(),
            binds: Vec::new(),
            once,
            sizes,
            in_order,
        };
        row.checks = build.checks([(root, &arm.pat)], in_order);
        rows.push_back(row);
    }
    let root = build.rows(rows, None)?;
    let mut tree = Tree {
        root,
        places: build.places,
        leaves: vec![0; arms.len()],
        rests: vec![0; build.rests as usize],
        anys: build.anys as usize,
    };
    let mut ways = Ways {
        leaves: &mut tree.leaves,
        rests: &mut tree.rests,
    };
    ways.walk(&tree.root);
    Ok(tree)
}

/// What a check is that asks whether one of several patterns fits.
enum Alternative<'p> {
    /// None can.
    None,
    /// One does, and it binds nothing.
    Fits,
    Only(&'p TypedPat),
    /// It is this check.
    Asked(What<'p>),
    /// The row is one row for each.
    Rows,
}

/// What came of asking for a row.
enum Asked<'p> {
    /// Nothing is left to try.
    Done(Node<'p>),
    /// What is left is tried where this goes on at the id.
    Link(RestId, Node<'p>),
    /// Nothing had to be asked yet, and this is left of the row.
    Again(Row<'p>),
}

/// What a row has to know, that others may share the answer to.
enum Question {
    /// Which of several values it is.
    Which,
    Long(Length),
    /// Whether the Int there is in this range.
    Within(Place, [ConstId; 2]),
}

struct Build<'a> {
    given: &'a Given<'a>,
    alone: bool,
    /// How large each arm's guard is, which is 0 when it has none.
    arms: Vec<usize>,
    places: Places,
    facts: Facts,
    rests: u32,
    anys: u32,
    /// How much may still be copied.
    allowance: usize,
    /// How many branches for no case the walk is in.
    depth: usize,
}

/// One case of a switch that is being made, or what is tried when there is no case.
#[derive(Debug, Default)]
struct Case<'p> {
    rows: VecDeque<Row<'p>>,
    /// Whether one of its rows takes every value that comes to it, so that none
    /// behind it is ever tried.
    closed: bool,
    /// Which rows of the switch are here for the case's own key, as long as each is
    /// what it was without its check. Two cases with the same ones are one, because
    /// the rows that ask something else are in both, or were not tried in either.
    own: Option<Vec<usize>>,
}

impl<'p> Case<'p> {
    /// Try `row` after those that are here. None is after one that `closes`.
    fn add(&mut self, row: Row<'p>, closes: bool) {
        if !self.closed {
            self.rows.push_back(row);
            self.closed = closes;
        }
    }

    /// Note that row `number` of the switch is here, and whether it is as it was,
    /// without its check.
    fn asked(&mut self, number: usize, as_it_was: bool) {
        match &mut self.own {
            Some(own) if as_it_was => own.push(number),
            Some(_) | None => self.own = None,
        }
    }
}

/// A switch that is being made.
struct Split<'p> {
    on: Place,
    cases: Vec<(Key, Case<'p>)>,
    /// Where each key's case is.
    of: BTreeMap<Key, usize>,
    /// The cases that are not closed.
    open: BTreeSet<usize>,
    default: Case<'p>,
    /// How many keys there are in all, when the type says.
    keys: Option<usize>,
    /// How many rows were placed that ask what the switch does.
    placed: usize,
}

impl Split<'_> {
    /// Whether there is a case for every value.
    fn covered(&self) -> bool {
        self.keys == Some(self.cases.len())
    }

    /// Whether a value that is in no case goes on to more rows.
    fn default_open(&self) -> bool {
        !self.default.closed && !self.covered()
    }

    /// Whether no row that is placed now can ever be tried.
    fn full(&self) -> bool {
        self.open.is_empty() && !self.default_open()
    }
}

/// What became of a row at a switch.
enum Placed<'p> {
    Yes,
    /// It would have had to be copied, and there is no allowance for it.
    No(Row<'p>),
}

impl<'p> Build<'_> {
    /// The checks that `patterns` are, in a row that may be `in_order`. One that fits
    /// everything and binds nothing is none.
    fn checks(
        &self,
        patterns: impl IntoIterator<Item = (Place, &'p TypedPat)>,
        in_order: bool,
    ) -> Vec<Check<'p>> {
        let all = patterns.into_iter();
        let all = all.filter(|(_, p)| !matches!(p, TypedPat::Wild { .. }));
        all.map(|(on, p)| Check {
            on,
            what: What::Pat(p),
            first: in_order && comes_first(p),
        })
        .collect()
    }

    fn used(&self, b: &TypedBind) -> bool {
        self.given.reads.get(b.id.0).is_none_or(|n| *n > 0)
    }

    /// How much it is to have one more of `row`.
    fn cost(&self, row: &Row<'p>) -> usize {
        let guard = match row.goal {
            Goal::Arm(i) => self.arms.get(i).copied().unwrap_or(0),
            Goal::Any(_) => 0,
        };
        let checks = row.checks.iter().map(|c| c.what.weight());
        let checks = checks.fold(row.binds.len(), usize::saturating_add);
        checks.saturating_add(guard).saturating_add(1)
    }

    /// Whether there is allowance for this much more, which is then spent.
    fn affords(&mut self, cost: usize) -> bool {
        match self.allowance.checked_sub(cost) {
            Some(left) => {
                self.allowance = left;
                true
            }
            None => false,
        }
    }

    fn guarded(&self, goal: Goal) -> bool {
        match goal {
            Goal::Arm(i) => self.arms.get(i).is_some_and(|size| *size > 0),
            Goal::Any(_) => false,
        }
    }

    /// Whether `row` takes every value that comes to it.
    fn takes_all(&self, row: &Row<'p>) -> bool {
        row.checks.is_empty() && !self.guarded(row.goal)
    }

    /// A place in the rows that come after others.
    fn rest(&mut self) -> R<RestId> {
        let id = RestId(self.rests);
        let at = self.rests as usize;
        self.rests = self
            .rests
            .checked_add(1)
            .ok_or(InternalLinkError::IndexTooWide(at))?;
        Ok(id)
    }

    /// The key of a literal.
    fn key(&self, c: ConstId) -> R<Key> {
        let odd = InternalLinkError::OddLiteral(c);
        match self.given.consts.get(c.0 as usize).ok_or(odd.clone())? {
            Const::Int(n) => Ok(Key::Int(*n)),
            Const::Float(x) => Ok(Key::Float(FloatKey::new(*x))),
            Const::String(_) => Ok(Key::Text(c)),
            Const::Binary { .. } => Err(odd),
        }
    }

    /// The Int that a range pattern has at one of its ends.
    fn bound(&self, c: ConstId) -> R<i64> {
        match self.given.consts.get(c.0 as usize) {
            Some(Const::Int(n)) => Ok(*n),
            Some(Const::Float(_) | Const::String(_) | Const::Binary { .. }) | None => {
                Err(InternalLinkError::OddBound(c))
            }
        }
    }

    /// What is known of whether what is at `on` fits `p`.
    fn look(&mut self, on: Place, p: &'p TypedPat) -> R<Look<'p>> {
        let nothing = Fact::default();
        let fact = self.facts.get(on).unwrap_or(&nothing);
        let fits = |holds: bool| match holds {
            true => Look::Parts(Vec::new(), None),
            false => Look::Never,
        };
        Ok(match p {
            TypedPat::Wild { .. } => fits(true),
            TypedPat::Bind(b) => Look::Parts(Vec::new(), Some((*b, on))),
            TypedPat::Tuple { elems, .. } => {
                let mut parts = Vec::with_capacity(elems.len());
                for (i, elem) in elems.iter().enumerate() {
                    if !matches!(elem, TypedPat::Wild { .. }) {
                        let part = Part::Element(index(i)?);
                        parts.push((self.places.part(on, part, elem.ty())?, elem));
                    }
                }
                Look::Parts(parts, None)
            }
            TypedPat::Ctor {
                variant, fields, ..
            } => {
                if fact.excludes(Key::Variant(*variant)) {
                    Look::Never
                } else if fact.key.is_some() {
                    Look::Parts(self.fields(on, *variant, fields)?, None)
                } else if fields.is_empty() && self.variants(*variant) == Some(1) {
                    // A type with one value, which is not worth a question.
                    fits(true)
                } else {
                    Look::Open
                }
            }
            TypedPat::Lit { value, .. } => {
                let key = self.key(*value)?;
                if fact.excludes(key) {
                    Look::Never
                } else if fact.key.is_some() {
                    fits(true)
                } else {
                    Look::Open
                }
            }
            TypedPat::Range { lo, hi, .. } => {
                let (lo, hi) = (self.bound(*lo)?, self.bound(*hi)?);
                match fact.key {
                    Some(Key::Int(n)) => fits(n >= lo && n < hi),
                    Some(Key::Variant(_) | Key::Float(_) | Key::Text(_)) | None if lo >= hi => {
                        Look::Never
                    }
                    Some(Key::Variant(_) | Key::Float(_) | Key::Text(_)) | None => Look::Open,
                }
            }
            TypedPat::Array {
                len, prefix, rest, ..
            } => {
                let n = prefix.len();
                let (or_more, short) = (*rest != PatRest::None, fact.most.is_some_and(|m| m <= n));
                let exact = fact.least == n && fact.most == n.checked_add(1);
                if short || (!or_more && fact.least > n) {
                    Look::Never
                } else if exact || (or_more && fact.least >= n) {
                    let mut parts = Vec::with_capacity(n);
                    for (i, p) in prefix.iter().enumerate() {
                        if !matches!(p, TypedPat::Wild { .. }) {
                            let part = Part::Item(index(i)?);
                            parts.push((self.places.part(on, part, p.ty())?, p));
                        }
                    }
                    let tail = match rest {
                        PatRest::Bind(b) if self.used(b) => Some(match n {
                            0 => (*b, on),
                            _ => (*b, self.places.part(on, Part::After(*len), b.ty)?),
                        }),
                        PatRest::Bind(_) | PatRest::Discard | PatRest::None => None,
                    };
                    Look::Parts(parts, tail)
                } else {
                    Look::Open
                }
            }
            TypedPat::Bin { segs, rest, .. } => match (segs.as_slice(), rest) {
                ([], PatRest::Discard) => fits(true),
                ([], PatRest::Bind(b)) => Look::Parts(Vec::new(), Some((*b, on))),
                ([], PatRest::None) | ([_, ..], _) => Look::Open,
            },
            TypedPat::Or { alts, .. } => {
                let mut left = Vec::with_capacity(alts.len());
                self.alternatives(on, alts, &mut left)?;
                Look::Or(left)
            }
        })
    }

    /// Put those of `alts` in `left` that may fit, in order.
    fn alternatives(
        &mut self,
        on: Place,
        alts: &'p [TypedPat],
        left: &mut Vec<&'p TypedPat>,
    ) -> R<()> {
        for alt in alts {
            match self.look(on, alt)? {
                Look::Never => {}
                Look::Or(more) => left.extend(more),
                Look::Parts(..) | Look::Open => left.push(alt),
            }
        }
        Ok(())
    }

    /// How many constructors the type of `v` has.
    fn variants(&self, v: VariantRef) -> Option<usize> {
        let shape = self.given.shapes.of(v.type_id)?;
        Some(shape.variants())
    }

    /// The fields of `variant` at `on`, each with what it has to fit.
    fn fields(
        &mut self,
        on: Place,
        variant: VariantRef,
        fields: &'p [TypedPat],
    ) -> R<Vec<(Place, &'p TypedPat)>> {
        let mut parts = Vec::with_capacity(fields.len());
        for (i, p) in fields.iter().enumerate() {
            if !matches!(p, TypedPat::Wild { .. }) {
                let part = Part::Field {
                    variant: variant.variant_idx,
                    index: index(i)?,
                };
                parts.push((self.places.part(on, part, p.ty())?, p));
            }
        }
        Ok(parts)
    }

    /// Put what is left of `row` in `out`, once what is known of its checks from the
    /// `from`th on is taken out of it. That is no row when it cannot fit, and several
    /// when what tells them apart is which alternative of an or-pattern fits.
    fn settle(&mut self, mut row: Row<'p>, mut from: usize, out: &mut Settled<'p>) -> R<()> {
        while let Some(check) = row.checks.get(from) {
            let (on, first) = (check.on, check.first);
            let look = match &check.what {
                What::Pat(p) => {
                    let p = *p;
                    self.look(on, p)?
                }
                What::OneOf(_) if self.facts.get(on).is_none() => Look::Open,
                What::OneOf(all) => {
                    let all = all.clone();
                    let mut left = Vec::with_capacity(all.len());
                    for p in all.iter() {
                        match self.look(on, p)? {
                            Look::Never => {}
                            Look::Parts(..) | Look::Or(_) | Look::Open => left.push(*p),
                        }
                    }
                    Look::Or(left)
                }
                What::Bits(_) | What::Any { .. } => Look::Open,
            };
            match look {
                Look::Open => {}
                Look::Never => return Ok(()),
                Look::Parts(parts, bind) => {
                    let parts = self.checks(parts, row.in_order);
                    row.checks.splice(from..=from, parts);
                    row.binds.extend(bind.filter(|(b, _)| self.used(b)));
                    continue;
                }
                Look::Or(alts) => match self.alternative(&row, on, &alts)? {
                    Alternative::None => return Ok(()),
                    Alternative::Fits => {
                        row.checks.remove(from);
                        continue;
                    }
                    Alternative::Only(p) => {
                        let only = self.checks([(on, p)], row.in_order);
                        row.checks.splice(from..=from, only);
                        continue;
                    }
                    Alternative::Asked(what) => {
                        if let Some(check) = row.checks.get_mut(from) {
                            check.what = what;
                        }
                    }
                    Alternative::Rows => {
                        for p in alts {
                            let mut one = row.clone();
                            let check = self.checks([(on, p)], row.in_order);
                            one.checks.splice(from..=from, check);
                            self.settle(one, from, out)?;
                        }
                        return Ok(());
                    }
                },
            }
            if first {
                break;
            }
            from = from.saturating_add(1);
        }
        out.push(row);
        Ok(())
    }

    /// What a check of `row` is that asks whether one of `alts` fits what is at `on`,
    /// each of which may.
    fn alternative(
        &mut self,
        row: &Row<'p>,
        on: Place,
        alts: &[&'p TypedPat],
    ) -> R<Alternative<'p>> {
        match alts {
            [] => return Ok(Alternative::None),
            [one] => return Ok(Alternative::Only(one)),
            _ => {}
        }
        let binding = alts.iter().any(|p| binds(p));
        let mut asked = !binding;
        for p in alts {
            match self.look(on, p)? {
                // A size that is read in front of it is read all the same.
                Look::Parts(parts, _) if parts.is_empty() && !binding && !row.sizes => {
                    return Ok(Alternative::Fits);
                }
                Look::Open if matches!(p, TypedPat::Ctor { .. } | TypedPat::Lit { .. }) => {}
                Look::Never | Look::Parts(..) | Look::Or(_) | Look::Open => asked = false,
            }
        }
        if asked {
            return Ok(Alternative::Asked(What::OneOf(alts.into())));
        }
        let more = alts.len().saturating_sub(1);
        let cost = self.cost(row).saturating_mul(more);
        // Each alternative is tried with the guard, so there is a row for each. That is
        // seen only when more than one can fit.
        if binding && row.in_order && (cost <= self.allowance || !self.one_at_most(alts)?) {
            self.allowance = self.allowance.saturating_sub(cost);
            return Ok(Alternative::Rows);
        }
        if (binding || !row.once) && self.affords(cost) {
            return Ok(Alternative::Rows);
        }
        let names = match alts.first() {
            Some(p) if binding => pat_binds(p),
            Some(_) | None => Vec::new(),
        };
        let names = names.into_iter().filter(|b| self.used(b));
        let alts = alts.iter().map(|p| self.checks([(on, *p)], row.in_order));
        Ok(Alternative::Asked(What::Any {
            names: names.collect(),
            alts: alts.collect(),
        }))
    }

    /// Whether `alts` are all the constructors that what is at `on` can be, and ask
    /// nothing of their fields.
    fn all_there_are(&self, on: Place, alts: &[&'p TypedPat]) -> bool {
        let mut seen = HashSet::new();
        let mut all = None;
        for p in alts {
            let TypedPat::Ctor {
                variant, fields, ..
            } = p
            else {
                return false;
            };
            if fields.iter().any(|p| !matches!(p, TypedPat::Wild { .. })) {
                return false;
            }
            all = self.variants(*variant);
            seen.insert(*variant);
        }
        let known = self.facts.get(on).and_then(|f| f.none_of.as_ref());
        let not = known.map_or(0, |keys| keys.len());
        all.is_some_and(|all| seen.len().saturating_add(not) >= all)
    }

    /// Whether a value can fit `alts` in one way at the most, and no size is read to
    /// find that out.
    fn one_at_most(&self, alts: &[&'p TypedPat]) -> R<bool> {
        if alts.iter().any(|p| comes_first(p)) {
            return Ok(false);
        }
        for (i, a) in alts.iter().enumerate() {
            for b in alts.iter().take(i) {
                if !self.never_both(a, b)? {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    /// Whether it is plain that no value fits both `a` and `b`.
    fn never_both(&self, a: &TypedPat, b: &TypedPat) -> R<bool> {
        let parts = match (a, b) {
            (TypedPat::Or { alts, .. }, other) | (other, TypedPat::Or { alts, .. }) => {
                for alt in alts {
                    if !self.never_both(alt, other)? {
                        return Ok(false);
                    }
                }
                return Ok(true);
            }
            (TypedPat::Lit { value: a, .. }, TypedPat::Lit { value: b, .. }) => {
                return Ok(self.key(*a)? != self.key(*b)?);
            }
            (
                TypedPat::Ctor {
                    variant: a,
                    fields: of_a,
                    ..
                },
                TypedPat::Ctor {
                    variant: b,
                    fields: of_b,
                    ..
                },
            ) => {
                if a != b {
                    return Ok(true);
                }
                of_a.iter().zip(of_b)
            }
            (TypedPat::Tuple { elems: a, .. }, TypedPat::Tuple { elems: b, .. }) => a.iter().zip(b),
            (
                TypedPat::Array {
                    prefix: a,
                    rest: rest_a,
                    ..
                },
                TypedPat::Array {
                    prefix: b,
                    rest: rest_b,
                    ..
                },
            ) => {
                let short = |rest: &PatRest, n: &[TypedPat], than: &[TypedPat]| {
                    *rest == PatRest::None && n.len() < than.len()
                };
                if short(rest_a, a, b) || short(rest_b, b, a) {
                    return Ok(true);
                }
                a.iter().zip(b)
            }
            (
                TypedPat::Wild { .. }
                | TypedPat::Bind(_)
                | TypedPat::Lit { .. }
                | TypedPat::Range { .. }
                | TypedPat::Ctor { .. }
                | TypedPat::Tuple { .. }
                | TypedPat::Array { .. }
                | TypedPat::Bin { .. },
                _,
            ) => return Ok(false),
        };
        for (a, b) in parts {
            if self.never_both(a, b)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The first of `rows` that may fit, settled, with those behind it left.
    fn next(&mut self, rows: &mut VecDeque<Row<'p>>) -> R<Option<Row<'p>>> {
        while let Some(row) = rows.pop_front() {
            let mut left = SmallVec::new();
            self.settle(row, 0, &mut left)?;
            let mut left = left.into_iter();
            if let Some(first) = left.next() {
                for row in left.rev() {
                    rows.push_front(row);
                }
                return Ok(Some(first));
            }
        }
        Ok(None)
    }

    /// The tree that tries `rows` in order, and goes on at `fail` when none fits.
    fn rows(&mut self, mut rows: VecDeque<Row<'p>>, fail: Fail) -> R<Node<'p>> {
        let mark = self.facts.mark();
        let mut links: Vec<(RestId, Node<'p>)> = Vec::new();
        let last = loop {
            let Some(row) = self.next(&mut rows)? else {
                break go_on(fail);
            };
            match self.ask(row, &mut rows, fail)? {
                Asked::Done(node) => break node,
                Asked::Link(id, node) => links.push((id, node)),
                Asked::Again(row) => rows.push_front(row),
            }
        };
        self.facts.back_to(mark);
        if links.is_empty() {
            return Ok(last);
        }
        Ok(Node::Chain {
            links,
            last: Box::new(last),
        })
    }

    /// Ask what `row` has to know first. It is settled, and `rows` are those behind
    /// it. What it leaves in `rows` is tried next.
    fn ask(&mut self, row: Row<'p>, rows: &mut VecDeque<Row<'p>>, fail: Fail) -> R<Asked<'p>> {
        if self.takes_all(&row) {
            let Row { goal, binds, .. } = row;
            return Ok(Asked::Done(Node::Leaf(Leaf {
                goal,
                binds,
                fail: None,
            })));
        }
        match self.choose(&row, rows)? {
            Some((at, Question::Which)) => self.switch(row, at, rows, fail),
            Some((at, Question::Long(test))) => self.long(row, at, test, rows, fail),
            Some((at, Question::Within(on, range))) => {
                let mut row = row;
                row.checks.remove(at);
                self.by_itself(row, rows, fail, |yes, fail| Node::Within {
                    on,
                    range,
                    yes: Box::new(yes),
                    fail,
                })
            }
            None => self.first(row, rows, fail),
        }
    }

    /// Where a question goes on when the answer is no, and the id of it when that is
    /// the rows behind, of which there are some.
    fn behind(&mut self, rows: &VecDeque<Row<'p>>, fail: Fail) -> R<(Option<RestId>, Fail)> {
        if rows.is_empty() {
            return Ok((None, fail));
        }
        let id = self.rest()?;
        Ok((Some(id), Some(id)))
    }

    /// Ask something of `row` alone. `node` makes the question, out of what is done
    /// when the answer is yes.
    fn by_itself(
        &mut self,
        row: Row<'p>,
        rows: &VecDeque<Row<'p>>,
        fail: Fail,
        node: impl FnOnce(Node<'p>, Fail) -> Node<'p>,
    ) -> R<Asked<'p>> {
        let (behind, fail) = self.behind(rows, fail)?;
        let yes = self.rows(VecDeque::from([row]), fail)?;
        Ok(asked(behind, node(yes, fail)))
    }

    /// Do what `row` has to do first when that is no question that others may share:
    /// its guard, an or-pattern, or what it reads of a binary.
    fn first(&mut self, mut row: Row<'p>, rows: &VecDeque<Row<'p>>, fail: Fail) -> R<Asked<'p>> {
        let Some(Check { on, what, .. }) = row.checks.first().cloned() else {
            let (behind, fail) = self.behind(rows, fail)?;
            let Row { goal, binds, .. } = row;
            return Ok(asked(behind, Node::Leaf(Leaf { goal, binds, fail })));
        };
        match what {
            What::Any { alts, names } => {
                row.checks.remove(0);
                let id = AnyId(self.anys);
                let at = self.anys as usize;
                self.anys = self
                    .anys
                    .checked_add(1)
                    .ok_or(InternalLinkError::IndexTooWide(at))?;
                let binds = match row.sizes {
                    true => row.binds.clone(),
                    false => Vec::new(),
                };
                let mut given = Vec::with_capacity(names.len());
                for b in names.iter() {
                    given.push((*b, self.places.add(Source::Given, b.ty)?));
                }
                row.binds.extend(&given);
                let tried = alts.iter().map(|checks| Row {
                    goal: Goal::Any(id),
                    checks: checks.clone(),
                    binds: Vec::new(),
                    once: row.sizes,
                    sizes: row.sizes,
                    in_order: row.in_order,
                });
                let tried: VecDeque<Row<'p>> = tried.collect();
                let (behind, fail) = self.behind(rows, fail)?;
                let first = Box::new(self.rows(tried, fail)?);
                let then = Box::new(self.rows(VecDeque::from([row]), fail)?);
                let node = Node::Any {
                    id,
                    binds,
                    names: given,
                    first,
                    then,
                };
                Ok(asked(behind, node))
            }
            What::Bits(bits) => self.bits(row, on, bits, rows, fail),
            What::Pat(TypedPat::Bin {
                zero, segs, rest, ..
            }) => {
                let ty = self.given.temps.int;
                let bits = Bits {
                    cursor: self.places.part(on, Part::Start(*zero), ty)?,
                    total: self.places.part(on, Part::Bits, ty)?,
                    zero: *zero,
                    segs,
                    rest: *rest,
                };
                self.bits(row, on, bits, rows, fail)
            }
            // Each of these is a question.
            What::Pat(_) | What::OneOf(_) => Err(InternalLinkError::OddRow),
        }
    }

    /// Read the next segment of the binary at `on`, which is what `row` does first.
    fn bits(
        &mut self,
        mut row: Row<'p>,
        on: Place,
        bits: Bits<'p>,
        rows: &VecDeque<Row<'p>>,
        fail: Fail,
    ) -> R<Asked<'p>> {
        let Bits {
            cursor,
            total,
            zero,
            segs,
            rest,
        } = bits;
        let Some((seg, segs)) = segs.split_first() else {
            row.checks.remove(0);
            return match rest {
                PatRest::None => self.by_itself(row, rows, fail, |yes, fail| Node::Ends {
                    total,
                    cursor,
                    yes: Box::new(yes),
                    fail,
                }),
                PatRest::Discard => Ok(Asked::Again(row)),
                PatRest::Bind(b) => {
                    if self.used(&b) {
                        let left = self.places.part(on, Part::From { cursor, total }, b.ty)?;
                        row.binds.push((b, left));
                    }
                    Ok(Asked::Again(row))
                }
            };
        };
        let next = self.places.add(Source::Given, self.given.temps.int)?;
        if let Some(check) = row.checks.first_mut() {
            check.what = What::Bits(Bits {
                cursor: next,
                total,
                zero,
                segs,
                rest,
            });
        }
        let mut value = None;
        if let Some(p) = value_of(seg).filter(|p| !matches!(p, TypedPat::Wild { .. })) {
            let at = self.places.add(Source::Given, p.ty())?;
            let checks = self.checks([(at, p)], row.in_order);
            row.checks.splice(0..0, checks);
            value = Some(at);
        }
        let binds = row.binds.clone();
        self.by_itself(row, rows, fail, |yes, fail| {
            Node::Segment(Box::new(Segment {
                on,
                cursor,
                total,
                zero,
                seg,
                binds,
                value,
                next,
                yes,
                fail,
            }))
        })
    }

    /// The question to ask for `row`, as which of its checks asks it, when it has one
    /// that others may share. It is the one that the most rows ask.
    fn choose(
        &mut self,
        row: &Row<'p>,
        rows: &mut VecDeque<Row<'p>>,
    ) -> R<Option<(usize, Question)>> {
        let mut all = Vec::new();
        for (at, check) in visible(row).iter().enumerate() {
            let question = match &check.what {
                What::OneOf(_) | What::Pat(TypedPat::Ctor { .. } | TypedPat::Lit { .. }) => {
                    Question::Which
                }
                What::Pat(TypedPat::Range { lo, hi, .. }) => Question::Within(check.on, [*lo, *hi]),
                What::Pat(TypedPat::Array {
                    len, prefix, rest, ..
                }) => Question::Long(Length {
                    n: prefix.len(),
                    pooled: *len,
                    or_more: *rest != PatRest::None,
                }),
                What::Pat(
                    TypedPat::Wild { .. }
                    | TypedPat::Bind(_)
                    | TypedPat::Tuple { .. }
                    | TypedPat::Bin { .. }
                    | TypedPat::Or { .. },
                )
                | What::Bits(_)
                | What::Any { .. } => continue,
            };
            all.push((at, check.on, question));
        }
        if all.len() > 1 && !rows.is_empty() {
            let mut near = SmallVec::new();
            for row in rows.drain(..rows.len().min(LOOKED_AT)).collect::<Vec<_>>() {
                self.settle(row, 0, &mut near)?;
            }
            let mut asks: HashMap<Place, usize> = HashMap::new();
            for check in near.iter().flat_map(visible) {
                let asks = asks.entry(check.on).or_default();
                *asks = asks.saturating_add(1);
            }
            for row in near.into_iter().rev() {
                rows.push_front(row);
            }
            // The first of those that as many ask.
            let by = |on: &Place| std::cmp::Reverse(asks.get(on).copied().unwrap_or(0));
            all.sort_by_key(|(_, on, _)| by(on));
        }
        let mut all = all.into_iter();
        Ok(all.next().map(|(at, _, question)| (at, question)))
    }

    /// Switch on what check `at` of `first` asks, for that row and those of `rows`
    /// that need not be copied more than there is allowance for.
    fn switch(
        &mut self,
        mut first: Row<'p>,
        at: usize,
        rows: &mut VecDeque<Row<'p>>,
        fail: Fail,
    ) -> R<Asked<'p>> {
        let Some(check) = first.checks.get_mut(at) else {
            return Err(InternalLinkError::OddRow);
        };
        let on = check.on;
        // Alternatives that cannot all be a case with the rest of the row in it come
        // together again in front of that.
        if let What::OneOf(alts) = &check.what {
            let more = alts.len().saturating_sub(1);
            let alts = alts.clone();
            let cost = self.cost(&first).saturating_sub(1).saturating_mul(more);
            if cost > self.allowance {
                if self.all_there_are(on, &alts) {
                    first.checks.remove(at);
                    return Ok(Asked::Again(first));
                }
                let lists = alts.iter().map(|p| self.checks([(on, *p)], first.in_order));
                let alts = lists.collect();
                if let Some(check) = first.checks.get_mut(at) {
                    check.what = What::Any {
                        alts,
                        names: Rc::new([]),
                    };
                }
                return Ok(Asked::Again(first));
            }
        }
        let mut split = Split {
            on,
            cases: Vec::new(),
            of: BTreeMap::new(),
            open: BTreeSet::new(),
            default: Case::default(),
            keys: None,
            placed: 0,
        };
        // The first is what the switch is for, so it is placed whatever that takes.
        let mut cut = match self.place(&mut split, first, true)? {
            Placed::Yes => false,
            Placed::No(_) => return Err(InternalLinkError::OddRow),
        };
        while !cut && let Some(row) = rows.pop_front() {
            if split.full() {
                rows.clear();
                break;
            }
            let mut settled = SmallVec::new();
            self.settle(row, 0, &mut settled)?;
            let mut left = Vec::new();
            for row in settled {
                if cut {
                    left.push(row);
                    continue;
                }
                match self.place(&mut split, row, false)? {
                    Placed::Yes => {}
                    Placed::No(row) => {
                        cut = true;
                        left.push(row);
                    }
                }
            }
            for row in left.into_iter().rev() {
                rows.push_front(row);
            }
        }
        let (behind, fail) = match cut {
            true => self.behind(rows, fail)?,
            false => (None, fail),
        };
        let covered = split.covered();
        // Cases with the same rows are one.
        let mut merged: Vec<(Vec<Key>, Case<'p>)> = Vec::with_capacity(split.cases.len());
        let mut with: BTreeMap<Vec<usize>, usize> = BTreeMap::new();
        for (key, case) in split.cases {
            let known = case.own.as_ref().and_then(|rows| with.get(rows));
            if let Some((keys, _)) = known.and_then(|at| merged.get_mut(*at)) {
                keys.push(key);
                continue;
            }
            if let Some(rows) = case.own.clone() {
                with.insert(rows, merged.len());
            }
            merged.push((vec![key], case));
        }
        let mut branches = Vec::with_capacity(merged.len());
        for (keys, case) in merged {
            let mark = self.facts.mark();
            let mut apart = Vec::new();
            if let [key] = keys.as_slice() {
                self.facts.set(on, |f| f.key = Some(*key));
                if fail.is_none() && (on != ROOT || self.alone) {
                    apart = self.apart(on, &case.rows);
                }
            } else {
                let keys = Rc::new(keys.iter().copied().collect());
                self.facts.set(on, |f| f.one_of = Some(keys));
            }
            let node = self.rows(case.rows, fail)?;
            self.facts.back_to(mark);
            branches.push(Branch { keys, apart, node });
        }
        // Where every constructor is one case, there is nothing to ask.
        if covered
            && let [branch] = branches.as_slice()
            && branch.keys.len() > 1
            && let Some(Branch { node, .. }) = branches.pop()
        {
            return Ok(asked(behind, node));
        }
        let node = |default| Node::Switch {
            on,
            branches,
            default,
        };
        if covered {
            return Ok(asked(behind, node(None)));
        }
        let mark = self.facts.mark();
        self.facts.set(on, |f| {
            let before = f.none_of.iter().flat_map(|keys| keys.iter().copied());
            let all = before.chain(split.of.into_keys()).collect();
            f.none_of = Some(Rc::new(all));
        });
        if self.depth >= DEEP && behind.is_none() && !split.default.rows.is_empty() {
            let id = self.rest()?;
            *rows = split.default.rows;
            return Ok(Asked::Link(id, node(Some(Box::new(Node::Rest(id))))));
        }
        self.depth = self.depth.saturating_add(1);
        let default = self.rows(split.default.rows, fail)?;
        self.depth = self.depth.saturating_sub(1);
        self.facts.back_to(mark);
        Ok(asked(behind, node(Some(Box::new(default)))))
    }

    /// Put `row` in the cases it may be tried in. With `whatever`, that is done
    /// whatever it takes.
    fn place(&mut self, split: &mut Split<'p>, row: Row<'p>, whatever: bool) -> R<Placed<'p>> {
        let number = split.placed;
        let at = visible(&row).iter().position(|c| c.on == split.on);
        let what = at.and_then(|at| row.checks.get(at)).map(|c| &c.what);
        let alts: Pats<'p> = match what {
            Some(What::Pat(p @ (TypedPat::Ctor { .. } | TypedPat::Lit { .. }))) => smallvec![*p],
            Some(What::OneOf(alts)) => alts.iter().copied().collect(),
            // It is of the same values, and no case decides it.
            Some(What::Pat(TypedPat::Range { .. })) => return Ok(Placed::No(row)),
            Some(What::Pat(_) | What::Bits(_) | What::Any { .. }) | None => {
                let default = split.default_open();
                let targets = split.open.len().saturating_add(usize::from(default));
                let copies = targets.saturating_sub(1);
                let cost = self.cost(&row).saturating_mul(copies);
                if !self.affords(cost) {
                    return Ok(Placed::No(row));
                }
                let closes = self.takes_all(&row);
                for at in &split.open {
                    if let Some((_, case)) = split.cases.get_mut(*at) {
                        case.add(row.clone(), closes);
                    }
                }
                if closes {
                    split.open.clear();
                }
                if default {
                    split.default.add(row, closes);
                }
                return Ok(Placed::Yes);
            }
        };
        let Some(at) = at else {
            return Ok(Placed::Yes);
        };
        // Each key, with those of its alternatives that fit a value with that key.
        let mut keys: SmallVec<[(Key, Pats<'p>); 1]> = SmallVec::new();
        let mut of: BTreeMap<Key, usize> = BTreeMap::new();
        for p in alts {
            let key = match p {
                TypedPat::Ctor { variant, .. } => {
                    split.keys = self.variants(*variant);
                    Key::Variant(*variant)
                }
                TypedPat::Lit { value, .. } => self.key(*value)?,
                TypedPat::Wild { .. }
                | TypedPat::Bind(_)
                | TypedPat::Tuple { .. }
                | TypedPat::Array { .. }
                | TypedPat::Bin { .. }
                | TypedPat::Or { .. }
                | TypedPat::Range { .. } => return Err(InternalLinkError::OddRow),
            };
            match of.get(&key).and_then(|at| keys.get_mut(*at)) {
                Some((_, alts)) => alts.push(p),
                None => {
                    of.insert(key, keys.len());
                    keys.push((key, smallvec![p]));
                }
            }
        }
        // It goes to each of them, and a new case has what no case is for so far.
        let more = keys.len().saturating_sub(1);
        let mut cost = self.cost(&row).saturating_sub(1).saturating_mul(more);
        let new = keys.iter().filter(|(key, _)| !split.of.contains_key(key));
        let so_far = split.default.rows.iter().map(|row| self.cost(row));
        let so_far = so_far.fold(0, usize::saturating_add);
        cost = cost.saturating_add(so_far.saturating_mul(new.count()));
        if !self.affords(cost) {
            if !whatever {
                return Ok(Placed::No(row));
            }
            self.allowance = 0;
        }
        split.placed = number.saturating_add(1);
        let last = keys.len().saturating_sub(1);
        let (in_order, sizes, once) = (row.in_order, row.sizes, row.once);
        let waits = row.checks.get(at).is_some_and(|c| c.first);
        let each = self.cost(&row);
        let mut row = Some(row);
        for (i, (key, alts)) in keys.into_iter().enumerate() {
            let case = match split.of.get(&key) {
                Some(case) => *case,
                None if split.default.closed => continue,
                None => {
                    let case = split.cases.len();
                    split.of.insert(key, case);
                    split.open.insert(case);
                    let so_far = Case {
                        rows: split.default.rows.clone(),
                        closed: false,
                        own: Some(Vec::new()),
                    };
                    split.cases.push((key, so_far));
                    case
                }
            };
            let Some((_, into)) = split.cases.get_mut(case).filter(|(_, c)| !c.closed) else {
                continue;
            };
            let mut lists: SmallVec<[Vec<Check<'p>>; 1]> = SmallVec::new();
            for p in alts {
                let mut parts = Vec::new();
                if let TypedPat::Ctor {
                    variant, fields, ..
                } = p
                {
                    parts = self.fields(split.on, *variant, fields)?;
                }
                lists.push(self.checks(parts, in_order));
            }
            // One of several that asks no more is enough, unless a size is read in
            // front of it.
            if lists.len() > 1 && !sizes && lists.iter().any(Vec::is_empty) {
                lists = smallvec![Vec::new()];
            }
            let plain = lists.iter().all(Vec::is_empty);
            let more = lists.len().saturating_sub(1);
            let extra = each.saturating_mul(more);
            if more > 0 && (once || !self.affords(extra)) {
                lists = smallvec![vec![Check {
                    on: split.on,
                    first: waits,
                    what: What::Any {
                        alts: lists.into_vec().into(),
                        names: Rc::new([]),
                    },
                }]];
            }
            let more = lists.len().saturating_sub(1);
            for (j, list) in lists.into_iter().enumerate() {
                let one = match i == last && j == more {
                    true => row.take(),
                    false => row.clone(),
                };
                let mut one = one.ok_or(InternalLinkError::OddRow)?;
                one.checks.splice(at..=at, list);
                let mut settled = SmallVec::new();
                self.settle(one, at, &mut settled)?;
                for row in settled {
                    let closes = self.takes_all(&row);
                    into.add(row, closes);
                }
            }
            into.asked(number, plain);
            if into.closed {
                split.open.remove(&case);
            }
        }
        Ok(Placed::Yes)
    }

    /// The fields of the constructor at `on` that `rows` need, which are tried for it.
    /// There are none when something in them needs the constructor itself.
    fn apart(&self, on: Place, rows: &VecDeque<Row<'p>>) -> Vec<Place> {
        let mut needs = Vec::new();
        for row in rows {
            needs.extend(row.binds.iter().map(|(_, p)| *p));
            asked_of(&row.checks, &mut needs);
        }
        if needs.contains(&on) {
            return Vec::new();
        }
        let parts = needs.iter().filter_map(|p| self.places.part_of(on, *p));
        let all: BTreeSet<Place> = parts.collect();
        all.into_iter().collect()
    }

    /// Ask whether the array that check `at` of `first` is of passes `test`, for that
    /// row and those of `rows` that need not be copied more than there is allowance
    /// for.
    fn long(
        &mut self,
        first: Row<'p>,
        at: usize,
        test: Length,
        rows: &mut VecDeque<Row<'p>>,
        fail: Fail,
    ) -> R<Asked<'p>> {
        let Some(on) = first.checks.get(at).map(|c| c.on) else {
            return Err(InternalLinkError::OddRow);
        };
        let [mut yes, mut no] = [Case::default(), Case::default()];
        let mut cut = None;
        let mut next = Some(first);
        while let Some(row) = next {
            let mut settled = SmallVec::new();
            self.settle(row, 0, &mut settled)?;
            for row in settled {
                let sides = match &cut {
                    None => self.sides(on, test, &row),
                    Some(_) => [false; 2],
                };
                let [in_yes, in_no] = [sides[0] && !yes.closed, sides[1] && !no.closed];
                let cost = self.cost(&row);
                if cut.is_some() || (in_yes && in_no && !self.affords(cost)) {
                    cut.get_or_insert_with(Vec::new).push(row);
                    continue;
                }
                if in_yes {
                    let mark = self.facts.mark();
                    self.facts.set(on, |f| test.passed(f));
                    self.add_settled(&mut yes, row.clone())?;
                    self.facts.back_to(mark);
                }
                if in_no {
                    let mark = self.facts.mark();
                    self.facts.set(on, |f| test.failed(f));
                    self.add_settled(&mut no, row)?;
                    self.facts.back_to(mark);
                }
            }
            if cut.is_some() {
                break;
            }
            if yes.closed && no.closed {
                rows.clear();
            }
            next = rows.pop_front();
        }
        for row in cut.into_iter().flatten().rev() {
            rows.push_front(row);
        }
        let (behind, fail) = self.behind(rows, fail)?;
        let length = self.places.part(on, Part::Length, self.given.temps.int)?;
        let node = |yes, no| Node::Long {
            length,
            test,
            yes: Box::new(yes),
            no: Box::new(no),
        };
        let mark = self.facts.mark();
        self.facts.set(on, |f| test.passed(f));
        let passed = self.rows(yes.rows, fail)?;
        self.facts.back_to(mark);
        self.facts.set(on, |f| test.failed(f));
        if behind.is_some() {
            let failed = self.rows(no.rows, fail)?;
            self.facts.back_to(mark);
            return Ok(asked(behind, node(passed, failed)));
        }
        // What is left is tried behind, where the test is known to have failed.
        *rows = no.rows;
        let (behind, fail) = self.behind(rows, fail)?;
        Ok(asked(behind, node(passed, go_on(fail))))
    }

    /// Whether an array that passes `test` can fit `row`, and whether one that does not
    /// can.
    fn sides(&self, on: Place, test: Length, row: &Row<'p>) -> [bool; 2] {
        let check = visible(row).iter().find(|c| c.on == on);
        let Some(What::Pat(TypedPat::Array { prefix, rest, .. })) = check.map(|c| &c.what) else {
            return [true; 2];
        };
        let (n, or_more) = (prefix.len(), *rest != PatRest::None);
        match (test.or_more, or_more) {
            (false, false) => [n == test.n, n != test.n],
            (false, true) => [n <= test.n, true],
            (true, false) => [n >= test.n, n < test.n],
            (true, true) => [true, n < test.n],
        }
    }

    fn add_settled(&mut self, to: &mut Case<'p>, row: Row<'p>) -> R<()> {
        let mut settled = SmallVec::new();
        self.settle(row, 0, &mut settled)?;
        for row in settled {
            let closes = self.takes_all(&row);
            to.add(row, closes);
        }
        Ok(())
    }
}

/// The node where a way goes on that nothing fits.
fn go_on<'p>(fail: Fail) -> Node<'p> {
    match fail {
        Some(id) => Node::Rest(id),
        None => Node::Nothing,
    }
}

/// What came of a question whose rows behind are at `behind`.
fn asked(behind: Option<RestId>, node: Node<'_>) -> Asked<'_> {
    match behind {
        Some(id) => Asked::Link(id, node),
        None => Asked::Done(node),
    }
}

fn index(i: usize) -> R<u16> {
    u16::try_from(i).map_err(|_| InternalLinkError::IndexTooWide(i))
}

#[cfg(test)]
mod tests;
