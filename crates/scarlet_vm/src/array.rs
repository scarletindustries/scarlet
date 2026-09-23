//! Arrays: a persistent RRB (relaxed radix-balanced) tree in the process's
//! heap, the same shape as the old VM's (`seq.rs` on master).
//!
//! ```text
//!   root [ len | shift | head | tree | tail ]
//!                         |      |      |
//!     a push to the front |      |      | a push to the back
//!     lands here (a leaf) |      |      | lands here (a leaf)
//!                                |
//!                 branches, 32 children each, with a
//!                 table of how many elements each holds
//!                      /         |          \
//!                   leaf        leaf        leaf    (32 elements each)
//! ```
//!
//! The two buffer leaves make a push at either end cheap: only a full buffer
//! spills into the tree, once every 32 pushes. Every branch keeps the running
//! total of its children's elements, so finding an element, cutting the array
//! and joining two of them, as in the RRB paper (Bagwell & Rompf, 2011), each
//! visit one node per level.
//!
//! Nothing is ever changed in place. An operation builds new nodes along the
//! one path it touches and shares every other node with the array it came
//! from, which stays as it was. A node shared this way holds one more
//! reference. (Changing a node in place when nothing else holds it waits for
//! moves on last use, `docs/vm-design.md`.)
//!
//! Layout, in words after each cell's header:
//!
//! - root: `len`, `shift` (the tree's height, 0 for a lone leaf), then the
//!   head, tree and tail, each a node or `Nil`;
//! - leaf: 1 to 32 elements;
//! - branch: `shift`, then `n` running totals, then `n` children, `n` from 1
//!   to 32, all one level down.
//!
//! The walks here go one level per call at most, so they are as deep as the
//! tree is tall: 13 levels at the very most, for 2^64 elements.
//!
//! An array can also be a range, `start..end`, which stores only its two
//! ends, as the old VM's did: its length, its elements, its tail and its
//! slices all come from those two numbers. [`Seq`] is either kind. Adding to a
//! range, or joining one to another array, first builds it as a tree.

use crate::heap::{Cell, Full, Heap, Kind};
use crate::value::Value;

/// Children per branch, and elements per leaf.
const B: usize = 32;
/// Bits of an index each level takes: `log2(B)`.
const BITS: usize = 5;
/// How many more nodes than the tightest packing a level may keep after a
/// join, which bounds the extra steps a lookup takes (the RRB paper's `e`).
const E_MAX: usize = 2;

/// An array as the VM holds it: a tree of its elements, or a range of Ints
/// that stores only its two ends.
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

/// How many Ints `start..end` holds: none when `end` is not past `start`.
pub(crate) fn range_len(start: i64, end: i64) -> u64 {
    u64::try_from(i128::from(end) - i128::from(start)).unwrap_or(0)
}

/// A new range cell, `start..end`.
pub(crate) fn range(heap: &mut Heap, start: i64, end: i64) -> Result<Cell, Full> {
    heap.make(Kind::Range, &[start as u64, end as u64])
}

/// The array `cell` holds, or `None` when it holds something else.
pub(crate) fn seq(heap: &Heap, cell: Cell) -> Option<Seq> {
    match heap.kind(cell)? {
        Kind::ArrayRoot => Some(Seq::Tree(cell)),
        Kind::Range => {
            let d = heap.data(cell);
            let w = |i: usize| d.get(i).copied().unwrap_or(0) as i64;
            Some(Seq::Range {
                start: w(0),
                end: w(1),
            })
        }
        Kind::String
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
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum End {
    Front,
    Back,
}

struct Root {
    len: usize,
    shift: usize,
    /// Items at the front of `head` this array has left behind. Dropping from
    /// the front moves this along and keeps the same leaf, so `[h, ..t]`
    /// copies nothing; every other operation settles it back to 0.
    offset: usize,
    head: Value,
    tree: Value,
    tail: Value,
}

enum Node {
    Leaf(Vec<Value>),
    Branch {
        shift: usize,
        sizes: Vec<usize>,
        children: Vec<Value>,
    },
}

fn root(heap: &Heap, cell: Cell) -> Root {
    let d = heap.data(cell);
    let w = |i: usize| d.get(i).copied().unwrap_or(Value::NIL.bits());
    Root {
        len: w(0) as usize,
        shift: w(1) as usize,
        offset: w(2) as usize,
        head: Value::from_bits(w(3)),
        tree: Value::from_bits(w(4)),
        tail: Value::from_bits(w(5)),
    }
}

/// The head's live items: what `offset` leaves of its leaf.
fn head_items(heap: &Heap, r: &Root) -> Vec<Value> {
    let items = slots(heap, r.head);
    items.get(r.offset..).unwrap_or(&[]).to_vec()
}

/// A leaf of exactly the head's live items, owned, or `Nil` when there are
/// none. Nothing left behind means the head itself, shared rather than copied.
fn own_head(heap: &mut Heap, r: &Root) -> Result<Value, Full> {
    if r.offset == 0 {
        return Ok(own(heap, r.head));
    }
    let items = head_items(heap, r);
    if items.is_empty() {
        return Ok(Value::NIL);
    }
    let kept = own_all(heap, &items);
    leaf(heap, &kept)
}

/// `v` as a node, or `None` for `Nil`.
fn node(heap: &Heap, v: Value) -> Option<Node> {
    let cell = v.as_cell()?;
    let d = heap.data(cell);
    match heap.kind(cell)? {
        Kind::ArrayLeaf => Some(Node::Leaf(
            d.iter().copied().map(Value::from_bits).collect(),
        )),
        Kind::ArrayBranch => {
            let n = d.len().saturating_sub(1) / 2;
            Some(Node::Branch {
                shift: d.first().copied().unwrap_or(0) as usize,
                sizes: d.iter().skip(1).take(n).map(|s| *s as usize).collect(),
                children: d
                    .iter()
                    .skip(1 + n)
                    .copied()
                    .map(Value::from_bits)
                    .collect(),
            })
        }
        Kind::String
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

/// `v`, with one more reference: for keeping it in a new node.
fn own(heap: &mut Heap, v: Value) -> Value {
    if let Some(cell) = v.as_cell() {
        heap.share(cell);
    }
    v
}

fn own_all(heap: &mut Heap, vs: &[Value]) -> Vec<Value> {
    vs.iter().map(|v| own(heap, *v)).collect()
}

fn release(heap: &mut Heap, v: Value) {
    if let Some(cell) = v.as_cell() {
        heap.release(cell);
    }
}

fn new_root(
    heap: &mut Heap,
    len: usize,
    shift: usize,
    head: Value,
    tree: Value,
    tail: Value,
) -> Result<Cell, Full> {
    root_at(heap, len, shift, 0, head, tree, tail)
}

fn root_at(
    heap: &mut Heap,
    len: usize,
    shift: usize,
    offset: usize,
    head: Value,
    tree: Value,
    tail: Value,
) -> Result<Cell, Full> {
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
fn leaf(heap: &mut Heap, items: &[Value]) -> Result<Value, Full> {
    let data: Vec<u64> = items.iter().map(|v| v.bits()).collect();
    Ok(Value::cell(heap.make(Kind::ArrayLeaf, &data)?))
}

/// A branch at height `shift` over `children`, whose references pass to it.
fn branch(heap: &mut Heap, shift: usize, children: &[Value]) -> Result<Value, Full> {
    let mut data = Vec::with_capacity(1 + 2 * children.len());
    data.push(shift as u64);
    let mut total = 0;
    for c in children {
        total += node_len(heap, *c);
        data.push(total as u64);
    }
    data.extend(children.iter().map(|c| c.bits()));
    Ok(Value::cell(heap.make(Kind::ArrayBranch, &data)?))
}

/// Elements under a node.
fn node_len(heap: &Heap, v: Value) -> usize {
    match node(heap, v) {
        None => 0,
        Some(Node::Leaf(items)) => items.len(),
        Some(Node::Branch { sizes, .. }) => sizes.last().copied().unwrap_or(0),
    }
}

/// A node's own slots: a leaf's elements, or a branch's children.
fn slots(heap: &Heap, v: Value) -> Vec<Value> {
    match node(heap, v) {
        None => Vec::new(),
        Some(Node::Leaf(items)) => items,
        Some(Node::Branch { children, .. }) => children,
    }
}

/// Where an element sits in a branch: which child holds it, and how many
/// elements the children before that one hold.
struct Slot {
    child: usize,
    before: usize,
}

/// The slot of element `idx` in a branch at `shift`. The scan starts at the
/// radix guess `idx >> shift`, which never overshoots, since a child holds at
/// most `1 << shift` elements.
fn size_slot(sizes: &[usize], idx: usize, shift: usize) -> Slot {
    let last = sizes.len().saturating_sub(1);
    let mut child = (idx >> shift).min(last);
    while child < last && sizes.get(child).is_some_and(|s| *s <= idx) {
        child += 1;
    }
    let before = match child.checked_sub(1) {
        Some(p) => sizes.get(p).copied().unwrap_or(0),
        None => 0,
    };
    Slot { child, before }
}

fn empty(heap: &mut Heap) -> Result<Cell, Full> {
    new_root(heap, 0, 0, Value::NIL, Value::NIL, Value::NIL)
}

/// An array of `items`, whose references pass to it. A whole number of full
/// leaves go in a tightly packed tree, and the rest, 1 to 32, in the tail.
pub(crate) fn from_values(heap: &mut Heap, items: &[Value]) -> Result<Cell, Full> {
    let n = items.len();
    if n == 0 {
        return empty(heap);
    }
    if n <= B {
        let tail = leaf(heap, items)?;
        return new_root(heap, n, 0, Value::NIL, Value::NIL, tail);
    }
    let tail_len = if n.is_multiple_of(B) { B } else { n % B };
    let (body, tail_items) = items.split_at(n - tail_len);
    let mut nodes = Vec::with_capacity(body.len() / B);
    for chunk in body.chunks(B) {
        nodes.push(leaf(heap, chunk)?);
    }
    let mut shift = 0;
    while nodes.len() > 1 {
        shift += BITS;
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
    root(heap, array).len
}

/// Element `i`, with no reference added, or `None` past the end.
pub(crate) fn get(heap: &Heap, array: Cell, i: usize) -> Option<Value> {
    let r = root(heap, array);
    if i >= r.len {
        return None;
    }
    let head_len = node_len(heap, r.head) - r.offset;
    if i < head_len {
        return slots(heap, r.head).get(i + r.offset).copied();
    }
    let mut idx = i - head_len;
    let tree_len = node_len(heap, r.tree);
    if idx >= tree_len {
        return slots(heap, r.tail).get(idx - tree_len).copied();
    }
    let mut n = r.tree;
    loop {
        match node(heap, n)? {
            Node::Leaf(items) => return items.get(idx).copied(),
            Node::Branch {
                shift,
                sizes,
                children,
            } => {
                let slot = size_slot(&sizes, idx, shift);
                idx -= slot.before;
                n = *children.get(slot.child)?;
            }
        }
    }
}

/// Every element, front to back, with no reference added.
pub(crate) fn elements(heap: &Heap, array: Cell) -> Vec<Value> {
    let r = root(heap, array);
    let mut out = Vec::with_capacity(r.len);
    out.extend(head_items(heap, &r));
    // A stack of nodes still to visit, last child first, so they pop in order.
    let mut todo = vec![r.tree];
    while let Some(n) = todo.pop() {
        match node(heap, n) {
            None => {}
            Some(Node::Leaf(items)) => out.extend(items),
            Some(Node::Branch { children, .. }) => todo.extend(children.into_iter().rev()),
        }
    }
    out.extend(slots(heap, r.tail));
    out
}

/// `array` with `x` added at `end`; `x`'s reference passes to the result.
/// The buffer at that end takes it, and a full buffer first goes into the
/// tree as a finished leaf.
pub(crate) fn push(heap: &mut Heap, array: Cell, x: Value, end: End) -> Result<Cell, Full> {
    let r = root(heap, array);
    // A head with items left behind is settled first: the leaf that goes into
    // the tree, or that the new item joins, must hold only live items.
    let settled = own_head(heap, &r)?;
    let (this, this_items, other) = match end {
        End::Front => (settled, head_items(heap, &r), r.tail),
        End::Back => (r.tail, slots(heap, r.tail), settled),
    };
    let (tree, shift, new) = if this_items.is_empty() {
        (own(heap, r.tree), r.shift, leaf(heap, &[x])?)
    } else if this_items.len() < B {
        let kept = own_all(heap, &this_items);
        let items = match end {
            End::Front => [vec![x], kept].concat(),
            End::Back => [kept, vec![x]].concat(),
        };
        (own(heap, r.tree), r.shift, leaf(heap, &items)?)
    } else {
        let (tree, shift) = tree_push_leaf(heap, r.tree, r.shift, this, end)?;
        (tree, shift, leaf(heap, &[x])?)
    };
    let other = own(heap, other);
    // `settled` is the head's own leaf when nothing was left behind, and the
    // branches above each took their own reference to what they kept.
    release(heap, settled);
    let (head, tail) = match end {
        End::Front => (new, other),
        End::Back => (other, new),
    };
    new_root(heap, r.len + 1, shift, head, tree, tail)
}

/// `tree` with the full leaf `leaf` hung under its edge at `end`, growing a
/// level when that edge is full. Both are borrowed; the result is owned.
fn tree_push_leaf(
    heap: &mut Heap,
    tree: Value,
    shift: usize,
    leaf: Value,
    end: End,
) -> Result<(Value, usize), Full> {
    if tree.as_cell().is_none() {
        return Ok((own(heap, leaf), 0));
    }
    if let Some(n) = try_push(heap, tree, shift, leaf, end)? {
        return Ok((n, shift));
    }
    let leaf = own(heap, leaf);
    let spine = spine(heap, leaf, shift)?;
    let tree = own(heap, tree);
    let pair = match end {
        End::Front => [spine, tree],
        End::Back => [tree, spine],
    };
    Ok((branch(heap, shift + BITS, &pair)?, shift + BITS))
}

/// A chain of one-child branches lifting the owned `leaf` to height `shift`.
fn spine(heap: &mut Heap, leaf: Value, shift: usize) -> Result<Value, Full> {
    let mut n = leaf;
    let mut s = BITS;
    while s <= shift {
        n = branch(heap, s, &[n])?;
        s += BITS;
    }
    Ok(n)
}

/// `node` with `leaf` hung under its edge at `end` without growing taller,
/// or `None` when every level on that edge is full.
fn try_push(
    heap: &mut Heap,
    n: Value,
    shift: usize,
    leaf: Value,
    end: End,
) -> Result<Option<Value>, Full> {
    if shift == 0 {
        return Ok(None);
    }
    let children = slots(heap, n);
    let Some(edge) = (match end {
        End::Front => Some(0),
        End::Back => children.len().checked_sub(1),
    }) else {
        return Ok(None);
    };
    let Some(&edge_child) = children.get(edge) else {
        return Ok(None);
    };
    if let Some(sub) = try_push(heap, edge_child, shift - BITS, leaf, end)? {
        let mut kept = Vec::with_capacity(children.len());
        for (i, c) in children.iter().enumerate() {
            kept.push(if i == edge { sub } else { own(heap, *c) });
        }
        return Ok(Some(branch(heap, shift, &kept)?));
    }
    if children.len() < B {
        let leaf = own(heap, leaf);
        let s = spine(heap, leaf, shift - BITS)?;
        let kept = own_all(heap, &children);
        let all = match end {
            End::Front => [vec![s], kept].concat(),
            End::Back => [kept, vec![s]].concat(),
        };
        return Ok(Some(branch(heap, shift, &all)?));
    }
    Ok(None)
}

/// The owned `n` without the one-child levels at its top.
fn collapse(heap: &mut Heap, mut n: Value, mut shift: usize) -> (Value, usize) {
    while shift > 0 {
        let children = slots(heap, n);
        let [only] = children.as_slice() else {
            break;
        };
        let only = own(heap, *only);
        release(heap, n);
        n = only;
        shift -= BITS;
    }
    (n, shift)
}

/// The first `n` elements.
fn take(heap: &mut Heap, array: Cell, n: usize) -> Result<Cell, Full> {
    let r = root(heap, array);
    if n == 0 {
        return empty(heap);
    }
    if n >= r.len {
        heap.share(array);
        return Ok(array);
    }
    let front = head_items(heap, &r);
    if n <= front.len() {
        let kept = own_all(heap, front.get(..n).unwrap_or(&[]));
        let tail = leaf(heap, &kept)?;
        return new_root(heap, n, 0, Value::NIL, Value::NIL, tail);
    }
    let m = n - front.len();
    let tree_len = node_len(heap, r.tree);
    if m <= tree_len {
        let cut = tree_take(heap, r.tree, m)?;
        let (tree, shift) = collapse(heap, cut, r.shift);
        let head = own(heap, r.head);
        return root_at(heap, n, shift, r.offset, head, tree, Value::NIL);
    }
    let tail_items = slots(heap, r.tail);
    let kept = own_all(heap, tail_items.get(..m - tree_len).unwrap_or(&[]));
    let tail = leaf(heap, &kept)?;
    let head = own(heap, r.head);
    let tree = own(heap, r.tree);
    root_at(heap, n, r.shift, r.offset, head, tree, tail)
}

/// The elements from `start` up to but not including `end`, or `None` when
/// that range is not inside the array.
pub(crate) fn slice(
    heap: &mut Heap,
    array: Cell,
    start: usize,
    end: usize,
) -> Result<Option<Cell>, Full> {
    if start > end || end > len(heap, array) {
        return Ok(None);
    }
    let front = take(heap, array, end)?;
    let cut = skip(heap, front, start)?;
    heap.release(front);
    Ok(Some(cut))
}

/// Every element but the first `n`.
pub(crate) fn skip(heap: &mut Heap, array: Cell, n: usize) -> Result<Cell, Full> {
    let r = root(heap, array);
    if n == 0 {
        heap.share(array);
        return Ok(array);
    }
    if n >= r.len {
        return empty(heap);
    }
    let front = head_items(heap, &r);
    if n < front.len() {
        // The cheap `[h, ..t]`: the same head leaf, tree and tail, with `n`
        // more items left behind. Nothing is copied.
        let head = own(heap, r.head);
        let tree = own(heap, r.tree);
        let tail = own(heap, r.tail);
        return root_at(heap, r.len - n, r.shift, r.offset + n, head, tree, tail);
    }
    let m = n - front.len();
    // The head is spent, so what is left comes out of the tree: move its
    // leftmost leaf up to be the new head, whole and shared, and leave `m` of
    // it behind. The tree is cut once for a leaf rather than once an element,
    // so walking an array cuts it every `B` steps and the steps between are
    // the offset above.
    if let Some(first) = first_leaf(heap, r.tree)
        && m < slots(heap, first).len()
    {
        let taken = slots(heap, first).len();
        // `tree_drop` of a whole leaf that is the whole tree would leave an
        // empty leaf behind, so a tree of one leaf becomes no tree at all.
        let (tree, shift) = if taken == node_len(heap, r.tree) {
            (Value::NIL, 0)
        } else {
            let cut = tree_drop(heap, r.tree, taken)?;
            collapse(heap, cut, r.shift)
        };
        let head = own(heap, first);
        let tail = own(heap, r.tail);
        return root_at(heap, r.len - n, shift, m, head, tree, tail);
    }
    let tree_len = node_len(heap, r.tree);
    if m < tree_len {
        let cut = tree_drop(heap, r.tree, m)?;
        let (tree, shift) = collapse(heap, cut, r.shift);
        let tail = own(heap, r.tail);
        return new_root(heap, r.len - n, shift, Value::NIL, tree, tail);
    }
    let k = m - tree_len;
    let tail = if k == 0 {
        own(heap, r.tail)
    } else {
        let tail_items = slots(heap, r.tail);
        let kept = own_all(heap, tail_items.get(k..).unwrap_or(&[]));
        leaf(heap, &kept)?
    };
    new_root(heap, r.len - n, 0, Value::NIL, Value::NIL, tail)
}

/// The tree's leftmost leaf, borrowed, or `None` when there is no tree.
fn first_leaf(heap: &Heap, n: Value) -> Option<Value> {
    let mut n = n;
    loop {
        match node(heap, n)? {
            Node::Leaf(_) => return Some(n),
            Node::Branch { children, .. } => n = *children.first()?,
        }
    }
}

/// The first `m` elements of a tree node, `m` from 1 to its length.
fn tree_take(heap: &mut Heap, n: Value, m: usize) -> Result<Value, Full> {
    match node(heap, n) {
        None => Ok(Value::NIL),
        Some(Node::Leaf(items)) if m >= items.len() => Ok(own(heap, n)),
        Some(Node::Leaf(items)) => {
            let kept = own_all(heap, items.get(..m).unwrap_or(&[]));
            leaf(heap, &kept)
        }
        Some(Node::Branch {
            shift,
            sizes,
            children,
        }) => {
            if m >= sizes.last().copied().unwrap_or(0) {
                return Ok(own(heap, n));
            }
            let slot = size_slot(&sizes, m - 1, shift);
            let Some(&child) = children.get(slot.child) else {
                return Ok(own(heap, n));
            };
            let cut = tree_take(heap, child, m - slot.before)?;
            let mut kept = own_all(heap, children.get(..slot.child).unwrap_or(&[]));
            kept.push(cut);
            branch(heap, shift, &kept)
        }
    }
}

/// A tree node without its first `m` elements, `m` below its length.
fn tree_drop(heap: &mut Heap, n: Value, m: usize) -> Result<Value, Full> {
    if m == 0 {
        return Ok(own(heap, n));
    }
    match node(heap, n) {
        None => Ok(Value::NIL),
        Some(Node::Leaf(items)) => {
            let kept = own_all(heap, items.get(m..).unwrap_or(&[]));
            leaf(heap, &kept)
        }
        Some(Node::Branch {
            shift,
            sizes,
            children,
        }) => {
            let slot = size_slot(&sizes, m, shift);
            let Some(&child) = children.get(slot.child) else {
                return Ok(Value::NIL);
            };
            let cut = tree_drop(heap, child, m - slot.before)?;
            let mut kept = vec![cut];
            kept.extend(own_all(heap, children.get(slot.child + 1..).unwrap_or(&[])));
            branch(heap, shift, &kept)
        }
    }
}

/// `l` then `r`. The two trees are merged down `l`'s right edge and `r`'s
/// left edge, and each level is repacked when it would otherwise be more than
/// `E_MAX` nodes looser than it could be, which keeps lookups a step per
/// level however many joins built the array.
pub(crate) fn concat(heap: &mut Heap, l: Cell, r: Cell) -> Result<Cell, Full> {
    let lr = root(heap, l);
    let rr = root(heap, r);
    if lr.len == 0 {
        heap.share(r);
        return Ok(r);
    }
    if rr.len == 0 {
        heap.share(l);
        return Ok(l);
    }
    // The buffers at the seam go into their trees, so the merge sees two
    // trees. The left array keeps its head, and the right one its tail.
    let (ltree, lshift) = if lr.tail.as_cell().is_none() {
        (own(heap, lr.tree), lr.shift)
    } else {
        tree_push_leaf(heap, lr.tree, lr.shift, lr.tail, End::Back)?
    };
    let rhead = own_head(heap, &rr)?;
    let (rtree, rshift) = if rhead.as_cell().is_none() {
        (own(heap, rr.tree), rr.shift)
    } else {
        tree_push_leaf(heap, rr.tree, rr.shift, rhead, End::Front)?
    };
    release(heap, rhead);
    let (tree, shift) = if ltree.as_cell().is_none() {
        (rtree, rshift)
    } else if rtree.as_cell().is_none() {
        (ltree, lshift)
    } else {
        let top = merge(heap, ltree, lshift, rtree, rshift)?;
        release(heap, ltree);
        release(heap, rtree);
        let h = lshift.max(rshift);
        match top.as_slice() {
            [one] => collapse(heap, *one, h),
            _ => (branch(heap, h + BITS, &top)?, h + BITS),
        }
    };
    let head = own(heap, lr.head);
    let tail = own(heap, rr.tail);
    root_at(heap, lr.len + rr.len, shift, lr.offset, head, tree, tail)
}

/// The borrowed trees `l` and `r` merged into one or two owned nodes at the
/// taller one's height.
fn merge(
    heap: &mut Heap,
    l: Value,
    lshift: usize,
    r: Value,
    rshift: usize,
) -> Result<Vec<Value>, Full> {
    let lc = slots(heap, l);
    let rc = slots(heap, r);
    let (left, right) = match (lc.split_last(), rc.split_first()) {
        (Some((last, left)), Some((first, right))) => ((left, *last), (*first, right)),
        _ => return Ok(own_all(heap, &[l, r])),
    };
    if lshift > rshift {
        let mid = merge(heap, left.1, lshift - BITS, r, rshift)?;
        rebalance(heap, left.0, mid, &[], lshift)
    } else if rshift > lshift {
        let mid = merge(heap, l, lshift, right.0, rshift - BITS)?;
        rebalance(heap, &[], mid, right.1, rshift)
    } else if lshift == 0 {
        if lc.len() + rc.len() <= B {
            let items = own_all(heap, &[lc, rc].concat());
            Ok(vec![leaf(heap, &items)?])
        } else {
            Ok(own_all(heap, &[l, r]))
        }
    } else {
        let mid = merge(heap, left.1, lshift - BITS, right.0, rshift - BITS)?;
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
) -> Result<Vec<Value>, Full> {
    let mut all = own_all(heap, left);
    all.extend(mid);
    all.extend(own_all(heap, right));
    let counts: Vec<usize> = all.iter().map(|n| slots(heap, *n).len()).collect();
    let total: usize = counts.iter().sum();
    let optimal = total.div_ceil(B);
    if all.len() > optimal + E_MAX {
        let plan = plan(counts, optimal);
        all = repack(heap, all, &plan, shift - BITS)?;
    }
    let mut out = Vec::with_capacity(2);
    let (first, second) = all.split_at(all.len().min(B));
    out.push(branch(heap, shift, first)?);
    if !second.is_empty() {
        out.push(branch(heap, shift, second)?);
    }
    Ok(out)
}

/// The slot counts to repack a level to: while it has more than `E_MAX`
/// nodes past `optimal`, the first node under `B - E_MAX / 2` slots is
/// poured into the ones after it, and one node fewer is left. Such a node
/// always exists while the loop runs: if every node held at least that many,
/// the level would already be within `E_MAX` of `optimal`.
fn plan(mut counts: Vec<usize>, optimal: usize) -> Vec<usize> {
    while counts.len() > optimal + E_MAX {
        let Some(i) = counts.iter().position(|c| *c < B - E_MAX / 2) else {
            break;
        };
        let mut spill = counts.get(i).copied().unwrap_or(0);
        let mut j = i;
        while spill > 0 && j + 1 < counts.len() {
            let next = counts.get(j + 1).copied().unwrap_or(0);
            let merged = (spill + next).min(B);
            spill = spill + next - merged;
            if let Some(c) = counts.get_mut(j) {
                *c = merged;
            }
            j += 1;
        }
        counts.remove(j);
    }
    counts
}

/// The owned nodes `old`, one level below, rebuilt to the slot counts in
/// `plan`, their slots streamed in order. A node whose count already matches,
/// and that starts on a node boundary, is kept as it is.
fn repack(
    heap: &mut Heap,
    old: Vec<Value>,
    plan: &[usize],
    child_shift: usize,
) -> Result<Vec<Value>, Full> {
    let mut old = old.into_iter().map(Some).collect::<Vec<_>>();
    let mut out = Vec::with_capacity(plan.len());
    let (mut src, mut off) = (0, 0);
    for &want in plan {
        if off == 0
            && let Some(Some(n)) = old.get(src)
            && slots(heap, *n).len() == want
        {
            out.extend(old.get_mut(src).and_then(Option::take));
            src += 1;
            continue;
        }
        let mut buf = Vec::with_capacity(want);
        while buf.len() < want {
            let Some(Some(n)) = old.get(src) else {
                break;
            };
            let items = slots(heap, *n);
            let take = (want - buf.len()).min(items.len() - off);
            buf.extend(own_all(heap, items.get(off..off + take).unwrap_or(&[])));
            off += take;
            if off == items.len() {
                if let Some(n) = old.get_mut(src).and_then(Option::take) {
                    release(heap, n);
                }
                src += 1;
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
        release(heap, n);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ints(heap: &Heap, array: Cell) -> Vec<i64> {
        elements(heap, array)
            .into_iter()
            .map(|v| match v.view() {
                crate::value::View::Int(n) => n,
                other => panic!("not an Int: {other:?}"),
            })
            .collect()
    }

    fn int(v: Value) -> Option<i64> {
        match v.view() {
            crate::value::View::Int(n) => Some(n),
            _ => None,
        }
    }

    fn array_of(heap: &mut Heap, range: std::ops::Range<i64>) -> Cell {
        let items: Vec<Value> = range.map(|n| Value::int(n).expect("small")).collect();
        from_values(heap, &items).expect("room")
    }

    /// Every invariant of the tree: each node's height, each leaf's and
    /// branch's slot count, each running total, and the root's length.
    fn check(heap: &Heap, array: Cell) {
        let r = root(heap, array);
        for buf in [r.head, r.tail] {
            let n = slots(heap, buf).len();
            assert!(
                buf.as_cell().is_none() || (1..=B).contains(&n),
                "a buffer of {n}"
            );
        }
        assert!(
            r.offset < node_len(heap, r.head).max(1),
            "an offset past its leaf"
        );
        let tree_len = if r.tree.as_cell().is_some() {
            check_node(heap, r.tree, r.shift)
        } else {
            0
        };
        assert_eq!(
            r.len,
            node_len(heap, r.head) - r.offset + tree_len + node_len(heap, r.tail)
        );
        assert_eq!(elements(heap, array).len(), r.len);
    }

    fn check_node(heap: &Heap, n: Value, shift: usize) -> usize {
        match node(heap, n).expect("a node") {
            Node::Leaf(items) => {
                assert_eq!(shift, 0, "a leaf above the bottom");
                assert!((1..=B).contains(&items.len()));
                items.len()
            }
            Node::Branch {
                shift: s,
                sizes,
                children,
            } => {
                assert_eq!(s, shift, "a branch at the wrong height");
                assert!((1..=B).contains(&children.len()));
                let mut total = 0;
                for (i, c) in children.iter().enumerate() {
                    total += check_node(heap, *c, shift - BITS);
                    assert_eq!(sizes[i], total);
                }
                total
            }
        }
    }

    #[test]
    fn an_array_reads_back_at_every_size() {
        let mut heap = Heap::default();
        for n in [0, 1, 31, 32, 33, 64, 65, 1024, 1025, 40_000] {
            let a = array_of(&mut heap, 0..n);
            check(&heap, a);
            assert_eq!(len(&heap, a), n as usize);
            assert_eq!(ints(&heap, a), (0..n).collect::<Vec<_>>());
            for i in [0, n / 2, n - 1] {
                if n > 0 {
                    assert_eq!(
                        get(&heap, a, i as usize).map(Value::view),
                        Value::int(i).map(Value::view)
                    );
                }
            }
            assert!(get(&heap, a, n as usize).is_none());
            heap.release(a);
            assert_eq!(heap.live(), 0, "size {n}");
        }
    }

    /// Pushes at both ends, 3,000 of each, keep every invariant and leave the
    /// array they started from as it was.
    #[test]
    fn pushing_at_either_end_leaves_the_old_array_alone() {
        let mut heap = Heap::default();
        let first = array_of(&mut heap, 0..10);
        let mut a = first;
        heap.share(a);
        let mut want: std::collections::VecDeque<i64> = (0..10).collect();
        for i in 0..3000 {
            let (end, x) = if i % 3 == 0 {
                (End::Front, -i)
            } else {
                (End::Back, 100 + i)
            };
            let next = push(&mut heap, a, Value::int(x).expect("small"), end).expect("room");
            heap.release(a);
            a = next;
            match end {
                End::Front => want.push_front(x),
                End::Back => want.push_back(x),
            }
        }
        check(&heap, a);
        assert_eq!(ints(&heap, a), want.into_iter().collect::<Vec<_>>());
        assert_eq!(ints(&heap, first), (0..10).collect::<Vec<_>>());
        heap.release(a);
        heap.release(first);
        assert_eq!(heap.live(), 0);
    }

    #[test]
    fn take_and_skip_cut_anywhere() {
        let mut heap = Heap::default();
        let mut a = array_of(&mut heap, 0..10);
        for i in 0..40 {
            let next =
                push(&mut heap, a, Value::int(-1 - i).expect("small"), End::Front).expect("room");
            heap.release(a);
            a = next;
        }
        let tail = array_of(&mut heap, 10..1500);
        let joined = concat(&mut heap, a, tail).expect("room");
        let all = ints(&heap, joined);
        for n in [0, 1, 5, 39, 40, 41, 50, 51, 700, 1539, 1540, 1541, 5000] {
            let t = take(&mut heap, joined, n).expect("room");
            check(&heap, t);
            assert_eq!(ints(&heap, t), all[..n.min(all.len())], "take {n}");
            let s = skip(&mut heap, joined, n).expect("room");
            check(&heap, s);
            assert_eq!(ints(&heap, s), all[n.min(all.len())..], "skip {n}");
            heap.release(t);
            heap.release(s);
        }
        for c in [a, tail, joined] {
            heap.release(c);
        }
        assert_eq!(heap.live(), 0);
    }

    /// Walking an array one element at a time, the way `[h, ..t]` does. Each
    /// step keeps the same leaf and moves the offset along, and the tree is
    /// cut only when a leaf runs out, so the cells this makes are a small
    /// multiple of the number of leaves rather than of the elements. Every
    /// step still reads back as the plain list does, from either end and
    /// through `get`.
    #[test]
    fn walking_an_array_from_the_front_copies_no_leaf() {
        let mut heap = Heap::default();
        let n = 1000;
        let a = array_of(&mut heap, 0..n);
        let mut cur = a;
        heap.share(cur);
        // Every leaf the walk reads from. A leaf it copied would be a cell of
        // its own, so counting them is counting the copies.
        let mut heads: Vec<Option<Cell>> = Vec::new();
        for i in 0..n {
            assert_eq!(get(&heap, cur, 0).and_then(int), Some(i), "head at {i}");
            assert_eq!(len(&heap, cur), (n - i) as usize);
            let head = root(&heap, cur).head.as_cell();
            if heads.last() != Some(&head) {
                heads.push(head);
            }
            let next = skip(&mut heap, cur, 1).expect("room");
            check(&heap, next);
            heap.release(cur);
            cur = next;
        }
        assert_eq!(len(&heap, cur), 0);
        heap.release(cur);
        let leaves = (n as usize).div_ceil(B);
        assert!(
            heads.len() <= leaves + 1,
            "{} leaves read for {leaves} in the array",
            heads.len()
        );
        heap.release(a);
        assert_eq!(heap.live(), 0);
    }

    /// An array partly walked is an ordinary array: what it left behind is
    /// invisible to reading it, cutting it, joining it and pushing onto it,
    /// and none of those reach past its own front.
    #[test]
    fn an_array_walked_part_way_behaves_like_the_rest_of_it() {
        let mut heap = Heap::default();
        let whole = array_of(&mut heap, 0..200);
        let other = array_of(&mut heap, 200..260);
        for k in [1, 5, 31, 32, 33, 40, 64, 100] {
            let s = skip(&mut heap, whole, k).expect("room");
            check(&heap, s);
            let rest: Vec<i64> = (k as i64..200).collect();
            assert_eq!(ints(&heap, s), rest, "skip {k}");
            assert_eq!(get(&heap, s, 0).and_then(int), Some(k as i64));
            assert_eq!(get(&heap, s, 3).and_then(int), Some(k as i64 + 3));

            let t = take(&mut heap, s, 7).expect("room");
            check(&heap, t);
            assert_eq!(ints(&heap, t), rest[..7], "take after skip {k}");

            let again = skip(&mut heap, s, 9).expect("room");
            check(&heap, again);
            assert_eq!(ints(&heap, again), rest[9..], "skip twice {k}");

            let joined = concat(&mut heap, s, other).expect("room");
            check(&heap, joined);
            let want: Vec<i64> = rest.iter().copied().chain(200..260).collect();
            assert_eq!(ints(&heap, joined), want, "concat after skip {k}");

            let front =
                push(&mut heap, s, Value::int(-1).expect("small"), End::Front).expect("room");
            check(&heap, front);
            let want: Vec<i64> = std::iter::once(-1).chain(rest.iter().copied()).collect();
            assert_eq!(ints(&heap, front), want, "push front after skip {k}");

            let back = push(&mut heap, s, Value::int(-1).expect("small"), End::Back).expect("room");
            check(&heap, back);
            let want: Vec<i64> = rest.iter().copied().chain(std::iter::once(-1)).collect();
            assert_eq!(ints(&heap, back), want, "push back after skip {k}");

            for c in [s, t, again, joined, front, back] {
                heap.release(c);
            }
        }
        heap.release(whole);
        heap.release(other);
        assert_eq!(heap.live(), 0);
    }

    /// Joins of every size against every size, and a long run of joins,
    /// keep every invariant, and the tree stays shallow.
    #[test]
    fn concat_keeps_the_tree_balanced() {
        let mut heap = Heap::default();
        let sizes = [0, 1, 17, 32, 33, 100, 1024, 1057, 5000];
        for &x in &sizes {
            for &y in &sizes {
                let l = array_of(&mut heap, 0..x);
                let r = array_of(&mut heap, x..x + y);
                let j = concat(&mut heap, l, r).expect("room");
                check(&heap, j);
                assert_eq!(ints(&heap, j), (0..x + y).collect::<Vec<_>>(), "{x} ++ {y}");
                for c in [l, r, j] {
                    heap.release(c);
                }
            }
        }
        let mut acc = empty(&mut heap).expect("room");
        let mut want = Vec::new();
        for i in 0..300 {
            let piece = array_of(&mut heap, i * 7..i * 7 + (i % 40));
            want.extend(i * 7..i * 7 + (i % 40));
            let next = concat(&mut heap, acc, piece).expect("room");
            heap.release(acc);
            heap.release(piece);
            acc = next;
        }
        check(&heap, acc);
        assert_eq!(ints(&heap, acc), want);
        assert!(
            root(&heap, acc).shift <= 3 * BITS,
            "height {}",
            root(&heap, acc).shift
        );
        heap.release(acc);
        assert_eq!(heap.live(), 0);
    }
}
