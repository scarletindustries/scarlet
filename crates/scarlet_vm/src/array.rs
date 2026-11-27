//! An array of 1 to 32 elements is one cell of them ([`flat`]). A longer one is a
//! persistent RRB (relaxed radix-balanced) tree in the process's heap, the same shape as
//! the old VM's (its `seq.rs`).
//!
//! ```text
//!   root [ len | shift | offset | head | tree | tail ]
//!                                  |      |      |
//!              a push to the front |      |      | a push to the back
//!              lands here (a leaf) |      |      | lands here (a leaf)
//!                                         |
//!                          branches, 32 children each, with a
//!                          table of how many elements each holds
//!                               /         |          \
//!                            leaf        leaf        leaf    (32 elements each)
//! ```
//!
//! The two buffer leaves make a push at either end cheap. Only a full buffer
//! spills into the tree, once every 32 pushes. Every branch keeps the running
//! total of its children's elements, so finding an element, cutting the array
//! and joining two of them, as in the RRB paper (Bagwell & Rompf, 2011), each
//! visit one node per level.
//!
//! An array something else holds is never changed. An operation on one
//! builds new nodes along the one path it touches and shares every other
//! node with the array it came from, which stays as it was. A node shared
//! this way holds one more reference.
//!
//! Everything here that gives an array is handed one reference to each array that it
//! works on, and to each element that goes in, and gives up what it does not put
//! in the array that it gives. So a count of 1 says that nothing else holds an array,
//! which may then change in place. The exceptions are [`uncons`] and [`flat::uncons`], of an
//! array that is not to change, which take nothing. What is refused for want of room has given up
//! what it was handed too, but for what a join of two trees had made by then.
//!
//! This is the layout of a tree, in words after each cell's header:
//!
//! - A root holds `len`, `shift` (the tree's height, 0 for a lone leaf) and
//!   the head's `offset`, then the head, tree and tail, each a node or `Nil`.
//! - A leaf holds 1 to 32 elements. A short array's cell that has 32, the first of them
//!   in its first slot, is a leaf too, so an array that outgrows its cell keeps it.
//! - A branch holds `shift`, then `n` running totals, then `n` children, all
//!   one level down, with `n` from 1 to 32.
//!
//! The walks here go down at most one level per call, so they are only as
//! deep as the tree is tall. That is 13 levels at the very most, for 2^64
//! elements.
//!
//! A tree is made where an array comes to have more than 32 elements. One that then loses
//! elements from its front, which is what `[h, ..t]` does, stays a tree until only its
//! tail is left, or nothing, whatever is put in it meanwhile. So does one that nothing else
//! holds and that a slice leaves with more than it takes from its front. So an array that is
//! now a little longer than 32 and now a little shorter is not copied each time. Whatever
//! else is of 32 elements or fewer is flat, every other slice of a tree that leaves something
//! out among them, and no reader of an array asks which of the two it is but to find the
//! elements.
//!
//! An array can also be a range, `start..end`, which stores only its two
//! ends, as the old VM did. Its length, its elements, its tail and its
//! slices all come from those two numbers. Adding to a range, or joining one to
//! another array, first builds the array of its elements.
//!
//! The array with no elements is none of these. It is a word of its own,
//! [`Value::EMPTY_ARRAY`], and no cell is ever one. Every cell of an array is made by
//! [`new_root`], [`range`], [`new_node`] or one of the makers of [`flat`], which make
//! none for no elements, or is a copy of one that was. [`Seq`] says which of them an
//! array is. What is written for one of the forms asks a cell whether it is that one, and
//! answers for no other.

use crate::code::{Length, Split};
use crate::heap::{Cell, DeadCell, Fault, Heap, Kind};
use crate::value::Value;
use crate::word::to_usize;

pub(crate) mod flat;

pub(crate) use flat::Flat;
use flat::MOST;

/// Children per branch, and elements per leaf.
const B: usize = 32;
/// Bits of an index each level takes, which is `log2(B)`.
const BITS: usize = 5;
/// How many more nodes than the tightest packing a level may keep after a
/// join, which bounds the extra steps a lookup takes (the RRB paper's `e`).
const E_MAX: usize = 2;

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
}

impl Seq {
    #[inline(always)]
    pub(crate) fn len(self, heap: &Heap) -> u64 {
        match self {
            Seq::Empty => 0,
            Seq::Flat(flat) => flat.len() as u64,
            Seq::Tree(cell) => tree(heap, cell).map_or(0, |r| r.len as u64),
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

    /// A head and a tail as the buffer at this end and the one at the other,
    /// or those two as a head and a tail.
    fn turn(self, a: Value, b: Value) -> (Value, Value) {
        match self {
            End::Front => (a, b),
            End::Back => (b, a),
        }
    }
}

/// Where a root keeps its head, tree and tail, in words after its header.
const ROOT_HEAD: usize = 3;
const ROOT_TREE: usize = 4;
const ROOT_TAIL: usize = 5;

#[derive(Clone, Copy)]
pub(crate) struct Root {
    len: usize,
    shift: usize,
    /// Items at the front of `head` this array has left behind. Dropping from
    /// the front moves this along and keeps the same leaf, so `[h, ..t]`
    /// copies nothing. A push at the front writes over them when nothing else holds
    /// the leaf, and otherwise settles it back to 0. `take` and `concat` keep it along
    /// with the head.
    offset: usize,
    head: Value,
    tree: Value,
    tail: Value,
}

impl Root {
    /// The empty array's.
    const EMPTY: Root = Root {
        len: 0,
        shift: 0,
        offset: 0,
        head: Value::NIL,
        tree: Value::NIL,
        tail: Value::NIL,
    };
}

/// A node copied out of the heap, for the paths that build new nodes and so
/// need the heap mutably while they hold it.
#[expect(
    clippy::large_enum_variant,
    reason = "a Node lives on the stack for one step of building a node; boxing the \
              bigger variant would bring back the allocation these buffers avoid"
)]
enum Node {
    Leaf(Slots),
    Branch {
        shift: usize,
        sizes: smallvec::SmallVec<[u64; B]>,
        children: Slots,
    },
}

/// A node's slots, taken out to build a new node from. A node has at most
/// `B`, so they stay on the stack.
type Slots = smallvec::SmallVec<[Value; B]>;

/// A branch's words after its header: its height, and the total and the
/// reference of each child.
type Words = smallvec::SmallVec<[u64; 2 * B + 1]>;

/// A node read in place. A lookup goes through this, so it copies nothing and
/// allocates nothing.
enum View<'h> {
    Leaf(&'h [u64]),
    Branch {
        shift: usize,
        sizes: &'h [u64],
        children: &'h [u64],
    },
}

impl View<'_> {
    /// Elements under this node.
    fn len(&self) -> usize {
        match self {
            View::Leaf(items) => items.len(),
            View::Branch { sizes, .. } => to_usize(sizes.last().copied().unwrap_or(0)),
        }
    }

    /// Slot `i`, which is an element of a leaf or a child of a branch.
    fn slot(&self, i: usize) -> Option<Value> {
        let words = match self {
            View::Leaf(items) => items,
            View::Branch { children, .. } => children,
        };
        words.get(i).copied().map(Value::from_bits)
    }
}

/// The root that `cell` is. The callers know it for a tree, so one that is none
/// has been freed.
#[inline(always)]
fn root(heap: &Heap, cell: Cell) -> Result<Root, DeadCell> {
    tree(heap, cell).ok_or(DeadCell)
}

/// The root that `cell` is, or `None` when it is something else.
#[inline(always)]
fn tree(heap: &Heap, cell: Cell) -> Option<Root> {
    let [len, shift, offset, head, tree, tail] = heap.chunk(cell, Kind::ArrayRoot)?;
    Some(Root {
        len: to_usize(*len),
        shift: to_usize(*shift),
        offset: to_usize(*offset),
        head: Value::from_bits(*head),
        tree: Value::from_bits(*tree),
        tail: Value::from_bits(*tail),
    })
}

/// How many live items the head has, which is what `offset` leaves of its
/// leaf.
#[inline(always)]
fn head_len(heap: &Heap, r: &Root) -> usize {
    leaf_items(heap, r.head).len().saturating_sub(r.offset)
}

/// How many elements of `r` are in its inner tree, which is what its two buffers leave, when
/// they have `head` and `tail` live items.
#[inline(always)]
fn left_for_tree(r: &Root, head: usize, tail: usize) -> usize {
    r.len.wrapping_sub(head).wrapping_sub(tail)
}

/// The same for a root whose buffers have not been looked at.
#[inline(always)]
fn tree_len(heap: &Heap, r: &Root) -> usize {
    let tail = leaf_items(heap, r.tail).len();
    left_for_tree(r, head_len(heap, r), tail)
}

/// The head's live items, which are what `offset` leaves of its leaf.
fn head_items(heap: &Heap, r: &Root) -> Slots {
    match view(heap, r.head) {
        Some(View::Leaf(items)) => items
            .get(r.offset..)
            .unwrap_or_default()
            .iter()
            .copied()
            .map(Value::from_bits)
            .collect(),
        Some(View::Branch { .. }) | None => Slots::new(),
    }
}

/// A leaf of exactly the head's live items, owned, or `Nil` when there are
/// none. When nothing was left behind, this is the head itself, shared
/// instead of copied.
fn own_head(heap: &mut Heap, r: &Root) -> Result<Value, Fault> {
    if r.offset == 0 {
        return Ok(heap.own(r.head)?);
    }
    leaf_behind(heap, r.head, r.offset)
}

/// `v` as a node read in place, or `None` for `Nil`.
fn view(heap: &Heap, v: Value) -> Option<View<'_>> {
    view_here(heap, v)
}

/// [`view`] inside its caller, where an array's elements are read.
#[inline(always)]
fn view_here(heap: &Heap, v: Value) -> Option<View<'_>> {
    let cell = v.as_cell()?;
    if let Some(items) = heap.leaf(cell) {
        return Some(View::Leaf(items));
    }
    if !heap.is(cell, Kind::ArrayBranch) {
        return None;
    }
    let d = heap.data(cell);
    let n = d.len().saturating_sub(1) / 2;
    let rest = d.get(1..).unwrap_or(&[]);
    let (sizes, children) = rest.split_at(n.min(rest.len()));
    Some(View::Branch {
        shift: to_usize(d.first().copied().unwrap_or(0)),
        sizes,
        children,
    })
}

/// `v` as a node copied out of the heap, or `None` for `Nil`.
fn node(heap: &Heap, v: Value) -> Option<Node> {
    Some(match view(heap, v)? {
        View::Leaf(items) => Node::Leaf(items.iter().copied().map(Value::from_bits).collect()),
        View::Branch {
            shift,
            sizes,
            children,
        } => Node::Branch {
            shift,
            sizes: sizes.iter().copied().collect(),
            children: children.iter().copied().map(Value::from_bits).collect(),
        },
    })
}

fn own_all(heap: &mut Heap, vs: &[Value]) -> Result<Slots, DeadCell> {
    for v in vs {
        heap.own(*v)?;
    }
    Ok(Slots::from_slice(vs))
}

fn release(heap: &mut Heap, v: Value) -> Result<(), DeadCell> {
    match v.as_cell() {
        Some(cell) => heap.release(cell),
        None => Ok(()),
    }
}

/// The array that `r` says. The references to its parts pass to it. One of no
/// elements has no parts.
// Kept inside its callers, as `Heap::make` is and for the same reason.
#[inline]
fn new_root(heap: &mut Heap, r: Root) -> Result<Value, Fault> {
    if r.len == 0 {
        return Ok(Value::EMPTY_ARRAY);
    }
    Ok(Value::cell(root_of(heap, r)?))
}

/// [`new_root`] of what has an element.
#[inline(always)]
fn root_of(heap: &mut Heap, r: Root) -> Result<Cell, Fault> {
    let words = [
        r.len as u64,
        r.shift as u64,
        r.offset as u64,
        r.head.bits(),
        r.tree.bits(),
        r.tail.bits(),
    ];
    heap.make(Kind::ArrayRoot, &words)
}

/// Make the tree `array`, which nothing else holds, the one that `r` says. The references
/// to the parts of `r` pass to it, and those that it had are the caller's.
#[inline(always)]
fn rewrite(heap: &mut Heap, array: Cell, r: &Root) {
    if let [l, s, o, h, t, e] = heap.data_mut(array) {
        (*l, *s, *o) = (r.len as u64, r.shift as u64, r.offset as u64);
        (*h, *t, *e) = (r.head.bits(), r.tree.bits(), r.tail.bits());
    }
}

/// A leaf or a branch of `words`, or `Nil` when there are none.
#[inline(always)]
fn new_node(heap: &mut Heap, kind: Kind, words: &[u64]) -> Result<Value, Fault> {
    if words.is_empty() {
        return Ok(Value::NIL);
    }
    Ok(Value::cell(heap.make(kind, words)?))
}

/// A leaf of `items`, whose references pass to it, or `Nil` when there are none.
fn leaf(heap: &mut Heap, items: &[Value]) -> Result<Value, Fault> {
    let data: smallvec::SmallVec<[u64; B]> = items.iter().map(|v| v.bits()).collect();
    new_node(heap, Kind::ArrayLeaf, &data)
}

/// A leaf of what the leaf `v` has from its item `from` on, with a reference of its own
/// to each, or `Nil` when it has none there.
fn leaf_behind(heap: &mut Heap, v: Value, from: usize) -> Result<Value, Fault> {
    let mut room = [0u64; B];
    let items = leaf_items(heap, v).get(from..).unwrap_or_default();
    let n = items.len().min(B);
    for (word, item) in room.iter_mut().zip(items) {
        *word = *item;
    }
    let kept = room.get(..n).unwrap_or_default();
    for word in kept {
        heap.own(Value::from_bits(*word))?;
    }
    new_node(heap, Kind::ArrayLeaf, kept)
}

/// A branch at height `shift` over `children`, whose references pass to it, or `Nil` when
/// there are none. Children shared between arrays can hold more elements together than a
/// word can count, and that is a branch too big to make.
fn branch(heap: &mut Heap, shift: usize, children: &[Value]) -> Result<Value, Fault> {
    if children.is_empty() {
        return Ok(Value::NIL);
    }
    let mut totals: smallvec::SmallVec<[u64; B]> = smallvec::SmallVec::new();
    let mut total: u64 = 0;
    for c in children {
        total = total
            .checked_add(node_len(heap, *c) as u64)
            .ok_or(Fault::Full)?;
        totals.push(total);
    }
    let mut data = Words::new();
    data.push(shift as u64);
    data.extend_from_slice(&totals);
    data.extend(children.iter().map(|c| c.bits()));
    new_node(heap, Kind::ArrayBranch, &data)
}

/// The height one level above `shift`. A tree is at most 13 levels tall, so
/// this can only fail on a height no tree has.
fn up(shift: usize) -> Result<usize, Fault> {
    shift.checked_add(BITS).ok_or(Fault::Full)
}

/// The height one level below `shift`, which is never below a leaf's.
fn down(shift: usize) -> usize {
    shift.saturating_sub(BITS)
}

/// The slots of `xs` after slot `i`.
fn after(xs: &[Value], i: usize) -> &[Value] {
    xs.get(i..).and_then(|rest| rest.get(1..)).unwrap_or(&[])
}

/// Elements under a node.
fn node_len(heap: &Heap, v: Value) -> usize {
    view(heap, v).map_or(0, |n| n.len())
}

/// How many items a leaf holds, or children a branch does, read in place.
fn slot_count(heap: &Heap, v: Value) -> usize {
    match view(heap, v) {
        None => 0,
        Some(View::Leaf(items)) => items.len(),
        Some(View::Branch { children, .. }) => children.len(),
    }
}

/// A node's own slots, which are a leaf's elements or a branch's children.
fn slots(heap: &Heap, v: Value) -> Slots {
    match node(heap, v) {
        None => Slots::new(),
        Some(Node::Leaf(items)) => items,
        Some(Node::Branch { children, .. }) => children,
    }
}

/// Where an element sits in a branch: which child holds it, and its index
/// inside that child.
struct Slot {
    child: usize,
    within: usize,
}

/// The slot of element `idx` in a branch at `shift`. The scan starts at the
/// radix guess `idx >> shift`, which never overshoots, since a child holds at
/// most `1 << shift` elements. It is `None` when the size table says the
/// children before that one hold more than `idx`, which no branch made here
/// does.
fn size_slot(sizes: &[u64], idx: usize, shift: usize) -> Option<Slot> {
    let last = sizes.len().saturating_sub(1);
    let start = (idx >> shift).min(last);
    let child = (start..last)
        .find(|c| sizes.get(*c).is_none_or(|s| to_usize(*s) > idx))
        .unwrap_or(last);
    let before = match child.checked_sub(1) {
        Some(p) => to_usize(sizes.get(p).copied().unwrap_or(0)),
        None => 0,
    };
    Some(Slot {
        child,
        within: idx.checked_sub(before)?,
    })
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

/// The words that a leaf and a branch take at most, and those that a root does.
const LEAF_WORDS: usize = B + 1;
const BRANCH_WORDS: usize = 2 * B + 2;
const ROOT_WORDS: usize = 7;

/// More words than it takes to put `items` items in a tree of height `shift`, or to copy
/// the way to one, with a root and the buffers of it.
fn words_for(items: usize, shift: usize) -> usize {
    let levels = (shift / BITS).saturating_add(3);
    let leaves = (items / B).saturating_add(3);
    let way = levels.saturating_mul(BRANCH_WORDS);
    let each = way.saturating_add(LEAF_WORDS);
    leaves.saturating_mul(each).saturating_add(ROOT_WORDS)
}

/// The array of `items`, whose references pass to it.
pub(crate) fn from_values(heap: &mut Heap, items: &[Value]) -> Result<Value, Fault> {
    stored(heap, items).map(Stored::value)
}

/// [`from_values`], as what stores its elements.
pub(crate) fn stored(heap: &mut Heap, items: &[Value]) -> Result<Stored, Fault> {
    let made = match items.len() <= MOST {
        true => flat::new_flat(heap, items),
        false => tree_of(heap, items),
    };
    made.map_err(|why| refused(heap, why, items))
}

/// A tree of `items`, whose references pass to it. A whole number of full leaves go
/// in a tightly packed tree, and the rest, 1 to 32, in the tail.
fn tree_of(heap: &mut Heap, items: &[Value]) -> Result<Stored, Fault> {
    // Every node is asked for here, so none is refused once there are some. A leaf and the
    // root are a word longer than what they hold, and a branch has two for each child.
    let full = items.len().saturating_sub(1) / B;
    let mut words = items.len().saturating_add(full).saturating_add(1);
    let mut nodes = full;
    while nodes > 1 {
        let above = nodes.div_ceil(B);
        let these = nodes.saturating_add(above).saturating_mul(2);
        words = words.saturating_add(these);
        nodes = above;
    }
    heap.reserve(words.saturating_add(ROOT_WORDS))?;
    let in_leaves = full.saturating_mul(B);
    let (full, tail_items) = items.split_at_checked(in_leaves).unwrap_or((items, &[]));
    let mut nodes = Slots::new();
    for chunk in full.chunks(B) {
        nodes.push(leaf(heap, chunk)?);
    }
    let mut shift = 0;
    while nodes.len() > 1 {
        shift = up(shift)?;
        let mut level = Slots::new();
        for chunk in nodes.chunks(B) {
            level.push(branch(heap, shift, chunk)?);
        }
        nodes = level;
    }
    let tree = nodes.pop().unwrap_or(Value::NIL);
    let tail = leaf(heap, tail_items)?;
    let r = Root {
        len: items.len(),
        shift,
        tree,
        tail,
        ..Root::EMPTY
    };
    Ok(Stored::Tree(root_of(heap, r)?))
}

/// What was found at an index.
pub(crate) enum Near {
    Found(Value),
    /// The array has no such index.
    Past,
    /// It is not an array that was asked about, and may have the index.
    Other,
}

/// What became of a write in place.
pub(crate) enum Wrote {
    Done,
    /// The array has no such index.
    Past,
    /// It is not an array that is written in place, and may have the index.
    Other,
}

/// Element `i` of `array`, when it is a tree, with no reference added.
// Joins make trees so long that an index below 0, taken for one that is not, is in them.
#[inline(always)]
pub(crate) fn near(heap: &Heap, array: Cell, i: i64) -> Near {
    let Some(r) = tree(heap, array) else {
        return Near::Other;
    };
    match usize::try_from(i) {
        Ok(i) if i < r.len => get(heap, array, i).map_or(Near::Other, Near::Found),
        Ok(_) | Err(_) => Near::Past,
    }
}

/// Element `i` of the tree `array`, with no reference added, or `None` past the end.
// Inside its callers, the first of which reads element after element.
#[inline(always)]
pub(crate) fn get(heap: &Heap, array: Cell, i: usize) -> Option<Value> {
    let r = tree(heap, array)?;
    if i >= r.len {
        return None;
    }
    let head = leaf_items(heap, r.head).get(r.offset..).unwrap_or_default();
    let Some(mut idx) = i.checked_sub(head.len()) else {
        return head.get(i).copied().map(Value::from_bits);
    };
    let tail = leaf_items(heap, r.tail);
    let in_tree = left_for_tree(&r, head.len(), tail.len());
    if let Some(in_tail) = idx.checked_sub(in_tree) {
        return tail.get(in_tail).copied().map(Value::from_bits);
    }
    let mut n = r.tree;
    loop {
        let v = view_here(heap, n)?;
        match v {
            View::Leaf(_) => return v.slot(idx),
            View::Branch { shift, sizes, .. } => {
                let slot = size_slot(sizes, idx, shift)?;
                idx = slot.within;
                n = v.slot(slot.child)?;
            }
        }
    }
}

/// `node` when nothing else holds it, and otherwise a copy of it. The
/// caller gives up its reference to `node` and holds the result's.
// The test goes inside the caller, so a node that is its holder's alone
// costs no call.
#[inline]
fn owned(heap: &mut Heap, node: Cell) -> Result<Cell, Fault> {
    if heap.unique(node)? {
        Ok(node)
    } else {
        copied(heap, node)
    }
}

/// A copy of `node`, which something else holds too. The caller gives up
/// its reference to `node`, which only takes one off its count.
#[inline(never)]
fn copied(heap: &mut Heap, node: Cell) -> Result<Cell, Fault> {
    let copy = heap.copy(node)?;
    heap.release(node)?;
    Ok(copy)
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
#[inline(always)]
fn set_in_tree(heap: &mut Heap, mut array: Cell, i: usize, x: Value) -> Result<Value, Fault> {
    let r = root(heap, array)?;
    let written = match i < r.len {
        true => write(heap, &mut array, &r, i, x),
        false => Ok(false),
    };
    match written {
        Ok(true) => {}
        Ok(false) => not_written(heap, &r, x)?,
        Err(why) => return Err(refused(heap, why, &[x, Value::cell(array)])),
    }
    Ok(Value::cell(array))
}

/// Give up `x`, which no slot of the tree `r` was there for. Only a tree that is not whole has
/// an index and not the slot: its totals are wrong, or a part of it is freed. `get` finds
/// nothing there either, so there is nothing to write over.
#[cold]
#[inline(never)]
fn not_written(heap: &mut Heap, r: &Root, x: Value) -> Result<(), DeadCell> {
    for part in [r.head, r.tree, r.tail] {
        if let Some(part) = part.as_cell() {
            heap.unique(part)?;
        }
    }
    release(heap, x)
}

/// Put `x`, with its reference, at index `i` of `array`, when it is a tree that nothing else
/// holds and has the index. Nodes under it that something else holds are copied on the way.
#[inline(always)]
pub(crate) fn set_alone(heap: &mut Heap, array: Cell, i: i64, x: Value) -> Result<Wrote, Fault> {
    let Some(r) = tree(heap, array) else {
        return Ok(Wrote::Other);
    };
    let Some(i) = usize::try_from(i).ok().filter(|i| *i < r.len) else {
        return Ok(Wrote::Past);
    };
    if !heap.unique(array)? {
        return Ok(Wrote::Other);
    }
    let mut alone = array;
    Ok(match write(heap, &mut alone, &r, i, x)? {
        true => Wrote::Done,
        false => Wrote::Other,
    })
}

/// Put `x` at index `i` of the tree `array`, whose parts are `r`. Whether it did. The
/// array is then one that nothing else holds.
///
/// The walk goes from the root down to the leaf that holds the slot. A node
/// nothing else holds is written where it is. A node something else holds is
/// copied first, and the copy takes its place in the node above, which by
/// then is this array's alone. A copy adds a reference to everything its
/// node held, so every node below it on the way is copied in its turn. So
/// an array with one holder changes in place, and a write to one something
/// else holds copies the root and one node per level, and shares the rest
/// with the array it came from, which stays as it was.
///
/// Where there is no room for a copy, the array is as it was, and `x` is not in it.
#[inline(never)]
fn write(heap: &mut Heap, array: &mut Cell, r: &Root, i: usize, x: Value) -> Result<bool, Fault> {
    *array = owned(heap, *array)?;
    // The part of the root that holds the slot: its word in the root, the
    // node there, and the slot's index under that node.
    let (mut word, mut node, mut idx) = match i.checked_sub(head_len(heap, r)) {
        None => (ROOT_HEAD, r.head, r.offset.saturating_add(i)),
        Some(past) => match past.checked_sub(tree_len(heap, r)) {
            None => (ROOT_TREE, r.tree, past),
            Some(in_tail) => (ROOT_TAIL, r.tail, in_tail),
        },
    };
    let mut holder = *array;
    while let Some(shared) = node.as_cell() {
        let cell = owned(heap, shared)?;
        if cell != shared
            && let Some(w) = heap.data_mut(holder).get_mut(word)
        {
            *w = Value::cell(cell).bits();
        }
        match view_here(heap, Value::cell(cell)) {
            Some(View::Branch {
                shift,
                sizes,
                children,
            }) => {
                let Some(at) = size_slot(sizes, idx, shift) else {
                    break;
                };
                let Some(child) = children.get(at.child) else {
                    break;
                };
                holder = cell;
                // A branch's children come after its height and size table.
                word = sizes.len().saturating_add(1).saturating_add(at.child);
                node = Value::from_bits(*child);
                idx = at.within;
            }
            Some(View::Leaf(_)) => {
                let Some(slot) = heap.data_mut(cell).get_mut(idx) else {
                    break;
                };
                let old = Value::from_bits(std::mem::replace(slot, x.bits()));
                release(heap, old)?;
                return Ok(true);
            }
            None => break,
        }
    }
    Ok(false)
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
    let mut out = Vec::with_capacity(r.len);
    out.extend(head_items(heap, &r));
    // A stack of nodes still to visit, last child first, so they pop in order.
    let mut todo = vec![r.tree];
    while let Some(n) = todo.pop() {
        match view(heap, n) {
            None => {}
            Some(View::Leaf(items)) => out.extend(items.iter().copied().map(Value::from_bits)),
            Some(View::Branch { children, .. }) => {
                todo.extend(children.iter().rev().copied().map(Value::from_bits));
            }
        }
    }
    if let Some(View::Leaf(items)) = view(heap, r.tail) {
        out.extend(items.iter().copied().map(Value::from_bits));
    }
    out
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
    let made = flat::new_flat(heap, &[x]);
    let made = made.map(Stored::value);
    made.map_err(|why| refused(heap, why, &[x]))
}

/// Put `x`, with its reference, at `end` of the tree `array`, when nothing else holds it or
/// the leaf there, and the leaf has room. Whether it did.
#[inline(never)]
pub(crate) fn push_alone(heap: &mut Heap, array: Cell, x: Value, end: End) -> Result<bool, Fault> {
    match heap.is(array, Kind::ArrayRoot) {
        true => pushed(heap, array, &[x], end),
        false => Ok(false),
    }
}

/// Put `items`, in their order, at `end` of `array`, whose one holder gives up a reference
/// to each of them, when nothing else holds the leaf there and it has room. Whether it
/// did. The leaf's items and these then move to a longer leaf.
#[inline(always)]
fn pushed(heap: &mut Heap, array: Cell, items: &[Value], end: End) -> Result<bool, Fault> {
    let r = root(heap, array)?;
    let (edge, slot) = match end {
        End::Front => (r.head, ROOT_HEAD),
        End::Back => (r.tail, ROOT_TAIL),
    };
    if !heap.unique(array)? {
        return Ok(false);
    }
    let Some(leaf) = edge.as_cell() else {
        return Ok(false);
    };
    if end == End::Front && r.offset > 0 {
        // As in `settled`.
        if items.len() > r.offset || !heap.unique(leaf)? {
            return Ok(false);
        }
        let len = r.len.checked_add(items.len()).ok_or(Fault::Full)?;
        put_behind(heap, leaf, r.offset, items)?;
        let offset = r.offset.saturating_sub(items.len());
        rewrite(heap, array, &Root { len, offset, ..r });
        return Ok(true);
    }
    let alone = heap.unique(leaf)? && heap.is(leaf, Kind::ArrayLeaf);
    let old = heap.data(leaf);
    let n = old.len().saturating_add(items.len());
    if !alone || n > B {
        return Ok(false);
    }
    let len = r.len.checked_add(items.len()).ok_or(Fault::Full)?;
    let mut words = [0u64; B];
    let (at, kept) = match end {
        End::Front => (0, items.len()..n),
        End::Back => (old.len(), 0..old.len()),
    };
    if let Some(dst) = words.get_mut(kept) {
        dst.copy_from_slice(old);
    }
    for (w, x) in words.iter_mut().skip(at).zip(items) {
        *w = x.bits();
    }
    let made = new_node(heap, Kind::ArrayLeaf, words.get(..n).unwrap_or(&[]))?;
    heap.discard(leaf)?;
    if let Some(d) = heap.data_mut(array).get_mut(..6) {
        if let Some(w) = d.get_mut(0) {
            *w = len as u64;
        }
        if let Some(w) = d.get_mut(slot) {
            *w = made.bits();
        }
    }
    Ok(true)
}

/// Put `items` in the slots of the leaf `leaf` in front of the one at `offset`, and give up
/// all that was left behind there, which is at least as many. Nothing else holds the leaf.
fn put_behind(heap: &mut Heap, leaf: Cell, offset: usize, items: &[Value]) -> Result<(), DeadCell> {
    let mut gone = Slots::new();
    let left = heap.data_mut(leaf).get_mut(..offset).unwrap_or_default();
    let (behind, room) = left
        .split_at_mut_checked(offset.saturating_sub(items.len()))
        .unwrap_or_default();
    // Those that an earlier push gave up are `Nil`.
    for word in behind.iter_mut().filter(|w| **w != Value::NIL.bits()) {
        gone.push(Value::from_bits(std::mem::replace(word, Value::NIL.bits())));
    }
    for (word, x) in room.iter_mut().zip(items) {
        gone.push(Value::from_bits(std::mem::replace(word, x.bits())));
    }
    for v in gone {
        release(heap, v)?;
    }
    Ok(())
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

/// The short array `flat` with `items` at `end`, where they are more than a short array
/// has. Its elements, with the first of `items` beside them, are as many as one has, and are
/// one leaf. A cell that has them all is the leaf itself, whoever else holds it.
#[inline(never)]
fn outgrown(heap: &mut Heap, flat: Flat, items: &[Value], end: End) -> Result<Value, Fault> {
    let cell = flat.cell();
    if let Err(why) = heap.reserve(words_for(items.len(), 0)) {
        let why = refused(heap, why, items);
        return Err(refused(heap, why, &[Value::cell(cell)]));
    }
    let Items { near, rest } = parted(items, MOST.saturating_sub(flat.len()), end);
    let tree = match flat::is_leaf(heap, flat) {
        true => Value::cell(cell),
        false => {
            let all = flat::taken_apart(heap, flat, near, end)?;
            leaf(heap, all.items())?
        }
    };
    // In the inner tree, and not in the buffer at the other end, whose leaf a walk would come
    // to copy at each step.
    let r = Root {
        len: MOST.saturating_add(rest.len()),
        tree,
        ..Root::EMPTY
    };
    let r = extended(heap, r, rest, end)?;
    new_root(heap, r)
}

/// Some items to put at an end of something.
struct Items<'v> {
    /// Those nearest to it.
    near: &'v [Value],
    rest: &'v [Value],
}

/// `items`, of which as many as `room` says are near.
fn parted(items: &[Value], room: usize, end: End) -> Items<'_> {
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

/// [`extend`] of a tree.
///
/// It makes the array that pushing the items one at a time does, with a leaf for
/// each run of them where that makes one for each item. The buffer at that end
/// takes as many as it has room for, and a full one goes into the tree before
/// the next is begun.
#[inline(never)]
fn extend_tree(heap: &mut Heap, cell: Cell, items: &[Value], end: End) -> Result<Value, Fault> {
    // A whole leaf of them never fits in one that has any.
    let put = match items.len() < B {
        true => pushed(heap, cell, items, end),
        false => Ok(false),
    };
    match put {
        Ok(true) => Ok(Value::cell(cell)),
        Ok(false) => extend_apart(heap, cell, items, end),
        Err(why) => Err(refused_push(heap, why, cell, items)),
    }
}

/// [`extend_tree`] where `items` are not put in the tree's leaf in place.
#[inline(never)]
fn extend_apart(heap: &mut Heap, cell: Cell, items: &[Value], end: End) -> Result<Value, Fault> {
    let alone = heap.unique(cell)?;
    let r = root(heap, cell)?;
    let len = match room_for(heap, &r, items) {
        Ok(len) => len,
        Err(why) => return Err(refused_push(heap, why, cell, items)),
    };
    // This holds a reference to each part of the root from here on. They are
    // the root's own when nothing else holds it.
    if !alone {
        for part in [r.head, r.tree, r.tail] {
            heap.own(part)?;
        }
    }
    let r = extended(heap, Root { len, ..r }, items, end)?;
    if alone {
        rewrite(heap, cell, &r);
        return Ok(Value::cell(cell));
    }
    heap.release(cell)?;
    new_root(heap, r)
}

/// `why`, where what was to be put in the tree `array` is given up with it.
#[cold]
#[inline(never)]
fn refused_push(heap: &mut Heap, why: Fault, array: Cell, items: &[Value]) -> Fault {
    let why = refused(heap, why, items);
    refused(heap, why, &[Value::cell(array)])
}

/// The length of the tree `r` with `items` in it, when there is room for them.
fn room_for(heap: &mut Heap, r: &Root, items: &[Value]) -> Result<usize, Fault> {
    let len = r.len.checked_add(items.len()).ok_or(Fault::Full)?;
    heap.reserve(words_for(items.len(), r.shift))?;
    Ok(len)
}

/// The parts of a tree with `items` at `end`, which its length counts already. The caller
/// gives up its reference to each of `r`'s parts and to each item, and gets those of the
/// result's parts.
#[inline(always)]
fn extended(heap: &mut Heap, r: Root, items: &[Value], end: End) -> Result<Root, Fault> {
    let (mut head, mut offset, mut left) = (r.head, r.offset, items);
    if end == End::Front && r.offset > 0 {
        Front { head, offset, left } = settled(heap, &r, items)?;
    }
    let (mut edge, other) = end.turn(head, r.tail);
    let (mut tree, mut shift) = (r.tree, r.shift);
    while !left.is_empty() {
        if slot_count(heap, edge) >= B {
            (tree, shift) = tree_push_leaf(heap, tree, shift, edge, end)?;
            edge = Value::NIL;
        }
        let room = B.saturating_sub(slot_count(heap, edge));
        let Items { near, rest } = parted(left, room, end);
        edge = longer_leaf(heap, edge, near, end)?;
        left = rest;
    }
    let (head, tail) = end.turn(edge, other);
    Ok(Root {
        len: r.len,
        shift,
        offset,
        head,
        tree,
        tail,
    })
}

/// A head that is to be added to, and what is `left` to add.
struct Front<'v> {
    head: Value,
    offset: usize,
    left: &'v [Value],
}

/// The head of `r`, which has left some items behind, with as many of `items` in their place
/// as there is room for, or, when something else holds the leaf, with only its live items. The caller gives up its
/// reference to the head, and gets that of the result.
#[inline(never)]
fn settled<'v>(heap: &mut Heap, r: &Root, items: &'v [Value]) -> Result<Front<'v>, Fault> {
    if let Some(leaf) = r.head.as_cell()
        && heap.unique(leaf)?
    {
        let Items { near, rest } = parted(items, r.offset, End::Front);
        put_behind(heap, leaf, r.offset, near)?;
        return Ok(Front {
            head: r.head,
            offset: r.offset.saturating_sub(near.len()),
            left: rest,
        });
    }
    let head = own_head(heap, r)?;
    release(heap, r.head)?;
    Ok(Front {
        head,
        offset: 0,
        left: items,
    })
}

/// A leaf of the items of the leaf `old`, or of none when it is `Nil`, with
/// `items` at `end` of them. The caller gives up its reference to `old` and
/// one to each item.
fn longer_leaf(heap: &mut Heap, old: Value, items: &[Value], end: End) -> Result<Value, Fault> {
    let Some(cell) = old.as_cell() else {
        return leaf(heap, items);
    };
    let mut room = [0u64; B];
    let kept = leaf_items(heap, old);
    let all = kept.len().saturating_add(items.len()).min(B);
    let (from, at) = match end {
        End::Back => (0, kept.len()),
        End::Front => (items.len(), 0),
    };
    let kept = from..from.saturating_add(kept.len());
    for (w, v) in room.iter_mut().skip(at).zip(items) {
        *w = v.bits();
    }
    if let Some(room) = room.get_mut(kept.clone()) {
        room.copy_from_slice(leaf_items(heap, old));
    }
    let alone = heap.unique(cell)?;
    if !alone {
        for w in room.get(kept).unwrap_or_default() {
            heap.own(Value::from_bits(*w))?;
        }
    }
    let made = new_node(heap, Kind::ArrayLeaf, room.get(..all).unwrap_or_default())?;
    // What a leaf that was this array's alone held is in the new one now.
    if alone {
        heap.discard(cell)?;
    } else {
        heap.release(cell)?;
    }
    Ok(made)
}

/// `tree` with the full leaf `leaf` hung under its edge at `end`, growing a
/// level when that edge is full. The caller gives up its reference to each.
fn tree_push_leaf(
    heap: &mut Heap,
    tree: Value,
    shift: usize,
    leaf: Value,
    end: End,
) -> Result<(Value, usize), Fault> {
    if tree.as_cell().is_none() {
        return Ok((leaf, 0));
    }
    if let Some(n) = try_push(heap, tree, shift, leaf, end)? {
        return Ok((n, shift));
    }
    let spine = spine(heap, leaf, shift)?;
    let pair = match end {
        End::Front => [spine, tree],
        End::Back => [tree, spine],
    };
    let shift = up(shift)?;
    Ok((branch(heap, shift, &pair)?, shift))
}

/// A chain of one-child branches lifting the owned `leaf` to height `shift`.
fn spine(heap: &mut Heap, leaf: Value, shift: usize) -> Result<Value, Fault> {
    let mut n = leaf;
    for s in (BITS..=shift).step_by(BITS) {
        n = branch(heap, s, &[n])?;
    }
    Ok(n)
}

/// `n` with `leaf` hung under its edge at `end` without growing taller, or
/// `None` when every level on that edge is full. The caller gives up its
/// reference to each, unless the answer is `None`. A branch nothing else holds
/// is changed in place, or its children move to one a child wider.
fn try_push(
    heap: &mut Heap,
    n: Value,
    shift: usize,
    leaf: Value,
    end: End,
) -> Result<Option<Value>, Fault> {
    let (Some(cell), Some(View::Branch { children, .. })) = (n.as_cell(), view(heap, n)) else {
        return Ok(None);
    };
    let edge = match end {
        End::Front => 0,
        End::Back => children.len().saturating_sub(1),
    };
    let Some(child) = children.get(edge).copied().map(Value::from_bits) else {
        return Ok(None);
    };
    let more = node_len(heap, leaf) as u64;
    let alone = heap.unique(cell)?;
    if !alone {
        heap.own(child)?;
    }
    if let Some(sub) = try_push(heap, child, down(shift), leaf, end)? {
        if alone {
            lengthen(heap, cell, edge, sub, more)?;
            return Ok(Some(n));
        }
        let mut kept = Slots::new();
        for (i, c) in slots(heap, n).into_iter().enumerate() {
            kept.push(if i == edge { sub } else { heap.own(c)? });
        }
        let made = branch(heap, shift, &kept)?;
        heap.release(cell)?;
        return Ok(Some(made));
    }
    if !alone {
        release(heap, child)?;
    }
    let Some(View::Branch {
        sizes, children, ..
    }) = view(heap, n)
    else {
        return Ok(None);
    };
    if children.len() >= B {
        return Ok(None);
    }
    let (mut data, slot) = wider(shift, sizes, children, more, end)?;
    let new = spine(heap, leaf, down(shift))?;
    if let Some(w) = data.get_mut(slot) {
        *w = new.bits();
    }
    if !alone {
        let shared = slots(heap, n);
        own_all(heap, &shared)?;
    }
    let made = new_node(heap, Kind::ArrayBranch, &data)?;
    if alone {
        heap.discard(cell)?;
    } else {
        heap.release(cell)?;
    }
    Ok(Some(made))
}

/// Put `sub` in slot `edge` of the branch in `cell`, for a child that has
/// `more` fewer elements. The child's reference was the caller's to give up.
fn lengthen(heap: &mut Heap, cell: Cell, edge: usize, sub: Value, more: u64) -> Result<(), Fault> {
    let data = heap.data_mut(cell);
    let n = data.len().saturating_sub(1) / 2;
    // Every total from the child's on counts its elements.
    let totals = data.get_mut(1..).and_then(|d| d.get_mut(..n));
    for total in totals.into_iter().flatten().skip(edge) {
        *total = total.checked_add(more).ok_or(Fault::Full)?;
    }
    let slot = n.saturating_add(1).saturating_add(edge);
    if let Some(w) = data.get_mut(slot) {
        *w = sub.bits();
    }
    Ok(())
}

/// The words of a branch at height `shift` over `children`, whose totals are
/// `sizes`, and a child of `more` elements at `end` of them. Also the place in
/// it of that child, which is `Nil` so far.
fn wider(
    shift: usize,
    sizes: &[u64],
    children: &[u64],
    more: u64,
    end: End,
) -> Result<(Words, usize), Fault> {
    let sum = |total: u64| total.checked_add(more).ok_or(Fault::Full);
    let mut data = Words::new();
    data.push(shift as u64);
    let slot = match end {
        End::Back => {
            data.extend_from_slice(sizes);
            data.push(sum(sizes.last().copied().unwrap_or(0))?);
            data.extend_from_slice(children);
            let slot = data.len();
            data.push(Value::NIL.bits());
            slot
        }
        End::Front => {
            data.push(more);
            for total in sizes {
                data.push(sum(*total)?);
            }
            let slot = data.len();
            data.push(Value::NIL.bits());
            data.extend_from_slice(children);
            slot
        }
    };
    Ok((data, slot))
}

/// The owned `n` without the one-child levels at its top.
fn collapse(heap: &mut Heap, mut n: Value, mut shift: usize) -> Result<(Value, usize), DeadCell> {
    while shift > 0 {
        let children = slots(heap, n);
        let [only] = children.as_slice() else {
            break;
        };
        let only = heap.own(*only)?;
        release(heap, n)?;
        n = only;
        shift = down(shift);
    }
    Ok((n, shift))
}

/// The first `n` elements.
fn take(heap: &mut Heap, array: Cell, n: usize) -> Result<Value, Fault> {
    let r = root(heap, array)?;
    if n >= r.len {
        heap.share(array)?;
        return Ok(Value::cell(array));
    }
    let front = head_items(heap, &r);
    let Some(m) = n.checked_sub(front.len()).filter(|m| *m > 0) else {
        let kept = front.get(..n).unwrap_or(&[]);
        heap.share_each(kept)?;
        let cut = Root {
            len: n,
            tail: leaf(heap, kept)?,
            ..Root::EMPTY
        };
        return new_root(heap, cut);
    };
    let Some(in_tail) = m.checked_sub(node_len(heap, r.tree)).filter(|k| *k > 0) else {
        let cut = tree_take(heap, r.tree, m)?;
        let (tree, shift) = collapse(heap, cut, r.shift)?;
        let head = heap.own(r.head)?;
        let cut = Root {
            len: n,
            shift,
            offset: r.offset,
            head,
            tree,
            tail: Value::NIL,
        };
        return new_root(heap, cut);
    };
    let tail_items = slots(heap, r.tail);
    let kept = tail_items.get(..in_tail).unwrap_or(&[]);
    heap.share_each(kept)?;
    let tail = leaf(heap, kept)?;
    let head = heap.own(r.head)?;
    let tree = heap.own(r.tree)?;
    let cut = Root {
        len: n,
        head,
        tree,
        tail,
        ..r
    };
    new_root(heap, cut)
}

/// The elements of `array` from `from` up to but not including `to`. Both are places in
/// it.
// Out of line, with the map's two owned edits and [`concat`]. Inside the
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
/// What comes to 32 elements or fewer is copied to one cell. But one that nothing else holds,
/// and that keeps more than it gives up, stays the tree that it is, as a walk of it does. So
/// an array that is pushed to and cut by turns is not copied at each.
#[inline(never)]
fn slice_tree(heap: &mut Heap, array: Cell, from: usize, to: usize) -> Result<Value, Fault> {
    let len = root(heap, array)?.len;
    let kept = len.saturating_sub(from);
    let alone = heap.unique(array)?;
    let to_the_end = to >= len;
    if to_the_end && alone && (kept > MOST || kept > from) {
        return skip_in_tree(heap, array, from);
    }
    let cut = match to_the_end && kept > MOST {
        true => skip_tree(heap, array, from),
        false => slice_of_tree(heap, array, from, to),
    };
    heap.release(array)?;
    cut
}

/// [`slice`] of a tree, to which the caller keeps its reference.
fn slice_of_tree(heap: &mut Heap, array: Cell, from: usize, to: usize) -> Result<Value, Fault> {
    let r = root(heap, array)?;
    let n = to.saturating_sub(from);
    if n == 0 {
        return Ok(Value::EMPTY_ARRAY);
    }
    if n >= r.len {
        heap.share(array)?;
        return Ok(Value::cell(array));
    }
    if n <= MOST {
        return short_slice(heap, &r, from, n);
    }
    heap.reserve(words_for(B, r.shift))?;
    let front = take(heap, array, to)?;
    let Some(front) = front.as_cell() else {
        return Ok(front);
    };
    let cut = skip_tree(heap, front, from);
    heap.release(front)?;
    cut
}

/// The `n` elements of the tree `r` from `from` on, as a short array.
fn short_slice(heap: &mut Heap, r: &Root, from: usize, n: usize) -> Result<Value, Fault> {
    let mut items = [Value::NIL; MOST];
    let found = run(heap, r, from, items.get_mut(..n).unwrap_or_default());
    let items = items.get(..found).unwrap_or_default();
    heap.share_each(items)?;
    let made = flat::new_flat(heap, items);
    let made = made.inspect_err(|_| heap.unshare_each(items))?;
    Ok(made.value())
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
        Stored::Tree(array) => skip_in_tree(heap, array, n),
    }
}

/// Every element of the tree `array` but the first `n`. The caller keeps its reference.
/// When there is no room for it nothing has changed.
fn skip_tree(heap: &mut Heap, array: Cell, n: usize) -> Result<Value, Fault> {
    let r = root(heap, array)?;
    if n == 0 {
        heap.share(array)?;
        return Ok(Value::cell(array));
    }
    if n >= r.len {
        return Ok(Value::EMPTY_ARRAY);
    }
    let Some(m) = n.checked_sub(head_len(heap, &r)) else {
        // This is the cheap `[h, ..t]`. It keeps the same head leaf, tree and
        // tail, with `n` more items left behind, so nothing is copied. The
        // head is one leaf, so the offset stays below `B`.
        return with_more_left_behind(heap, &r, n);
    };
    if m >= tree_len(heap, &r) {
        // Only the tail is left, or some of it, which one cell has room for.
        return short_slice(heap, &r, n, r.len.saturating_sub(n));
    }
    if r.shift > 0 {
        let cut = skip_into_tree(heap, &r, n, m)?;
        return new_root(heap, cut);
    }
    let cut = new_root(heap, leaf_as_head(&r, n, m))?;
    heap.own(r.tree)?;
    heap.own(r.tail)?;
    Ok(cut)
}

/// The parts of the tree `r`, which has a branch, without its first `n` elements, of which
/// the last `m` are in its inner tree and are not all of that. Each part holds a reference of
/// its own. There is room for their root, and when there is none, nothing has changed.
///
/// The tree's leftmost leaf moves up to be the new head, whole and shared, and `m` of it are
/// left behind. The tree is cut once per leaf instead of once per element, so walking an
/// array cuts it every `B` steps, and the offset covers the steps between.
fn skip_into_tree(heap: &mut Heap, r: &Root, n: usize, m: usize) -> Result<Root, Fault> {
    let first = first_leaf(heap, r.tree).filter(|leaf| m < leaf_items(heap, *leaf).len());
    let taken = first.map_or(0, |leaf| leaf_items(heap, leaf).len());
    heap.reserve(words_for(0, r.shift))?;
    let (gone, offset) = match taken {
        0 => (m, 0),
        taken => (taken, m),
    };
    let (tree, shift) = if gone == node_len(heap, r.tree) {
        (Value::NIL, 0)
    } else {
        let cut = tree_drop(heap, r.tree, gone)?;
        collapse(heap, cut, r.shift)?
    };
    let head = heap.own(first.unwrap_or(Value::NIL))?;
    let tail = heap.own(r.tail)?;
    Ok(Root {
        len: r.len.saturating_sub(n),
        shift,
        offset,
        head,
        tree,
        tail,
    })
}

/// [`skip_into_tree`] of one leaf, which is no tree once it is the head. No node is made, and
/// no reference added.
fn leaf_as_head(r: &Root, n: usize, m: usize) -> Root {
    Root {
        len: r.len.saturating_sub(n),
        offset: m,
        head: r.tree,
        tree: Value::NIL,
        ..*r
    }
}

/// Leave `n` more items of the head of `array`, whose parts are `r`, behind in place.
#[inline(always)]
fn left_behind(heap: &mut Heap, array: Cell, r: &Root, n: usize) {
    if let [l, _, o, ..] = heap.data_mut(array) {
        *l = r.len.saturating_sub(n) as u64;
        *o = r.offset.saturating_add(n) as u64;
    }
}

/// A tree of the parts of `r` that leaves `n` more items of its head behind, of which
/// there are more. When there is no room for it nothing has changed.
#[inline(always)]
fn with_more_left_behind(heap: &mut Heap, r: &Root, n: usize) -> Result<Value, Fault> {
    let cut = Root {
        len: r.len.saturating_sub(n),
        offset: r.offset.saturating_add(n),
        ..*r
    };
    let cut = new_root(heap, cut)?;
    for part in [r.head, r.tree, r.tail] {
        heap.own(part)?;
    }
    Ok(cut)
}

/// [`skip`] of a tree. One that nothing else holds is changed in place for as long as some
/// of its head or its inner tree is left. While the head has more than `n` items, which is
/// every step of a walk but one in `B`, the root just leaves `n` more of them behind.
#[inline(always)]
fn skip_in_tree(heap: &mut Heap, array: Cell, n: usize) -> Result<Value, Fault> {
    let r = root(heap, array)?;
    if !heap.unique(array)? {
        return skip_held(heap, array, n);
    }
    if n > 0 && n < head_len(heap, &r) {
        left_behind(heap, array, &r, n);
        return Ok(Value::cell(array));
    }
    skip_alone(heap, array, &r, n)
}

/// [`skip_in_tree`] of a tree that is not changed.
#[inline(never)]
fn skip_held(heap: &mut Heap, array: Cell, n: usize) -> Result<Value, Fault> {
    let made = skip_tree(heap, array, n);
    heap.release(array)?;
    made
}

/// [`skip_in_tree`] of one that nothing else holds, whose parts are `r`. What is left starts
/// in its inner tree or behind it, or is all of it.
#[inline(never)]
fn skip_alone(heap: &mut Heap, array: Cell, r: &Root, n: usize) -> Result<Value, Fault> {
    let past_head = n.checked_sub(head_len(heap, r));
    let Some(m) = past_head.filter(|m| n > 0 && *m < tree_len(heap, r)) else {
        return skip_held(heap, array, n);
    };
    if r.shift == 0 {
        rewrite(heap, array, &leaf_as_head(r, n, m));
        release(heap, r.head)?;
        return Ok(Value::cell(array));
    }
    let cut = match skip_into_tree(heap, r, n, m) {
        Ok(cut) => cut,
        Err(why) => return Err(refused(heap, why, &[Value::cell(array)])),
    };
    rewrite(heap, array, &cut);
    for part in [r.head, r.tree, r.tail] {
        release(heap, part)?;
    }
    Ok(Value::cell(array))
}

/// The first element of the tree `array`, when nothing else holds it and it only has to
/// leave one more item of its head behind, which is every step of a walk but one in `B`. It
/// does. The element holds a reference of its own.
#[inline(always)]
pub(crate) fn leave_first(heap: &mut Heap, array: Cell) -> Result<Option<Value>, DeadCell> {
    let Some(r) = tree(heap, array) else {
        return Ok(None);
    };
    let first = match leaf_items(heap, r.head).get(r.offset..) {
        Some([first, _, ..]) => Value::from_bits(*first),
        Some(_) | None => return Ok(None),
    };
    if !heap.unique(array)? {
        return Ok(None);
    }
    heap.own(first)?;
    left_behind(heap, array, &r, 1);
    Ok(Some(first))
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

/// The first element of the tree `array` and the array of the others, or `None` when its
/// cell says that it has none, which no array's does. The element holds a reference of its
/// own, and is `Nil` unless `head` asks for it. This changes nothing in place, and the
/// caller keeps its reference to `array`.
#[inline(always)]
pub(crate) fn uncons(
    heap: &mut Heap,
    array: Cell,
    head: bool,
) -> Result<Option<(Value, Value)>, Fault> {
    let r = root(heap, array)?;
    if r.len == 0 {
        return Ok(None);
    }
    let first = first_of(heap, &r, head)?;
    let cut = match head_len(heap, &r) > 1 {
        true => with_more_left_behind(heap, &r, 1),
        false => skip_tree(heap, array, 1),
    };
    with_first(heap, first, cut)
}

/// [`uncons`], where the caller gives up its reference to `array`, unless the array has no
/// element. One that nothing else holds may change in place.
#[inline(never)]
pub(crate) fn uncons_owned(
    heap: &mut Heap,
    array: Cell,
    head: bool,
) -> Result<Option<(Value, Value)>, Fault> {
    let r = root(heap, array)?;
    if r.len == 0 {
        return Ok(None);
    }
    let first = first_of(heap, &r, head)?;
    let cut = skip_in_tree(heap, array, 1);
    with_first(heap, first, cut)
}

/// The first element of the tree `r`, with a reference of its own, or `Nil` unless `head`
/// asks for it.
#[inline(always)]
fn first_of(heap: &mut Heap, r: &Root, head: bool) -> Result<Value, DeadCell> {
    if !head {
        return Ok(Value::NIL);
    }
    let first = match leaf_items(heap, r.head).get(r.offset) {
        Some(first) => Value::from_bits(*first),
        None => first_past_head(heap, r),
    };
    heap.own(first)
}

/// `first` and the array `cut`, where there was room for that.
#[inline(always)]
fn with_first(
    heap: &mut Heap,
    first: Value,
    cut: Result<Value, Fault>,
) -> Result<Option<(Value, Value)>, Fault> {
    match cut {
        Ok(cut) => Ok(Some((first, cut))),
        Err(why) => Err(refused(heap, why, &[first])),
    }
}

/// Whether an array of `has` elements has the `length`.
#[inline(always)]
pub(crate) fn fits(has: u64, length: Length) -> bool {
    let len = u64::from(length.len);
    has == len || (length.more && has > len)
}

/// What [`split`] found.
pub(crate) enum Found {
    /// The array has another length.
    Miss,
    /// The elements, which are in the buffer.
    All,
    /// A tree that has the length, and whose elements are not all in its first leaf.
    Apart(Root),
    /// It is no tree.
    Other,
}

/// Put the elements of `array`, when it is a tree, from `split.from` on in `out`, as many as
/// there is room for, when it has a length that `split` asks for and they are all in its
/// first leaf. No reference is added.
#[inline(always)]
pub(crate) fn split(heap: &Heap, array: Cell, split: Split, out: &mut [Value]) -> Found {
    let Some(r) = tree(heap, array) else {
        return Found::Other;
    };
    if !fits(r.len as u64, split.length) {
        return Found::Miss;
    }
    if out.is_empty() {
        return Found::All;
    }
    let head = leaf_items(heap, r.head).get(r.offset..).unwrap_or_default();
    let items = if head.is_empty() && r.tree.as_cell().is_none() {
        leaf_items(heap, r.tail)
    } else {
        head
    };
    let items = items.get(split.from as usize..);
    match items.and_then(|items| items.get(..out.len())) {
        Some(items) => {
            for (v, word) in out.iter_mut().zip(items) {
                *v = Value::from_bits(*word);
            }
            Found::All
        }
        None => Found::Apart(r),
    }
}

/// Put the elements of the tree `r` from `from` on in `out`, as many as there is room
/// for, with no reference added. Gives how many it found, which is fewer only in a tree
/// whose totals are wrong.
#[inline(never)]
pub(crate) fn run(heap: &Heap, r: &Root, from: usize, out: &mut [Value]) -> usize {
    let head = leaf_items(heap, r.head).get(r.offset..).unwrap_or_default();
    let in_tree = view_here(heap, r.tree).map_or(0, |n| n.len());
    let (mut at, mut left) = (from, out);
    let mut found = 0usize;
    while !left.is_empty() {
        let items = match at.checked_sub(head.len()) {
            None => head.get(at..),
            Some(past) => match past.checked_sub(in_tree) {
                None => leaf_from(heap, r.tree, past),
                Some(in_tail) => leaf_items(heap, r.tail).get(in_tail..),
            },
        };
        let items = items.unwrap_or_default();
        let n = items.len().min(left.len());
        if n == 0 {
            break;
        }
        let (now, rest) = left.split_at_mut_checked(n).unwrap_or_default();
        for (v, word) in now.iter_mut().zip(items) {
            *v = Value::from_bits(*word);
        }
        (at, left, found) = (at.saturating_add(n), rest, found.saturating_add(n));
    }
    found
}

/// The items of the leaf `v`, which are none when it is no leaf.
#[inline(always)]
fn leaf_items(heap: &Heap, v: Value) -> &[u64] {
    let items = v.as_cell().and_then(|cell| heap.leaf(cell));
    items.unwrap_or_default()
}

/// The items from `i` on of the leaf under the node `n` that element `i` of it is in.
#[inline(always)]
fn leaf_from(heap: &Heap, mut n: Value, mut i: usize) -> Option<&[u64]> {
    loop {
        let v = view_here(heap, n)?;
        match v {
            View::Leaf(items) => return items.get(i..),
            View::Branch { shift, sizes, .. } => {
                let slot = size_slot(sizes, i, shift)?;
                i = slot.within;
                n = v.slot(slot.child)?;
            }
        }
    }
}

/// The first element of the tree `r` that is not in its head, with no reference added.
#[inline(never)]
fn first_past_head(heap: &Heap, r: &Root) -> Value {
    let leaf = first_leaf(heap, r.tree).unwrap_or(r.tail);
    let first = leaf_items(heap, leaf).first();
    first.map_or(Value::NIL, |w| Value::from_bits(*w))
}

/// The tree's leftmost leaf, borrowed, or `None` when there is no tree.
fn first_leaf(heap: &Heap, n: Value) -> Option<Value> {
    let mut n = n;
    loop {
        match view(heap, n)? {
            View::Leaf(_) => return Some(n),
            View::Branch { children, .. } => n = Value::from_bits(*children.first()?),
        }
    }
}

/// The first `m` elements of a tree node, `m` from 1 to its length.
fn tree_take(heap: &mut Heap, n: Value, m: usize) -> Result<Value, Fault> {
    match node(heap, n) {
        None => Ok(Value::NIL),
        Some(Node::Leaf(items)) if m >= items.len() => Ok(heap.own(n)?),
        Some(Node::Leaf(items)) => {
            let kept = items.get(..m).unwrap_or(&[]);
            heap.share_each(kept)?;
            leaf(heap, kept)
        }
        Some(Node::Branch {
            shift,
            sizes,
            children,
        }) => {
            if m >= to_usize(sizes.last().copied().unwrap_or(0)) {
                return Ok(heap.own(n)?);
            }
            // The slot of the last element kept, so `within` is one less
            // than what that child keeps.
            let Some(slot) = m
                .checked_sub(1)
                .and_then(|last| size_slot(&sizes, last, shift))
            else {
                return Ok(Value::NIL);
            };
            let Some(&child) = children.get(slot.child) else {
                return Ok(heap.own(n)?);
            };
            let cut = tree_take(heap, child, slot.within.saturating_add(1))?;
            let mut kept = children;
            kept.truncate(slot.child);
            heap.share_each(&kept)?;
            kept.push(cut);
            branch(heap, shift, &kept)
        }
    }
}

/// A tree node without its first `m` elements, `m` below its length.
fn tree_drop(heap: &mut Heap, n: Value, m: usize) -> Result<Value, Fault> {
    if m == 0 {
        return Ok(heap.own(n)?);
    }
    match node(heap, n) {
        None => Ok(Value::NIL),
        Some(Node::Leaf(items)) => {
            let kept = items.get(m..).unwrap_or(&[]);
            heap.share_each(kept)?;
            leaf(heap, kept)
        }
        Some(Node::Branch {
            shift,
            sizes,
            children,
        }) => {
            let Some(slot) = size_slot(&sizes, m, shift) else {
                return Ok(Value::NIL);
            };
            let Some(&child) = children.get(slot.child) else {
                return Ok(Value::NIL);
            };
            let cut = tree_drop(heap, child, slot.within)?;
            heap.share_each(after(&children, slot.child))?;
            // The cut child is in front of those behind it.
            let mut kept = children;
            if let Some(child) = kept.get_mut(slot.child) {
                *child = cut;
            }
            branch(heap, shift, kept.get(slot.child..).unwrap_or(&[]))
        }
    }
}

/// `l` then `r`.
// Out of line for the interpreter's loop, as [`slice`] is.
#[inline(never)]
pub(crate) fn concat(heap: &mut Heap, l: Stored, r: Stored) -> Result<Value, Fault> {
    match (l, r) {
        (Stored::Empty, one) | (one, Stored::Empty) => Ok(one.value()),
        (Stored::Tree(l), Stored::Tree(r)) => {
            let joined = concat_trees(heap, l, r);
            heap.release(l)?;
            heap.release(r)?;
            joined
        }
        (Stored::Flat(a), Stored::Flat(b)) => {
            if flat::is_leaf(heap, a) && flat::is_leaf(heap, b) {
                return two_leaves(heap, a, b);
            }
            let fit = a.len().saturating_add(b.len()) <= MOST;
            // One that is a leaf is full, so the two do not fit in one cell, and it is shared.
            let onto_b = flat::is_leaf(heap, b);
            if flat::has_room(heap, a, b.len()) || !(fit || onto_b) {
                join_flat(heap, b, l, End::Back)
            } else if onto_b || flat::has_room(heap, b, a.len()) {
                join_flat(heap, a, r, End::Front)
            } else {
                flat::joined(heap, a, b)
            }
        }
        (Stored::Tree(_), Stored::Flat(b)) => join_flat(heap, b, l, End::Back),
        (Stored::Flat(a), Stored::Tree(_)) => join_flat(heap, a, r, End::Front),
    }
}

/// The tree of two short arrays that are both leaves, whoever else holds them.
fn two_leaves(heap: &mut Heap, a: Flat, b: Flat) -> Result<Value, Fault> {
    let [tree, tail] = [a, b].map(|flat| Value::cell(flat.cell()));
    let r = Root {
        len: a.len().saturating_add(b.len()),
        tree,
        tail,
        ..Root::EMPTY
    };
    new_root(heap, r).map_err(|why| refused(heap, why, &[tree, tail]))
}

/// `onto` with the elements of `flat` at `end`.
fn join_flat(heap: &mut Heap, flat: Flat, onto: Stored, end: End) -> Result<Value, Fault> {
    let all = flat::taken_apart(heap, flat, &[], end)?;
    extend(heap, onto, all.items(), end)
}

/// The array of two trees, to which the caller keeps its references. They are merged down
/// `l`'s right edge and `r`'s left edge, and each level is repacked when it would otherwise
/// be more than `E_MAX` nodes looser than it could be, which keeps lookups a step per
/// level however many joins built the array.
fn concat_trees(heap: &mut Heap, l: Cell, r: Cell) -> Result<Value, Fault> {
    let lr = root(heap, l)?;
    let rr = root(heap, r)?;
    // Two arrays that share their nodes can be longer together than a word
    // counts, without taking that much memory.
    let len = lr.len.checked_add(rr.len).ok_or(Fault::Full)?;
    // The buffers at the seam go into their trees, so the merge sees two
    // trees. The left array keeps its head, and the right one its tail.
    let (ltree, lshift) = if lr.tail.as_cell().is_none() {
        (heap.own(lr.tree)?, lr.shift)
    } else {
        let (tree, tail) = (heap.own(lr.tree)?, heap.own(lr.tail)?);
        tree_push_leaf(heap, tree, lr.shift, tail, End::Back)?
    };
    let rhead = own_head(heap, &rr)?;
    let (rtree, rshift) = if rhead.as_cell().is_none() {
        (heap.own(rr.tree)?, rr.shift)
    } else {
        let tree = heap.own(rr.tree)?;
        tree_push_leaf(heap, tree, rr.shift, rhead, End::Front)?
    };
    let (tree, shift) = if ltree.as_cell().is_none() {
        (rtree, rshift)
    } else if rtree.as_cell().is_none() {
        (ltree, lshift)
    } else {
        let top = merge(heap, ltree, lshift, rtree, rshift)?;
        release(heap, ltree)?;
        release(heap, rtree)?;
        let h = lshift.max(rshift);
        match top.as_slice() {
            [one] => collapse(heap, *one, h)?,
            _ => {
                let h = up(h)?;
                (branch(heap, h, &top)?, h)
            }
        }
    };
    let head = heap.own(lr.head)?;
    let tail = heap.own(rr.tail)?;
    let r = Root {
        len,
        shift,
        offset: lr.offset,
        head,
        tree,
        tail,
    };
    new_root(heap, r)
}

/// The borrowed trees `l` and `r` merged into one or two owned nodes at the
/// taller one's height.
fn merge(
    heap: &mut Heap,
    l: Value,
    lshift: usize,
    r: Value,
    rshift: usize,
) -> Result<Vec<Value>, Fault> {
    let lc = slots(heap, l);
    let rc = slots(heap, r);
    let (left, right) = match (lc.split_last(), rc.split_first()) {
        (Some((last, left)), Some((first, right))) => ((left, *last), (*first, right)),
        _ => return Ok(own_all(heap, &[l, r])?.into_vec()),
    };
    if lshift > rshift {
        let mid = merge(heap, left.1, down(lshift), r, rshift)?;
        rebalance(heap, left.0, mid, &[], lshift)
    } else if rshift > lshift {
        let mid = merge(heap, l, lshift, right.0, down(rshift))?;
        rebalance(heap, &[], mid, right.1, rshift)
    } else if lshift == 0 {
        if lc.len().checked_add(rc.len()).is_some_and(|n| n <= B) {
            let items = own_all(heap, &[lc, rc].concat())?;
            Ok(vec![leaf(heap, &items)?])
        } else {
            Ok(own_all(heap, &[l, r])?.into_vec())
        }
    } else {
        let mid = merge(heap, left.1, down(lshift), right.0, down(rshift))?;
        rebalance(heap, left.0, mid, right.1, lshift)
    }
}

/// The borrowed `left`, the owned `mid` and the borrowed `right`, all one
/// level below `shift`, regrouped into one or two owned nodes at `shift`.
/// When the level holds more than `E_MAX` nodes past the tightest packing,
/// the loose nodes' slots are poured into their neighbours first.
fn rebalance(
    heap: &mut Heap,
    left: &[Value],
    mid: Vec<Value>,
    right: &[Value],
    shift: usize,
) -> Result<Vec<Value>, Fault> {
    let mut all = own_all(heap, left)?.into_vec();
    all.extend(mid);
    all.extend(own_all(heap, right)?);
    let counts: Vec<usize> = all.iter().map(|n| slot_count(heap, *n)).collect();
    let total: usize = counts.iter().sum();
    let optimal = total.div_ceil(B);
    if all.len().saturating_sub(optimal) > E_MAX {
        let plan = plan(counts, optimal);
        all = repack(heap, all, &plan, down(shift))?;
    }
    let mut out = Vec::with_capacity(2);
    let (first, second) = all.split_at(all.len().min(B));
    out.push(branch(heap, shift, first)?);
    if !second.is_empty() {
        out.push(branch(heap, shift, second)?);
    }
    Ok(out)
}

/// The slot counts to repack a level to. While it has more than `E_MAX`
/// nodes past `optimal`, the first node under `B - E_MAX / 2` slots is
/// poured into the ones after it, leaving one node fewer. Such a node always
/// exists while the loop runs, because if every node held at least that
/// many, the level would already be within `E_MAX` of `optimal`.
fn plan(mut counts: Vec<usize>, optimal: usize) -> Vec<usize> {
    while counts.len().saturating_sub(optimal) > E_MAX {
        let Some(i) = counts.iter().position(|c| *c < B - E_MAX / 2) else {
            break;
        };
        let mut spill = counts.get(i).copied().unwrap_or(0);
        let mut j = i;
        // Counts are at most `B`, so the sums here are exact.
        while spill > 0 {
            let Some(next_j) = j.checked_add(1).filter(|k| *k < counts.len()) else {
                break;
            };
            let next = counts.get(next_j).copied().unwrap_or(0);
            let sum = spill.saturating_add(next);
            let merged = sum.min(B);
            spill = sum.saturating_sub(merged);
            if let Some(c) = counts.get_mut(j) {
                *c = merged;
            }
            j = next_j;
        }
        counts.remove(j);
    }
    counts
}

/// The owned nodes `old`, one level below, rebuilt to the slot counts in
/// `plan`, with their slots streamed in order. A node whose count already
/// matches, and that starts on a node boundary, is kept as it is.
fn repack(
    heap: &mut Heap,
    old: Vec<Value>,
    plan: &[usize],
    child_shift: usize,
) -> Result<Vec<Value>, Fault> {
    let mut old = old.into_iter().map(Some).collect::<Vec<_>>();
    let mut out = Vec::with_capacity(plan.len());
    let (mut src, mut off) = (0, 0);
    for &want in plan {
        if off == 0
            && let Some(Some(n)) = old.get(src)
            && slot_count(heap, *n) == want
        {
            out.extend(old.get_mut(src).and_then(Option::take));
            src = src.saturating_add(1);
            continue;
        }
        let mut buf = Vec::with_capacity(want);
        while buf.len() < want {
            let Some(Some(n)) = old.get(src) else {
                break;
            };
            let items = slots(heap, *n);
            // `buf` is short of `want` and `off` is inside `items`, so
            // neither subtraction saturates, and `off` stays inside too.
            let take = want
                .saturating_sub(buf.len())
                .min(items.len().saturating_sub(off));
            let taking = items.get(off..).and_then(|rest| rest.get(..take));
            buf.extend(own_all(heap, taking.unwrap_or(&[]))?);
            off = off.saturating_add(take);
            if off == items.len() {
                if let Some(n) = old.get_mut(src).and_then(Option::take) {
                    release(heap, n)?;
                }
                src = src.saturating_add(1);
                off = 0;
            }
        }
        out.push(if child_shift == 0 {
            leaf(heap, &buf)?
        } else {
            branch(heap, child_shift, &buf)?
        });
    }
    for n in old.into_iter().flatten() {
        release(heap, n)?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests;
