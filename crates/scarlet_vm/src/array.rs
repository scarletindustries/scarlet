//! An array is a persistent RRB (relaxed radix-balanced) tree in the
//! process's heap, the same shape as the old VM's (its `seq.rs`).
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
//! this way holds one more reference. An operation whose caller gives up its
//! reference ([`set`], [`push_owned`], [`extend_owned`], [`uncons`], [`skip_owned`])
//! changes a node in place when its count says nothing else holds it.
//!
//! This is the layout, in words after each cell's header:
//!
//! - A root holds `len`, `shift` (the tree's height, 0 for a lone leaf) and
//!   the head's `offset`, then the head, tree and tail, each a node or `Nil`.
//! - A leaf holds 1 to 32 elements.
//! - A branch holds `shift`, then `n` running totals, then `n` children, all
//!   one level down, with `n` from 1 to 32.
//!
//! The walks here go down at most one level per call, so they are only as
//! deep as the tree is tall. That is 13 levels at the very most, for 2^64
//! elements.
//!
//! An array can also be a range, `start..end`, which stores only its two
//! ends, as the old VM's did. Its length, its elements, its tail and its
//! slices all come from those two numbers. [`Seq`] is either kind. Adding to a
//! range, or joining one to another array, first builds it as a tree.

use crate::code::{Length, Split};
use crate::heap::{Cell, DeadCell, Fault, Heap, Kind};
use crate::value::Value;
use crate::word::to_usize;

/// Children per branch, and elements per leaf.
const B: usize = 32;
/// Bits of an index each level takes, which is `log2(B)`.
const BITS: usize = 5;
/// How many more nodes than the tightest packing a level may keep after a
/// join, which bounds the extra steps a lookup takes (the RRB paper's `e`).
const E_MAX: usize = 2;

/// An array as the VM holds it. It is either a tree of its elements or a
/// range of Ints that stores only its two ends.
#[derive(Clone, Copy)]
pub(crate) enum Seq {
    Tree(Cell),
    Range { start: i64, end: i64 },
}

impl Seq {
    pub(crate) fn len(self, heap: &Heap) -> u64 {
        match self {
            Seq::Tree(cell) => len(heap, cell) as u64,
            Seq::Range { start, end } => range_len(start, end),
        }
    }
}

/// How many Ints `start..end` holds. It is none when `end` is not past
/// `start`.
pub(crate) fn range_len(start: i64, end: i64) -> u64 {
    if end > start { end.abs_diff(start) } else { 0 }
}

/// A new range cell, `start..end`.
// Kept inside its callers, as `Heap::make` is and for the same reason.
#[inline(always)]
pub(crate) fn range(heap: &mut Heap, start: i64, end: i64) -> Result<Cell, Fault> {
    heap.make(Kind::Range, &[start.cast_unsigned(), end.cast_unsigned()])
}

/// Where the range that `v` is starts and ends, or `None` when it is no range.
#[inline(always)]
fn ends(heap: &Heap, v: Value) -> Option<(i64, i64)> {
    let [start, end] = heap.chunk(v.as_cell()?, Kind::Range)?;
    Some((start.cast_signed(), end.cast_signed()))
}

/// The array `cell` holds, or `None` when it holds something else.
pub(crate) fn seq(heap: &Heap, cell: Cell) -> Option<Seq> {
    match heap.kind(cell)? {
        Kind::ArrayRoot => Some(Seq::Tree(cell)),
        Kind::Range => {
            let d = heap.data(cell);
            let w = |i: usize| d.get(i).copied().unwrap_or(0).cast_signed();
            Some(Seq::Range {
                start: w(0),
                end: w(1),
            })
        }
        Kind::Handle
        | Kind::String
        | Kind::BigInt
        | Kind::Ctor
        | Kind::Closure
        | Kind::Tuple
        | Kind::ArrayLeaf
        | Kind::ArrayBranch
        | Kind::Binary
        | Kind::BinarySlice
        | Kind::Map
        | Kind::MapNode
        | Kind::MapCollision => None,
    }
}

/// Which end of an array.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum End {
    Front,
    Back,
}

impl End {
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

pub(crate) struct Root {
    len: usize,
    shift: usize,
    /// Items at the front of `head` this array has left behind. Dropping from
    /// the front moves this along and keeps the same leaf, so `[h, ..t]`
    /// copies nothing. A push settles it back to 0, while `take` and `concat`
    /// keep it along with the head.
    offset: usize,
    head: Value,
    tree: Value,
    tail: Value,
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

    /// Slot `i` counted from slot `from`, which is how a head that has left
    /// `from` items behind is read.
    fn slot_from(&self, from: usize, i: usize) -> Option<Value> {
        let words = match self {
            View::Leaf(items) => items,
            View::Branch { children, .. } => children,
        };
        words.get(from..)?.get(i).copied().map(Value::from_bits)
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
fn head_len(heap: &Heap, r: &Root) -> usize {
    slot_count(heap, r.head).saturating_sub(r.offset)
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
    if head_len(heap, r) == 0 {
        return Ok(Value::NIL);
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
    let d = heap.data(cell);
    match heap.kind(cell)? {
        Kind::ArrayLeaf => Some(View::Leaf(d)),
        Kind::ArrayBranch => {
            let n = d.len().saturating_sub(1) / 2;
            let rest = d.get(1..).unwrap_or(&[]);
            let (sizes, children) = rest.split_at(n.min(rest.len()));
            Some(View::Branch {
                shift: to_usize(d.first().copied().unwrap_or(0)),
                sizes,
                children,
            })
        }
        Kind::Handle
        | Kind::String
        | Kind::BigInt
        | Kind::Ctor
        | Kind::Closure
        | Kind::Tuple
        | Kind::ArrayRoot
        | Kind::Range
        | Kind::Binary
        | Kind::BinarySlice
        | Kind::Map
        | Kind::MapNode
        | Kind::MapCollision => None,
    }
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

/// `slots` with `x` added at `end`.
fn at_end(mut slots: Slots, x: Value, end: End) -> Slots {
    match end {
        End::Front => slots.insert(0, x),
        End::Back => slots.push(x),
    }
    slots
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

fn new_root(
    heap: &mut Heap,
    len: usize,
    shift: usize,
    head: Value,
    tree: Value,
    tail: Value,
) -> Result<Cell, Fault> {
    root_at(heap, len, shift, 0, head, tree, tail)
}

// Kept inside its callers, as `Heap::make` is and for the same reason.
#[inline]
fn root_at(
    heap: &mut Heap,
    len: usize,
    shift: usize,
    offset: usize,
    head: Value,
    tree: Value,
    tail: Value,
) -> Result<Cell, Fault> {
    heap.make(
        Kind::ArrayRoot,
        &[
            len as u64,
            shift as u64,
            offset as u64,
            head.bits(),
            tree.bits(),
            tail.bits(),
        ],
    )
}

/// A leaf of `items`, whose references pass to it.
fn leaf(heap: &mut Heap, items: &[Value]) -> Result<Value, Fault> {
    let data: smallvec::SmallVec<[u64; B]> = items.iter().map(|v| v.bits()).collect();
    Ok(Value::cell(heap.make(Kind::ArrayLeaf, &data)?))
}

/// A leaf of what the leaf `v` has from its item `from` on, with a reference of its own
/// to each.
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
    Ok(Value::cell(heap.make(Kind::ArrayLeaf, kept)?))
}

/// A branch at height `shift` over `children`, whose references pass to it.
/// Children shared between arrays can hold more elements together than a
/// word can count, and that is a branch too big to make.
fn branch(heap: &mut Heap, shift: usize, children: &[Value]) -> Result<Value, Fault> {
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
    Ok(Value::cell(heap.make(Kind::ArrayBranch, &data)?))
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

fn empty(heap: &mut Heap) -> Result<Cell, Fault> {
    new_root(heap, 0, 0, Value::NIL, Value::NIL, Value::NIL)
}

/// An array of `items`, whose references pass to it. A whole number of full
/// leaves go in a tightly packed tree, and the rest, 1 to 32, in the tail.
pub(crate) fn from_values(heap: &mut Heap, items: &[Value]) -> Result<Cell, Fault> {
    let n = items.len();
    if n == 0 {
        return empty(heap);
    }
    if n <= B {
        let tail = leaf(heap, items)?;
        return new_root(heap, n, 0, Value::NIL, Value::NIL, tail);
    }
    // Every chunk but the last is a full leaf for the tree, and the last, of
    // 1 to 32 items, is the tail.
    let mut chunks: Vec<&[Value]> = items.chunks(B).collect();
    let tail_items = chunks.pop().unwrap_or(&[]);
    let mut nodes = Vec::with_capacity(chunks.len());
    for chunk in chunks {
        nodes.push(leaf(heap, chunk)?);
    }
    let mut shift = 0;
    while nodes.len() > 1 {
        shift = up(shift)?;
        let mut level = Vec::with_capacity(nodes.len().div_ceil(B));
        for chunk in nodes.chunks(B) {
            level.push(branch(heap, shift, chunk)?);
        }
        nodes = level;
    }
    let tree = nodes.pop().unwrap_or(Value::NIL);
    let tail = leaf(heap, tail_items)?;
    new_root(heap, n, shift, Value::NIL, tree, tail)
}

fn len(heap: &Heap, array: Cell) -> usize {
    tree(heap, array).map_or(0, |r| r.len)
}

/// Element `i`, with no reference added, or `None` past the end.
// Inside its one caller that reads element after element. Where it is called
// from more places, the optimizer leaves it out of them all.
#[inline(always)]
pub(crate) fn get(heap: &Heap, array: Cell, i: usize) -> Option<Value> {
    let r = tree(heap, array)?;
    if i >= r.len {
        return None;
    }
    let head = view(heap, r.head);
    let head_len = head.as_ref().map_or(0, View::len).saturating_sub(r.offset);
    let Some(mut idx) = i.checked_sub(head_len) else {
        return head?.slot_from(r.offset, i);
    };
    if let Some(in_tail) = idx.checked_sub(node_len(heap, r.tree)) {
        return view(heap, r.tail)?.slot(in_tail);
    }
    let mut n = r.tree;
    loop {
        let v = view(heap, n)?;
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

/// `array` with `x` at index `i`. The caller gives up its reference to
/// `array` and holds the result's, and `x`'s reference passes to the array.
/// An index the array does not have changes nothing.
///
/// The walk goes from the root down to the leaf that holds the slot. A node
/// nothing else holds is written where it is. A node something else holds is
/// copied first, and the copy takes its place in the node above, which by
/// then is this array's alone. A copy adds a reference to everything its
/// node held, so every node below it on the way is copied in its turn. So
/// an array with one holder changes in place, and a write to one something
/// else holds copies the root and one node per level, and shares the rest
/// with the array it came from, which stays as it was.
pub(crate) fn set(heap: &mut Heap, array: Cell, i: usize, x: Value) -> Result<Cell, Fault> {
    let r = root(heap, array)?;
    if i >= r.len {
        release(heap, x)?;
        return Ok(array);
    }
    let array = owned(heap, array)?;
    // The part of the root that holds the slot: its word in the root, the
    // node there, and the slot's index under that node.
    let (mut word, mut node, mut idx) = match i.checked_sub(head_len(heap, &r)) {
        None => (ROOT_HEAD, r.head, r.offset.saturating_add(i)),
        Some(past) => match past.checked_sub(node_len(heap, r.tree)) {
            None => (ROOT_TREE, r.tree, past),
            Some(in_tail) => (ROOT_TAIL, r.tail, in_tail),
        },
    };
    let mut holder = array;
    while let Some(shared) = node.as_cell() {
        let cell = owned(heap, shared)?;
        if cell != shared
            && let Some(w) = heap.data_mut(holder).get_mut(word)
        {
            *w = Value::cell(cell).bits();
        }
        match view(heap, Value::cell(cell)) {
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
                return Ok(array);
            }
            None => break,
        }
    }
    // Only a tree whose totals are wrong ends here. `get` finds nothing at
    // this index of such a tree, so there is nothing to write over.
    release(heap, x)?;
    Ok(array)
}

/// Every element, front to back, with no reference added.
pub(crate) fn elements(heap: &Heap, array: Cell) -> Vec<Value> {
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

/// `array` with `x` added at `end`. `x`'s reference passes to the result.
/// The buffer at that end takes it, and a full buffer first goes into the
/// tree as a finished leaf.
fn push(heap: &mut Heap, array: Cell, x: Value, end: End) -> Result<Cell, Fault> {
    let r = root(heap, array)?;
    let len = r.len.checked_add(1).ok_or(Fault::Full)?;
    // Settle a head with items left behind first. The leaf that goes into the
    // tree, or that the new item joins, must hold only live items.
    let settled = own_head(heap, &r)?;
    let (this, this_items, other) = match end {
        End::Front => (settled, head_items(heap, &r), r.tail),
        End::Back => (r.tail, slots(heap, r.tail), settled),
    };
    let (tree, shift, new) = if this_items.is_empty() {
        (heap.own(r.tree)?, r.shift, leaf(heap, &[x])?)
    } else if this_items.len() < B {
        let kept = own_all(heap, &this_items)?;
        let items = at_end(kept, x, end);
        (heap.own(r.tree)?, r.shift, leaf(heap, &items)?)
    } else {
        let (tree, this) = (heap.own(r.tree)?, heap.own(this)?);
        let (tree, shift) = tree_push_leaf(heap, tree, r.shift, this, end)?;
        (tree, shift, leaf(heap, &[x])?)
    };
    let other = heap.own(other)?;
    // `settled` is the head's own leaf when nothing was left behind, and the
    // branches above each took their own reference to what they kept.
    release(heap, settled)?;
    let (head, tail) = end.turn(new, other);
    new_root(heap, len, shift, head, tree, tail)
}

/// [`push`] for a caller that gives up its reference to `array`. When
/// nothing else holds the array or the leaf at `end`, and that leaf has room,
/// the leaf's items move to a leaf one longer and the root changes in place,
/// so nothing is shared or released. `x`'s reference passes in, as for
/// [`push`].
pub(crate) fn push_owned(heap: &mut Heap, array: Cell, x: Value, end: End) -> Result<Cell, Fault> {
    if pushed(heap, array, &[x], end)? {
        return Ok(array);
    }
    let made = push(heap, array, x, end);
    heap.release(array)?;
    made
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
    if !heap.unique(array)? || (end == End::Front && r.offset > 0) {
        return Ok(false);
    }
    let Some(leaf) = edge.as_cell() else {
        return Ok(false);
    };
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
    let made = heap.make(Kind::ArrayLeaf, words.get(..n).unwrap_or(&[]))?;
    heap.discard(leaf)?;
    if let Some(d) = heap.data_mut(array).get_mut(..6) {
        if let Some(w) = d.get_mut(0) {
            *w = len as u64;
        }
        if let Some(w) = d.get_mut(slot) {
            *w = Value::cell(made).bits();
        }
    }
    Ok(true)
}

/// `array` with `items`, in their order, at `end`. The caller gives up its
/// reference to `array` and one to each item.
///
/// It makes the array that pushing them one at a time does, with a leaf for
/// each run of them where that makes one for each item. The buffer at that end
/// takes as many as it has room for, and a full one goes into the tree before
/// the next is begun.
pub(crate) fn extend_owned(
    heap: &mut Heap,
    array: Cell,
    items: &[Value],
    end: End,
) -> Result<Cell, Fault> {
    // A whole leaf of them never fits in one that has any.
    if items.len() < B && pushed(heap, array, items, end)? {
        return Ok(array);
    }
    let r = root(heap, array)?;
    let len = r.len.checked_add(items.len()).ok_or(Fault::Full)?;
    // This holds a reference to each part of the root from here on. They are
    // the root's own when nothing else holds it.
    let alone = heap.unique(array)?;
    if !alone {
        for part in [r.head, r.tree, r.tail] {
            heap.own(part)?;
        }
    }
    // As in `push`, a head that is added to holds only live items.
    let (head, offset) = if end == End::Front && r.offset > 0 {
        let settled = own_head(heap, &r)?;
        release(heap, r.head)?;
        (settled, 0)
    } else {
        (r.head, r.offset)
    };
    let (mut edge, other) = end.turn(head, r.tail);
    let (mut tree, mut shift) = (r.tree, r.shift);
    let mut left = items;
    while !left.is_empty() {
        if slot_count(heap, edge) >= B {
            (tree, shift) = tree_push_leaf(heap, tree, shift, edge, end)?;
            edge = Value::NIL;
        }
        let room = B.saturating_sub(slot_count(heap, edge)).min(left.len());
        let (near, rest) = match end {
            End::Back => left.split_at_checked(room),
            End::Front => {
                let split = left.split_at_checked(left.len().saturating_sub(room));
                split.map(|(rest, near)| (near, rest))
            }
        }
        .unwrap_or((left, &[]));
        edge = longer_leaf(heap, edge, near, end)?;
        left = rest;
    }
    let (head, tail) = end.turn(edge, other);
    if !alone {
        heap.release(array)?;
        return root_at(heap, len, shift, offset, head, tree, tail);
    }
    if let [l, s, o, h, t, e] = heap.data_mut(array) {
        (*l, *s, *o) = (len as u64, shift as u64, offset as u64);
        (*h, *t, *e) = (head.bits(), tree.bits(), tail.bits());
    }
    Ok(array)
}

/// A leaf of the items of the leaf `old`, or of none when it is `Nil`, with
/// `items` at `end` of them. The caller gives up its reference to `old` and
/// one to each item.
fn longer_leaf(heap: &mut Heap, old: Value, items: &[Value], end: End) -> Result<Value, Fault> {
    let Some(cell) = old.as_cell() else {
        return leaf(heap, items);
    };
    let kept = slots(heap, old);
    let alone = heap.unique(cell)?;
    if !alone {
        own_all(heap, &kept)?;
    }
    let mut all = Slots::new();
    let (first, second) = match end {
        End::Back => (kept.as_slice(), items),
        End::Front => (items, kept.as_slice()),
    };
    all.extend_from_slice(first);
    all.extend_from_slice(second);
    let made = leaf(heap, &all)?;
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
    let made = heap.make(Kind::ArrayBranch, &data)?;
    if alone {
        heap.discard(cell)?;
    } else {
        heap.release(cell)?;
    }
    Ok(Some(Value::cell(made)))
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
fn take(heap: &mut Heap, array: Cell, n: usize) -> Result<Cell, Fault> {
    let r = root(heap, array)?;
    if n == 0 {
        return empty(heap);
    }
    if n >= r.len {
        heap.share(array)?;
        return Ok(array);
    }
    let front = head_items(heap, &r);
    let Some(m) = n.checked_sub(front.len()).filter(|m| *m > 0) else {
        let kept = own_all(heap, front.get(..n).unwrap_or(&[]))?;
        let tail = leaf(heap, &kept)?;
        return new_root(heap, n, 0, Value::NIL, Value::NIL, tail);
    };
    let Some(in_tail) = m.checked_sub(node_len(heap, r.tree)).filter(|k| *k > 0) else {
        let cut = tree_take(heap, r.tree, m)?;
        let (tree, shift) = collapse(heap, cut, r.shift)?;
        let head = heap.own(r.head)?;
        return root_at(heap, n, shift, r.offset, head, tree, Value::NIL);
    };
    let tail_items = slots(heap, r.tail);
    let kept = own_all(heap, tail_items.get(..in_tail).unwrap_or(&[]))?;
    let tail = leaf(heap, &kept)?;
    let head = heap.own(r.head)?;
    let tree = heap.own(r.tree)?;
    root_at(heap, n, r.shift, r.offset, head, tree, tail)
}

/// The elements from `start` up to but not including `end`. An end past the array's
/// is the array's.
// Out of line, with the map's two owned edits and [`concat`]. Inside the
// interpreter's loop these four are so long that the loop keeps fewer of its
// own values in registers, which costs 4 machine instructions at each VM
// instruction that tests a variant, reads a field or drops a register.
#[inline(never)]
pub(crate) fn slice(heap: &mut Heap, array: Cell, start: usize, end: usize) -> Result<Cell, Fault> {
    let front = take(heap, array, end)?;
    let cut = skip(heap, front, start);
    heap.release(front)?;
    cut
}

/// Every element but the first `n`.
pub(crate) fn skip(heap: &mut Heap, array: Cell, n: usize) -> Result<Cell, Fault> {
    let r = root(heap, array)?;
    if n == 0 {
        heap.share(array)?;
        return Ok(array);
    }
    let Some(left) = r.len.checked_sub(n).filter(|left| *left > 0) else {
        return empty(heap);
    };
    let Some(m) = n.checked_sub(head_len(heap, &r)) else {
        // This is the cheap `[h, ..t]`. It keeps the same head leaf, tree and
        // tail, with `n` more items left behind, so nothing is copied. The
        // head is one leaf, so the offset stays below `B`.
        let offset = r.offset.saturating_add(n);
        let head = heap.own(r.head)?;
        let tree = heap.own(r.tree)?;
        let tail = heap.own(r.tail)?;
        return root_at(heap, left, r.shift, offset, head, tree, tail);
    };
    // The head is spent, so the rest comes out of the tree. Move the tree's
    // leftmost leaf up to be the new head, whole and shared, and leave `m` of
    // it behind. The tree is cut once per leaf instead of once per element,
    // so walking an array cuts it every `B` steps, and the offset above
    // covers the steps between.
    if let Some(first) = first_leaf(heap, r.tree)
        && m < slot_count(heap, first)
    {
        let taken = slot_count(heap, first);
        // `tree_drop` of a whole leaf that is the whole tree would leave an
        // empty leaf behind, so a tree of one leaf becomes no tree at all.
        let (tree, shift) = if taken == node_len(heap, r.tree) {
            (Value::NIL, 0)
        } else {
            let cut = tree_drop(heap, r.tree, taken)?;
            collapse(heap, cut, r.shift)?
        };
        let head = heap.own(first)?;
        let tail = heap.own(r.tail)?;
        return root_at(heap, left, shift, m, head, tree, tail);
    }
    let Some(k) = m.checked_sub(node_len(heap, r.tree)) else {
        let cut = tree_drop(heap, r.tree, m)?;
        let (tree, shift) = collapse(heap, cut, r.shift)?;
        let tail = heap.own(r.tail)?;
        return new_root(heap, left, shift, Value::NIL, tree, tail);
    };
    let tail = if k == 0 {
        heap.own(r.tail)?
    } else {
        leaf_behind(heap, r.tail, k)?
    };
    new_root(heap, left, 0, Value::NIL, Value::NIL, tail)
}

/// [`skip`] for a caller that gives up its reference to `array`. When
/// nothing else holds it and the head still has more than `n` items, which
/// is every step of a walk but one in `B`, the root just leaves `n` more of
/// them behind, in place.
pub(crate) fn skip_owned(heap: &mut Heap, array: Cell, n: usize) -> Result<Cell, Fault> {
    let r = root(heap, array)?;
    if heap.unique(array)? && n > 0 && n < head_len(heap, &r) {
        let len = r.len.saturating_sub(n);
        let offset = r.offset.saturating_add(n);
        if let [l, _, o, ..] = heap.data_mut(array) {
            *l = len as u64;
            *o = offset as u64;
        }
        return Ok(array);
    }
    let made = skip(heap, array, n);
    heap.release(array)?;
    made
}

/// The first element of `array` and the array of the others, or `None` when it has no
/// element. The element holds a reference of its own, and is `Nil` unless `head` asks
/// for it. When `taken`, the caller gives up its reference to `array` where there is an
/// element.
#[inline(always)]
pub(crate) fn uncons(
    heap: &mut Heap,
    array: Cell,
    head: bool,
    taken: bool,
) -> Result<Option<(Value, Cell)>, Fault> {
    let r = root(heap, array)?;
    let Some(left) = r.len.checked_sub(1) else {
        return Ok(None);
    };
    let items = match view(heap, r.head) {
        Some(View::Leaf(items)) => items.get(r.offset..).unwrap_or_default(),
        Some(View::Branch { .. }) | None => &[],
    };
    let first = match (items.first(), head) {
        (_, false) => Value::NIL,
        (Some(first), true) => Value::from_bits(*first),
        (None, true) => first(heap, array),
    };
    // Only the place in the head moves along, which is every step but one in `B`.
    let near = items.len() > 1;
    let alone = taken && heap.unique(array)?;
    heap.own(first)?;
    if alone && near {
        let offset = r.offset.saturating_add(1);
        if let [l, _, o, ..] = heap.data_mut(array) {
            *l = left as u64;
            *o = offset as u64;
        }
        return Ok(Some((first, array)));
    }
    let cut = match skip(heap, array, 1) {
        Ok(cut) => cut,
        Err(why) => {
            release(heap, first)?;
            return Err(why);
        }
    };
    if taken {
        heap.release(array)?;
    }
    Ok(Some((first, cut)))
}

/// Whether an array of `has` elements has the `length`.
#[inline(always)]
pub(crate) fn fits(has: u64, length: Length) -> bool {
    let len = u64::from(length.len);
    has == len || (length.more && has > len)
}

/// How many elements the tree that `v` is has, or `None` when `v` is no tree.
#[inline(always)]
pub(crate) fn tree_len(heap: &Heap, v: Value) -> Option<u64> {
    let [len] = heap.chunk(v.as_cell()?, Kind::ArrayRoot)?;
    Some(*len)
}

/// What [`split`] found.
pub(crate) enum Found {
    /// The array has another length.
    Miss,
    /// The elements, which are in the buffer.
    All,
    /// An array that has the length, and whose elements are not all in its first leaf.
    Apart(Root),
    /// A range that has the length, and where it starts.
    Range(i64),
    /// No array.
    Other,
}

/// Put the elements of the array that `v` is from `split.from` on in `out`, as many as
/// there is room for, when it has a length that `split` asks for and they are all in its
/// first leaf. No reference is added.
#[inline(always)]
pub(crate) fn split(heap: &Heap, v: Value, split: Split, out: &mut [Value]) -> Found {
    let Some(r) = v.as_cell().and_then(|cell| tree(heap, cell)) else {
        return match ends(heap, v) {
            Some((start, end)) if fits(range_len(start, end), split.length) => Found::Range(start),
            Some(_) => Found::Miss,
            None => Found::Other,
        };
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
    match v.as_cell() {
        Some(cell) => match heap.is_with_data(cell, Kind::ArrayLeaf) {
            (true, items) => items,
            (false, _) => &[],
        },
        None => &[],
    }
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

/// The first element of `array`, with no reference added.
#[inline(never)]
fn first(heap: &Heap, array: Cell) -> Value {
    get(heap, array, 0).unwrap_or(Value::NIL)
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
            let kept = own_all(heap, items.get(..m).unwrap_or(&[]))?;
            leaf(heap, &kept)
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
            let mut kept = own_all(heap, children.get(..slot.child).unwrap_or(&[]))?;
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
            let kept = own_all(heap, items.get(m..).unwrap_or(&[]))?;
            leaf(heap, &kept)
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
            let mut kept: Slots = smallvec::smallvec![cut];
            kept.extend(own_all(heap, after(&children, slot.child))?);
            branch(heap, shift, &kept)
        }
    }
}

/// `l` then `r`. The two trees are merged down `l`'s right edge and `r`'s
/// left edge, and each level is repacked when it would otherwise be more than
/// `E_MAX` nodes looser than it could be, which keeps lookups a step per
/// level however many joins built the array.
// Out of line for the interpreter's loop, as [`slice`] is.
#[inline(never)]
pub(crate) fn concat(heap: &mut Heap, l: Cell, r: Cell) -> Result<Cell, Fault> {
    let lr = root(heap, l)?;
    let rr = root(heap, r)?;
    if lr.len == 0 {
        heap.share(r)?;
        return Ok(r);
    }
    if rr.len == 0 {
        heap.share(l)?;
        return Ok(l);
    }
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
    root_at(heap, len, shift, lr.offset, head, tree, tail)
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
