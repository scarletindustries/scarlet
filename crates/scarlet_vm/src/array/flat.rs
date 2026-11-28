//! A short array, one of 1 to [`MOST`] elements, is one cell.
//!
//! ```text
//!          slots, 4, 8, 16 or 32 of them        window
//!   [ c | d | Nil | Nil | Nil | Nil | a | b | 6th, 4 ]
//! ```
//!
//! The elements are a run of slots that goes on at the first slot where it comes to
//! the last. The word behind the slots, its window, says where the run starts and how
//! long it is. Every other slot is `Nil`, so no slot holds anything that is not the
//! array's, and the header says that the slots are values.
//!
//! So a push at either end writes one slot, until there is none to write, and the first
//! element is taken out of its slot with nothing else moved. An array that is grown gets
//! a cell of twice the slots, and one that loses elements keeps its cell. So however the
//! two take turns, what is copied is no more than was pushed.
//!
//! What changes a cell in place asks for one that nothing else holds. What has another
//! makes a new cell of as few slots as the elements fit in.

use super::{End, Near, Stored};
use crate::heap::{Alone, Cell, DeadCell, Fault, Heap, Run};
use crate::value::Value;
use crate::word::{low_u32, to_usize};

/// The most elements that a short array has.
pub(crate) const MOST: usize = super::B;

/// The fewest slots that a cell has.
const LEAST_SLOTS: usize = 4;

/// How many slots a new cell has, for an array of `len` elements.
#[inline(always)]
fn slots_for(len: usize) -> usize {
    let slots = len.checked_next_power_of_two();
    slots.map_or(MOST, |n| n.clamp(LEAST_SLOTS, MOST))
}

/// Which slots hold an array's elements: how many do in the low half, and in the high
/// half a number that is, for as many slots as there are, the first of them. It counts
/// past the last slot, and back past the first, and which slot it is then is in its low
/// bits, because there are 4, 8, 16 or 32.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Window(u64);

const ONE_LONGER: u64 = 1;
const ONE_ON: u64 = 1 << 32;

impl Window {
    /// The window of `len` elements from the first slot on.
    #[inline(always)]
    fn of(len: usize) -> Window {
        Window::from(0, len)
    }

    /// The window of `len` elements from slot `at` on.
    #[inline(always)]
    fn from(at: usize, len: usize) -> Window {
        Window((at as u64) << 32 | len as u64)
    }

    #[inline(always)]
    fn len(self) -> usize {
        low_u32(self.0) as usize
    }

    /// The slot of element `i`, out of `slots`.
    #[inline(always)]
    fn place(self, i: usize, slots: usize) -> usize {
        // What is past 32 bits of the sum is past the bits that count.
        let at = (self.0 >> 32) as usize;
        at.wrapping_add(i) & slots.wrapping_sub(1)
    }

    /// With one element more at the end.
    #[inline(always)]
    fn and_one(self, end: End) -> Window {
        Window(match end {
            End::Back => self.0.wrapping_add(ONE_LONGER),
            End::Front => self.0.wrapping_sub(ONE_ON).wrapping_add(ONE_LONGER),
        })
    }

    /// Without the first `from` elements, and with `len` of those behind them.
    #[inline(always)]
    fn cut(self, from: usize, len: usize) -> Window {
        let start = (self.0 >> 32).wrapping_add(from as u64);
        Window(start << 32 | len as u64)
    }
}

/// A short array: its cell, and where in it the elements are.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Flat {
    cell: Cell,
    window: Window,
}

impl Flat {
    /// The short array that `cell` is.
    #[inline(always)]
    pub(crate) fn of(heap: &Heap, cell: Cell) -> Option<Flat> {
        let (_, last) = heap.flat(cell)?;
        let window = Window(last);
        Some(Flat { cell, window })
    }

    #[inline(always)]
    pub(crate) fn cell(self) -> Cell {
        self.cell
    }

    #[inline(always)]
    pub(crate) fn len(self) -> usize {
        self.window.len()
    }

    /// Element `i`, with no reference added, or `None` past the end.
    #[inline(always)]
    pub(crate) fn get(self, heap: &Heap, i: usize) -> Option<Value> {
        if i >= self.len() {
            return None;
        }
        let (slots, _) = heap.flat(self.cell)?;
        let word = slots.get(self.window.place(i, slots.len()))?;
        Some(Value::from_bits(*word))
    }

    /// The slots of the `n` elements from `from` on.
    #[inline(always)]
    fn run(self, heap: &Heap, from: usize, n: usize) -> Run {
        let slots = heap.flat(self.cell).map_or(0, |(slots, _)| slots.len());
        Run {
            of: self.cell,
            at: self.window.place(from, slots),
            n,
        }
    }

    /// Put the elements from `from` on in `out`, as many as there is room for, with no
    /// reference added.
    #[inline(always)]
    pub(crate) fn read_run(self, heap: &Heap, from: usize, out: &mut [Value]) {
        let slots = heap.flat(self.cell).map_or(&[][..], |(slots, _)| slots);
        for (i, v) in out.iter_mut().enumerate() {
            let at = self.window.place(from.wrapping_add(i), slots.len());
            if let Some(word) = slots.get(at) {
                *v = Value::from_bits(*word);
            }
        }
    }

    /// The `n` elements from `from` on, as the run of slots up to the last one and that from
    /// the first one on.
    #[inline(always)]
    pub(super) fn slices(self, heap: &Heap, from: usize, n: usize) -> [&[u64]; 2] {
        let slots = heap.flat(self.cell).map_or(&[][..], |(slots, _)| slots);
        let at = self.window.place(from, slots.len());
        let (before, behind) = slots.split_at_checked(at).unwrap_or_default();
        let near = behind.get(..n).unwrap_or(behind);
        let far = before.get(..n.saturating_sub(near.len()));
        [near, far.unwrap_or_default()]
    }

    /// Every element, front to back, with no reference added.
    pub(crate) fn items(
        self,
        heap: &Heap,
    ) -> impl DoubleEndedIterator<Item = Value> + ExactSizeIterator {
        let slots = heap.flat(self.cell).map_or(&[][..], |(slots, _)| slots);
        (0..self.len()).map(move |i| {
            let word = slots.get(self.window.place(i, slots.len()));
            word.map_or(Value::NIL, |w| Value::from_bits(*w))
        })
    }
}

/// Element `index` of `of`, when it is a short array and that is a small Int, with no
/// reference added.
#[inline(always)]
pub(crate) fn near(heap: &Heap, of: Value, index: Value) -> Near {
    match index.small_int() {
        // One below 0 is past the end this way.
        Some(i) => look(heap, of, to_usize(i.cast_unsigned())),
        None => Near::Other,
    }
}

/// Element `i` of `of`, when it is a short array, with no reference added.
#[inline(always)]
fn look(heap: &Heap, of: Value, i: usize) -> Near {
    let Some((slots, last)) = of.as_cell().and_then(|cell| heap.flat(cell)) else {
        return Near::Other;
    };
    let window = Window(last);
    if i >= window.len() {
        return Near::Past;
    }
    match slots.get(window.place(i, slots.len())) {
        Some(word) => Near::Found(Value::from_bits(*word)),
        None => Near::Other,
    }
}

/// [`look`], or `None` where there is none.
#[inline(always)]
pub(super) fn at(heap: &Heap, of: Value, i: usize) -> Option<Value> {
    match look(heap, of, i) {
        Near::Found(v) => Some(v),
        Near::Past | Near::Other => None,
    }
}

/// What an audit finds wrong with a short array's cell.
pub(crate) enum Wrong {
    /// It has another number of slots, or counts more elements than it has slots.
    Slots,
    /// It has no elements.
    Empty,
    /// A slot that is none of its elements' is not `Nil`.
    Stale,
}

/// Whether `words`, those behind a header, are laid out as a short array's are.
pub(crate) fn laid_out(words: &[u64]) -> Result<(), Wrong> {
    let (last, slots) = words.split_last().ok_or(Wrong::Slots)?;
    let window = Window(*last);
    let n = slots.len();
    if slots_for(n) != n || window.len() > n {
        return Err(Wrong::Slots);
    }
    if window.len() == 0 {
        return Err(Wrong::Empty);
    }
    let stale = |i| slots.get(window.place(i, n)) != Some(&Value::NIL.bits());
    if (window.len()..n).any(stale) {
        return Err(Wrong::Stale);
    }
    Ok(())
}

/// As many elements as a short array may have, on the way from one cell to another.
pub(super) struct Few {
    items: [Value; MOST],
    len: usize,
}

impl Few {
    #[inline(always)]
    fn new() -> Few {
        Few {
            // All zeros, which costs the least to write.
            items: [Value::from_bits(0); MOST],
            len: 0,
        }
    }

    #[inline(always)]
    pub(super) fn items(&self) -> &[Value] {
        self.items.get(..self.len).unwrap_or_default()
    }

    /// Add as many of `items` as there is room for.
    #[inline(always)]
    fn add(&mut self, items: impl Iterator<Item = Value>) {
        for (room, v) in self.items.iter_mut().skip(self.len).zip(items) {
            *room = v;
            self.len = self.len.wrapping_add(1);
        }
    }

    /// Add the elements of `flat`.
    #[inline(always)]
    fn read(&mut self, heap: &Heap, flat: Flat) {
        for run in flat.slices(heap, 0, flat.len()) {
            let room = self.items.get_mut(self.len..).unwrap_or_default();
            for (v, word) in room.iter_mut().zip(run) {
                *v = Value::from_bits(*word);
            }
            self.len = self.len.saturating_add(run.len()).min(MOST);
        }
    }
}

/// A new short array of `items`, whose references pass to it, and of which there are no
/// more than [`MOST`]. One of none is the empty array.
#[inline(always)]
pub(super) fn new_flat(heap: &mut Heap, items: &[Value]) -> Result<Stored, Fault> {
    if items.is_empty() {
        return Ok(Stored::Empty);
    }
    let window = Window::of(items.len());
    let cell = heap.flat_of(slots_for(items.len()), items, window.0)?;
    Ok(Stored::Flat(Flat { cell, window }))
}

/// A new short array of `items`, which are 1 to [`MOST`], whose references pass to it. It has
/// slots for as many as there may be.
pub(super) fn with_room(heap: &mut Heap, items: &[Value]) -> Result<Value, Fault> {
    let cell = heap.flat_of(MOST, items, Window::of(items.len()).0)?;
    Ok(Value::cell(cell))
}

/// A new short array of `values`, whose references pass to it, in the space of a freed
/// cell, when there is one. There is at least one value, and no more than [`MOST`].
/// Nothing has been asked of `values` when there is no such cell.
#[inline(always)]
pub(crate) fn in_freed(
    heap: &mut Heap,
    values: impl ExactSizeIterator<Item = Value>,
) -> Option<Value> {
    let len = values.len();
    let cell = heap.flat_in_freed(slots_for(len), values, Window::of(len).0)?;
    Some(Value::cell(cell))
}

/// The same at the end of the heap.
#[inline(always)]
pub(crate) fn at_end(
    heap: &mut Heap,
    values: impl ExactSizeIterator<Item = Value>,
) -> Result<Value, Fault> {
    let len = values.len();
    let cell = heap.flat_at_end(slots_for(len), values, Window::of(len).0)?;
    Ok(Value::cell(cell))
}

// What changes a cell where it is. Each is for a short array that nothing else holds, and
// says so when `cell` is none, or when there is nothing for it to do there. It has then
// changed nothing.

/// Put `x` at index `i`, and find what was there. Both references are the caller's.
#[inline(always)]
pub(crate) fn set_alone(heap: &mut Heap, cell: Cell, i: usize, x: Value) -> Near {
    let Some(Alone { slots, last }) = heap.flat_alone(cell) else {
        return Near::Other;
    };
    let window = Window(*last);
    if i >= window.len() {
        return Near::Past;
    }
    match slots.get_mut(window.place(i, slots.len())) {
        Some(word) => Near::Found(Value::from_bits(std::mem::replace(word, x.bits()))),
        None => Near::Other,
    }
}

/// Put `x` at `end`, with its reference. Whether there was a slot for it.
#[inline(always)]
pub(crate) fn push_alone(heap: &mut Heap, cell: Cell, x: Value, end: End) -> bool {
    let Some(Alone { slots, last }) = heap.flat_alone(cell) else {
        return false;
    };
    let window = Window(*last);
    let len = window.len();
    if len >= slots.len() {
        return false;
    }
    let longer = window.and_one(end);
    let Some(word) = slots.get_mut(longer.place(end.put_at(len), slots.len())) else {
        return false;
    };
    *word = x.bits();
    *last = longer.0;
    true
}

/// The first element, taken out of a short array.
pub(crate) struct First {
    pub(crate) value: Value,
    /// Whether it was the only one. Its cell has none now, and is no array's.
    pub(crate) only: bool,
}

/// Take the first element out. Its reference is the caller's.
#[inline(always)]
pub(crate) fn take_first(heap: &mut Heap, cell: Cell) -> Option<First> {
    let Alone { slots, last } = heap.flat_alone(cell)?;
    let window = Window(*last);
    let left = window.len().checked_sub(1)?;
    let word = slots.get_mut(window.place(0, slots.len()))?;
    let value = std::mem::replace(word, Value::NIL.bits());
    *last = window.0.wrapping_add(ONE_ON).wrapping_sub(ONE_LONGER);
    Some(First {
        value: Value::from_bits(value),
        only: left == 0,
    })
}

/// Give up the first `n` elements, where there are more. Whether it did.
#[inline(always)]
pub(super) fn drop_first(heap: &mut Heap, cell: Cell, n: usize) -> Result<bool, DeadCell> {
    let Some(Alone { slots, last }) = heap.flat_alone(cell) else {
        return Ok(false);
    };
    // Those up to the first that is a cell in one go.
    let window = Window(*last);
    let mut plain = 0usize;
    while plain < n
        && let Some(word) = slots.get_mut(window.place(plain, slots.len()))
        && Value::from_bits(*word).as_cell().is_none()
    {
        *word = Value::NIL.bits();
        plain = plain.wrapping_add(1);
    }
    *last = window.cut(plain, window.len().wrapping_sub(plain)).0;
    for _ in plain..n {
        if let Some(first) = take_first(heap, cell) {
            super::release(heap, first.value)?;
        }
    }
    Ok(true)
}

/// Take the elements from `from` on out, as many as `out` has room for and those that
/// `wanted` says. Their slots are `Nil`, and the array still counts them. Whether it did.
#[inline(always)]
pub(crate) fn take_run(
    heap: &mut Heap,
    cell: Cell,
    from: usize,
    wanted: impl Iterator<Item = bool>,
    out: &mut [Value],
) -> bool {
    let Some(Alone { slots, last }) = heap.flat_alone(cell) else {
        return false;
    };
    let window = Window(*last);
    for (i, (v, wanted)) in out.iter_mut().zip(wanted).enumerate() {
        let at = window.place(from.wrapping_add(i), slots.len());
        if wanted && let Some(word) = slots.get_mut(at) {
            let taken = std::mem::replace(word, Value::NIL.bits());
            *v = Value::from_bits(taken);
        }
    }
    true
}

/// Put `items`, in their order and with their references, at `end`. Whether there were
/// slots for them all.
#[inline(always)]
pub(super) fn extend_alone(heap: &mut Heap, cell: Cell, items: &[Value], end: End) -> bool {
    let Some(Alone { slots, last }) = heap.flat_alone(cell) else {
        return false;
    };
    let window = Window(*last);
    let len = window.len();
    if len.saturating_add(items.len()) > slots.len() {
        return false;
    }
    let n = items.len() as u64;
    let start = match end {
        End::Front => window.0.wrapping_sub(n << 32),
        End::Back => window.0,
    };
    let longer = Window(start.wrapping_add(n));
    let at = longer.place(end.put_at(len), slots.len());
    // Up to the last slot, and the rest from the first one on.
    let (before, behind) = slots.split_at_mut_checked(at).unwrap_or_default();
    let (near, far) = items.split_at_checked(behind.len()).unwrap_or((items, &[]));
    for (items, slots) in [(near, behind), (far, before)] {
        for (x, word) in items.iter().zip(slots) {
            *word = x.bits();
        }
    }
    *last = longer.0;
    true
}

/// Leave only the elements from `from` up to `to`, of which there is at least one, and
/// give up the others. Whether it did.
fn cut_alone(heap: &mut Heap, cell: Cell, from: usize, to: usize) -> Result<bool, DeadCell> {
    let Some(Alone { slots, last }) = heap.flat_alone(cell) else {
        return Ok(false);
    };
    let window = Window(*last);
    let mut gone = Few::new();
    for i in (0..from).chain(to..window.len()) {
        if let Some(word) = slots.get_mut(window.place(i, slots.len())) {
            let taken = std::mem::replace(word, Value::NIL.bits());
            gone.add(std::iter::once(Value::from_bits(taken)));
        }
    }
    *last = window.cut(from, to.saturating_sub(from)).0;
    give_up(heap, gone.items())?;
    Ok(true)
}

/// Give up the reference that each of `items` is.
fn give_up(heap: &mut Heap, items: &[Value]) -> Result<(), DeadCell> {
    for v in items {
        super::release(heap, *v)?;
    }
    Ok(())
}

/// A new short array of `before`, the elements of `flat` from `from` up to `to`, and `after`.
/// The caller gives up its reference to `flat` and to each of the others, whether or not
/// there is room for the array.
#[inline(always)]
fn remade(
    heap: &mut Heap,
    flat: Flat,
    (from, to): (usize, usize),
    (before, after): (&[Value], &[Value]),
) -> Result<Value, Fault> {
    let n = to.wrapping_sub(from);
    // What nothing else holds hands over the references of its elements.
    let alone = heap.unique(flat.cell)?;
    match copied(heap, flat, (from, n), (before, after)) {
        Ok((new, at)) => {
            if alone {
                heap.discard(flat.cell)?;
            } else {
                heap.share_slots(new, at..at.wrapping_add(n))?;
                heap.release(flat.cell)?;
            }
            Ok(Value::cell(new))
        }
        Err(why) => {
            give_up(heap, before)?;
            give_up(heap, after)?;
            heap.release(flat.cell)?;
            Err(why)
        }
    }
}

/// The cell of [`remade`], with the references of `flat`'s elements still to be given, and
/// the first of their slots. One that is given something in front has its other slots
/// there, so that what comes next fills it to its first slot.
#[inline(always)]
fn copied(
    heap: &mut Heap,
    flat: Flat,
    (from, n): (usize, usize),
    (before, after): (&[Value], &[Value]),
) -> Result<(Cell, usize), Fault> {
    let run = flat.run(heap, from, n);
    let len = before.len().wrapping_add(n).wrapping_add(after.len());
    let slots = slots_for(len);
    let at = match before.is_empty() {
        true => 0,
        false => slots.saturating_sub(len),
    };
    let window = Window::from(at, len);
    let new = heap.flat_beside((before, run, after), (slots, at), window.0)?;
    Ok((new, at.wrapping_add(before.len())))
}

// Everything that gives a short array another. The caller gives up its reference to the
// array, and to each value it hands over, even where there is no room for what is made.

/// `flat` with `x` at index `i`. An index that it does not have changes nothing.
#[inline(always)]
pub(super) fn set(heap: &mut Heap, flat: Flat, i: usize, x: Value) -> Result<Value, Fault> {
    match set_alone(heap, flat.cell, i, x) {
        Near::Found(old) => {
            super::release(heap, old)?;
            Ok(Value::cell(flat.cell))
        }
        Near::Past | Near::Other => set_held(heap, flat, i, x),
    }
}

/// [`set`] in a new cell.
#[inline(never)]
fn set_held(heap: &mut Heap, flat: Flat, i: usize, x: Value) -> Result<Value, Fault> {
    if i >= flat.len() {
        super::release(heap, x)?;
        return Ok(Value::cell(flat.cell));
    }
    heap.unique(flat.cell)?;
    let new = match copied(heap, flat, (0, flat.len()), (&[], &[])) {
        Ok((new, _)) => new,
        Err(why) => return Err(super::refused(heap, why, &[x, Value::cell(flat.cell)])),
    };
    heap.share_slots(new, 0..i)?;
    heap.share_slots(new, i.wrapping_add(1)..flat.len())?;
    if let Some(word) = heap.data_mut(new).get_mut(i) {
        *word = x.bits();
    }
    heap.release(flat.cell)?;
    Ok(Value::cell(new))
}

/// `flat` with `x` at `end`.
#[inline(always)]
pub(super) fn push(heap: &mut Heap, flat: Flat, x: Value, end: End) -> Result<Value, Fault> {
    if push_alone(heap, flat.cell, x, end) {
        return Ok(Value::cell(flat.cell));
    }
    grown(heap, flat, &[x], end)
}

/// `flat` with `items`, in their order, at `end`.
#[inline(always)]
pub(super) fn extend(
    heap: &mut Heap,
    flat: Flat,
    items: &[Value],
    end: End,
) -> Result<Value, Fault> {
    if extend_alone(heap, flat.cell, items, end) {
        return Ok(Value::cell(flat.cell));
    }
    grown(heap, flat, items, end)
}

/// [`extend`] in a new cell, or as a tree when that is too long for one.
#[inline(never)]
fn grown(heap: &mut Heap, flat: Flat, items: &[Value], end: End) -> Result<Value, Fault> {
    let len = flat.len();
    if len.saturating_add(items.len()) > MOST {
        return super::outgrown(heap, flat, items, end);
    }
    let beside: (&[Value], &[Value]) = match end {
        End::Front => (items, &[]),
        End::Back => (&[], items),
    };
    remade(heap, flat, (0, len), beside)
}

/// `a` and then `b`, which are short enough for one cell together. It is one of theirs, where
/// nothing else holds that and it has slots for the other.
#[inline(never)]
pub(super) fn join(heap: &mut Heap, a: Flat, b: Flat) -> Result<Value, Fault> {
    for (onto, flat, end) in [(a, b, End::Back), (b, a, End::Front)] {
        if has_room(heap, onto, flat.len()) {
            let items = taken_apart(heap, flat)?;
            return extend(heap, onto, items.items(), end);
        }
    }
    joined(heap, a, b)
}

/// [`join`] in a new cell.
#[inline(always)]
fn joined(heap: &mut Heap, a: Flat, b: Flat) -> Result<Value, Fault> {
    heap.unique(a.cell)?;
    heap.unique(b.cell)?;
    let len = a.len().wrapping_add(b.len());
    let runs = [a.run(heap, 0, a.len()), b.run(heap, 0, b.len())];
    let new = match heap.flat_joined(runs, slots_for(len), Window::of(len).0) {
        Ok(new) => new,
        Err(why) => {
            let both = [a.cell, b.cell].map(Value::cell);
            return Err(super::refused(heap, why, &both));
        }
    };
    for (flat, from) in [(a, 0), (b, a.len())] {
        // Where the two are one cell, the second finds whether it is its holder's alone
        // once the first has given it up.
        if heap.unique(flat.cell)? {
            heap.discard(flat.cell)?;
        } else {
            heap.share_slots(new, from..from.wrapping_add(flat.len()))?;
            heap.release(flat.cell)?;
        }
    }
    Ok(Value::cell(new))
}

/// The first element of `flat` and the array of the others, in a new cell, or `None` when
/// its cell says that it has none, which no array's does. The element holds a reference of
/// its own, and is `Nil` unless `head` asks for it. The caller keeps its reference to
/// `flat`.
#[inline(always)]
pub(crate) fn uncons(
    heap: &mut Heap,
    flat: Flat,
    head: bool,
) -> Result<Option<(Value, Value)>, Fault> {
    let len = flat.len();
    let Some(first) = flat.get(heap, 0) else {
        return Ok(None);
    };
    let first = if head { first } else { Value::NIL };
    if len == 1 {
        heap.own(first)?;
        return Ok(Some((first, Value::EMPTY_ARRAY)));
    }
    let left = after(heap, flat, 1)?;
    heap.own(first)?;
    Ok(Some((first, left)))
}

/// The elements of `flat` but the first `n`, of which there are more, in a new cell. The caller
/// keeps its reference to `flat`.
#[inline(always)]
pub(super) fn after(heap: &mut Heap, flat: Flat, n: usize) -> Result<Value, Fault> {
    let left = flat.len().wrapping_sub(n);
    let (new, _) = copied(heap, flat, (n, left), (&[], &[]))?;
    heap.share_slots(new, 0..left)?;
    Ok(Value::cell(new))
}

/// The elements of `flat` from `from` up to `to`, or up to its last one. When nothing else
/// holds its cell and they fill a quarter of the slots or less, they go to one of fewer.
#[inline(never)]
pub(super) fn slice(heap: &mut Heap, flat: Flat, from: usize, to: usize) -> Result<Value, Fault> {
    let to = to.min(flat.len());
    if from >= to {
        heap.release(flat.cell)?;
        return Ok(Value::EMPTY_ARRAY);
    }
    if from == 0 && to == flat.len() {
        // It is asked so that a freed cell is not given back.
        heap.unique(flat.cell)?;
        return Ok(Value::cell(flat.cell));
    }
    if cut_alone(heap, flat.cell, from, to)? {
        return fitted(heap, flat.cell);
    }
    remade(heap, flat, (from, to), (&[], &[]))
}

/// The elements of `flat` but the first `n`, in its own cell when nothing else holds that.
/// A walk does not keep what it leaves, as what a slice gives may be kept.
#[inline(never)]
pub(super) fn skip(heap: &mut Heap, flat: Flat, n: usize) -> Result<Value, Fault> {
    if n >= flat.len() {
        heap.release(flat.cell)?;
        return Ok(Value::EMPTY_ARRAY);
    }
    match n == 0 || drop_first(heap, flat.cell, n)? {
        true => {
            // It is asked so that a freed cell is not given back.
            heap.unique(flat.cell)?;
            Ok(Value::cell(flat.cell))
        }
        false => remade(heap, flat, (n, flat.len()), (&[], &[])),
    }
}

/// The short array `cell`, which nothing else holds, in a cell of fewer slots when it fills a
/// quarter of them or less.
#[inline(always)]
fn fitted(heap: &mut Heap, cell: Cell) -> Result<Value, Fault> {
    let Some((slots, last)) = heap.flat(cell) else {
        return Ok(Value::cell(cell));
    };
    let window = Window(last);
    let (has, len) = (slots.len(), window.len());
    if len > has / 4 || slots_for(len) >= has {
        return Ok(Value::cell(cell));
    }
    refitted(heap, Flat { cell, window })
}

#[inline(never)]
fn refitted(heap: &mut Heap, flat: Flat) -> Result<Value, Fault> {
    remade(heap, flat, (0, flat.len()), (&[], &[]))
}

/// The elements of `flat`, with a reference to each.
pub(super) fn taken_apart(heap: &mut Heap, flat: Flat) -> Result<Few, DeadCell> {
    let mut all = Few::new();
    all.read(heap, flat);
    if heap.unique(flat.cell)? {
        heap.discard(flat.cell)?;
        return Ok(all);
    }
    heap.share_each(all.items())?;
    heap.release(flat.cell)?;
    Ok(all)
}

/// Whether `flat` has slots for `more` elements in a cell that nothing else holds.
fn has_room(heap: &mut Heap, flat: Flat, more: usize) -> bool {
    let alone = heap.flat_alone(flat.cell);
    alone.is_some_and(|a| flat.len().saturating_add(more) <= a.slots.len())
}

#[cfg(test)]
mod tests;
