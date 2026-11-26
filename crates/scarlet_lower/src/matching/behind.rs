//! What the rows behind a guard still read, and so what is done in front of it.
//!
//! A guard, or a size in a `<<>>` pattern, may call anything. What the match holds while
//! it runs can be seen: a handle that only the array holds closes when the array is given
//! up, and a tail is taken in place only when nothing else holds the array. So what is
//! given up in front of it, and what is kept for the rows behind, follows from what those
//! rows read. This is all decided here, and there are three things to decide.
//!
//! * A tail that the guard does not read waits for it, where a row behind reads the array
//!   again ([`Build::late`]). A guard that fails then leaves the array to those rows, and
//!   one that holds lets the arm take the array over. Where nothing reads the array again,
//!   the tail is taken first and the array is given up with it.
//! * A tail that waited for one guard is taken in front of a later one, once that tail is
//!   all that is read of the array ([`Build::due`]). Otherwise the array would be held for
//!   it.
//! * A length is read in front of a guard, where that is all that is asked of the array
//!   behind it ([`Build::unmeasured`]).
//!
//! The first sixteen rows behind are looked at, and what each [`RestId`] where they go on
//! reads. Where there are more rows than that, they may ask any array for its length, and
//! are taken to read nothing else. A tail is then taken early, which costs a copy of the
//! array's root and changes nothing else.
//!
//! A match with no guard and no such size, or with no array pattern, needs none of this,
//! and then [`Build::runs`] is none.

use std::collections::{BTreeSet, VecDeque};

use smallvec::SmallVec;

use crate::program::InternalLinkError;
use crate::typed_ir::{PatRest, TypedArm, TypedBind, TypedPat, each_read};

use super::rows::{Check, Look, Row, What, holds, reads_size, visible};
use super::{
    Build, Cut, Fail, Goal, LOOKED_AT, On, Piece, Place, Places, R, ROOT, RestId, Source, wide,
};

/// What has to be known of the arms of a match where something runs, which is a guard or
/// a size, between questions of an array.
pub(super) struct Runs {
    /// Whether the guard and the body of each arm read the matched value itself.
    reads_value: Vec<bool>,
}

impl Runs {
    /// `None` when nothing does.
    pub(super) fn of(on: On, arms: &[TypedArm]) -> Option<Runs> {
        let array = |p: &TypedPat| matches!(p, TypedPat::Array { .. });
        let runs = |arm: &TypedArm| arm.guard.is_some() || holds(&arm.pat, &reads_size);
        let both = arms.iter().any(runs) && arms.iter().any(|arm| holds(&arm.pat, &array));
        let reads_value = |arm: &TypedArm| {
            let mut reads = false;
            if let Some(name) = on.name
                && !on.alone
            {
                let exprs = arm.guard.iter().chain(std::iter::once(&arm.body));
                exprs.for_each(|e| each_read(e, |b| reads |= b == name));
            }
            reads
        };
        both.then(|| Runs {
            reads_value: arms.iter().map(reads_value).collect(),
        })
    }
}

/// What is read at a [`RestId`].
pub(super) struct Behind {
    /// The places that are read as a whole or cut, and the arrays that are asked for a
    /// length.
    read: PlaceSet,
    /// Where it goes on when no row fits.
    then: Fail,
}

/// Some places, with a bit for each.
#[derive(Default)]
struct PlaceSet(Vec<u64>);

impl PlaceSet {
    fn insert(&mut self, p: Place) {
        let (at, bit) = (p.index() / 64, p.index() % 64);
        if self.0.len() <= at {
            self.0.resize(at.saturating_add(1), 0);
        }
        if let Some(word) = self.0.get_mut(at) {
            *word |= 1 << bit;
        }
    }

    fn contains(&self, p: Place) -> bool {
        let word = self.0.get(p.index() / 64).copied().unwrap_or(0);
        word >> (p.index() % 64) & 1 == 1
    }

    fn iter(&self) -> impl Iterator<Item = Place> {
        (0u32..).step_by(64).zip(&self.0).flat_map(|(at, word)| {
            let bits = (0..64u32).filter(move |bit| word >> bit & 1 == 1);
            bits.map(move |bit| Place(at.saturating_add(bit)))
        })
    }
}

/// The rows behind something that runs, which are settled.
#[derive(Debug, Clone, Copy)]
pub(super) struct After {
    /// How many of them are looked at.
    near: usize,
    /// Where it goes on when no row fits.
    then: Fail,
}

/// What comes after the rows that are looked at.
struct End {
    /// Whether there are rows that are not looked at.
    more: bool,
    then: Fail,
}

/// Something that is told what rows read.
trait Reader {
    /// Whether it is told of anything but lengths.
    const ALL: bool;

    /// `of` is read as a whole, or cut for a piece of it.
    fn read(&mut self, places: &Places, of: Place, piece: Option<Piece>);

    /// The length of `on` is asked for, and is not read yet.
    fn length(&mut self, on: Place);

    /// Whether it keeps `p`, which a name is for, to be told of later.
    fn keep(&mut self, _: Place) -> bool {
        false
    }

    /// Whether it needs to be told no more.
    fn enough(&self) -> bool;
}

/// Everything that is read.
#[derive(Default)]
struct All {
    read: PlaceSet,
    /// What names are for. Rows behind one another have most of their names for the same
    /// places.
    named: PlaceSet,
}

impl Reader for All {
    const ALL: bool = true;

    fn read(&mut self, _: &Places, of: Place, _: Option<Piece>) {
        self.read.insert(of);
    }

    fn length(&mut self, on: Place) {
        self.read.insert(on);
    }

    fn keep(&mut self, p: Place) -> bool {
        self.named.insert(p);
        true
    }

    fn enough(&self) -> bool {
        false
    }
}

/// The arrays whose length is asked for.
#[derive(Default)]
struct Lengths(Vec<Place>);

impl Reader for Lengths {
    const ALL: bool = false;

    fn read(&mut self, _: &Places, _: Place, _: Option<Piece>) {}

    fn length(&mut self, on: Place) {
        if !self.0.contains(&on) {
            self.0.push(on);
        }
    }

    fn enough(&self) -> bool {
        false
    }
}

/// Whether a place, or something that holds it, is read.
struct Held<'a> {
    /// The place, and everything that it is in.
    holds: SmallVec<[Place; 4]>,
    /// The pieces that do not count, in order.
    but: &'a [Place],
    /// The array whose length does not count.
    length: Option<Place>,
    /// A piece that does not count either, and whether it is read.
    tail: Option<Place>,
    tail_read: bool,
    read: bool,
}

impl Reader for Held<'_> {
    const ALL: bool = true;

    fn read(&mut self, places: &Places, of: Place, piece: Option<Piece>) {
        if !self.holds.contains(&of) {
            return;
        }
        match piece.and_then(|piece| places.found(of, piece)) {
            Some(p) if self.but.binary_search(&p).is_ok() => {}
            Some(p) if self.tail == Some(p) => self.tail_read = true,
            Some(_) | None => self.read = true,
        }
    }

    fn length(&mut self, on: Place) {
        self.read |= self.length != Some(on) && self.holds.contains(&on);
    }

    fn enough(&self) -> bool {
        self.read
    }
}

impl<'p> Build<'_> {
    /// A place for `rows`, which come after others and go on at `then` when none fits.
    pub(super) fn rest(
        &mut self,
        rows: &mut VecDeque<Row<'p>>,
        then: Fail,
        after: Option<After>,
    ) -> R<RestId> {
        let id = RestId(self.rests);
        let at = self.rests as usize;
        self.rests = self
            .rests
            .checked_add(1)
            .ok_or(InternalLinkError::IndexTooWide(at))?;
        if self.runs.is_some() {
            let after = match after {
                Some(after) => after,
                None => self.look_behind(rows, then)?,
            };
            let mut all = All::default();
            let End { then, .. } = self.each_read_by(rows, after, &mut all)?;
            for p in std::mem::take(&mut all.named).iter() {
                self.read_of(p, &mut all);
            }
            let read = all.read;
            self.behind.push(Behind { read, then });
        }
        Ok(id)
    }

    fn look_behind(&mut self, rows: &mut VecDeque<Row<'p>>, then: Fail) -> R<After> {
        let near = self.settle_near(rows)?;
        Ok(After { near, then })
    }

    /// The `rows` behind `row`, and `fail`, where something of `row` runs in front of them.
    pub(super) fn after(
        &mut self,
        row: &Row<'p>,
        rows: &mut VecDeque<Row<'p>>,
        fail: Fail,
    ) -> R<Option<After>> {
        if self.runs.is_none() || !self.may_run(row) {
            return Ok(None);
        }
        self.look_behind(rows, fail).map(Some)
    }

    /// Whether something runs for `row` that may say that it does not fit.
    pub(super) fn may_run(&self, row: &Row<'p>) -> bool {
        row.sizes || self.guarded(row.goal)
    }

    /// Tell `to` what `rows` read, as far as `after` looks at them.
    fn each_read_by(
        &mut self,
        rows: &VecDeque<Row<'p>>,
        after: After,
        to: &mut impl Reader,
    ) -> R<End> {
        for row in rows.iter().take(after.near) {
            if to.enough() {
                break;
            }
            if self.fails_in_one(row)? {
                continue;
            }
            self.read_by(&row.checks, to)?;
            self.read_for_names(row, to);
            if self.takes_all(row) {
                return Ok(End {
                    more: false,
                    then: None,
                });
            }
        }
        Ok(End {
            more: rows.len() > after.near,
            then: after.then,
        })
    }

    /// Tell `to` what `row` reads for its names and for its arm.
    fn read_for_names<T: Reader>(&self, row: &Row<'p>, to: &mut T) {
        if !T::ALL {
            return;
        }
        for (_, p) in &row.binds {
            if to.enough() {
                return;
            }
            if !to.keep(*p) {
                self.read_of(*p, to);
            }
        }
        if let (Goal::Arm(i), Some(runs)) = (row.goal, &self.runs)
            && runs.reads_value.get(i) == Some(&true)
        {
            self.read_of(ROOT, to);
        }
    }

    /// Tell `to` what is read to come by `p`. A piece that is found is not read out of its
    /// array again.
    fn read_of(&self, p: Place, to: &mut impl Reader) {
        match self.unfound(p) {
            Some((of, piece)) => to.read(&self.places, of, Some(piece)),
            None => to.read(&self.places, p, None),
        }
    }

    /// Whether `row`, which is settled, has an or-pattern that is known to fail.
    fn fails_in_one(&mut self, row: &Row<'p>) -> R<bool> {
        for check in visible(row) {
            if let What::Any { alts, .. } = &check.what
                && self.all_fail(alts)?
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn all_fail(&mut self, alts: &[Vec<Check<'p>>]) -> R<bool> {
        for alt in alts {
            if !self.fails(alt)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Whether one of `checks` is known to fail. Those behind one that they wait for are
    /// not looked at.
    fn fails(&mut self, checks: &[Check<'p>]) -> R<bool> {
        for check in checks {
            let fails = match &check.what {
                What::Pat(p) => self.never(check.on, p)?,
                What::Any { alts, .. } => self.all_fail(alts)?,
                What::OneOf(_) | What::Bits(_) => false,
            };
            if fails || check.first {
                return Ok(fails);
            }
        }
        Ok(false)
    }

    /// Tell `to` what `checks` read, none of which is known to fail.
    fn read_by<T: Reader>(&mut self, checks: &[Check<'p>], to: &mut T) -> R<()> {
        for check in checks {
            match &check.what {
                What::Pat(p) => self.read_for(check.on, p, to)?,
                // These ask which of several it is, and that reads no part of it.
                What::OneOf(_) | What::Bits(_) => self.read_of(check.on, to),
                What::Any { alts, .. } => {
                    for alt in alts.iter() {
                        if !self.fails(alt)? {
                            self.read_by(alt, to)?;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Tell `to` what is read to see whether what is at `on` fits `p`.
    fn read_for<T: Reader>(&mut self, on: Place, p: &'p TypedPat, to: &mut T) -> R<()> {
        if to.enough() || self.asks_nothing(p) {
            return Ok(());
        }
        match p {
            TypedPat::Array { prefix, rest, .. } => {
                match self.unfound(on) {
                    Some((of, piece)) => to.read(&self.places, of, Some(piece)),
                    None if self.facts.get(on).is_some_and(|f| f.measured) => {}
                    None => to.length(on),
                }
                for (i, p) in (0u32..).zip(prefix) {
                    if !T::ALL && !asks_lengths(p) || self.asks_nothing(p) {
                        continue;
                    }
                    let piece = Piece::Element(i);
                    match self.found(on, piece) {
                        Some(element) => self.read_for(element, p, to)?,
                        None => to.read(&self.places, on, Some(piece)),
                    }
                }
                if let PatRest::Bind(b) = rest
                    && self.used(b)
                {
                    let tail = Piece::Tail(wide(prefix.len())?);
                    to.read(&self.places, on, Some(tail));
                }
            }
            TypedPat::Tuple { .. } => {
                if let Look::Parts(parts, _) = self.look(on, p)? {
                    for (on, p) in parts {
                        self.read_for(on, p, to)?;
                    }
                }
            }
            TypedPat::Or { alts, .. } => {
                for p in alts {
                    self.read_for(on, p, to)?;
                }
            }
            TypedPat::Wild { .. }
            | TypedPat::Bind(_)
            | TypedPat::Lit { .. }
            | TypedPat::Range { .. }
            | TypedPat::Ctor { .. }
            | TypedPat::Bin { .. } => self.read_of(on, to),
        }
        Ok(())
    }

    /// Whether `on`, or something that holds it, is read by `rows` or where they go on,
    /// with nothing that `held` leaves out counted.
    fn is_held(&mut self, held: &mut Held<'_>, rows: &VecDeque<Row<'p>>, after: After) -> R<bool> {
        let mut then = self.each_read_by(rows, after, held)?.then;
        for _ in 1..LOOKED_AT {
            let Some(behind) = then.and_then(|id| self.behind.get(id.index())) else {
                break;
            };
            held.read |= held.holds.iter().any(|p| behind.read.contains(*p));
            if held.read {
                break;
            }
            then = behind.then;
        }
        Ok(held.read)
    }

    /// What is asked of whether `on` is held, with the pieces in `but` left out, which are
    /// in order.
    fn held<'a>(&self, on: Place, but: &'a [Place]) -> Held<'a> {
        let mut holds = SmallVec::from_slice(&[on]);
        let mut at = on;
        while let Some(of) = self.places.parent(at) {
            holds.push(of);
            at = of;
        }
        Held {
            holds,
            but,
            length: None,
            tail: None,
            tail_read: false,
            read: false,
        }
    }

    /// The lengths that are read in front of a question whose rows behind are
    /// `rows`, and go on at `fail`, so that no array is held for one alone where
    /// something in front of them `runs`.
    pub(super) fn measure(
        &mut self,
        runs: bool,
        rows: &mut VecDeque<Row<'p>>,
        fail: Fail,
    ) -> R<Vec<Place>> {
        let mut lengths = Vec::new();
        if !runs || rows.is_empty() || self.runs.is_none() {
            return Ok(lengths);
        }
        let after = self.look_behind(rows, fail)?;
        while let Some(of) = self.unmeasured(&[], rows, Some(after))? {
            self.facts.set(of, |f| f.measured = true);
            lengths.push(self.length(of)?);
        }
        Ok(lengths)
    }

    /// An array whose length is asked for in what comes `after`, and of which nothing else
    /// is read there. With its length read first, the array is given up in front of what
    /// runs before that. Where not all that comes after is looked at, it may be any that
    /// was asked for one. A piece that is in `names` is found for the rows behind too.
    #[inline(never)]
    pub(super) fn unmeasured(
        &mut self,
        names: &[(TypedBind, Place)],
        rows: &VecDeque<Row<'p>>,
        after: Option<After>,
    ) -> R<Option<Place>> {
        let Some(after) = after else {
            return Ok(None);
        };
        let mut lengths = Lengths::default();
        let more = self.each_read_by(rows, after, &mut lengths)?.more;
        let mut all = lengths.0;
        // Before anything is asked of it the array is held in any case.
        all.retain(|on| self.asked.contains(on) || self.facts.get(*on).is_some());
        for on in self.asked.iter().filter(|_| more) {
            if !all.contains(on) && !self.facts.get(*on).is_some_and(|f| f.measured) {
                all.push(*on);
            }
        }
        if all.is_empty() {
            return Ok(None);
        }
        // A tail that was put off may be taken first too.
        let but = in_order(names.iter().chain(&self.put_off));
        for on in all {
            let mut held = self.held(on, &but);
            held.length = Some(on);
            if !self.is_held(&mut held, rows, after)? {
                return Ok(Some(on));
            }
        }
        Ok(None)
    }

    /// The tails that `row` names and that are taken only once what runs next has: those
    /// that it does not read, which is what is in `reads`, of arrays that are read again
    /// in what comes `after` when it fails. One of an array that nothing reads again is
    /// taken first, and the array goes with it.
    pub(super) fn late(
        &mut self,
        row: &Row<'p>,
        reads: &BTreeSet<usize>,
        rows: &VecDeque<Row<'p>>,
        after: Option<After>,
    ) -> R<Vec<(TypedBind, Place)>> {
        let mut late = Vec::new();
        let Some(after) = after else {
            return Ok(late);
        };
        let mut named: Option<Vec<Place>> = None;
        for (b, p) in &row.binds {
            let Some((
                Source::Found {
                    of,
                    piece: Piece::Tail(_),
                },
                _,
            )) = self.places.get(*p)
            else {
                continue;
            };
            if self.is_found(*p) || reads.contains(&b.id.0) {
                continue;
            }
            let named = named.get_or_insert_with(|| in_order(&row.binds));
            let mut held = self.held(of, named);
            if self.is_held(&mut held, rows, after)? {
                late.push((*b, *p));
            }
        }
        Ok(late)
    }

    /// What is cut out of an array first where `binds` are bound: what is missing of them,
    /// or a tail that is due.
    pub(super) fn first_cut(
        &mut self,
        row: &Row<'p>,
        binds: &[(TypedBind, Place)],
        rows: &VecDeque<Row<'p>>,
        after: Option<After>,
    ) -> R<Option<Box<Cut>>> {
        match (self.missing(binds), after) {
            (Some(cut), _) => Ok(Some(cut)),
            (None, Some(after)) => self.due(row, rows, after),
            (None, None) => Ok(None),
        }
    }

    /// The cut that takes a tail that was put off, where nothing else of its array is read
    /// in what comes `after` any more. The array is given up with it.
    #[inline(never)]
    fn due(
        &mut self,
        row: &Row<'p>,
        rows: &VecDeque<Row<'p>>,
        after: After,
    ) -> R<Option<Box<Cut>>> {
        if self.put_off.is_empty() {
            return Ok(None);
        }
        // What `row` names itself is found for the rows behind too.
        let named = in_order(&row.binds);
        for (b, p) in self.put_off.clone() {
            let Some((of, _)) = self.unfound(p) else {
                continue;
            };
            let mut held = self.held(of, &named);
            held.tail = Some(p);
            if !self.is_held(&mut held, rows, after)? && held.tail_read {
                return Ok(self.missing(&[(b, p)]));
            }
        }
        Ok(None)
    }
}

/// The places of `names`, in order.
fn in_order<'a>(names: impl IntoIterator<Item = &'a (TypedBind, Place)>) -> Vec<Place> {
    let places: BTreeSet<Place> = names.into_iter().map(|(_, p)| *p).collect();
    places.into_iter().collect()
}

/// Whether `p` may ask for the length of an array, with no constructor in between.
fn asks_lengths(p: &TypedPat) -> bool {
    match p {
        TypedPat::Array { .. } | TypedPat::Tuple { .. } | TypedPat::Or { .. } => true,
        TypedPat::Wild { .. }
        | TypedPat::Bind(_)
        | TypedPat::Lit { .. }
        | TypedPat::Range { .. }
        | TypedPat::Ctor { .. }
        | TypedPat::Bin { .. } => false,
    }
}

#[cfg(test)]
mod tests;
