//! An array of 1 to 32 elements is one cell of them ([`flat`]), which is called short here.
//! A longer one is two short arrays and, between them, a tree of short arrays that are full.
//!
//! ```text
//!   root [ front | head | tree | tail | how many each has, and where ]
//!                    |      |      |
//!                    |      |      +--> a short array, to which a push at the back goes
//!                    |      |
//!                    |   branches, which have 32 places each
//!                    |     /      |       \
//!                    |  leaf     leaf     leaf    short arrays of 32
//!                    |
//!                    +--> a short array, from which `[h, ..t]` takes, and to which a push
//!                         at the front goes
//! ```
//!
//! Every leaf is full, so which place of each branch an element is under is in the bits of
//! its number, five for each level, and a read asks for no more than that. This is
//! Clojure's vector with a head as it has a tail. The leaves go round: they are in a run of
//! places that starts at `first` and goes on at the first place where it comes to the last,
//! as the elements of a short array do in its slots. So a leaf is put at either end, and
//! taken from either, without any other being moved. A tree is as tall as so many leaves
//! need and no taller, whatever it was before. When they come to fill it, it becomes a level
//! taller ([`taller`]), and when one less would do, a level less tall ([`lower`]). Both
//! touch one branch or two a level.
//!
//! An array something else holds is never changed. An operation on one makes new nodes
//! along the paths that it touches, and shares every other with the array that it came
//! from, which stays as it was. A node that is shared in this way holds one more reference.
//!
//! Everything here that gives an array is handed one reference to each array that it
//! works on, and to each element that goes in, and gives up what it does not put
//! in the array that it gives. So a count of 1 says that nothing else holds an array,
//! which may then change in place. The one exception is [`flat::uncons`], which takes
//! nothing. What is refused for want of room has given up what it was handed too. What
//! makes more than one cell asks for all their words first, and so is refused before it has
//! changed anything.
//!
//! These are the rules for a tree:
//!
//! - It has a leaf, or both a head and a tail. One that comes to be left with a head or a tail
//!   alone is that short array from then on ([`close`]).
//! - All that is in its leaves and its tail is the array's. It may have lost the first elements
//!   of its head while another array has that, and they may still be there, but only where they
//!   are [`plain`]: no cells, or strings and the like, or, where the array is held, cells of no more
//!   than such ([`plain_inside`]). So no array holds a handle that is not in it. The root says where in the head
//!   the array starts, so `[h, ..t]` of an array that something else holds makes a root, and
//!   both arrays have the head.
//! - When its first element is not plain, that would copy the rest of the head. But when no other
//!   array has the head, the element goes to the root of the array that is held, as its *front*,
//!   and then both have the rest of the head ([`move_to_root`]). The array that something else
//!   holds is the same array as before.
//! - Before a head is changed, its front goes back into it, and what the array has lost goes out
//!   of it ([`open`]).
//! - A head or a tail that is made for a push has all 32 slots, because it comes to be full.
//! - A place in a branch that no leaf is under is `Nil`.
//!
//! The walks here go down at most one level per call, and a tree is 7 levels tall at the
//! most, because a heap has no room for a taller one.
//!
//! A tree is made where an array comes to have more than 32 elements. One that then loses
//! elements stays a tree while it keeps more than it loses at once, or loses them to
//! `[h, ..t]`, so one that hovers about 32 is not copied at each turn. Every other slice of
//! 32 or fewer is a short array. No reader of an array asks which of the two it is but to
//! find the elements.
//!
//! An array can also be a range, `start..end`, which stores only its two
//! ends, as the old VM did. Its length, its elements, its tail and its
//! slices all come from those two numbers. Adding to a range, or joining one to
//! another array, first builds the array of its elements.
//!
//! The array with no elements is none of these. It is a word of its own,
//! [`Value::EMPTY_ARRAY`], and no cell is ever one. Every cell of an array is made by
//! [`close`], [`range`], [`branch`] or one of the makers of [`flat`], which make
//! none for no elements, or is a copy of one that was. Two more make a root, and only of what
//! stays a tree: [`behind`], where a head or a leaf is left, and a join, of a tree that it has
//! put in order. [`Seq`] says which of them an array is. What is written for one of the forms
//! asks a cell whether it is that one, and answers for no other.

use crate::code::Length;
use crate::heap::{Cell, DeadCell, Fault, Heap, Kind};
use crate::value::{SMALL_INT_MAX, SMALL_INT_MIN, Value};
use crate::word::{low_byte, low_u32, to_usize};

pub(crate) mod flat;

pub(crate) use flat::Flat;
use flat::MOST;

/// Places in a branch, and elements in a leaf.
pub(crate) const B: usize = 32;
/// Bits of a place each level takes, which is `log2(B)`.
const BITS: u32 = 5;

/// How many elements one step of a join moves. It is 64 leaves.
const STEP: usize = 64 * B;

/// An array as the VM holds it: the empty one, one cell of its elements, a tree of them,
/// or a range of Ints that stores only its two ends.
#[derive(Clone, Copy)]
pub(crate) enum Seq {
    Empty,
    Flat(Flat),
    Tree(Cell),
    Range { start: i64, end: i64 },
}

/// An array that stores its elements.
#[derive(Clone, Copy)]
pub(crate) enum Stored {
    Empty,
    Flat(Flat),
    Tree(Cell),
}

impl Stored {
    /// The array as a value. Its reference passes to it.
    #[inline(always)]
    pub(crate) fn value(self) -> Value {
        match self {
            Stored::Empty => Value::EMPTY_ARRAY,
            Stored::Flat(flat) => Value::cell(flat.cell()),
            Stored::Tree(cell) => Value::cell(cell),
        }
    }

    fn len(self, heap: &Heap) -> usize {
        match self {
            Stored::Empty => 0,
            Stored::Flat(flat) => flat.len(),
            Stored::Tree(cell) => tree(heap, cell).map_or(0, |r| r.len()),
        }
    }
}

impl Seq {
    #[inline(always)]
    pub(crate) fn len(self, heap: &Heap) -> u64 {
        match self {
            Seq::Empty => 0,
            Seq::Flat(flat) => flat.len() as u64,
            Seq::Tree(cell) => tree(heap, cell).map_or(0, |r| r.len() as u64),
            Seq::Range { start, end } => range_len(start, end),
        }
    }
}

/// How many Ints `start..end` holds. It is none when `end` is not past
/// `start`.
pub(crate) fn range_len(start: i64, end: i64) -> u64 {
    if end > start { end.abs_diff(start) } else { 0 }
}

/// The array of the Ints from `start` up to but not including `end`.
// Kept inside its callers, as `Heap::make` is and for the same reason.
#[inline(always)]
pub(crate) fn range(heap: &mut Heap, start: i64, end: i64) -> Result<Value, Fault> {
    if start >= end {
        return Ok(Value::EMPTY_ARRAY);
    }
    let ends = [start.cast_unsigned(), end.cast_unsigned()];
    Ok(Value::cell(heap.make(Kind::Range, &ends)?))
}

/// The array `v` is, or `None` when it is something else.
#[inline(always)]
pub(crate) fn seq(heap: &Heap, v: Value) -> Option<Seq> {
    let Some(cell) = v.as_cell() else {
        return (v.bits() == Value::EMPTY_ARRAY.bits()).then_some(Seq::Empty);
    };
    if let Some(flat) = Flat::of(heap, cell) {
        return Some(Seq::Flat(flat));
    }
    if tree(heap, cell).is_some() {
        return Some(Seq::Tree(cell));
    }
    let (start, end) = range_at(heap, cell)?;
    Some(Seq::Range { start, end })
}

/// The ends of the range that `cell` is.
#[inline(always)]
pub(crate) fn range_at(heap: &Heap, cell: Cell) -> Option<(i64, i64)> {
    let [start, end] = heap.chunk(cell, Kind::Range)?;
    Some((start.cast_signed(), end.cast_signed()))
}

/// Which end of an array.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum End {
    Front,
    Back,
}

impl End {
    /// Where the first of what is put at this end of `len` elements comes to be.
    #[inline(always)]
    fn put_at(self, len: usize) -> usize {
        match self {
            End::Front => 0,
            End::Back => len,
        }
    }

    fn other(self) -> End {
        match self {
            End::Front => End::Back,
            End::Back => End::Front,
        }
    }
}

/// A short array that a tree has at one end, and how many elements it has. It is `Nil` when
/// there is none.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Edge {
    items: Value,
    len: usize,
}

impl Edge {
    const NONE: Edge = Edge {
        items: Value::NIL,
        len: 0,
    };

    fn of(flat: Flat) -> Edge {
        Edge {
            items: Value::cell(flat.cell()),
            len: flat.len(),
        }
    }
}

/// Where an element is, and which one it is there.
enum Place {
    /// It is this, and in the root.
    Front(Value),
    Edge(Value, usize),
    /// Which one of all in the leaves.
    Leaf(usize),
}

/// What a tree's root says.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Root {
    /// Which element of the head is the first of the array's, or [`FRONT`]. Those before it
    /// have been lost, and are [`plain_inside`].
    start: usize,
    /// The first of the array's elements when `start` says [`FRONT`], and otherwise `Nil`.
    front: Value,
    head: Edge,
    tail: Edge,
    tree: Value,
    /// How many elements are in the leaves.
    tree_len: usize,
    /// Which place the first of those has.
    first: usize,
    /// How many bits of a place are below the top branch's. It is 0 when the tree is one leaf.
    shift: u32,
}

/// Where an array starts whose first element is in its root: one before the first of its head.
const FRONT: usize = usize::MAX;

/// How many words a root has after its header.
pub(crate) const ROOT: usize = 7;
/// The bits of a root's fifth word that its small numbers are from: how many are in its head and
/// in its tail, and how tall the tree is. The low byte is where it starts.
const HEAD: u32 = 8;
const TAIL: u32 = 16;
const HEIGHT: u32 = 24;
/// One more in the head and in the tail.
const IN_HEAD: u64 = 1 << HEAD;
const IN_TAIL: u64 = 1 << TAIL;
/// The bits of where it starts.
const START: u64 = 0xFF;

impl Root {
    /// The empty array's.
    const EMPTY: Root = Root {
        start: 0,
        front: Value::NIL,
        head: Edge::NONE,
        tail: Edge::NONE,
        tree: Value::NIL,
        tree_len: 0,
        first: 0,
        shift: 0,
    };

    /// That of a short array, as one that is added to at `end`.
    // A full one is a leaf in front of which something comes. As a tail, it would be copied
    // at each step of a walk of it that something else holds too.
    fn of(flat: Flat, end: End) -> Root {
        match end {
            End::Front if flat.len() == B => Root {
                tree: Value::cell(flat.cell()),
                tree_len: B,
                ..Root::EMPTY
            },
            End::Front => Root {
                tail: Edge::of(flat),
                ..Root::EMPTY
            },
            End::Back => Root {
                head: Edge::of(flat),
                ..Root::EMPTY
            },
        }
    }

    #[inline(always)]
    pub(crate) fn len(&self) -> usize {
        // A start of one before the head's first adds one.
        let edges = self.head.len.wrapping_add(self.tail.len);
        edges.wrapping_add(self.tree_len).wrapping_sub(self.start)
    }

    /// The element that is in the root.
    #[inline(always)]
    fn front(&self) -> Option<Value> {
        (self.start == FRONT).then_some(self.front)
    }

    /// How many elements its head has that the array has lost.
    #[inline(always)]
    fn lost(&self) -> usize {
        if self.start == FRONT { 0 } else { self.start }
    }

    fn edge(&mut self, end: End) -> &mut Edge {
        match end {
            End::Front => &mut self.head,
            End::Back => &mut self.tail,
        }
    }

    /// The short array that element `i` is in, and which one it is there, unless it is past the
    /// end. A leaf is the tree's to find.
    #[inline(always)]
    fn place(&self, i: usize) -> Option<Place> {
        if i >= self.len() {
            return None;
        }
        let i = self.start.wrapping_add(i);
        let Some(behind) = i.checked_sub(self.head.len) else {
            return Some(Place::Edge(self.head.items, i));
        };
        Some(match behind.checked_sub(self.tree_len) {
            None => Place::Leaf(behind),
            Some(_) if i == FRONT => Place::Front(self.front),
            Some(i) => Place::Edge(self.tail.items, i),
        })
    }

    /// What [`Self::words`] says.
    #[inline(always)]
    fn from_words(words: &[u64; ROOT]) -> Root {
        let [front, head, tree, tail, lens, tree_len, first] = words.map(Value::from_bits);
        let lens = lens.bits();
        let edge = |items, at: u32| Edge {
            items,
            len: usize::from(low_byte(lens >> at)),
        };
        Root {
            start: isize::from(low_byte(lens).cast_signed()).cast_unsigned(),
            front,
            head: edge(head, HEAD),
            tail: edge(tail, TAIL),
            tree,
            tree_len: to_usize(tree_len.bits()),
            first: to_usize(first.bits()),
            shift: low_u32(lens >> HEIGHT),
        }
    }

    /// The words after the header of its cell.
    #[inline(always)]
    fn words(&self) -> [u64; ROOT] {
        let [front, head, tree, tail] = self.parts().map(Value::bits);
        let [in_head, in_tail] = [self.head, self.tail].map(|edge| edge.len as u64);
        let start = u64::from(low_byte(self.start as u64));
        let height = u64::from(self.shift) << HEIGHT;
        let lens = start | in_head << HEAD | in_tail << TAIL | height;
        let (tree_len, first) = (self.tree_len as u64, self.first as u64);
        [front, head, tree, tail, lens, tree_len, first]
    }

    #[inline(always)]
    fn parts(&self) -> [Value; 4] {
        [
            self.front().unwrap_or(Value::NIL),
            self.head.items,
            self.tree,
            self.tail.items,
        ]
    }
}

/// How many elements a tree of height `shift` has room for.
#[inline(always)]
fn room(shift: u32) -> usize {
    B.wrapping_shl(shift)
}

/// The place that is `at` places on from the first one, in a tree of height `shift`. Past the
/// last one it is the first one again.
#[inline(always)]
fn round(at: usize, shift: u32) -> usize {
    at & room(shift).wrapping_sub(1)
}

/// The root that `cell` is. The callers know it for a tree, so one that is none
/// has been freed.
#[inline(always)]
fn root(heap: &Heap, cell: Cell) -> Result<Root, DeadCell> {
    tree(heap, cell).ok_or(DeadCell)
}

/// The root that `cell` is, or `None` when it is something else.
#[inline(always)]
pub(crate) fn tree(heap: &Heap, cell: Cell) -> Option<Root> {
    heap.chunk(cell, Kind::ArrayRoot).map(Root::from_words)
}

/// The same when nothing else holds it.
#[inline(always)]
fn tree_alone(heap: &Heap, cell: Cell) -> Option<Root> {
    heap.chunk_alone(cell, Kind::ArrayRoot)
        .map(Root::from_words)
}

/// The short array that `v` is. A part of a tree that is none has been freed.
#[inline(always)]
fn short(heap: &Heap, v: Value) -> Result<Flat, DeadCell> {
    let flat = v.as_cell().and_then(|cell| Flat::of(heap, cell));
    flat.ok_or(DeadCell)
}

fn release(heap: &mut Heap, v: Value) -> Result<(), DeadCell> {
    match v.as_cell() {
        Some(cell) => heap.release(cell),
        None => Ok(()),
    }
}

/// `why`, where what was to be handed to a new array is given up, because there is none.
#[cold]
#[inline(never)]
fn refused(heap: &mut Heap, why: Fault, handed: &[Value]) -> Fault {
    for v in handed {
        if let Err(dead) = release(heap, *v) {
            return dead.into();
        }
    }
    why
}

// Reading.

/// What was found at an index.
pub(crate) enum Near {
    Found(Value),
    /// The array has no such index.
    Past,
    /// It is not an array that was asked about, and may have the index.
    Other,
}

/// Element `i` of `array`, when it is a tree, with no reference added.
#[inline(always)]
pub(crate) fn near(heap: &Heap, array: Cell, i: i64) -> Near {
    let Some(r) = tree(heap, array) else {
        return Near::Other;
    };
    // One below 0 is past the end this way.
    let i = to_usize(i.cast_unsigned());
    if i >= r.len() {
        return Near::Past;
    }
    item(heap, &r, i).map_or(Near::Other, Near::Found)
}

/// Element `i` of the tree `array`, with no reference added, or `None` past the end.
pub(crate) fn get(heap: &Heap, array: Cell, i: usize) -> Option<Value> {
    item(heap, &tree(heap, array)?, i)
}

#[inline(always)]
fn item(heap: &Heap, r: &Root, i: usize) -> Option<Value> {
    match r.place(i)? {
        Place::Leaf(i) => flat::at(heap, leaf(heap, r, i)?, i % B),
        Place::Edge(items, i) => flat::at(heap, items, i),
        Place::Front(v) => Some(v),
    }
}

/// The leaf that element `i` of the leaves of `r` is in.
#[inline(always)]
fn leaf(heap: &Heap, r: &Root, i: usize) -> Option<Value> {
    leaf_under::<false>(heap, r, i)
}

/// [`leaf`], when every branch above it is `ALONE`, if that is asked for.
#[inline(always)]
fn leaf_under<const ALONE: bool>(heap: &Heap, r: &Root, i: usize) -> Option<Value> {
    let at = r.first.wrapping_add(i);
    let (mut node, mut shift) = (r.tree, r.shift);
    while shift > 0 {
        let branch = node.as_cell()?;
        let children: &[u64; B] = match ALONE {
            true => heap.chunk_alone(branch, Kind::ArrayBranch)?,
            false => heap.chunk(branch, Kind::ArrayBranch)?,
        };
        node = Value::from_bits(*children.get(at.wrapping_shr(shift) % B)?);
        shift = shift.saturating_sub(BITS);
    }
    Some(node)
}

/// A short array that a tree is made of, and which element of it was asked for.
struct Part {
    flat: Flat,
    at: usize,
}

impl Part {
    /// How many elements it has from that one on.
    #[inline(always)]
    fn left(&self) -> usize {
        self.flat.len().saturating_sub(self.at)
    }
}

/// The part of `r` that element `i` is in, unless it is the root itself.
#[inline(always)]
fn part(heap: &Heap, r: &Root, i: usize) -> Option<Part> {
    let (items, at) = match r.place(i)? {
        Place::Leaf(i) => (leaf(heap, r, i)?, i % B),
        Place::Edge(items, i) => (items, i),
        Place::Front(_) => return None,
    };
    let flat = short(heap, items).ok()?;
    (at < flat.len()).then_some(Part { flat, at })
}

/// Put the elements of the tree `r` from `at` on in `out`, as many as there is room
/// for, with no reference added. Gives how many it found, which is fewer only where there are
/// no more, or where a part is freed.
pub(crate) fn run(heap: &Heap, r: &Root, at: usize, out: &mut [Value]) -> usize {
    let mut found = 0usize;
    if let (Some(front), 0, [first, ..]) = (r.front(), at, &mut *out) {
        *first = front;
        found = 1;
    }
    let mut left = out.get_mut(found..).unwrap_or_default();
    while !left.is_empty()
        && let Some(part) = part(heap, r, at.saturating_add(found))
    {
        let n = part.left().min(left.len());
        let (now, rest) = left.split_at_mut_checked(n).unwrap_or_default();
        part.flat.read_run(heap, part.at, now);
        (left, found) = (rest, found.saturating_add(n));
    }
    found
}

/// The head of `r`, when the `n` elements from `from` on are all in it, and which of its
/// elements is the first of them.
#[inline(always)]
pub(crate) fn head_with(heap: &Heap, r: &Root, from: usize, n: usize) -> Option<(Flat, usize)> {
    // From one behind what is in the root, that comes to a place in the head too.
    let at = r.start.wrapping_add(from);
    let whole = at.saturating_add(n) <= r.head.len;
    let head = short(heap, r.head.items).ok().filter(|_| whole)?;
    Some((head, at))
}

/// Every element of `s`, front to back, with no reference added, or `None` when it is a
/// range, which stores none.
pub(crate) fn elements(heap: &Heap, s: Seq) -> Option<Vec<Value>> {
    Some(match s {
        Seq::Empty => Vec::new(),
        Seq::Flat(flat) => flat.items(heap).collect(),
        Seq::Tree(array) => elements_of_tree(heap, array),
        Seq::Range { .. } => return None,
    })
}

/// [`elements`] of a tree.
pub(crate) fn elements_of_tree(heap: &Heap, array: Cell) -> Vec<Value> {
    let Some(r) = tree(heap, array) else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(r.len());
    out.extend(r.front());
    while let Some(part) = part(heap, &r, out.len()) {
        for run in part.flat.slices(heap, part.at, part.left()) {
            out.extend(run.iter().copied().map(Value::from_bits));
        }
    }
    out
}

/// Whether an array of `has` elements has the `length`.
#[inline(always)]
pub(crate) fn fits(has: u64, length: Length) -> bool {
    let len = u64::from(length.len);
    has == len || (length.more && has > len)
}

/// What an audit finds wrong with a tree.
pub(crate) enum Malformed {
    /// Its root does not say what is under it, or says it of a short array.
    Root,
    /// A leaf of it is no short array of 32.
    Leaf,
}

/// Whether the tree `array` is laid out as one is.
pub(crate) fn audit(heap: &Heap, array: Cell) -> Result<(), Malformed> {
    let r = tree(heap, array).ok_or(Malformed::Root)?;
    // No word of it says more than was read, so one that starts elsewhere has no front.
    let said = heap.chunk(array, Kind::ArrayRoot) == Some(&r.words());
    let right = |e: Edge| match e.len {
        0 => e.items.bits() == Value::NIL.bits(),
        len => short(heap, e.items).is_ok_and(|flat| flat.len() == len),
    };
    let edges = right(r.head) && right(r.tail);
    let start = match r.lost() {
        0 => r.start == 0 || r.head.len > 0,
        lost => lost < r.head.len && all(heap, r.head.items, (0, lost), plain_inside),
    };
    let least = r.shift.checked_sub(BITS).map(room);
    let fits = r.tree_len <= room(r.shift) && least.is_none_or(|least| r.tree_len > least);
    let placed = r.first < room(r.shift) && r.first.is_multiple_of(B);
    let more = r.tree_len > 0 || (r.head.len > 0 && r.tail.len > 0);
    let whole = said && edges && start && fits && placed && more;
    if !whole || leaves(heap, r.tree)? != r.tree_len {
        return Err(Malformed::Root);
    }
    let there = |i| leaf(heap, &r, i).and_then(|v| short(heap, v).ok());
    match (0..r.tree_len).step_by(B).all(|i| there(i).is_some()) {
        true => Ok(()),
        false => Err(Malformed::Root),
    }
}

/// How many elements are in the leaves under `node`.
fn leaves(heap: &Heap, node: Value) -> Result<usize, Malformed> {
    let Some(cell) = node.as_cell() else {
        let none = node.bits() == Value::NIL.bits();
        return if none { Ok(0) } else { Err(Malformed::Root) };
    };
    if !heap.is(cell, Kind::ArrayBranch) {
        let full = short(heap, node).is_ok_and(|flat| flat.len() == B);
        return if full { Ok(B) } else { Err(Malformed::Leaf) };
    }
    let mut all = 0usize;
    for child in heap.values(cell) {
        all = all.saturating_add(leaves(heap, child)?);
    }
    Ok(all)
}

// Branches. Each of these is handed the reference to the node that it works on.

/// A branch that nothing is under.
fn branch(heap: &mut Heap) -> Result<Cell, Fault> {
    heap.make(Kind::ArrayBranch, &[Value::NIL.bits(); B])
}

/// `node` when nothing else holds it, and otherwise a copy of it. The
/// caller gives up its reference to `node` and holds the result's.
#[inline]
fn owned(heap: &mut Heap, node: Cell) -> Result<Cell, Fault> {
    if heap.unique(node)? {
        return Ok(node);
    }
    let words: &[u64; B] = heap.chunk(node, Kind::ArrayBranch).ok_or(DeadCell)?;
    let words = *words;
    heap.share_each(&words.map(Value::from_bits))?;
    let copy = heap.make(Kind::ArrayBranch, &words)?;
    heap.release(node)?;
    Ok(copy)
}

/// Put `v` in place `i` of `node`, and find what was there.
#[inline]
fn swap(heap: &mut Heap, node: Cell, i: usize, v: Value) -> Value {
    heap.set_field(node, i % B, v).unwrap_or(Value::NIL)
}

/// `node`, at height `shift`, with what `edit` makes of the leaf at place `at`, which is
/// `Nil` where there is none. The branches above a new leaf are made.
fn with_leaf(
    heap: &mut Heap,
    node: Value,
    shift: u32,
    at: usize,
    edit: impl FnOnce(&mut Heap, Value) -> Result<Value, Fault>,
) -> Result<Value, Fault> {
    let Some(below) = shift.checked_sub(BITS) else {
        return edit(heap, node);
    };
    let node = match node.as_cell() {
        Some(node) => owned(heap, node)?,
        None => branch(heap)?,
    };
    let child = swap(heap, node, at.wrapping_shr(shift), Value::NIL);
    let child = with_leaf(heap, child, below, at, edit)?;
    swap(heap, node, at.wrapping_shr(shift), child);
    Ok(Value::cell(node))
}

/// `node`, at height `shift`, without what is in the places from `at` up to `to`, of which
/// there is one. It is `Nil` when nothing is left.
fn without(heap: &mut Heap, node: Value, shift: u32, at: usize, to: usize) -> Result<Value, Fault> {
    let (Some(node), Some(below)) = (node.as_cell(), shift.checked_sub(BITS)) else {
        release(heap, node)?;
        return Ok(Value::NIL);
    };
    let node = owned(heap, node)?;
    let last = to.saturating_sub(1).wrapping_shr(shift);
    for i in at.wrapping_shr(shift)..=last {
        let start = i.wrapping_shl(shift);
        let (at, to) = (
            at.saturating_sub(start),
            to.saturating_sub(start).min(room(below)),
        );
        let child = swap(heap, node, i, Value::NIL);
        if at == 0 && to == room(below) {
            release(heap, child)?;
        } else {
            let child = without(heap, child, below, at, to)?;
            swap(heap, node, i, child);
        }
    }
    let bare = heap.values(node).all(|v| v.as_cell().is_none());
    if bare {
        heap.discard(node)?;
        return Ok(Value::NIL);
    }
    Ok(Value::cell(node))
}

/// The places of `node`, at height `shift`, as two nodes: those in front of `at`, and the
/// others. Either is `Nil` when nothing is there.
fn parted(heap: &mut Heap, node: Value, shift: u32, at: usize) -> Result<[Value; 2], Fault> {
    let at = round(at, shift);
    let (Some(behind), Some(below)) = (node.as_cell(), shift.checked_sub(BITS)) else {
        return Ok([Value::NIL, node]);
    };
    if at == 0 {
        return Ok([Value::NIL, node]);
    }
    let (behind, front) = (owned(heap, behind)?, branch(heap)?);
    let last = at.wrapping_shr(shift);
    for i in 0..last {
        let child = swap(heap, behind, i, Value::NIL);
        swap(heap, front, i, child);
    }
    let child = swap(heap, behind, last, Value::NIL);
    let [a, b] = parted(heap, child, below, at)?;
    swap(heap, front, last, a);
    swap(heap, behind, last, b);
    Ok([front, behind].map(Value::cell))
}

/// `a` and `b` as one node. They have no leaf in one place.
fn united(heap: &mut Heap, a: Value, b: Value) -> Result<Value, Fault> {
    let (x, y) = match (a.as_cell(), b.as_cell()) {
        (Some(x), Some(y)) => (owned(heap, x)?, owned(heap, y)?),
        (Some(_), None) => return Ok(a),
        (None, _) => return Ok(b),
    };
    for i in 0..B {
        let (left, right) = (swap(heap, x, i, Value::NIL), swap(heap, y, i, Value::NIL));
        let both = united(heap, left, right)?;
        swap(heap, x, i, both);
    }
    heap.discard(y)?;
    Ok(Value::cell(x))
}

// The leaves.

/// Make the tree of `r`, every place of which has a leaf, a level taller. Where its leaves
/// go on at the first place, those there come after the others.
fn taller(heap: &mut Heap, r: &mut Root) -> Result<(), Fault> {
    let [after, first] = parted(heap, r.tree, r.shift, r.first)?;
    let top = branch(heap)?;
    swap(heap, top, 0, first);
    swap(heap, top, 1, after);
    r.tree = Value::cell(top);
    r.shift = r.shift.saturating_add(BITS);
    Ok(())
}

/// Make the tree of `r` as many levels less tall as its leaves allow. They are then under
/// one place of its top or under two, and where they are under two they go round.
fn lower(heap: &mut Heap, r: &mut Root) -> Result<(), Fault> {
    while let Some(below) = r.shift.checked_sub(BITS)
        && r.tree_len <= room(below)
    {
        let under = |v: Value| v.as_cell().into_iter().flat_map(|c| heap.values(c));
        let both: smallvec::SmallVec<[Value; 2]> =
            under(r.tree).filter(|v| v.as_cell().is_some()).collect();
        heap.share_each(&both)?;
        release(heap, r.tree)?;
        r.tree = Value::NIL;
        for node in both {
            r.tree = united(heap, r.tree, node)?;
        }
        r.shift = below;
    }
    r.first = round(r.first, r.shift);
    Ok(())
}

/// Put the full `leaf` at `end` of the leaves of `r`.
fn hang(heap: &mut Heap, r: &mut Root, leaf: Value, end: End) -> Result<(), Fault> {
    if r.tree_len == room(r.shift) {
        taller(heap, r)?;
    }
    let at = match end {
        End::Back => r.first.wrapping_add(r.tree_len),
        End::Front => {
            r.first = round(r.first.wrapping_sub(B), r.shift);
            r.first
        }
    };
    r.tree = with_leaf(heap, r.tree, r.shift, at, |_, _| Ok(leaf))?;
    r.tree_len = r.tree_len.wrapping_add(B);
    Ok(())
}

/// Give up the leaves at `end` of `r` that have its `n` last elements there.
fn drop_leaves(heap: &mut Heap, r: &mut Root, n: usize, end: End) -> Result<(), Fault> {
    if n == 0 {
        return Ok(());
    }
    r.tree_len = r.tree_len.saturating_sub(n);
    if r.tree_len == 0 {
        release(heap, r.tree)?;
        (r.tree, r.first, r.shift) = (Value::NIL, 0, 0);
        return Ok(());
    }
    let at = match end {
        End::Back => r.first.wrapping_add(r.tree_len),
        End::Front => {
            let at = r.first;
            r.first = at.wrapping_add(n);
            at
        }
    };
    let (at, all) = (round(at, r.shift), room(r.shift));
    // Up to the last place, and the others from the first one on.
    let to = at.saturating_add(n);
    r.tree = without(heap, r.tree, r.shift, at, to.min(all))?;
    if to > all {
        r.tree = without(heap, r.tree, r.shift, 0, to.saturating_sub(all))?;
    }
    lower(heap, r)
}

/// Make the last leaf at `end` of `r` the edge there, where it has none.
fn take_leaf(heap: &mut Heap, r: &mut Root, end: End) -> Result<(), Fault> {
    let i = match end {
        End::Front => 0,
        End::Back => r.tree_len.saturating_sub(B),
    };
    let leaf = leaf(heap, r, i).ok_or(DeadCell)?;
    heap.own(leaf)?;
    *r.edge(end) = Edge::of(short(heap, leaf)?);
    drop_leaves(heap, r, B, end)
}

// Changing a tree. `open` takes one apart, what comes after it works on the parts, and
// `close` puts them together.

/// More words than it takes to change a tree without adding to it. A change makes a few
/// nodes a level at the most, and no tree is 8 levels tall.
const CHANGE: usize = 8 * 16 * (B + 1);

/// The same for one that puts `items` more elements in it.
fn words_for(items: usize) -> usize {
    let leaves = (items / B).saturating_add(1);
    // A leaf is 34 words, and the branches above the new ones come to 2 for each.
    leaves.saturating_mul(B + 4).saturating_add(CHANGE)
}

/// The words of a big Int that an `i64` holds: its header, its sign and length, and a digit.
const BIG_WORDS: usize = 3;

/// [`words_for`] the Ints from `start` up to `end`, which are more than [`MOST`], and the
/// words of the cells of those that are big.
pub(crate) fn words_for_range(start: i64, end: i64) -> usize {
    let after_small = SMALL_INT_MAX.saturating_add(1);
    let small = range_len(start.max(SMALL_INT_MIN), end.min(after_small));
    let len = range_len(start, end);
    let words = |n: u64| usize::try_from(n).unwrap_or(usize::MAX);
    let big = words(len.saturating_sub(small)).saturating_mul(BIG_WORDS);
    words_for(words(len)).saturating_add(big)
}

/// The parts of the tree `array`, with a reference to each, and its cell when nothing else
/// holds that. `close` then writes it again. Its head is made to have what is in front of it,
/// and nothing that the array has lost.
#[inline(always)]
fn open(heap: &mut Heap, array: Cell, r: Root) -> Result<(Option<Cell>, Root), Fault> {
    let alone = heap.unique(array)?;
    if !alone {
        for part in r.parts() {
            heap.own(part)?;
        }
        heap.release(array)?;
    }
    let mut r = r;
    if let Some(front) = r.front() {
        let head = short(heap, r.head.items)?;
        r.head.items = flat::push(heap, head, front, End::Front)?;
        r.head.len = r.head.len.saturating_add(1);
    }
    let lost = r.lost();
    cut_edge(heap, &mut r.head, lost, End::Front)?;
    r.start = 0;
    Ok((alone.then_some(array), r))
}

/// The array of the parts of `r`, in `cell` when it is there.
#[inline(always)]
fn close(heap: &mut Heap, cell: Option<Cell>, r: Root) -> Result<Value, Fault> {
    if r.tree_len == 0 && (r.head.len == 0 || r.tail.len == 0) {
        if let Some(cell) = cell {
            heap.discard(cell)?;
        }
        return Ok(match (r.head.len, r.tail.len) {
            (0, 0) => Value::EMPTY_ARRAY,
            (0, _) => r.tail.items,
            (_, _) => r.head.items,
        });
    }
    Ok(Value::cell(close_tree(heap, cell, &r)?))
}

/// [`close`] of what stays a tree.
#[inline(always)]
fn close_tree(heap: &mut Heap, cell: Option<Cell>, r: &Root) -> Result<Cell, Fault> {
    let Some(cell) = cell else {
        return heap.make(Kind::ArrayRoot, &r.words());
    };
    write_root(heap, cell, r);
    Ok(cell)
}

/// Have the root `array` say what `r` does.
#[inline(always)]
fn write_root(heap: &mut Heap, array: Cell, r: &Root) {
    if let Some(words) = heap.chunk_mut(array, Kind::ArrayRoot) {
        *words = r.words();
    }
}

/// Leave the edge at `end` of `r` with less than 32.
fn make_room(heap: &mut Heap, r: &mut Root, end: End) -> Result<(), Fault> {
    if r.edge(end).len == B {
        let full = std::mem::replace(r.edge(end), Edge::NONE);
        hang(heap, r, full.items, end)?;
    }
    Ok(())
}

/// Put `items`, in their order and with their references, at `end` of `r`.
fn add(heap: &mut Heap, r: &mut Root, items: &[Value], end: End) -> Result<(), Fault> {
    let mut left = items;
    while !left.is_empty() {
        make_room(heap, r, end)?;
        let edge = r.edge(end);
        let Items { near, rest } = nearest(left, B.saturating_sub(edge.len), end);
        edge.items = match edge.len {
            0 => flat::with_room(heap, near)?,
            _ => {
                let flat = short(heap, edge.items)?;
                flat::extend(heap, flat, near, end)?
            }
        };
        edge.len = edge.len.saturating_add(near.len());
        left = rest;
    }
    Ok(())
}

/// Some items to put at an end of something.
struct Items<'v> {
    /// Those nearest to it.
    near: &'v [Value],
    rest: &'v [Value],
}

/// `items`, of which as many as `room` says are near.
fn nearest(items: &[Value], room: usize, end: End) -> Items<'_> {
    let room = room.min(items.len());
    let (near, rest) = match end {
        End::Back => items.split_at_checked(room),
        End::Front => {
            let split = items.split_at_checked(items.len().saturating_sub(room));
            split.map(|(rest, near)| (near, rest))
        }
    }
    .unwrap_or((items, &[]));
    Items { near, rest }
}

/// Put the elements of `flat` at `end` of `r`. It is the edge there when there is none.
fn add_flat(heap: &mut Heap, r: &mut Root, flat: Flat, end: End) -> Result<(), Fault> {
    make_room(heap, r, end)?;
    if r.edge(end).len == 0 {
        *r.edge(end) = Edge::of(flat);
        return Ok(());
    }
    let all = flat::taken_apart(heap, flat)?;
    add(heap, r, all.items(), end)
}

/// Give up the `n` elements at `end` of `r`, which has more.
fn cut(heap: &mut Heap, r: &mut Root, n: usize, end: End) -> Result<(), Fault> {
    let Some(past) = n.checked_sub(r.edge(end).len) else {
        return cut_near(heap, r, n, end);
    };
    let gone = std::mem::replace(r.edge(end), Edge::NONE);
    release(heap, gone.items)?;
    if let Some(past) = past.checked_sub(r.tree_len) {
        drop_leaves(heap, r, r.tree_len, end)?;
        return cut_edge(heap, r.edge(end.other()), past, end);
    }
    let part = past % B;
    drop_leaves(heap, r, past.saturating_sub(part), end)?;
    if part == 0 {
        return Ok(());
    }
    take_leaf(heap, r, end)?;
    cut_near(heap, r, part, end)
}

/// [`cut`] of some of the edge there. A head that something else holds is not copied when what
/// it loses is plain, and the array stays a tree.
fn cut_near(heap: &mut Heap, r: &mut Root, n: usize, end: End) -> Result<(), Fault> {
    let stays = end == End::Front && (r.tree_len > 0 || r.tail.len > 0);
    if stays && !alone(heap, r.head)? && all(heap, r.head.items, (0, n), plain) {
        r.start = n;
        return Ok(());
    }
    cut_edge(heap, r.edge(end), n, end)
}

/// Whether nothing else holds the short array of `edge`.
fn alone(heap: &Heap, edge: Edge) -> Result<bool, DeadCell> {
    edge.items
        .as_cell()
        .map_or(Ok(true), |cell| heap.unique(cell))
}

/// Give up the `n` elements at `end` of one of the edges of `r`, which has more.
fn cut_edge(heap: &mut Heap, edge: &mut Edge, n: usize, end: End) -> Result<(), Fault> {
    if n == 0 {
        return Ok(());
    }
    let flat = short(heap, edge.items)?;
    edge.len = edge.len.saturating_sub(n);
    edge.items = match end {
        End::Front => flat::skip(heap, flat, n)?,
        End::Back => flat::slice(heap, flat, 0, edge.len)?,
    };
    Ok(())
}

// What the instructions do. Each is handed a reference to the array, and gives it up
// even where there is no room for what is made.

/// The array of `items`, whose references pass to it.
pub(crate) fn from_values(heap: &mut Heap, items: &[Value]) -> Result<Value, Fault> {
    stored(heap, items).map(Stored::value)
}

/// [`from_values`], as what stores its elements.
pub(crate) fn stored(heap: &mut Heap, items: &[Value]) -> Result<Stored, Fault> {
    if items.len() <= MOST {
        let made = flat::new_flat(heap, items);
        return made.map_err(|why| refused(heap, why, items));
    }
    heap.reserve(words_for(items.len()))
        .map_err(|why| refused(heap, why, items))?;
    // The first are the head. The tail is made by itself, because no more may come, and then
    // it has as few slots as its elements fit in.
    let (first, rest) = items.split_at_checked(B).unwrap_or((items, &[]));
    let most = rest.len().saturating_sub(1);
    let rest = rest.split_at_checked(most.saturating_sub(most % B));
    let (full, last) = rest.unwrap_or_default();
    let mut r = Root::EMPTY;
    if let Stored::Flat(head) = flat::new_flat(heap, first)? {
        r.head = Edge::of(head);
    }
    add(heap, &mut r, full, End::Back)?;
    make_room(heap, &mut r, End::Back)?;
    if let Stored::Flat(tail) = flat::new_flat(heap, last)? {
        r.tail = Edge::of(tail);
    }
    let made = close(heap, None, r)?;
    Ok(match made.as_cell() {
        Some(cell) => Stored::Tree(cell),
        None => Stored::Empty,
    })
}

/// What became of a write in place.
pub(crate) enum Wrote {
    Done,
    /// The array has no such index.
    Past,
    /// It is not an array that is written in place, and may have the index.
    Other,
}

/// Put `x`, with its reference, at index `i` of `array`, when it is a tree that has the
/// index and nothing else holds any node on the way there.
#[inline(always)]
pub(crate) fn set_alone(heap: &mut Heap, array: Cell, i: i64, x: Value) -> Result<Wrote, Fault> {
    match heap.is(array, Kind::ArrayRoot) {
        true => set_tree_alone(heap, array, i, x),
        false => Ok(Wrote::Other),
    }
}

#[inline(never)]
fn set_tree_alone(heap: &mut Heap, array: Cell, i: i64, x: Value) -> Result<Wrote, Fault> {
    let Some(r) = tree_alone(heap, array) else {
        return Ok(Wrote::Other);
    };
    // One below 0 is past the end this way.
    let Some(place) = r.place(to_usize(i.cast_unsigned())) else {
        return Ok(Wrote::Past);
    };
    match write_alone(heap, &r, place, x) {
        Some(old) => {
            release(heap, old)?;
            Ok(Wrote::Done)
        }
        None => Ok(Wrote::Other),
    }
}

#[inline(always)]
fn write_alone(heap: &mut Heap, r: &Root, place: Place, x: Value) -> Option<Value> {
    let (items, i) = match place {
        Place::Edge(items, i) => (items, i),
        Place::Leaf(i) => (leaf_under::<true>(heap, r, i)?, i % B),
        Place::Front(_) => return None,
    };
    match flat::set_alone(heap, items.as_cell()?, i, x) {
        Near::Found(old) => Some(old),
        Near::Past | Near::Other => None,
    }
}

/// `array` with `x` at index `i`. An index the array does not have changes nothing.
#[inline]
pub(crate) fn set(heap: &mut Heap, array: Stored, i: usize, x: Value) -> Result<Value, Fault> {
    match array {
        Stored::Empty => {
            release(heap, x)?;
            Ok(Value::EMPTY_ARRAY)
        }
        Stored::Flat(flat) => flat::set(heap, flat, i, x),
        Stored::Tree(array) => set_in_tree(heap, array, i, x),
    }
}

/// [`set`] of a tree.
#[inline(never)]
fn set_in_tree(heap: &mut Heap, array: Cell, i: usize, x: Value) -> Result<Value, Fault> {
    let r = root(heap, array)?;
    if i >= r.len() {
        release(heap, x)?;
        return Ok(Value::cell(array));
    }
    if let Err(why) = heap.reserve(CHANGE) {
        return Err(refused(heap, why, &[x, Value::cell(array)]));
    }
    let (cell, mut r) = open(heap, array, r)?;
    let put = |heap: &mut Heap, items: Value, i: usize| {
        let flat = short(heap, items)?;
        flat::set(heap, flat, i, x)
    };
    let behind = i.saturating_sub(r.head.len);
    if i < r.head.len {
        r.head.items = put(heap, r.head.items, i)?;
    } else if let Some(i) = behind.checked_sub(r.tree_len) {
        r.tail.items = put(heap, r.tail.items, i)?;
    } else {
        let at = r.first.wrapping_add(behind);
        let edit = |heap: &mut Heap, leaf| put(heap, leaf, behind % B);
        r.tree = with_leaf(heap, r.tree, r.shift, at, edit)?;
    }
    close(heap, cell, r)
}

/// Put `x`, with its reference, at `end` of the tree `array`, when nothing else holds it or
/// the short array there, and that has room. Whether it did.
#[inline(never)]
pub(crate) fn push_alone(heap: &mut Heap, array: Cell, x: Value, end: End) -> Result<bool, Fault> {
    put_alone(heap, array, (1, end), |heap, edge| {
        flat::push_alone(heap, edge, x, end)
    })
}

/// Have `put` put `n` elements in the short array at `end` of the tree `array`, when nothing
/// else holds that. Whether it did.
#[inline(always)]
fn put_alone(
    heap: &mut Heap,
    array: Cell,
    (n, end): (usize, End),
    put: impl FnOnce(&mut Heap, Cell) -> bool,
) -> Result<bool, Fault> {
    let Some(r) = tree_alone(heap, array) else {
        return Ok(false);
    };
    let (edge, one) = match end {
        End::Front => (r.head, IN_HEAD),
        End::Back => (r.tail, IN_TAIL),
    };
    let Some(cell) = edge.items.as_cell() else {
        return Ok(false);
    };
    if (end == End::Front && r.start != 0) || !put(heap, cell) {
        return Ok(false);
    }
    add_lens(heap, array, one.wrapping_mul(n as u64));
    Ok(true)
}

/// Add `more` to how many the edges of the root `array` have.
#[inline(always)]
fn add_lens(heap: &mut Heap, array: Cell, more: u64) {
    if let Some([_, _, _, _, lens, _, _]) = heap.chunk_mut(array, Kind::ArrayRoot) {
        *lens = lens.wrapping_add(more);
    }
}

/// `array` with `x` added at `end`.
#[inline(always)]
pub(crate) fn push(heap: &mut Heap, array: Stored, x: Value, end: End) -> Result<Value, Fault> {
    match array {
        Stored::Empty => one(heap, x),
        Stored::Flat(flat) => flat::push(heap, flat, x, end),
        // A handler has asked `push_alone` first.
        Stored::Tree(cell) => extend_apart(heap, cell, &[x], end),
    }
}

/// The array of `x`.
#[inline(never)]
fn one(heap: &mut Heap, x: Value) -> Result<Value, Fault> {
    from_values(heap, &[x])
}

/// `array` with `items`, in their order, at `end`.
#[inline]
pub(crate) fn extend(
    heap: &mut Heap,
    array: Stored,
    items: &[Value],
    end: End,
) -> Result<Value, Fault> {
    match array {
        Stored::Empty => from_values(heap, items),
        Stored::Flat(flat) => flat::extend(heap, flat, items, end),
        Stored::Tree(cell) => extend_tree(heap, cell, items, end),
    }
}

/// [`extend`] of a tree.
#[inline(never)]
fn extend_tree(heap: &mut Heap, array: Cell, items: &[Value], end: End) -> Result<Value, Fault> {
    if put_alone(heap, array, (items.len(), end), |heap, edge| {
        flat::extend_alone(heap, edge, items, end)
    })? {
        return Ok(Value::cell(array));
    }
    extend_apart(heap, array, items, end)
}

/// [`extend_tree`], where there is no room for `items` beside what is there.
#[inline(never)]
fn extend_apart(heap: &mut Heap, array: Cell, items: &[Value], end: End) -> Result<Value, Fault> {
    let r = root(heap, array)?;
    if let Err(why) = heap.reserve(words_for(items.len())) {
        let why = refused(heap, why, items);
        return Err(refused(heap, why, &[Value::cell(array)]));
    }
    let (cell, mut r) = open(heap, array, r)?;
    add(heap, &mut r, items, end)?;
    close(heap, cell, r)
}

/// The short array `flat` with `items` at `end`, where one cell has no room for them all.
#[inline(never)]
fn outgrown(heap: &mut Heap, flat: Flat, items: &[Value], end: End) -> Result<Value, Fault> {
    heap.unique(flat.cell())?;
    if let Err(why) = heap.reserve(words_for(items.len())) {
        let why = refused(heap, why, items);
        return Err(refused(heap, why, &[Value::cell(flat.cell())]));
    }
    let mut r = Root::of(flat, end);
    add(heap, &mut r, items, end)?;
    close(heap, None, r)
}

/// The elements of `array` from `from` up to but not including `to`. Both are places in
/// it.
// Out of line, with the map's two owned edits and [`join`]. Inside the
// interpreter's loop these four are so long that the loop keeps fewer of its
// own values in registers, which costs 4 machine instructions at each VM
// instruction that tests a variant, reads a field or drops a register.
#[inline(never)]
pub(crate) fn slice(
    heap: &mut Heap,
    array: Stored,
    from: usize,
    to: usize,
) -> Result<Value, Fault> {
    match array {
        Stored::Empty => Ok(Value::EMPTY_ARRAY),
        Stored::Flat(flat) => flat::slice(heap, flat, from, to),
        Stored::Tree(array) => slice_tree(heap, array, from, to),
    }
}

/// [`slice`] of a tree.
///
/// What comes to 32 elements or fewer is a short array. But one that nothing else holds,
/// and that keeps more than it gives up, stays the tree that it is. So an array that is
/// pushed to and cut by turns is not copied at each.
#[inline(never)]
fn slice_tree(heap: &mut Heap, array: Cell, from: usize, to: usize) -> Result<Value, Fault> {
    let r = root(heap, array)?;
    let n = to.min(r.len()).saturating_sub(from);
    if n == 0 || n > MOST || n == r.len() || (heap.unique(array)? && n > r.len().saturating_sub(n))
    {
        return cut_to(heap, array, &r, (from, to));
    }
    let made = short_slice(heap, &r, from, n);
    heap.release(array)?;
    made
}

/// The `n` elements of the tree `r` from `from` on, as a short array. It is a leaf, or the
/// head or the tail, when they are all of that and no more.
fn short_slice(heap: &mut Heap, r: &Root, from: usize, n: usize) -> Result<Value, Fault> {
    if let Some(part) = part(heap, r, from)
        && part.at == 0
        && part.flat.len() == n
    {
        return Ok(heap.own(Value::cell(part.flat.cell()))?);
    }
    let mut items = [Value::NIL; MOST];
    let found = run(heap, r, from, items.get_mut(..n).unwrap_or_default());
    let items = items.get(..found).unwrap_or_default();
    heap.share_each(items)?;
    let made = flat::new_flat(heap, items);
    let made = made.inspect_err(|_| heap.unshare_each(items))?;
    Ok(made.value())
}

/// The elements of the tree `array`, whose parts are `r`, from `from` up to `to`. They stay a
/// tree unless they are all in its head or its tail.
fn cut_to(
    heap: &mut Heap,
    array: Cell,
    r: &Root,
    (from, to): (usize, usize),
) -> Result<Value, Fault> {
    let len = r.len();
    if from >= to.min(len) {
        heap.release(array)?;
        return Ok(Value::EMPTY_ARRAY);
    }
    if to >= len {
        if from == 0 {
            // It is asked so that a freed cell is not given back.
            heap.unique(array)?;
            return Ok(Value::cell(array));
        }
        if let Some(left) = skip_in_head(heap, array, r, from)? {
            return Ok(left);
        }
    }
    if let Err(why) = heap.reserve(CHANGE) {
        return Err(refused(heap, why, &[Value::cell(array)]));
    }
    // What it has lost goes with what it loses.
    let (mut r, mut from) = (*r, from);
    if from > 0 {
        from = from.saturating_add(r.lost());
        r.start = r.start.wrapping_sub(r.lost());
    }
    let (cell, mut r) = open(heap, array, r)?;
    cut(heap, &mut r, len.saturating_sub(to), End::Back)?;
    cut(heap, &mut r, from, End::Front)?;
    close(heap, cell, r)
}

/// The tree `array`, whose parts are `r`, without its first `n` elements, when they are its
/// front and in its head, and it is a tree without them. No more than a head and a root are
/// made.
fn skip_in_head(heap: &mut Heap, array: Cell, r: &Root, n: usize) -> Result<Option<Value>, Fault> {
    if !heap.unique(array)? {
        let left = behind(heap, array, r, n);
        let left = left.map_err(|why| refused(heap, why, &[Value::cell(array)]))?;
        if left.is_some() {
            heap.release(array)?;
        }
        return Ok(left);
    }
    let to = r.start.wrapping_add(n);
    if to < r.head.len && skip_alone(heap, array, r, to)? {
        return Ok(Some(Value::cell(array)));
    }
    let left = root_behind(heap, r, to, None);
    let left = left.map_err(|why| refused(heap, why, &[Value::cell(array)]))?;
    let Some(left) = left else {
        return Ok(None);
    };
    share_parts(heap, &left)?;
    write_root(heap, array, &left.root);
    for part in r.parts() {
        release(heap, part)?;
    }
    Ok(Some(Value::cell(array)))
}

/// [`skip_in_head`] of an array that the caller keeps its reference to.
#[inline]
fn behind(heap: &mut Heap, array: Cell, r: &Root, n: usize) -> Result<Option<Value>, Fault> {
    let Some(left) = root_behind(heap, r, r.start.wrapping_add(n), Some(array))? else {
        return Ok(None);
    };
    match heap.make(Kind::ArrayRoot, &left.root.words()) {
        Ok(made) => {
            share_parts(heap, &left)?;
            Ok(Some(Value::cell(made)))
        }
        Err(why) if left.copied => Err(refused(heap, why, &[left.root.head.items])),
        Err(why) => Err(why),
    }
}

/// What is left of a tree.
struct Left {
    root: Root,
    /// Whether its head is a copy, which has a reference already.
    copied: bool,
}

/// Give what is left of a tree a reference to each of its parts.
#[inline(always)]
fn share_parts(heap: &mut Heap, left: &Left) -> Result<(), DeadCell> {
    for part in left.root.parts() {
        heap.own(part)?;
    }
    match left.copied {
        true => release(heap, left.root.head.items),
        false => Ok(()),
    }
}

/// The root of the tree `r` from element `to` of its head on, where that is a tree. What goes may
/// stay in the head when it is plain, and one element in the root of the tree that `held` is.
#[inline(always)]
fn root_behind(
    heap: &mut Heap,
    r: &Root,
    to: usize,
    held: Option<Cell>,
) -> Result<Option<Left>, Fault> {
    if to > r.head.len || (to == r.head.len && r.tree_len == 0) {
        return Ok(None);
    }
    let mut left = Left {
        root: Root { start: 0, ..*r },
        copied: false,
    };
    if to == r.head.len {
        left.root.head = Edge::NONE;
    } else if all(heap, r.head.items, (r.lost(), to), plain) {
        left.root.start = to;
    } else if let Some(array) = held.filter(|_| to == r.start.wrapping_add(1))
        && let Some(root) = move_to_root(heap, array, r)?
    {
        left.root = root;
    } else {
        left.copied = keep_rest(heap, &mut left.root, (r.lost(), to), held.is_some())?;
    }
    Ok(Some(left))
}

/// Have the root `r` start at element `to` of its head, when it has lost none but `from` and what
/// goes is plain inside, and is of an array that is `held`. Otherwise copy the rest of the head.
/// Whether it did.
#[inline(never)]
fn keep_rest(
    heap: &mut Heap,
    r: &mut Root,
    (from, to): (usize, usize),
    held: bool,
) -> Result<bool, Fault> {
    if held && all(heap, r.head.items, (from, to), plain_inside) {
        r.start = to;
        return Ok(false);
    }
    let head = short(heap, r.head.items)?;
    r.head = Edge {
        items: flat::after(heap, head, to)?,
        len: r.head.len.saturating_sub(to),
    };
    Ok(true)
}

/// Whether the elements of the short array `items` from `from` up to `to` are all what `asks`
/// asks for.
#[inline(always)]
fn all(
    heap: &Heap,
    items: Value,
    (from, to): (usize, usize),
    asks: impl Fn(&Heap, Value) -> bool,
) -> bool {
    (from..to).all(|i| flat::at(heap, items, i).is_some_and(|v| asks(heap, v)))
}

/// Whether `v` is no cell, or one that holds nothing. Then an array may lose it and leave it in
/// its head, and nothing can tell, because there is no handle in it.
#[inline(always)]
fn plain(heap: &Heap, v: Value) -> bool {
    v.as_cell().is_none_or(|cell| heap.holds_nothing(cell))
}

/// Whether `v` is [`plain`], or a cell that holds nothing but what is. Nothing can tell either.
fn plain_inside(heap: &Heap, v: Value) -> bool {
    plain(heap, v) || v.as_cell().is_some_and(|cell| heap.holds_no_holder(cell))
}

/// [`skip_in_head`] to element `to` of the head, where nothing else holds the tree or its head.
/// Whether it was that.
fn skip_alone(heap: &mut Heap, array: Cell, r: &Root, to: usize) -> Result<bool, DeadCell> {
    let Some(head) = r.head.items.as_cell() else {
        return Ok(false);
    };
    if !flat::drop_first(heap, head, to)? {
        return Ok(false);
    }
    let front = start_at(heap, array, 0);
    add_lens(heap, array, IN_HEAD.wrapping_mul(to as u64).wrapping_neg());
    release(heap, front)?;
    Ok(true)
}

/// Have the root `array` start at `start` of its head, where its one holder takes the reference
/// to its front, if it has one.
#[inline(always)]
fn start_at(heap: &mut Heap, array: Cell, start: usize) -> Value {
    let Some([front, _, _, _, lens, _, _]) = heap.chunk_mut(array, Kind::ArrayRoot) else {
        return Value::NIL;
    };
    *lens = *lens & !START | u64::from(low_byte(start as u64));
    Value::from_bits(std::mem::replace(front, Value::NIL.bits()))
}

/// Move the first element of the tree `array` from its head to its root, when nothing else holds
/// the head, and find the root that there is without it. So both have the head that is left,
/// whole, and neither has a cell that is not its own. The array is the same to everything that
/// holds it. This is each step of a walk of an array that something holds at each step.
#[inline(always)]
fn move_to_root(heap: &mut Heap, array: Cell, r: &Root) -> Result<Option<Root>, DeadCell> {
    let head = r.head.items.as_cell().ok_or(DeadCell)?;
    if r.start > 0 && !flat::drop_first(heap, head, r.start)? {
        return Ok(None);
    }
    let Some(first) = flat::take_first(heap, head) else {
        return Ok(None);
    };
    let mut left = Root { start: 0, ..*r };
    left.head.len = r.head.len.saturating_sub(r.start).saturating_sub(1);
    let whole = Root {
        start: FRONT,
        front: first.value,
        ..left
    };
    write_root(heap, array, &whole);
    Ok(Some(left))
}

/// The Ints of a range that starts at `start` from its element `from` up to its element
/// `to`, both of which are at most its length.
pub(crate) fn slice_of_range(
    heap: &mut Heap,
    start: i64,
    from: u64,
    to: u64,
) -> Result<Value, Fault> {
    // Each sum lands within the range's ends, and so the add is exact.
    let [from, to] = [from, to].map(|i| start.wrapping_add_unsigned(i));
    range(heap, from, to)
}

/// `array` without its first `n` elements.
#[inline(always)]
pub(crate) fn skip(heap: &mut Heap, array: Stored, n: usize) -> Result<Value, Fault> {
    match array {
        Stored::Empty => Ok(Value::EMPTY_ARRAY),
        Stored::Flat(flat) => flat::skip(heap, flat, n),
        Stored::Tree(array) => skip_tree(heap, array, n),
    }
}

/// [`skip`] of a tree.
#[inline(never)]
fn skip_tree(heap: &mut Heap, array: Cell, n: usize) -> Result<Value, Fault> {
    let r = root(heap, array)?;
    cut_to(heap, array, &r, (n, usize::MAX))
}

/// The first element of the tree `array` and the array of the others, or `None` when it has no
/// such element. That has a reference of its own, and is `Nil` unless `head` asks for it. The
/// caller keeps its reference to `array`.
#[inline(never)]
pub(crate) fn uncons(
    heap: &mut Heap,
    array: Cell,
    head: bool,
) -> Result<Option<(Value, Value)>, Fault> {
    let r = root(heap, array)?;
    let Some(first) = item(heap, &r, 0) else {
        return Ok(None);
    };
    let left = match behind(heap, array, &r, 1)? {
        Some(left) => left,
        None => {
            heap.share(array)?;
            cut_to(heap, array, &r, (1, usize::MAX))?
        }
    };
    let first = if head { first } else { Value::NIL };
    heap.own(first)?;
    Ok(Some((first, left)))
}

/// The first element of the tree `array`, taken out, when nothing else holds that, and nothing
/// else holds its head either, or the element is in the root, or it is plain and is shared with the
/// head. This is every step of a walk but one in 32. It is the array of the others then.
#[inline(always)]
pub(crate) fn take_first(heap: &mut Heap, array: Cell) -> Option<Value> {
    let r = tree_alone(heap, array)?;
    let head = r.head.items.as_cell()?;
    if r.start != 0 || r.head.len == 1 {
        return take_plain(heap, array, r.start, r.head);
    }
    match flat::take_first(heap, head) {
        Some(first) => {
            add_lens(heap, array, IN_HEAD.wrapping_neg());
            Some(first.value)
        }
        None => take_plain(heap, array, 0, r.head),
    }
}

/// [`take_first`] of an element that is in the root, or that stays where it is.
#[inline(never)]
fn take_plain(heap: &mut Heap, array: Cell, start: usize, head: Edge) -> Option<Value> {
    let to = start.wrapping_add(1);
    if start == FRONT {
        return Some(start_at(heap, array, to));
    }
    let first = flat::at(heap, head.items, start)?;
    if to >= head.len || !plain(heap, first) {
        return None;
    }
    heap.own(first).ok()?;
    start_at(heap, array, to);
    Some(first)
}

/// The first Int of the range `array`, when it is a small one, nothing else holds the range
/// and there are more. It then starts at the next.
#[inline(always)]
pub(crate) fn next_of_range(heap: &mut Heap, array: Cell) -> Result<Option<Value>, DeadCell> {
    let Some((start, end)) = range_at(heap, array) else {
        return Ok(None);
    };
    let next = start.checked_add(1).filter(|next| *next < end);
    let (Some(first), Some(next)) = (Value::int(start), next) else {
        return Ok(None);
    };
    if !heap.unique(array)? {
        return Ok(None);
    }
    let [at, _] = heap.data_mut(array) else {
        return Ok(None);
    };
    *at = next.cast_unsigned();
    Ok(Some(first))
}

/// What a step of a join came to.
pub(crate) enum Joined {
    All(Value),
    /// Two arrays that are still to be joined.
    Some(Value, Value),
}

/// `a` and then `b`, or as much of that as one step does. The shorter one's elements are
/// put on the longer, leaf by leaf, and a step moves no more than [`STEP`] of them and a leaf.
// Out of line for the interpreter's loop, as [`slice`] is.
#[inline(never)]
pub(crate) fn join(heap: &mut Heap, a: Stored, b: Stored) -> Result<Joined, Fault> {
    match (a, b) {
        (Stored::Empty, one) | (one, Stored::Empty) => Ok(Joined::All(one.value())),
        (Stored::Flat(x), Stored::Flat(y)) if x.len().saturating_add(y.len()) <= MOST => {
            flat::join(heap, x, y).map(Joined::All)
        }
        (Stored::Flat(_) | Stored::Tree(_), Stored::Flat(_) | Stored::Tree(_)) => {
            let (x, y) = (a.len(heap), b.len(heap));
            match x >= y {
                true => join_onto(heap, a, b, y, End::Back),
                false => join_onto(heap, b, a, x, End::Front),
            }
        }
    }
}

/// `from`, which has `len` elements, at `end` of `to`.
fn join_onto(
    heap: &mut Heap,
    to: Stored,
    from: Stored,
    len: usize,
    end: End,
) -> Result<Joined, Fault> {
    let r = match to {
        Stored::Tree(cell) => root(heap, cell)?,
        Stored::Flat(flat) => Root::of(flat, end),
        Stored::Empty => Root::EMPTY,
    };
    // They are asked so that no freed cell is taken for an array.
    for cell in [to, from].into_iter().filter_map(|a| a.value().as_cell()) {
        heap.unique(cell)?;
    }
    if let Err(why) = heap.reserve(words_for(len.min(STEP))) {
        return Err(refused(heap, why, &[to.value(), from.value()]));
    }
    let (cell, mut r) = match to {
        Stored::Tree(cell) => open(heap, cell, r)?,
        Stored::Flat(_) | Stored::Empty => (None, r),
    };
    let left = match from {
        Stored::Empty => None,
        Stored::Flat(flat) => {
            add_flat(heap, &mut r, flat, end)?;
            None
        }
        Stored::Tree(from) => {
            let mut from = from;
            let of = root(heap, from)?;
            if of.start != 0 {
                let (cell, of) = open(heap, from, of)?;
                from = close_tree(heap, cell, &of)?;
            }
            let moved = add_some(heap, &mut r, from, end)?;
            Some((from, moved))
        }
    };
    let to = close(heap, cell, r)?;
    let Some((from, moved)) = left else {
        return Ok(Joined::All(to));
    };
    if moved >= len {
        heap.release(from)?;
        return Ok(Joined::All(to));
    }
    let of = root(heap, from)?;
    let left = match end {
        End::Back => cut_to(heap, from, &of, (moved, len)),
        End::Front => cut_to(heap, from, &of, (0, len.saturating_sub(moved))),
    };
    match (left, end) {
        (Ok(left), End::Back) => Ok(Joined::Some(to, left)),
        (Ok(left), End::Front) => Ok(Joined::Some(left, to)),
        (Err(why), _) => Err(refused(heap, why, &[to])),
    }
}

/// Put some elements of `from`, those nearest to it, at `end` of `r`. How many. `from` has
/// them still.
fn add_some(heap: &mut Heap, r: &mut Root, from: Cell, end: End) -> Result<usize, Fault> {
    let of = root(heap, from)?;
    let len = of.len();
    let mut moved = 0usize;
    while moved < len.min(STEP) {
        let i = match end {
            End::Back => moved,
            End::Front => len.saturating_sub(moved).saturating_sub(1),
        };
        let part = part(heap, &of, i).ok_or(DeadCell)?;
        heap.share(part.flat.cell())?;
        add_flat(heap, r, part.flat, end)?;
        moved = moved.saturating_add(part.flat.len());
    }
    Ok(moved)
}

#[cfg(test)]
mod tests;
