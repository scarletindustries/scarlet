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
//! ## Persistence and the GC
//!
//! Nodes are ordinary arena objects traced by the collector; "mutation" is
//! path copying — an operation allocates replacements for the O(log32 n)
//! nodes on the root-to-target path and shares every untouched subtree with
//! the previous version. The copying GC preserves that sharing: a node
//! evacuated once is reused by every referrer via its forwarding pointer.
//!
//! ## Allocation discipline (the rooting rule)
//!
//! Every builder here allocates but **never collects**; callers reserve the
//! operation's worst-case need up front with `ensure(cost_*(...))` *before*
//! popping their operands (the `cost_*` functions at the bottom of this file
//! are the budget table). Costs may overshoot — slack is harmless — but a
//! debug-build watermark panics on any allocation past the reserved budget.
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

/// Worst-case arena words for a leaf / branch / root object.
const LEAF_WORDS: usize = 2 + B;
const BRANCH_WORDS: usize = 3 + 2 * B;
const ROOT_WORDS: usize = 6;

// ---- node accessors -------------------------------------------------------

#[inline]
fn expect_root(root: Value) -> *const u64 {
    debug_assert!(root.is_tag(HeapTag::Seq), "seq op on a non-array value");
    root.heap_obj()
}

/// `(len, shift, head, tree, tail)` of a root. `head`/`tree`/`tail` are
/// node values or nil; `shift` is the tree height (0 = leaf or nil).
#[inline]
fn root_parts(root: Value) -> (usize, usize, Value, Value, Value) {
    let obj = expect_root(root);
    // SAFETY: tag-checked Seq root has the 5-word layout.
    unsafe {
        (
            payload_word(obj, 0) as usize,
            payload_word(obj, 1) as usize,
            payload_value(obj, 2),
            payload_value(obj, 3),
            payload_value(obj, 4),
        )
    }
}

fn root_in<A: Arena + ?Sized>(
    a: &mut A,
    len: usize,
    shift: usize,
    head: Value,
    tree: Value,
    tail: Value,
) -> Value {
    let obj = alloc_obj(a, HeapTag::Seq, 5, false);
    // SAFETY: freshly allocated 5-word payload.
    unsafe {
        let p = obj.as_ptr().add(1);
        p.write(len as u64);
        p.add(1).write(shift as u64);
        p.add(2).write(head.to_bits());
        p.add(3).write(tree.to_bits());
        p.add(4).write(tail.to_bits());
    }
    Value::from_object_ptr(obj)
}

#[inline]
fn node_is_leaf(node: Value) -> bool {
    node.is_tag(HeapTag::SeqLeaf)
}

/// Elements of a leaf. Unbounded lifetime — callers use it within the
/// current (non-collecting) operation only.
#[inline]
fn leaf_elems<'x>(leaf: Value) -> &'x [Value] {
    debug_assert!(node_is_leaf(leaf));
    let obj = leaf.heap_obj();
    // SAFETY: tag-checked leaf; count word bounds the slice.
    unsafe {
        let n = payload_word(obj, 0) as usize;
        payload_values(obj, 1, n)
    }
}

/// `(sizes, children)` of a branch. Unbounded lifetimes as [`leaf_elems`].
#[inline]
fn branch_parts<'x>(branch: Value) -> (&'x [u64], &'x [Value]) {
    debug_assert!(branch.is_tag(HeapTag::SeqBranch));
    let obj = branch.heap_obj();
    // SAFETY: tag-checked branch; count word bounds both slices.
    unsafe {
        let n = payload_word(obj, 0) as usize;
        let sizes = std::slice::from_raw_parts(obj.add(3), n);
        (sizes, payload_values(obj, 2 + n, n))
    }
}

/// Total element count under a node (leaf count or last cumulative size).
#[inline]
fn node_len(node: Value) -> usize {
    if node.is_nil() {
        0
    } else if node_is_leaf(node) {
        leaf_elems(node).len()
    } else {
        let (sizes, _) = branch_parts(node);
        sizes[sizes.len() - 1] as usize
    }
}

/// Direct slot count of a node (elements of a leaf, children of a branch)
/// — the density measure the rebalance invariant is defined over.
#[inline]
fn slot_count(node: Value) -> usize {
    if node_is_leaf(node) {
        leaf_elems(node).len()
    } else {
        branch_parts(node).1.len()
    }
}

/// Direct slots of a node as values (elements or children).
#[inline]
fn slots<'x>(node: Value) -> &'x [Value] {
    if node_is_leaf(node) {
        leaf_elems(node)
    } else {
        branch_parts(node).1
    }
}

fn leaf_from<A: Arena + ?Sized>(a: &mut A, items: &[Value]) -> Value {
    debug_assert!(!items.is_empty() && items.len() <= B);
    let obj = alloc_obj(a, HeapTag::SeqLeaf, 1 + items.len(), false);
    // SAFETY: freshly allocated payload of exactly 1 + len words.
    unsafe {
        let p = obj.as_ptr().add(1);
        p.write(items.len() as u64);
        for (i, v) in items.iter().enumerate() {
            p.add(1 + i).write(v.to_bits());
        }
    }
    Value::from_object_ptr(obj)
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
/// `shift - BITS`), computing the cumulative size table.
fn branch_from<A: Arena + ?Sized>(a: &mut A, shift: usize, children: &[Value]) -> Value {
    debug_assert!(!children.is_empty() && children.len() <= B);
    debug_assert!(shift >= BITS);
    let n = children.len();
    let obj = alloc_obj(a, HeapTag::SeqBranch, 2 + 2 * n, false);
    // SAFETY: freshly allocated payload of exactly 2 + 2n words.
    unsafe {
        let p = obj.as_ptr().add(1);
        p.write(n as u64);
        p.add(1).write(shift as u64);
        let mut total = 0u64;
        for (i, c) in children.iter().enumerate() {
            debug_assert!(if shift == BITS {
                node_is_leaf(*c)
            } else {
                c.is_tag(HeapTag::SeqBranch)
            });
            total += node_len(*c) as u64;
            p.add(2 + i).write(total);
            p.add(2 + n + i).write(c.to_bits());
        }
    }
    Value::from_object_ptr(obj)
}

// ---- public queries ---------------------------------------------------------

#[inline]
pub fn len(root: Value) -> usize {
    root_parts(root).0
}

pub fn get(root: Value, i: usize) -> Option<Value> {
    let (len, _, head, tree, tail) = root_parts(root);
    if i >= len {
        return None;
    }
    let hl = node_len(head);
    if i < hl {
        return Some(leaf_elems(head)[i]);
    }
    let mut idx = i - hl;
    let tree_len = node_len(tree);
    if idx >= tree_len {
        return Some(leaf_elems(tail)[idx - tree_len]);
    }
    let mut node = tree;
    loop {
        if node_is_leaf(node) {
            return Some(leaf_elems(node)[idx]);
        }
        let (sizes, children) = branch_parts(node);
        // Size tables are cumulative: descend into the first child whose
        // cumulative size exceeds the index.
        let mut k = 0;
        while sizes[k] as usize <= idx {
            k += 1;
        }
        if k > 0 {
            idx -= sizes[k - 1] as usize;
        }
        node = children[k];
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
    root_in(a, n, shift, Value::nil(), nodes[0], tail)
}

// ---- push/pop at the ends -----------------------------------------------------

/// Append one element. Amortized O(1): the tail buffer absorbs pushes and
/// is pushed into the tree as a full leaf every 32nd call.
pub fn push_back<A: Arena + ?Sized>(a: &mut A, root: Value, x: Value) -> Value {
    push_end::<A, false>(a, root, x)
}

/// Prepend one element — the mirror of [`push_back`] via the head buffer.
pub fn push_front<A: Arena + ?Sized>(a: &mut A, root: Value, x: Value) -> Value {
    push_end::<A, true>(a, root, x)
}

/// Push one element into the head (`FRONT`) or tail buffer, spilling a full
/// buffer into the tree as a finished leaf.
fn push_end<A: Arena + ?Sized, const FRONT: bool>(a: &mut A, root: Value, x: Value) -> Value {
    let (len, mut shift, head, mut tree, tail) = root_parts(root);
    let old = if FRONT { head } else { tail };
    let new = if old.is_nil() {
        leaf_from(a, &[x])
    } else {
        let elems = leaf_elems(old);
        if elems.len() < B {
            let mut buf: Vec<Value> = Vec::with_capacity(elems.len() + 1);
            if FRONT {
                buf.push(x);
                buf.extend_from_slice(elems);
            } else {
                buf.extend_from_slice(elems);
                buf.push(x);
            }
            leaf_from(a, &buf)
        } else {
            (tree, shift) = tree_push_leaf::<A, FRONT>(a, tree, shift, old);
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
pub fn pop_front<A: Arena + ?Sized>(a: &mut A, root: Value) -> Option<(Value, Value)> {
    let (len, shift, head, tree, tail) = root_parts(root);
    if len == 0 {
        return None;
    }
    if !head.is_nil() {
        let elems = leaf_elems(head);
        let e = elems[0];
        let head = opt_leaf_from(a, &elems[1..]);
        return Some((e, root_in(a, len - 1, shift, head, tree, tail)));
    }
    if !tree.is_nil() {
        // Pull the leftmost leaf out of the tree to become the head.
        let (leaf, rest) = tree_pop_leftmost(a, tree, shift);
        let (tree, shift) = collapse(rest, shift);
        let elems = leaf_elems(leaf);
        let e = elems[0];
        let head = opt_leaf_from(a, &elems[1..]);
        return Some((e, root_in(a, len - 1, shift, head, tree, tail)));
    }
    let elems = leaf_elems(tail);
    let e = elems[0];
    let tail = opt_leaf_from(a, &elems[1..]);
    Some((e, root_in(a, len - 1, 0, Value::nil(), Value::nil(), tail)))
}

/// Replace element `i`, path-copying the spine. O(log32 n).
pub fn update<A: Arena + ?Sized>(a: &mut A, root: Value, i: usize, x: Value) -> Option<Value> {
    let (len, shift, head, tree, tail) = root_parts(root);
    if i >= len {
        return None;
    }
    let hl = node_len(head);
    if i < hl {
        let head = leaf_replace(a, head, i, x);
        return Some(root_in(a, len, shift, head, tree, tail));
    }
    let idx = i - hl;
    let tree_len = node_len(tree);
    if idx < tree_len {
        let tree = tree_update(a, tree, idx, x);
        return Some(root_in(a, len, shift, head, tree, tail));
    }
    let tail = leaf_replace(a, tail, idx - tree_len, x);
    Some(root_in(a, len, shift, head, tree, tail))
}

fn leaf_replace<A: Arena + ?Sized>(a: &mut A, leaf: Value, i: usize, x: Value) -> Value {
    let mut buf: Vec<Value> = leaf_elems(leaf).to_vec();
    buf[i] = x;
    leaf_from(a, &buf)
}

fn tree_update<A: Arena + ?Sized>(a: &mut A, node: Value, idx: usize, x: Value) -> Value {
    if node_is_leaf(node) {
        return leaf_replace(a, node, idx, x);
    }
    let (sizes, children) = branch_parts(node);
    let mut k = 0;
    while sizes[k] as usize <= idx {
        k += 1;
    }
    let before = if k > 0 { sizes[k - 1] as usize } else { 0 };
    let child = tree_update(a, children[k], idx - before, x);
    let mut buf: Vec<Value> = children.to_vec();
    buf[k] = child;
    let (_, shift) = node_shift(node);
    branch_from(a, shift, &buf)
}

#[inline]
fn node_shift(node: Value) -> (bool, usize) {
    if node_is_leaf(node) {
        (true, 0)
    } else {
        let obj = node.heap_obj();
        // SAFETY: tag-checked branch stores its shift at payload word 1.
        (false, unsafe { payload_word(obj, 1) as usize })
    }
}

/// Push a full leaf under the right (or left, when `FRONT`) edge of `tree`;
/// grows the root when that spine is full. Returns the new `(tree, shift)`.
fn tree_push_leaf<A: Arena + ?Sized, const FRONT: bool>(
    a: &mut A,
    tree: Value,
    shift: usize,
    leaf: Value,
) -> (Value, usize) {
    if tree.is_nil() {
        return (leaf, 0);
    }
    match try_push::<A, FRONT>(a, tree, shift, leaf) {
        Some(n) => (n, shift),
        None => {
            let spine = make_spine(a, leaf, shift);
            let pair = if FRONT { [spine, tree] } else { [tree, spine] };
            (branch_from(a, shift + BITS, &pair), shift + BITS)
        }
    }
}

/// A chain of single-child branches lifting `leaf` to height `shift`.
fn make_spine<A: Arena + ?Sized>(a: &mut A, leaf: Value, shift: usize) -> Value {
    let mut node = leaf;
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
    node: Value,
    shift: usize,
    leaf: Value,
) -> Option<Value> {
    if shift == 0 {
        return None;
    }
    let (_, children) = branch_parts(node);
    let edge = if FRONT { 0 } else { children.len() - 1 };
    if let Some(sub) = try_push::<A, FRONT>(a, children[edge], shift - BITS, leaf) {
        let mut buf: Vec<Value> = children.to_vec();
        buf[edge] = sub;
        return Some(branch_from(a, shift, &buf));
    }
    if children.len() < B {
        let spine = make_spine(a, leaf, shift - BITS);
        let mut buf: Vec<Value> = Vec::with_capacity(children.len() + 1);
        if FRONT {
            buf.push(spine);
            buf.extend_from_slice(children);
        } else {
            buf.extend_from_slice(children);
            buf.push(spine);
        }
        return Some(branch_from(a, shift, &buf));
    }
    None
}

/// Detach the leftmost leaf. Returns `(leaf, rest)` where `rest` is a node
/// at the same height as `node` or nil.
fn tree_pop_leftmost<A: Arena + ?Sized>(a: &mut A, node: Value, shift: usize) -> (Value, Value) {
    if shift == 0 {
        return (node, Value::nil());
    }
    let (_, children) = branch_parts(node);
    let (leaf, sub) = tree_pop_leftmost(a, children[0], shift - BITS);
    if sub.is_nil() && children.len() == 1 {
        return (leaf, Value::nil());
    }
    let mut buf: Vec<Value> = Vec::with_capacity(children.len());
    if !sub.is_nil() {
        buf.push(sub);
    }
    buf.extend_from_slice(&children[1..]);
    (leaf, branch_from(a, shift, &buf))
}

/// Drop redundant single-child levels off the top of a tree.
fn collapse(mut node: Value, mut shift: usize) -> (Value, usize) {
    if node.is_nil() {
        return (Value::nil(), 0);
    }
    while shift > 0 {
        let (_, children) = branch_parts(node);
        if children.len() != 1 {
            break;
        }
        node = children[0];
        shift -= BITS;
    }
    (node, shift)
}

// ---- slicing --------------------------------------------------------------------

/// The first `n` elements. O(log32 n) path copy.
pub fn take<A: Arena + ?Sized>(a: &mut A, root: Value, n: usize) -> Value {
    let (len, shift, head, tree, tail) = root_parts(root);
    if n == 0 {
        return empty_in(a);
    }
    if n >= len {
        return root;
    }
    let hl = node_len(head);
    if n <= hl {
        let tail = leaf_from(a, &leaf_elems(head)[..n]);
        return root_in(a, n, 0, Value::nil(), Value::nil(), tail);
    }
    let tree_len = node_len(tree);
    let m = n - hl;
    if m <= tree_len {
        let cut = tree_take(a, tree, m);
        let (tree, shift) = collapse(cut, shift);
        return root_in(a, n, shift, head, tree, Value::nil());
    }
    let tail = leaf_from(a, &leaf_elems(tail)[..m - tree_len]);
    root_in(a, n, shift, head, tree, tail)
}

/// All but the first `n` elements. O(log32 n) path copy.
pub fn skip<A: Arena + ?Sized>(a: &mut A, root: Value, n: usize) -> Value {
    let (len, shift, head, tree, tail) = root_parts(root);
    if n == 0 {
        return root;
    }
    if n >= len {
        return empty_in(a);
    }
    let hl = node_len(head);
    if n < hl {
        let head = leaf_from(a, &leaf_elems(head)[n..]);
        return root_in(a, len - n, shift, head, tree, tail);
    }
    let tree_len = node_len(tree);
    let m = n - hl;
    if m < tree_len {
        let cut = if m == 0 { tree } else { tree_drop(a, tree, m) };
        let (tree, shift) = collapse(cut, shift);
        return root_in(a, len - n, shift, Value::nil(), tree, tail);
    }
    let k = m - tree_len;
    if k == 0 {
        return root_in(a, len - n, 0, Value::nil(), Value::nil(), tail);
    }
    let tail = leaf_from(a, &leaf_elems(tail)[k..]);
    root_in(a, len - n, 0, Value::nil(), Value::nil(), tail)
}

/// Keep the first `m` (1 <= m <= node_len) elements of a tree node.
fn tree_take<A: Arena + ?Sized>(a: &mut A, node: Value, m: usize) -> Value {
    if m >= node_len(node) {
        return node;
    }
    if node_is_leaf(node) {
        return leaf_from(a, &leaf_elems(node)[..m]);
    }
    let (sizes, children) = branch_parts(node);
    let mut k = 0;
    while (sizes[k] as usize) < m {
        k += 1;
    }
    let before = if k > 0 { sizes[k - 1] as usize } else { 0 };
    let child = tree_take(a, children[k], m - before);
    let mut buf: Vec<Value> = children[..k].to_vec();
    buf.push(child);
    let (_, shift) = node_shift(node);
    branch_from(a, shift, &buf)
}

/// Drop the first `m` (0 <= m < node_len) elements of a tree node.
fn tree_drop<A: Arena + ?Sized>(a: &mut A, node: Value, m: usize) -> Value {
    if m == 0 {
        return node;
    }
    if node_is_leaf(node) {
        return leaf_from(a, &leaf_elems(node)[m..]);
    }
    let (sizes, children) = branch_parts(node);
    let mut k = 0;
    while sizes[k] as usize <= m {
        k += 1;
    }
    let before = if k > 0 { sizes[k - 1] as usize } else { 0 };
    let child = tree_drop(a, children[k], m - before);
    let mut buf: Vec<Value> = Vec::with_capacity(children.len() - k);
    buf.push(child);
    buf.extend_from_slice(&children[k + 1..]);
    let (_, shift) = node_shift(node);
    branch_from(a, shift, &buf)
}

// ---- concatenation (RRB merge with rebalancing) -----------------------------------

/// Concatenate two arrays. O(log n): the RRB merge walks the right spine
/// of `l` and the left spine of `r`, rebalancing each level within the
/// `E_MAX` slack invariant so lookup depth stays logarithmic no matter how
/// many concatenations build a vector.
pub fn concat<A: Arena + ?Sized>(a: &mut A, l: Value, r: Value) -> Value {
    let (llen, lshift, lhead, ltree, ltail) = root_parts(l);
    let (rlen, rshift, rhead, rtree, rtail) = root_parts(r);
    if llen == 0 {
        return r;
    }
    if rlen == 0 {
        return l;
    }
    // Fold the boundary buffers into their trees so the merge sees two
    // pure trees: left keeps its head buffer, right keeps its tail buffer.
    let (ltree, lshift) = if ltail.is_nil() {
        (ltree, lshift)
    } else {
        tree_push_leaf::<A, false>(a, ltree, lshift, ltail)
    };
    let (rtree, rshift) = if rhead.is_nil() {
        (rtree, rshift)
    } else {
        tree_push_leaf::<A, true>(a, rtree, rshift, rhead)
    };
    let (tree, shift) = if ltree.is_nil() {
        (rtree, rshift)
    } else if rtree.is_nil() {
        (ltree, lshift)
    } else {
        let top = concat_sub(a, ltree, lshift, rtree, rshift);
        let h = lshift.max(rshift);
        if top.len() == 1 {
            collapse(top[0], h)
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
    l: Value,
    lshift: usize,
    r: Value,
    rshift: usize,
) -> Vec<Value> {
    if lshift > rshift {
        let (_, lc) = branch_parts(l);
        let mid = concat_sub(a, lc[lc.len() - 1], lshift - BITS, r, rshift);
        rebalance(a, &lc[..lc.len() - 1], &mid, &[], lshift)
    } else if rshift > lshift {
        let (_, rc) = branch_parts(r);
        let mid = concat_sub(a, l, lshift, rc[0], rshift - BITS);
        rebalance(a, &[], &mid, &rc[1..], rshift)
    } else if lshift == 0 {
        let (le, re) = (leaf_elems(l), leaf_elems(r));
        if le.len() + re.len() <= B {
            let mut buf: Vec<Value> = Vec::with_capacity(le.len() + re.len());
            buf.extend_from_slice(le);
            buf.extend_from_slice(re);
            vec![leaf_from(a, &buf)]
        } else {
            vec![l, r]
        }
    } else {
        let (_, lc) = branch_parts(l);
        let (_, rc) = branch_parts(r);
        let mid = concat_sub(a, lc[lc.len() - 1], lshift - BITS, rc[0], rshift - BITS);
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
    let mut all: Vec<Value> = Vec::with_capacity(left.len() + mid.len() + right.len());
    all.extend_from_slice(left);
    all.extend_from_slice(mid);
    all.extend_from_slice(right);
    debug_assert!(!all.is_empty() && all.len() <= 2 * B);

    let total: usize = all.iter().map(|n| slot_count(*n)).sum();
    let optimal = total.div_ceil(B);
    if all.len() > optimal + E_MAX {
        let mut plan: Vec<usize> = all.iter().map(|n| slot_count(*n)).collect();
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
) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::with_capacity(plan.len());
    let mut src = 0usize;
    let mut off = 0usize;
    for &want in plan {
        if off == 0 && slot_count(old[src]) == want {
            out.push(old[src]);
            src += 1;
            continue;
        }
        let mut buf: Vec<Value> = Vec::with_capacity(want);
        while buf.len() < want {
            let items = slots(old[src]);
            let take = (want - buf.len()).min(items.len() - off);
            buf.extend_from_slice(&items[off..off + take]);
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

// ---- worst-case allocation bounds (for the VM's ensure discipline) ----------------

#[inline]
fn depth(root: Value) -> usize {
    root_parts(root).1 / BITS + 1
}

/// Worst-case words allocated by [`from_slice`] of `n` items.
pub fn cost_from_slice(n: usize) -> usize {
    let leaves = n.div_ceil(B).max(1);
    let mut nodes = leaves;
    let mut branch_words = 0;
    while nodes > 1 {
        nodes = nodes.div_ceil(B);
        branch_words += nodes * BRANCH_WORDS;
    }
    ROOT_WORDS + (leaves + 1) * LEAF_WORDS + branch_words
}

/// Worst-case words for one [`push_back`]/[`push_front`].
pub fn cost_push(root: Value) -> usize {
    ROOT_WORDS + 2 * LEAF_WORDS + (depth(root) + 2) * BRANCH_WORDS
}

/// Worst-case words for one [`pop_front`].
pub fn cost_pop(root: Value) -> usize {
    ROOT_WORDS + 2 * LEAF_WORDS + (depth(root) + 1) * BRANCH_WORDS
}

/// Worst-case words for one [`update`].
pub fn cost_update(root: Value) -> usize {
    ROOT_WORDS + LEAF_WORDS + depth(root) * BRANCH_WORDS
}

/// Worst-case words for one [`take`] or [`skip`].
pub fn cost_slice(root: Value) -> usize {
    ROOT_WORDS + 2 * LEAF_WORDS + (depth(root) + 1) * BRANCH_WORDS
}

/// Worst-case words for one [`concat`].
pub fn cost_concat(l: Value, r: Value) -> usize {
    let d = depth(l).max(depth(r));
    // Two boundary-buffer pushes + one rebalance per level (each may
    // rebuild up to 2B nodes at the child level plus 2 parents) + the
    // top-level grouping and root.
    2 * (ROOT_WORDS + 2 * LEAF_WORDS + (d + 2) * BRANCH_WORDS)
        + (d + 2) * (2 * B * BRANCH_WORDS + 2 * BRANCH_WORDS)
        + 2 * BRANCH_WORDS
        + ROOT_WORDS
}

// ---- iteration ----------------------------------------------------------------

/// Element iterator over an array, front to back: head buffer, in-order tree
/// walk, tail buffer. Holds raw pointers into the arena — see
/// [`SeqRef::iter`] for the (no collection while live) rooting caveat.
pub struct SeqIter<'a> {
    /// Root sections still to be walked, in element order.
    sections: [Value; 3],
    section: usize,
    /// Branch path to the current leaf: each entry is a branch and the index
    /// of its next unvisited child.
    stack: Vec<(Value, usize)>,
    cur: &'a [Value],
    pos: usize,
    remaining: usize,
}

impl<'a> SeqIter<'a> {
    pub(super) fn new(root: Value) -> SeqIter<'a> {
        let (_, _, head, tree, tail) = root_parts(root);
        SeqIter {
            sections: [head, tree, tail],
            section: 0,
            stack: Vec::new(),
            cur: &[],
            pos: 0,
            remaining: len(root),
        }
    }

    /// Descend `node`'s leftmost spine and land on its first leaf.
    fn enter(&mut self, mut node: Value) {
        while !node_is_leaf(node) {
            let children = branch_parts(node).1;
            self.stack.push((node, 1));
            node = children[0];
        }
        self.cur = leaf_elems(node);
        self.pos = 0;
    }

    /// Step to the next leaf: resume the deepest unfinished branch, or open
    /// the next non-nil section. Returns false when the walk is exhausted.
    fn advance(&mut self) -> bool {
        while let Some((node, idx)) = self.stack.pop() {
            let children = branch_parts(node).1;
            if idx < children.len() {
                self.stack.push((node, idx + 1));
                self.enter(children[idx]);
                return true;
            }
        }
        while self.section < 3 {
            let v = self.sections[self.section];
            self.section += 1;
            if !v.is_nil() {
                self.enter(v);
                return true;
            }
        }
        false
    }
}

impl Iterator for SeqIter<'_> {
    type Item = Value;

    fn next(&mut self) -> Option<Value> {
        loop {
            if self.pos < self.cur.len() {
                let v = self.cur[self.pos];
                self.pos += 1;
                self.remaining -= 1;
                return Some(v);
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

impl ExactSizeIterator for SeqIter<'_> {}

// ---- test support ---------------------------------------------------------------

/// Validate every structural invariant of an array; test builds only.
#[cfg(test)]
pub fn check_invariants(root: Value) {
    let (len, shift, head, tree, tail) = root_parts(root);
    for buf in [head, tail] {
        if !buf.is_nil() {
            let n = leaf_elems(buf).len();
            assert!((1..=B).contains(&n), "buffer leaf count {n} out of range");
        }
    }
    let tree_len = if tree.is_nil() {
        0
    } else {
        check_node(tree, shift)
    };
    assert_eq!(
        len,
        node_len(head) + tree_len + node_len(tail),
        "root len disagrees with sections"
    );
}

#[cfg(test)]
fn check_node(node: Value, shift: usize) -> usize {
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
    let (_, stored_shift) = node_shift(node);
    assert_eq!(stored_shift, shift, "stored shift disagrees with position");
    let (sizes, children) = branch_parts(node);
    assert!(!children.is_empty() && children.len() <= B);
    let mut total = 0usize;
    for (i, c) in children.iter().enumerate() {
        total += check_node(*c, shift - BITS);
        assert_eq!(sizes[i] as usize, total, "size table mismatch at {i}");
    }
    total
}
