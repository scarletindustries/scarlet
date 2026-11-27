//! The cell of a short array: some slots, each of which holds a value, and behind
//! them one word that is none. [`crate::array`] says what that word is and which of
//! the slots are the array's.

use super::{
    CLASS_AND_COUNT, Cell, Class, DeadCell, Fault, Heap, Index, Kind, Make, STRIDE, class, scan,
    with_class,
};
use crate::value::Value;

const FLAT: Make = Make::of(Kind::ArrayFlat);

/// What a short array's header says in those bits when it has one holder.
const ALONE: u64 = with_class(1, Class::of(Kind::ArrayFlat));

/// The slots and the last word of a short array's cell, for its one holder to change.
pub(crate) struct Alone<'h> {
    pub(crate) slots: &'h mut [u64],
    pub(crate) last: &'h mut u64,
}

/// Some slots of a short array's cell: `n` of them from slot `at` on, and from the first on
/// past the last.
#[derive(Clone, Copy)]
pub(crate) struct Run {
    pub(crate) of: Cell,
    pub(crate) at: usize,
    pub(crate) n: usize,
}

impl Heap {
    /// The slots and the last word of `cell`, when it is a short array's.
    #[inline(always)]
    pub(crate) fn flat(&self, cell: Cell) -> Option<(&[u64], u64)> {
        let (h, words) = self.arena.get(cell.index().at()..)?.split_first()?;
        if !class(*h).is(Kind::ArrayFlat) {
            return None;
        }
        let (slots, rest) = words.split_at_checked(scan(*h))?;
        Some((slots, *rest.first()?))
    }

    /// The items of `cell`, when it is the leaf of an array's tree. A short array's cell may
    /// be one, and its slots are then all items.
    #[inline(always)]
    pub(crate) fn leaf(&self, cell: Cell) -> Option<&[u64]> {
        let (h, words) = self.arena.get(cell.index().at()..)?.split_first()?;
        let is_leaf = class(*h).is(Kind::ArrayLeaf) || class(*h).is(Kind::ArrayFlat);
        words.get(..scan(*h)).filter(|_| is_leaf)
    }

    /// [`Self::flat`], when `cell` has one holder too.
    #[inline(always)]
    pub(crate) fn flat_alone(&mut self, cell: Cell) -> Option<Alone<'_>> {
        let (h, words) = self.arena.get_mut(cell.index().at()..)?.split_first_mut()?;
        if *h & CLASS_AND_COUNT != ALONE {
            return None;
        }
        let (slots, rest) = words.split_at_mut_checked(scan(*h))?;
        let last = rest.first_mut()?;
        Some(Alone { slots, last })
    }

    /// A new cell of `slots` slots, the first of which hold `values` and their
    /// references, and the others `Nil`, with `last` behind them, in the space of a
    /// freed one. It is `None` when there is none of the size, and `values` has then
    /// been asked for nothing.
    #[inline(always)]
    pub(crate) fn flat_in_freed(
        &mut self,
        slots: usize,
        values: impl Iterator<Item = Value>,
        last: u64,
    ) -> Option<Cell> {
        let (cell, words) = self.freed(FLAT, slots.wrapping_add(1))?;
        let (behind, words) = words.split_last_mut()?;
        *behind = last;
        let mut left = words.iter_mut();
        for (v, word) in values.zip(left.by_ref()) {
            *word = v.bits();
        }
        for word in left {
            *word = Value::NIL.bits();
        }
        Some(cell)
    }

    /// The same at the arena's end. One that is refused has asked `values` for nothing.
    #[inline(always)]
    pub(crate) fn flat_at_end(
        &mut self,
        slots: usize,
        values: impl Iterator<Item = Value>,
        last: u64,
    ) -> Result<Cell, Fault> {
        let n = slots.saturating_add(1);
        let at = self.room(n.saturating_add(1))?;
        let index = Index::new(at).ok_or(Fault::Full)?;
        self.arena.push(FLAT.header(n));
        self.arena.extend(values.take(slots).map(Value::bits));
        self.arena.resize(at.saturating_add(n), Value::NIL.bits());
        self.arena.push(last);
        self.made = self.made.wrapping_add(1);
        Ok(Cell::new(index, 0))
    }

    /// The same wherever there is room.
    #[inline(always)]
    pub(crate) fn flat_of(
        &mut self,
        slots: usize,
        values: &[Value],
        last: u64,
    ) -> Result<Cell, Fault> {
        match self.flat_in_freed(slots, values.iter().copied(), last) {
            Some(cell) => Ok(cell),
            None => self.flat_of_at_end(slots, values, last),
        }
    }

    #[inline(never)]
    fn flat_of_at_end(&mut self, slots: usize, values: &[Value], last: u64) -> Result<Cell, Fault> {
        self.flat_at_end(slots, values.iter().copied(), last)
    }

    /// A new cell of `slots` slots. From slot `at` on they hold `before`, whose references
    /// pass to it, then what `run` holds, and then `after` and their references. The others
    /// are `Nil`, and `last` is behind them. The caller gives the references of what `run`
    /// holds. There are slots for all of it behind `at`.
    #[inline(always)]
    pub(crate) fn flat_beside(
        &mut self,
        (before, run, after): (&[Value], Run, &[Value]),
        (slots, at): (usize, usize),
        last: u64,
    ) -> Result<Cell, Fault> {
        let new = self.blank(FLAT, slots.wrapping_add(1))?;
        let put = at.wrapping_add(before.len());
        self.copy_run(run, new, put);
        if let Some((behind, words)) = self.data_mut(new).split_last_mut() {
            *behind = last;
            let (room, words) = words.split_at_mut_checked(at).unwrap_or_default();
            room.fill(Value::NIL.bits());
            for (word, v) in words.iter_mut().zip(before) {
                *word = v.bits();
            }
            let end = before.len().wrapping_add(run.n);
            let mut left = words.iter_mut().skip(end);
            for (v, word) in after.iter().zip(left.by_ref()) {
                *word = v.bits();
            }
            for word in left {
                *word = Value::NIL.bits();
            }
        }
        Ok(new)
    }

    /// The same of what two runs hold, one after the other.
    #[inline(always)]
    pub(crate) fn flat_joined(
        &mut self,
        [a, b]: [Run; 2],
        slots: usize,
        last: u64,
    ) -> Result<Cell, Fault> {
        let new = self.blank(FLAT, slots.wrapping_add(1))?;
        self.copy_run(a, new, 0);
        self.copy_run(b, new, a.n);
        if let Some((behind, words)) = self.data_mut(new).split_last_mut() {
            *behind = last;
            for word in words.iter_mut().skip(a.n.wrapping_add(b.n)) {
                *word = Value::NIL.bits();
            }
        }
        Ok(new)
    }

    /// Copy what `run` holds to the slots of `new` from `put` on.
    #[inline(always)]
    fn copy_run(&mut self, run: Run, new: Cell, put: usize) {
        // Up to the cell's last slot, and the rest from the first one on.
        let slots = scan(self.header(run.of.index()));
        let near = run.n.min(slots.wrapping_sub(run.at));
        self.copy_words(run.of, run.at..run.at.wrapping_add(near), new, put);
        if near < run.n {
            let far = run.n.wrapping_sub(near);
            self.copy_words(run.of, 0..far, new, put.wrapping_add(near));
        }
    }

    /// Add a reference to what each of `slots` of a short array's cell holds.
    #[inline(always)]
    pub(crate) fn share_slots(
        &mut self,
        cell: Cell,
        slots: std::ops::Range<usize>,
    ) -> Result<(), DeadCell> {
        let first = cell.index().at().wrapping_add(1);
        let mut at = first.wrapping_add(slots.start);
        let end = first.wrapping_add(slots.end);
        // A share may write anywhere in the arena, so each lookup has to be made again
        // after it. A stride is copied out to make fewer.
        while let Some(words) = self.arena.get(at..end).and_then(|w| w.first_chunk()) {
            let words: [u64; STRIDE] = *words;
            for w in words {
                self.own(Value::from_bits(w))?;
            }
            at = at.wrapping_add(STRIDE);
        }
        while at < end
            && let Some(w) = self.arena.get(at)
        {
            self.own(Value::from_bits(*w))?;
            at = at.wrapping_add(1);
        }
        Ok(())
    }

    /// Add a reference to each of `values`, or at a freed cell to none of them.
    #[inline(always)]
    pub(crate) fn share_each(&mut self, values: &[Value]) -> Result<(), DeadCell> {
        for (n, v) in values.iter().enumerate() {
            if let Some(cell) = v.as_cell()
                && let Err(dead) = self.share(cell)
            {
                self.unshare_each(values.get(..n).unwrap_or_default());
                return Err(dead);
            }
        }
        Ok(())
    }

    /// Take back the references [`Self::share_each`] added.
    #[cold]
    #[inline(never)]
    pub(crate) fn unshare_each(&mut self, values: &[Value]) {
        for v in values {
            self.count_down(&[v.bits()]);
        }
    }
}
