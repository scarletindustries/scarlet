//! The persistent vector backing `Array` values: an arena-native RRB
//! (relaxed radix-balanced) tree.
//!
//! ## The shape of a sequence
//!
//! ```text
//!   SeqRoot [ len | shift | head | tree | tail ]
//!                             |      |      |
//!     push_front lands -------+      |      +--- push_back lands here
//!     here (a SeqLeaf buffer)        |           (a SeqLeaf buffer)
//!                                    |
//!                  SeqBranch: 32-way radix tree,
//!                  cumulative size table per branch
//!                           /        |        \
//!                      SeqLeaf   SeqLeaf   SeqLeaf
//!                      (32 elements each)
//! ```
//!
//! The two buffer cells make `push_back`/`push_front` amortized O(1): pushes
//! fill a buffer word by word, and only a *full* buffer (32 elements) spills
//! into the tree as a finished leaf — the tree is touched once per 32 pushes.
//! Every branch stores a cumulative size table, so index search, slicing, and
//! the rebalancing concatenation of the RRB paper (Bagwell & Rompf, 2011) all
//! run in O(log32 n) node visits.
//!
//! ## Persistence
//!
//! Nodes are reference-counted arena objects; "mutation" is path copying — an
//! operation allocates replacements for the O(log32 n) nodes on the
//! root-to-target path and shares every untouched subtree with the previous
//! version. A shared node carries one count per referrer, so the prior version
//! stays valid, and a version's exclusive nodes are freed as it drops.
//!
//! ## Operation set and complexity
//!
//! `from_slice` O(n) · `len` O(1) · `get`/`update` O(log32 n) ·
//! `push_back`/`push_front`/`pop_front` amortized O(1) · `take`/`skip` and
//! `concat` O(log n) · `iter` O(n) total.

use super::value::*;

/// Branching factor.
const B: usize = 32;
/// Bits per level (`log2(B)`); a node at `shift` has children at
/// `shift - BITS`, and `shift == 0` means leaf.
const BITS: usize = 5;
/// RRB slack invariant: a rebalance may leave at most `E_MAX` more nodes
/// than the optimal packing, bounding extra search steps per level.
const E_MAX: usize = 2;

/// Scratch buffer for assembling a replacement node's slots before handing
/// them to a builder. Holds owned `Value`s: `extend` clones (incref) and the
/// buffer's drop decrements — balanced by the builder's `store_child`, so the
/// reference counts come out exactly right (every node image is <= `2 * B`
/// slots, the reserved capacity).
struct Buf {
    items: Vec<Value>,
}

impl Buf {
    #[inline]
    fn new() -> Buf {
        Buf {
            items: Vec::with_capacity(2 * B),
        }
    }

    #[inline]
    fn push(&mut self, v: Value) {
        self.items.push(v);
    }

    #[inline]
    fn extend(&mut self, vs: &[Value]) {
        self.items.extend(vs.iter().cloned());
    }
}

impl std::ops::Deref for Buf {
    type Target = [Value];
    #[inline]
    fn deref(&self) -> &[Value] {
        &self.items
    }
}

impl std::ops::DerefMut for Buf {
    #[inline]
    fn deref_mut(&mut self) -> &mut [Value] {
        &mut self.items
    }
}

// ---- node accessors -------------------------------------------------------
//
// All layout knowledge lives in `value.rs`: nodes are read through the typed
// `SeqRootRef` / `SeqNodeRef` views and allocated through the
// `seq_root_in` / `seq_leaf_in` / `seq_branch_in` builders, so this module
// contains no unsafe code.

/// `(len, shift, head, tree, tail)` of a root. `head`/`tree`/`tail` are
/// node values or nil; `shift` is the tree height (0 = leaf or nil).
#[inline]
fn root_parts(root: &Value) -> (usize, usize, Value, Value, Value) {
    let r = SeqRootRef::new(root);
    (r.len, r.shift, r.head, r.tree, r.tail)
}

fn root_in<A: Arena + ?Sized>(
    a: &mut A,
    len: usize,
    shift: usize,
    head: Value,
    tree: Value,
    tail: Value,
) -> Value {
    debug_assert_eq!(len, node_len(&head) + node_len(&tree) + node_len(&tail));
    seq_root_in(a, len, shift, head, tree, tail)
}

#[inline]
fn node_is_leaf(node: &Value) -> bool {
    node.is_tag(HeapTag::SeqLeaf)
}

/// Elements of a leaf.
#[inline]
fn leaf_elems(leaf: &Value) -> &[Value] {
    match SeqNodeRef::of(leaf) {
        SeqNodeRef::Leaf(elems) => elems,
        SeqNodeRef::Branch { .. } => view_mismatch("seq"),
    }
}

/// `(sizes, children)` of a branch.
#[inline]
fn branch_parts(branch: &Value) -> (&[u64], &[Value]) {
    match SeqNodeRef::of(branch) {
        SeqNodeRef::Branch {
            sizes, children, ..
        } => (sizes, children),
        SeqNodeRef::Leaf(_) => view_mismatch("seq"),
    }
}

/// Descend one level: in a branch's cumulative size table, the child slot
/// containing element index `idx`, and the cumulative count before that slot.
#[inline]
fn size_slot(sizes: &[u64], idx: usize) -> (usize, usize) {
    let mut k = 0;
    while (sizes[k] as usize) <= idx {
        k += 1;
    }
    let before = if k > 0 { sizes[k - 1] as usize } else { 0 };
    (k, before)
}

/// Total element count under a node (leaf count or last cumulative size).
#[inline]
fn node_len(node: &Value) -> usize {
    if node.is_nil() {
        return 0;
    }
    match SeqNodeRef::of(node) {
        SeqNodeRef::Leaf(elems) => elems.len(),
        SeqNodeRef::Branch { sizes, .. } => sizes[sizes.len() - 1] as usize,
    }
}

/// Direct slot count of a node (elements of a leaf, children of a branch)
/// — the density measure the rebalance invariant is defined over.
#[inline]
fn slot_count(node: &Value) -> usize {
    match SeqNodeRef::of(node) {
        SeqNodeRef::Leaf(elems) => elems.len(),
        SeqNodeRef::Branch { children, .. } => children.len(),
    }
}

/// Direct slots of a node as values (elements or children).
#[inline]
fn slots(node: &Value) -> &[Value] {
    match SeqNodeRef::of(node) {
        SeqNodeRef::Leaf(elems) => elems,
        SeqNodeRef::Branch { children, .. } => children,
    }
}

fn leaf_from<A: Arena + ?Sized>(a: &mut A, items: &[Value]) -> Value {
    debug_assert!(!items.is_empty() && items.len() <= B);
    seq_leaf_in(a, items)
}

/// A leaf, or nil for an empty slice.
fn opt_leaf_from<A: Arena + ?Sized>(a: &mut A, items: &[Value]) -> Value {
    if items.is_empty() {
        Value::nil()
    } else {
        leaf_from(a, items)
    }
}

/// Build a branch at height `shift` over `children` (all at
/// `shift - BITS`); `seq_branch_in` computes the cumulative size table.
fn branch_from<A: Arena + ?Sized>(a: &mut A, shift: usize, children: &[Value]) -> Value {
    debug_assert!(!children.is_empty() && children.len() <= B);
    debug_assert!(shift >= BITS);
    debug_assert!(children.iter().all(|c| if shift == BITS {
        node_is_leaf(c)
    } else {
        c.is_tag(HeapTag::SeqBranch)
    }));
    seq_branch_in(a, shift, children)
}

// ---- public queries ---------------------------------------------------------

#[inline]
pub fn len(root: &Value) -> usize {
    root_parts(root).0
}

pub fn get(root: &Value, i: usize) -> Option<Value> {
    let (len, _, head, tree, tail) = root_parts(root);
    if i >= len {
        return None;
    }
    let hl = node_len(&head);
    if i < hl {
        return Some(leaf_elems(&head)[i].clone());
    }
    let mut idx = i - hl;
    let tree_len = node_len(&tree);
    if idx >= tree_len {
        return Some(leaf_elems(&tail)[idx - tree_len].clone());
    }
    let mut node = tree;
    loop {
        // Compute the next node as the match value so the view's borrow of
        // `node` ends before the reassignment.
        let next = match SeqNodeRef::of(&node) {
            SeqNodeRef::Leaf(elems) => return Some(elems[idx].clone()),
            SeqNodeRef::Branch {
                sizes, children, ..
            } => {
                let (k, before) = size_slot(sizes, idx);
                idx -= before;
                children[k].clone()
            }
        };
        node = next;
    }
}

// ---- construction -------------------------------------------------------------

pub fn empty_in<A: Arena + ?Sized>(a: &mut A) -> Value {
    root_in(a, 0, 0, Value::nil(), Value::nil(), Value::nil())
}

/// Bulk build: a strict (fully packed) tree plus a tail. O(n).
pub fn from_slice<A: Arena + ?Sized>(a: &mut A, items: &[Value]) -> Value {
    let n = items.len();
    if n == 0 {
        return empty_in(a);
    }
    if n <= B {
        let tail = leaf_from(a, items);
        return root_in(a, n, 0, Value::nil(), Value::nil(), tail);
    }
    // Keep the trailing 1..=32 elements as the tail; pack the rest.
    let tail_len = if n.is_multiple_of(B) { B } else { n % B };
    let (body, tail_items) = items.split_at(n - tail_len);
    let mut nodes: Vec<Value> = body.chunks(B).map(|c| leaf_from(a, c)).collect();
    let mut shift = 0;
    while nodes.len() > 1 {
        shift += BITS;
        nodes = nodes.chunks(B).map(|c| branch_from(a, shift, c)).collect();
    }
    let tail = leaf_from(a, tail_items);
    let tree = nodes.remove(0);
    root_in(a, n, shift, Value::nil(), tree, tail)
}

// ---- push/pop at the ends -----------------------------------------------------

/// Append one element. Amortized O(1): the tail buffer absorbs pushes and
/// is pushed into the tree as a full leaf every 32nd call.
pub fn push_back<A: Arena + ?Sized>(a: &mut A, root: &Value, x: Value) -> Value {
    push_end::<A, false>(a, root, x)
}

/// Prepend one element — the mirror of [`push_back`] via the head buffer.
pub fn push_front<A: Arena + ?Sized>(a: &mut A, root: &Value, x: Value) -> Value {
    push_end::<A, true>(a, root, x)
}

/// Push one element into the head (`FRONT`) or tail buffer, spilling a full
/// buffer into the tree as a finished leaf.
fn push_end<A: Arena + ?Sized, const FRONT: bool>(a: &mut A, root: &Value, x: Value) -> Value {
    let (len, mut shift, head, mut tree, tail) = root_parts(root);
    let old = if FRONT { head.clone() } else { tail.clone() };
    let new = if old.is_nil() {
        leaf_from(a, &[x])
    } else {
        let elems = leaf_elems(&old);
        if elems.len() < B {
            let mut buf = Buf::new();
            if FRONT {
                buf.push(x);
                buf.extend(elems);
            } else {
                buf.extend(elems);
                buf.push(x);
            }
            leaf_from(a, &buf)
        } else {
            (tree, shift) = tree_push_leaf::<A, FRONT>(a, &tree, shift, &old);
            leaf_from(a, &[x])
        }
    };
    if FRONT {
        root_in(a, len + 1, shift, new, tree, tail)
    } else {
        root_in(a, len + 1, shift, head, tree, new)
    }
}

/// Remove the first element, returning `(element, rest)`. Amortized O(1).
pub fn pop_front<A: Arena + ?Sized>(a: &mut A, root: &Value) -> Option<(Value, Value)> {
    let (len, shift, head, tree, tail) = root_parts(root);
    if len == 0 {
        return None;
    }
    if !head.is_nil() {
        let elems = leaf_elems(&head);
        let e = elems[0].clone();
        let new_head = opt_leaf_from(a, &elems[1..]);
        return Some((e, root_in(a, len - 1, shift, new_head, tree, tail)));
    }
    if !tree.is_nil() {
        // Pull the leftmost leaf out of the tree to become the head.
        let (leaf, rest) = tree_pop_leftmost(a, &tree, shift);
        let (new_tree, new_shift) = collapse(rest, shift);
        let elems = leaf_elems(&leaf);
        let e = elems[0].clone();
        let new_head = opt_leaf_from(a, &elems[1..]);
        return Some((e, root_in(a, len - 1, new_shift, new_head, new_tree, tail)));
    }
    let elems = leaf_elems(&tail);
    let e = elems[0].clone();
    let new_tail = opt_leaf_from(a, &elems[1..]);
    Some((
        e,
        root_in(a, len - 1, 0, Value::nil(), Value::nil(), new_tail),
    ))
}

/// Replace element `i`, path-copying the spine. O(log32 n).
pub fn update<A: Arena + ?Sized>(a: &mut A, root: &Value, i: usize, x: Value) -> Option<Value> {
    let (len, shift, head, tree, tail) = root_parts(root);
    if i >= len {
        return None;
    }
    let hl = node_len(&head);
    if i < hl {
        let new_head = leaf_replace(a, &head, i, x);
        return Some(root_in(a, len, shift, new_head, tree, tail));
    }
    let idx = i - hl;
    let tree_len = node_len(&tree);
    if idx < tree_len {
        let new_tree = tree_update(a, &tree, idx, x);
        return Some(root_in(a, len, shift, head, new_tree, tail));
    }
    let new_tail = leaf_replace(a, &tail, idx - tree_len, x);
    Some(root_in(a, len, shift, head, tree, new_tail))
}

fn leaf_replace<A: Arena + ?Sized>(a: &mut A, leaf: &Value, i: usize, x: Value) -> Value {
    let mut buf = Buf::new();
    buf.extend(leaf_elems(leaf));
    buf[i] = x;
    leaf_from(a, &buf)
}

fn tree_update<A: Arena + ?Sized>(a: &mut A, node: &Value, idx: usize, x: Value) -> Value {
    match SeqNodeRef::of(node) {
        SeqNodeRef::Leaf(_) => leaf_replace(a, node, idx, x),
        SeqNodeRef::Branch {
            shift,
            sizes,
            children,
        } => {
            let (k, before) = size_slot(sizes, idx);
            let child = tree_update(a, &children[k], idx - before, x);
            let mut buf = Buf::new();
            buf.extend(children);
            buf[k] = child;
            branch_from(a, shift, &buf)
        }
    }
}

/// Push a full leaf under the right (or left, when `FRONT`) edge of `tree`;
/// grows the root when that spine is full. Returns the new `(tree, shift)`.
fn tree_push_leaf<A: Arena + ?Sized, const FRONT: bool>(
    a: &mut A,
    tree: &Value,
    shift: usize,
    leaf: &Value,
) -> (Value, usize) {
    if tree.is_nil() {
        return (leaf.clone(), 0);
    }
    match try_push::<A, FRONT>(a, tree, shift, leaf) {
        Some(n) => (n, shift),
        None => {
            let spine = make_spine(a, leaf, shift);
            let pair = if FRONT {
                [spine, tree.clone()]
            } else {
                [tree.clone(), spine]
            };
            (branch_from(a, shift + BITS, &pair), shift + BITS)
        }
    }
}

/// A chain of single-child branches lifting `leaf` to height `shift`.
fn make_spine<A: Arena + ?Sized>(a: &mut A, leaf: &Value, shift: usize) -> Value {
    let mut node = leaf.clone();
    let mut s = BITS;
    while s <= shift {
        node = branch_from(a, s, &[node]);
        s += BITS;
    }
    node
}

/// Try to hang `leaf` under the rightmost (or leftmost, when `FRONT`) edge
/// of `node` (at `shift`) without increasing the height. `None` when every
/// spine level on that side is full.
fn try_push<A: Arena + ?Sized, const FRONT: bool>(
    a: &mut A,
    node: &Value,
    shift: usize,
    leaf: &Value,
) -> Option<Value> {
    if shift == 0 {
        return None;
    }
    let (_, children) = branch_parts(node);
    let edge = if FRONT { 0 } else { children.len() - 1 };
    if let Some(sub) = try_push::<A, FRONT>(a, &children[edge], shift - BITS, leaf) {
        let mut buf = Buf::new();
        buf.extend(children);
        buf[edge] = sub;
        return Some(branch_from(a, shift, &buf));
    }
    if children.len() < B {
        let spine = make_spine(a, leaf, shift - BITS);
        let mut buf = Buf::new();
        if FRONT {
            buf.push(spine);
            buf.extend(children);
        } else {
            buf.extend(children);
            buf.push(spine);
        }
        return Some(branch_from(a, shift, &buf));
    }
    None
}

/// Detach the leftmost leaf. Returns `(leaf, rest)` where `rest` is a node
/// at the same height as `node` or nil.
fn tree_pop_leftmost<A: Arena + ?Sized>(a: &mut A, node: &Value, shift: usize) -> (Value, Value) {
    if shift == 0 {
        return (node.clone(), Value::nil());
    }
    let (_, children) = branch_parts(node);
    let (leaf, sub) = tree_pop_leftmost(a, &children[0], shift - BITS);
    if sub.is_nil() && children.len() == 1 {
        return (leaf, Value::nil());
    }
    let mut buf = Buf::new();
    if !sub.is_nil() {
        buf.push(sub);
    }
    buf.extend(&children[1..]);
    (leaf, branch_from(a, shift, &buf))
}

/// Drop redundant single-child levels off the top of a tree.
fn collapse(mut node: Value, mut shift: usize) -> (Value, usize) {
    if node.is_nil() {
        return (Value::nil(), 0);
    }
    while shift > 0 {
        let next = {
            let (_, children) = branch_parts(&node);
            if children.len() != 1 {
                break;
            }
            children[0].clone()
        };
        node = next;
        shift -= BITS;
    }
    (node, shift)
}

// ---- slicing --------------------------------------------------------------------

/// The first `n` elements. O(log32 n) path copy.
pub fn take<A: Arena + ?Sized>(a: &mut A, root: &Value, n: usize) -> Value {
    let (len, shift, head, tree, tail) = root_parts(root);
    if n == 0 {
        return empty_in(a);
    }
    if n >= len {
        return root.clone();
    }
    let hl = node_len(&head);
    if n <= hl {
        let new_tail = leaf_from(a, &leaf_elems(&head)[..n]);
        return root_in(a, n, 0, Value::nil(), Value::nil(), new_tail);
    }
    let tree_len = node_len(&tree);
    let m = n - hl;
    if m <= tree_len {
        let cut = tree_take(a, &tree, m);
        let (new_tree, new_shift) = collapse(cut, shift);
        return root_in(a, n, new_shift, head, new_tree, Value::nil());
    }
    let new_tail = leaf_from(a, &leaf_elems(&tail)[..m - tree_len]);
    root_in(a, n, shift, head, tree, new_tail)
}

/// All but the first `n` elements. O(log32 n) path copy.
pub fn skip<A: Arena + ?Sized>(a: &mut A, root: &Value, n: usize) -> Value {
    let (len, shift, head, tree, tail) = root_parts(root);
    if n == 0 {
        return root.clone();
    }
    if n >= len {
        return empty_in(a);
    }
    let hl = node_len(&head);
    if n < hl {
        let new_head = leaf_from(a, &leaf_elems(&head)[n..]);
        return root_in(a, len - n, shift, new_head, tree, tail);
    }
    let tree_len = node_len(&tree);
    let m = n - hl;
    if m < tree_len {
        let cut = if m == 0 {
            tree.clone()
        } else {
            tree_drop(a, &tree, m)
        };
        let (new_tree, new_shift) = collapse(cut, shift);
        return root_in(a, len - n, new_shift, Value::nil(), new_tree, tail);
    }
    let k = m - tree_len;
    if k == 0 {
        return root_in(a, len - n, 0, Value::nil(), Value::nil(), tail);
    }
    let new_tail = leaf_from(a, &leaf_elems(&tail)[k..]);
    root_in(a, len - n, 0, Value::nil(), Value::nil(), new_tail)
}

/// Keep the first `m` (1 <= m <= node_len) elements of a tree node.
fn tree_take<A: Arena + ?Sized>(a: &mut A, node: &Value, m: usize) -> Value {
    match SeqNodeRef::of(node) {
        SeqNodeRef::Leaf(elems) => {
            if m >= elems.len() {
                node.clone()
            } else {
                leaf_from(a, &elems[..m])
            }
        }
        SeqNodeRef::Branch {
            shift,
            sizes,
            children,
        } => {
            if m >= sizes[sizes.len() - 1] as usize {
                return node.clone();
            }
            // Keeping the first `m` means the last kept element is index `m - 1`.
            let (k, before) = size_slot(sizes, m - 1);
            let child = tree_take(a, &children[k], m - before);
            let mut buf = Buf::new();
            buf.extend(&children[..k]);
            buf.push(child);
            branch_from(a, shift, &buf)
        }
    }
}

/// Drop the first `m` (0 <= m < node_len) elements of a tree node.
fn tree_drop<A: Arena + ?Sized>(a: &mut A, node: &Value, m: usize) -> Value {
    if m == 0 {
        return node.clone();
    }
    match SeqNodeRef::of(node) {
        SeqNodeRef::Leaf(elems) => leaf_from(a, &elems[m..]),
        SeqNodeRef::Branch {
            shift,
            sizes,
            children,
        } => {
            let (k, before) = size_slot(sizes, m);
            let child = tree_drop(a, &children[k], m - before);
            let mut buf = Buf::new();
            buf.push(child);
            buf.extend(&children[k + 1..]);
            branch_from(a, shift, &buf)
        }
    }
}

// ---- concatenation (RRB merge with rebalancing) -----------------------------------

/// Concatenate two arrays. O(log n): the RRB merge walks the right spine
/// of `l` and the left spine of `r`, rebalancing each level within the
/// `E_MAX` slack invariant so lookup depth stays logarithmic no matter how
/// many concatenations build a vector.
pub fn concat<A: Arena + ?Sized>(a: &mut A, l: &Value, r: &Value) -> Value {
    let (llen, lshift, lhead, ltree, ltail) = root_parts(l);
    let (rlen, rshift, rhead, rtree, rtail) = root_parts(r);
    if llen == 0 {
        return r.clone();
    }
    if rlen == 0 {
        return l.clone();
    }
    // Fold the boundary buffers into their trees so the merge sees two
    // pure trees: left keeps its head buffer, right keeps its tail buffer.
    let (ltree, lshift) = if ltail.is_nil() {
        (ltree, lshift)
    } else {
        tree_push_leaf::<A, false>(a, &ltree, lshift, &ltail)
    };
    let (rtree, rshift) = if rhead.is_nil() {
        (rtree, rshift)
    } else {
        tree_push_leaf::<A, true>(a, &rtree, rshift, &rhead)
    };
    let (tree, shift) = if ltree.is_nil() {
        (rtree, rshift)
    } else if rtree.is_nil() {
        (ltree, lshift)
    } else {
        let top = concat_sub(a, &ltree, lshift, &rtree, rshift);
        let h = lshift.max(rshift);
        if top.len() == 1 {
            collapse(top[0].clone(), h)
        } else {
            (branch_from(a, h + BITS, &top), h + BITS)
        }
    };
    root_in(a, llen + rlen, shift, lhead, tree, rtail)
}

/// Merge two trees, returning one or two nodes at height
/// `max(lshift, rshift)`.
fn concat_sub<A: Arena + ?Sized>(
    a: &mut A,
    l: &Value,
    lshift: usize,
    r: &Value,
    rshift: usize,
) -> Vec<Value> {
    if lshift > rshift {
        let (_, lc) = branch_parts(l);
        let mid = concat_sub(a, &lc[lc.len() - 1], lshift - BITS, r, rshift);
        rebalance(a, &lc[..lc.len() - 1], &mid, &[], lshift)
    } else if rshift > lshift {
        let (_, rc) = branch_parts(r);
        let mid = concat_sub(a, l, lshift, &rc[0], rshift - BITS);
        rebalance(a, &[], &mid, &rc[1..], rshift)
    } else if lshift == 0 {
        let (le, re) = (leaf_elems(l), leaf_elems(r));
        if le.len() + re.len() <= B {
            let mut buf = Buf::new();
            buf.extend(le);
            buf.extend(re);
            vec![leaf_from(a, &buf)]
        } else {
            vec![l.clone(), r.clone()]
        }
    } else {
        let (_, lc) = branch_parts(l);
        let (_, rc) = branch_parts(r);
        let mid = concat_sub(a, &lc[lc.len() - 1], lshift - BITS, &rc[0], rshift - BITS);
        rebalance(a, &lc[..lc.len() - 1], &mid, &rc[1..], lshift)
    }
}

/// Regroup up to 64 children (left siblings ++ merged middle ++ right
/// siblings, all at `shift - BITS`) into one or two nodes at `shift`,
/// redistributing slots when the packing is more than `E_MAX` nodes worse
/// than optimal (the RRB invariant that bounds tree depth).
fn rebalance<A: Arena + ?Sized>(
    a: &mut A,
    left: &[Value],
    mid: &[Value],
    right: &[Value],
    shift: usize,
) -> Vec<Value> {
    let mut all = Buf::new();
    all.extend(left);
    all.extend(mid);
    all.extend(right);
    debug_assert!(!all.is_empty() && all.len() <= 2 * B);

    let total: usize = all.iter().map(slot_count).sum();
    let optimal = total.div_ceil(B);
    if all.len() > optimal + E_MAX {
        let mut plan: Vec<usize> = all.iter().map(slot_count).collect();
        // Shrink the plan: find a sparse node (fewer than B - E_MAX/2
        // slots) and pour its contents into its successors, removing one
        // node per pass until the count meets the invariant. The threshold
        // guarantees a sparse node exists whenever the loop runs: if every
        // node held >= B - E_MAX/2 slots, then total >= len * (B - E_MAX/2),
        // so optimal = ceil(total / B) >= len - len * E_MAX / (2 * B), and
        // with len <= 2 * B that is >= len - E_MAX — contradicting
        // `plan.len() > optimal + E_MAX`. Skipping near-full nodes also
        // keeps the redistribution local instead of rewriting the whole run.
        while plan.len() > optimal + E_MAX {
            let mut i = 0;
            while i < plan.len() && plan[i] >= B - E_MAX / 2 {
                i += 1;
            }
            if i >= plan.len() {
                // Unreachable per the bound above (every node near-full
                // implies the count is already within optimal + E_MAX);
                // backstop so a violated invariant degrades to a slightly
                // overfull level rather than an infinite loop.
                break;
            }
            let mut r = plan[i];
            let mut j = i;
            while r > 0 && j + 1 < plan.len() {
                let merged = (r + plan[j + 1]).min(B);
                r = r + plan[j + 1] - merged;
                plan[j] = merged;
                j += 1;
            }
            debug_assert!(r == 0, "rebalance plan lost slots");
            plan.remove(j);
        }
        all = execute_plan(a, &all, &plan, shift - BITS);
    }

    if all.len() <= B {
        vec![branch_from(a, shift, &all)]
    } else {
        let (a1, a2) = all.split_at(B);
        vec![branch_from(a, shift, a1), branch_from(a, shift, a2)]
    }
}

/// Rebuild a run of sibling nodes (at `child_shift`) to the slot counts in
/// `plan`, streaming their slots in order. Nodes whose count already
/// matches the plan are reused as-is, preserving sharing; only the
/// reshaped ones are reallocated (their grandchildren are shared either
/// way).
fn execute_plan<A: Arena + ?Sized>(
    a: &mut A,
    old: &[Value],
    plan: &[usize],
    child_shift: usize,
) -> Buf {
    let mut out = Buf::new();
    let mut src = 0usize;
    let mut off = 0usize;
    for &want in plan {
        if off == 0 && slot_count(&old[src]) == want {
            out.push(old[src].clone());
            src += 1;
            continue;
        }
        let mut buf = Buf::new();
        while buf.len() < want {
            let items = slots(&old[src]);
            let take = (want - buf.len()).min(items.len() - off);
            buf.extend(&items[off..off + take]);
            off += take;
            if off == items.len() {
                src += 1;
                off = 0;
            }
        }
        out.push(if child_shift == 0 {
            leaf_from(a, &buf)
        } else {
            branch_from(a, child_shift, &buf)
        });
    }
    debug_assert!(
        src == old.len() && off == 0,
        "plan does not cover all slots"
    );
    out
}

// ---- iteration ----------------------------------------------------------------

/// Element iterator over an array, front to back: head buffer, in-order tree
/// walk, tail buffer. Holds raw pointers into the arena — see
/// [`SeqRef::iter`] for the (no collection while live) rooting caveat.
pub struct SeqIter {
    /// Root sections still to be walked, in element order.
    sections: [Value; 3],
    section: usize,
    /// Branch path to the current leaf: each entry is a branch and the index
    /// of its next unvisited child.
    stack: Vec<(Value, usize)>,
    /// The current leaf, owned so its elements stay alive while we yield them
    /// (nil when no leaf is active yet).
    cur: Value,
    pos: usize,
    remaining: usize,
}

impl SeqIter {
    pub(super) fn new(root: &Value) -> SeqIter {
        let (_, _, head, tree, tail) = root_parts(root);
        SeqIter {
            sections: [head, tree, tail],
            section: 0,
            stack: Vec::new(),
            cur: Value::nil(),
            pos: 0,
            remaining: len(root),
        }
    }

    /// Descend `node`'s leftmost spine and land on its first leaf.
    fn enter(&mut self, mut node: Value) {
        loop {
            let child0 = match SeqNodeRef::of(&node) {
                SeqNodeRef::Leaf(_) => {
                    self.cur = node;
                    self.pos = 0;
                    return;
                }
                SeqNodeRef::Branch { children, .. } => children[0].clone(),
            };
            self.stack.push((node, 1));
            node = child0;
        }
    }

    /// Step to the next leaf: resume the deepest unfinished branch, or open
    /// the next non-nil section. Returns false when the walk is exhausted.
    fn advance(&mut self) -> bool {
        while let Some((node, idx)) = self.stack.pop() {
            let child = {
                let children = branch_parts(&node).1;
                if idx < children.len() {
                    Some(children[idx].clone())
                } else {
                    None
                }
            };
            if let Some(child) = child {
                self.stack.push((node, idx + 1));
                self.enter(child);
                return true;
            }
        }
        while self.section < 3 {
            let v = std::mem::replace(&mut self.sections[self.section], Value::nil());
            self.section += 1;
            if !v.is_nil() {
                self.enter(v);
                return true;
            }
        }
        false
    }
}

impl Iterator for SeqIter {
    type Item = Value;

    fn next(&mut self) -> Option<Value> {
        loop {
            if !self.cur.is_nil() {
                let elems = leaf_elems(&self.cur);
                if self.pos < elems.len() {
                    let v = elems[self.pos].clone();
                    self.pos += 1;
                    self.remaining -= 1;
                    return Some(v);
                }
            }
            if !self.advance() {
                return None;
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl ExactSizeIterator for SeqIter {}

// ---- test support ---------------------------------------------------------------

/// Validate every structural invariant of an array; test builds only.
#[cfg(test)]
pub fn check_invariants(root: &Value) {
    let (len, shift, head, tree, tail) = root_parts(root);
    for buf in [&head, &tail] {
        if !buf.is_nil() {
            let n = leaf_elems(buf).len();
            assert!((1..=B).contains(&n), "buffer leaf count {n} out of range");
        }
    }
    let tree_len = if tree.is_nil() {
        0
    } else {
        check_node(&tree, shift)
    };
    assert_eq!(
        len,
        node_len(&head) + tree_len + node_len(&tail),
        "root len disagrees with sections"
    );
}

#[cfg(test)]
fn check_node(node: &Value, shift: usize) -> usize {
    if shift == 0 {
        assert!(node_is_leaf(node), "shift 0 must be a leaf");
        let n = leaf_elems(node).len();
        assert!((1..=B).contains(&n), "leaf count {n} out of range");
        return n;
    }
    assert!(
        node.is_tag(HeapTag::SeqBranch),
        "shift {shift} must be a branch"
    );
    let stored_shift = match SeqNodeRef::of(node) {
        SeqNodeRef::Branch { shift, .. } => shift,
        SeqNodeRef::Leaf(_) => unreachable!(),
    };
    assert_eq!(stored_shift, shift, "stored shift disagrees with position");
    let (sizes, children) = branch_parts(node);
    assert!(!children.is_empty() && children.len() <= B);
    let mut total = 0usize;
    for (i, c) in children.iter().enumerate() {
        total += check_node(c, shift - BITS);
        assert_eq!(sizes[i] as usize, total, "size table mismatch at {i}");
    }
    total
}
