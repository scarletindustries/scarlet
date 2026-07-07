//! The persistent hash array mapped trie behind the in-memory `Map` backing.
//!
//! ## Shape
//!
//! A `Map` (Hamt backing) is a thin root `[backing, size, root]` over a tree of
//! three arena node kinds (built and read through the typed layer in
//! [`super::value`]):
//!
//! ```text
//!   HamtBranch [ bitmap | child… ]   one child per set bit; the bit chosen at
//!        |                           depth d is bits [5d, 5d+5) of the key hash
//!     ┌──┴───────────┐
//!   HamtEntry      HamtBranch …      a leaf key/value pair, or a deeper branch
//!   [ key | value ]
//!
//!   HamtCollision [ hash | count | key value … ]   ≥2 distinct keys, one hash
//! ```
//!
//! Keys are placed by [`hash_value`]; the trie consumes 5 hash bits per level
//! (32-way branches), so a lookup or update visits O(log₃₂ n) nodes. Two
//! distinct keys whose full 64-bit hashes are equal share a `HamtCollision`
//! bucket, compared with [`values_equal`] — the same `==` the language exposes.
//!
//! ## Persistence
//!
//! Like the `seq` vector, nodes are reference-counted arena objects; "mutation"
//! is *path copying* — [`insert`]/[`remove`] allocate replacements only for the
//! O(log₃₂ n) nodes on the root-to-leaf path and share every untouched subtree
//! with the previous version. A shared node carries one count per referrer, so
//! a previous version stays valid (`next = map.set(m, k, v)` leaves `m`
//! untouched) and a version's exclusive nodes are freed as it drops.

use super::value::{
    Arena, HamtMapRef, HamtNodeRef, MapRef, Value, hamt_branch_in, hamt_collision_in,
    hamt_entry_in, hamt_map_in, hash_value, values_equal,
};

/// Hash bits consumed per trie level (32-way branching).
const BITS: u32 = 5;
const MASK: u64 = 0x1F;
/// Levels before a 64-bit hash is fully consumed: ⌈64 / 5⌉. Past this depth two
/// surviving keys must have equal hashes and go to a `HamtCollision`.
const MAX_DEPTH: usize = 13;

/// The 5-bit slot a hash selects at `shift`.
#[inline]
fn slot(hash: u64, shift: u32) -> usize {
    ((hash >> shift) & MASK) as usize
}

/// The single-bit mask of `hash`'s slot at `shift`.
#[inline]
fn bit(hash: u64, shift: u32) -> u32 {
    1u32 << slot(hash, shift)
}

/// Compact index of `bit` within a branch: popcount of the occupied slots below
/// it.
#[inline]
fn compact(bitmap: u32, bit: u32) -> usize {
    (bitmap & (bit - 1)).count_ones() as usize
}

// ---- construction & reads ----------------------------------------------------

/// The empty map.
pub fn empty<A: Arena + ?Sized>(a: &mut A) -> Value {
    hamt_map_in(a, 0, Value::nil())
}

/// Entry count.
pub fn size(map: &Value) -> usize {
    HamtMapRef::of(map).size
}

/// Look up `key` (whose [`hash_value`] is `hash`); `None` if absent.
pub fn get(map: &Value, key: &Value, hash: u64) -> Option<Value> {
    let root = HamtMapRef::of(map).root;
    if root.is_nil() {
        return None;
    }
    node_get(&root, key, hash, 0)
}

fn node_get(node: &Value, key: &Value, hash: u64, shift: u32) -> Option<Value> {
    match HamtNodeRef::of(node) {
        HamtNodeRef::Entry { key: k, value } => values_equal(&k, key).then_some(value),
        HamtNodeRef::Collision { pairs, .. } => {
            collision_find(pairs, key).map(|i| pairs[i + 1].clone())
        }
        HamtNodeRef::Branch { bitmap, children } => {
            let b = bit(hash, shift);
            if bitmap & b == 0 {
                None
            } else {
                node_get(&children[compact(bitmap, b)], key, hash, shift + BITS)
            }
        }
    }
}

/// Index of `key` within an interleaved collision `[k, v, …]`, or `None`.
fn collision_find(pairs: &[Value], key: &Value) -> Option<usize> {
    (0..pairs.len())
        .step_by(2)
        .find(|&i| values_equal(&pairs[i], key))
}

/// Visit every `(key, value)`. Used for iteration (`keys`/`values`/`to_list`)
/// and structural hashing/equality.
fn for_each_entry(node: &Value, f: &mut impl FnMut(Value, Value)) {
    if node.is_nil() {
        return;
    }
    match HamtNodeRef::of(node) {
        HamtNodeRef::Entry { key, value } => f(key, value),
        HamtNodeRef::Collision { pairs, .. } => {
            for i in (0..pairs.len()).step_by(2) {
                f(pairs[i].clone(), pairs[i + 1].clone());
            }
        }
        HamtNodeRef::Branch { children, .. } => {
            // `children` aliases the arena; this read never allocates, so the
            // borrow stays valid for the whole walk.
            for child in children {
                for_each_entry(child, f);
            }
        }
    }
}

/// Collect every `(key, value)` into host memory. The returned `Value`s are
/// owned (counted) references the caller takes ownership of.
pub fn collect_entries(map: &Value) -> Vec<(Value, Value)> {
    let m = HamtMapRef::of(map);
    let mut out = Vec::with_capacity(m.size);
    for_each_entry(&m.root, &mut |k, v| out.push((k, v)));
    out
}

// ---- insert ------------------------------------------------------------------

/// `map` with `key` bound to `value` — a new map sharing all untouched
/// subtrees. Overwrites an existing binding (size unchanged) or adds a new one.
pub fn insert<A: Arena + ?Sized>(
    a: &mut A,
    map: &Value,
    key: Value,
    value: Value,
    hash: u64,
) -> Value {
    let m = HamtMapRef::of(map);
    let (root, added) = if m.root.is_nil() {
        (hamt_entry_in(a, key, value), true)
    } else {
        node_insert(a, &m.root, key, value, hash, 0)
    };
    hamt_map_in(a, m.size + usize::from(added), root)
}

/// Returns the rebuilt node and whether a *new* key was added (vs. overwritten).
fn node_insert<A: Arena + ?Sized>(
    a: &mut A,
    node: &Value,
    key: Value,
    value: Value,
    hash: u64,
    shift: u32,
) -> (Value, bool) {
    match HamtNodeRef::of(node) {
        HamtNodeRef::Entry { key: ek, value: _ } => {
            if values_equal(&ek, &key) {
                (hamt_entry_in(a, key, value), false)
            } else {
                // Two distinct keys at this slot: grow a subtree that separates
                // them by their differing hash bits (or a collision if equal).
                (
                    split(a, node, hash_value(&ek), key, value, hash, shift),
                    true,
                )
            }
        }
        HamtNodeRef::Collision { hash: chash, pairs } => {
            if chash == hash {
                match collision_find(pairs, &key) {
                    Some(i) => {
                        let mut np = pairs.to_vec();
                        np[i + 1] = value;
                        (hamt_collision_in(a, chash, &np), false)
                    }
                    None => {
                        let mut np = pairs.to_vec();
                        np.push(key);
                        np.push(value);
                        (hamt_collision_in(a, chash, &np), true)
                    }
                }
            } else {
                // The new key shares this branch path with the bucket but has a
                // different hash: separate them deeper.
                (split(a, node, chash, key, value, hash, shift), true)
            }
        }
        HamtNodeRef::Branch { bitmap, children } => {
            let b = bit(hash, shift);
            let i = compact(bitmap, b);
            if bitmap & b == 0 {
                let mut nc = children.to_vec();
                nc.insert(i, hamt_entry_in(a, key, value));
                (hamt_branch_in(a, bitmap | b, &nc), true)
            } else {
                let (child, added) = node_insert(a, &children[i], key, value, hash, shift + BITS);
                let mut nc = children.to_vec();
                nc[i] = child;
                (hamt_branch_in(a, bitmap, &nc), added)
            }
        }
    }
}

/// Build a subtree at `shift` holding the existing leaf node `left` (hash
/// `lhash`) and a fresh entry `(rkey, rvalue)` (hash `rhash`). If the hashes
/// are fully equal, `left` is an entry and the two merge into a collision.
fn split<A: Arena + ?Sized>(
    a: &mut A,
    left: &Value,
    lhash: u64,
    rkey: Value,
    rvalue: Value,
    rhash: u64,
    shift: u32,
) -> Value {
    if shift as usize >= MAX_DEPTH * BITS as usize {
        // Hashes are fully consumed and equal: a collision bucket. `left` is an
        // entry here (a collision would have absorbed the new key upstream
        // where its hash matched).
        return merge_into_collision(a, left, lhash, rkey, rvalue);
    }
    let li = slot(lhash, shift);
    let ri = slot(rhash, shift);
    if li == ri {
        let child = split(a, left, lhash, rkey, rvalue, rhash, shift + BITS);
        hamt_branch_in(a, 1u32 << li, &[child])
    } else {
        let right = hamt_entry_in(a, rkey, rvalue);
        if li < ri {
            hamt_branch_in(a, (1 << li) | (1 << ri), &[left.clone(), right])
        } else {
            hamt_branch_in(a, (1 << li) | (1 << ri), &[right, left.clone()])
        }
    }
}

/// Merge an entry `left` and a fresh `(rkey, rvalue)` that share a 64-bit hash
/// into a `HamtCollision`.
fn merge_into_collision<A: Arena + ?Sized>(
    a: &mut A,
    left: &Value,
    hash: u64,
    rkey: Value,
    rvalue: Value,
) -> Value {
    let (lk, lv) = match HamtNodeRef::of(left) {
        HamtNodeRef::Entry { key, value } => (key, value),
        // Only entries reach the full depth as `left`: a collision absorbs new
        // keys at its own level via the `chash == hash` arm above.
        _ => unreachable_collision(),
    };
    hamt_collision_in(a, hash, &[lk, lv, rkey, rvalue])
}

fn unreachable_collision() -> ! {
    unreachable!("hamt split reached full depth with a non-entry node")
}

// ---- remove ------------------------------------------------------------------

/// The outcome of removing a key from a subtree.
enum Removed {
    /// Key absent; the subtree is unchanged.
    Absent,
    /// Key removed and the subtree is now empty.
    Empty,
    /// Key removed; here is the rebuilt subtree.
    Node(Value),
}

/// `map` without `key`. Returns `map` unchanged (same value) if the key is
/// absent, so the caller can skip rebuilding the root.
pub fn remove<A: Arena + ?Sized>(a: &mut A, map: &Value, key: &Value, hash: u64) -> Value {
    let m = HamtMapRef::of(map);
    if m.root.is_nil() {
        return map.clone();
    }
    match node_remove(a, &m.root, key, hash, 0) {
        Removed::Absent => map.clone(),
        Removed::Empty => hamt_map_in(a, m.size - 1, Value::nil()),
        Removed::Node(root) => hamt_map_in(a, m.size - 1, root),
    }
}

fn node_remove<A: Arena + ?Sized>(
    a: &mut A,
    node: &Value,
    key: &Value,
    hash: u64,
    shift: u32,
) -> Removed {
    match HamtNodeRef::of(node) {
        HamtNodeRef::Entry { key: k, .. } => {
            if values_equal(&k, key) {
                Removed::Empty
            } else {
                Removed::Absent
            }
        }
        HamtNodeRef::Collision { hash: chash, pairs } => match collision_find(pairs, key) {
            None => Removed::Absent,
            Some(i) => {
                let mut np = pairs.to_vec();
                np.drain(i..i + 2);
                // Two keys collided; one left collapses the bucket to an entry.
                if np.len() == 2 {
                    let k = np.remove(0);
                    let v = np.remove(0);
                    Removed::Node(hamt_entry_in(a, k, v))
                } else {
                    Removed::Node(hamt_collision_in(a, chash, &np))
                }
            }
        },
        HamtNodeRef::Branch { bitmap, children } => {
            let b = bit(hash, shift);
            if bitmap & b == 0 {
                return Removed::Absent;
            }
            let i = compact(bitmap, b);
            match node_remove(a, &children[i], key, hash, shift + BITS) {
                Removed::Absent => Removed::Absent,
                Removed::Node(child) => {
                    // A single-child branch whose sole child collapsed to a
                    // leaf collapses too, so the chain of one-child branches
                    // built by `split` unwinds all the way up on remove.
                    if children.len() == 1 && is_leaf(&child) {
                        return Removed::Node(child);
                    }
                    let mut nc = children.to_vec();
                    nc[i] = child;
                    Removed::Node(hamt_branch_in(a, bitmap, &nc))
                }
                Removed::Empty => {
                    let nbitmap = bitmap & !b;
                    if nbitmap == 0 {
                        return Removed::Empty;
                    }
                    let mut nc = children.to_vec();
                    nc.remove(i);
                    // Collapse a branch that now holds a single leaf so equal
                    // maps keep a canonical shape and depth stays minimal.
                    if nc.len() == 1 && is_leaf(&nc[0]) {
                        Removed::Node(nc.remove(0))
                    } else {
                        Removed::Node(hamt_branch_in(a, nbitmap, &nc))
                    }
                }
            }
        }
    }
}

/// Whether `node` is a leaf (entry or collision) rather than a branch.
fn is_leaf(node: &Value) -> bool {
    !matches!(HamtNodeRef::of(node), HamtNodeRef::Branch { .. })
}

// ---- structural hash & equality (called from `super::value`) -----------------

/// Order-independent hash of a HAMT's entries. The caller has already
/// dispatched on `MapBacking::Hamt`; folded into [`super::value::hash_value`].
pub fn hamt_hash(m: MapRef<'_>) -> u64 {
    let mut acc = 0u64;
    let root = m.as_hamt().root;
    for_each_entry(&root, &mut |k, v| {
        // Per-entry hash combines key and value; the cross-entry fold is
        // commutative so insertion order does not matter.
        acc = acc.wrapping_add(hash_value(&k).wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ hash_value(&v));
    });
    acc
}

/// Structural equality of two HAMT-backed maps. The caller has already
/// dispatched on `MapBacking::Hamt` for both sides.
pub fn hamts_equal(a: MapRef<'_>, b: MapRef<'_>) -> bool {
    let (a, b) = (a.as_hamt(), b.as_hamt());
    if a.size != b.size {
        return false;
    }
    let mut equal = true;
    for_each_entry(&a.root, &mut |k, v| {
        if equal {
            equal = match node_get_root(&b.root, &k, hash_value(&k)) {
                Some(bv) => values_equal(&v, &bv),
                None => false,
            };
        }
    });
    equal
}

/// Lookup against a (possibly empty) root node.
fn node_get_root(root: &Value, key: &Value, hash: u64) -> Option<Value> {
    if root.is_nil() {
        None
    } else {
        node_get(root, key, hash, 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::heap::ProcHeap;

    /// A test heap for the HAMT unit tests.
    fn heap() -> ProcHeap {
        ProcHeap::new()
    }

    fn key(i: i64) -> Value {
        Value::small_int(i)
    }

    fn set(h: &mut ProcHeap, m: &Value, k: i64, v: i64) -> Value {
        let kv = key(k);
        let hash = hash_value(&kv);
        insert(h, m, kv, key(v), hash)
    }

    fn lookup(m: &Value, k: i64) -> Option<i64> {
        get(m, &key(k), hash_value(&key(k))).and_then(|v| v.as_int())
    }

    #[test]
    fn empty_has_no_entries() {
        let mut h = heap();
        let m = empty(&mut h);
        assert_eq!(size(&m), 0);
        assert_eq!(lookup(&m, 1), None);
    }

    #[test]
    fn insert_get_and_overwrite() {
        let mut h = heap();
        let m = empty(&mut h);
        let m = set(&mut h, &m, 1, 10);
        let m = set(&mut h, &m, 2, 20);
        assert_eq!(size(&m), 2);
        assert_eq!(lookup(&m, 1), Some(10));
        assert_eq!(lookup(&m, 2), Some(20));
        assert_eq!(lookup(&m, 3), None);

        // Overwrite does not grow the map.
        let m = set(&mut h, &m, 1, 99);
        assert_eq!(size(&m), 2);
        assert_eq!(lookup(&m, 1), Some(99));
    }

    #[test]
    fn updates_are_persistent() {
        let mut h = heap();
        let m0 = empty(&mut h);
        let base = set(&mut h, &m0, 1, 10);
        let derived = set(&mut h, &base, 2, 20);
        // The earlier version is untouched by the later insert.
        assert_eq!(size(&base), 1);
        assert_eq!(lookup(&base, 2), None);
        assert_eq!(size(&derived), 2);
        assert_eq!(lookup(&derived, 2), Some(20));
    }

    #[test]
    fn many_keys_round_trip() {
        let mut h = heap();
        let mut m = empty(&mut h);
        for i in 0..2000 {
            m = set(&mut h, &m, i, i * 3);
        }
        assert_eq!(size(&m), 2000);
        for i in 0..2000 {
            assert_eq!(lookup(&m, i), Some(i * 3), "key {i}");
        }
        assert_eq!(lookup(&m, 2000), None);
    }

    #[test]
    fn remove_present_and_absent() {
        let mut h = heap();
        let mut m = empty(&mut h);
        for i in 0..100 {
            m = set(&mut h, &m, i, i);
        }
        // Removing an absent key returns the same map value unchanged.
        let same = remove(&mut h, &m, &key(1000), hash_value(&key(1000)));
        assert_eq!(size(&same), 100);

        // Remove the even keys; odds survive, evens are gone, size halves.
        let mut r = m.clone();
        for i in (0..100).step_by(2) {
            r = remove(&mut h, &r, &key(i), hash_value(&key(i)));
        }
        assert_eq!(size(&r), 50);
        for i in 0..100 {
            assert_eq!(lookup(&r, i), if i % 2 == 0 { None } else { Some(i) });
        }
        // The pre-removal version still has everything (persistence).
        assert_eq!(size(&m), 100);
        assert_eq!(lookup(&m, 0), Some(0));
    }

    #[test]
    fn remove_collapses_single_child_branch_chain() {
        let mut h = heap();
        // Two keys whose hashes share the level-0 slot, so `split` builds at
        // least one single-child branch on the path between them.
        let a = 0i64;
        let sa = slot(hash_value(&key(a)), 0);
        let b = (1..).find(|&i| slot(hash_value(&key(i)), 0) == sa).unwrap();

        let m0 = empty(&mut h);
        let m_a = set(&mut h, &m0, a, 1);
        let m_ab = set(&mut h, &m_a, b, 2);
        let m_back = remove(&mut h, &m_ab, &key(b), hash_value(&key(b)));

        // After removing `b` the trie must collapse to the same shape as a
        // fresh single insert of `a`: a leaf at the root, no branch chain.
        assert!(is_leaf(&HamtMapRef::of(&m_back).root));
        assert_eq!(size(&m_back), 1);
        assert_eq!(lookup(&m_back, a), Some(1));
    }

    #[test]
    fn remove_to_empty() {
        let mut h = heap();
        let m0 = empty(&mut h);
        let m = set(&mut h, &m0, 42, 1);
        let m = remove(&mut h, &m, &key(42), hash_value(&key(42)));
        assert_eq!(size(&m), 0);
        assert_eq!(lookup(&m, 42), None);
    }

    #[test]
    fn structural_equality_is_order_independent() {
        let mut h = heap();
        let e1 = empty(&mut h);
        let a1 = set(&mut h, &e1, 1, 10);
        let a = set(&mut h, &a1, 2, 20);
        let e2 = empty(&mut h);
        let b1 = set(&mut h, &e2, 2, 20);
        let b = set(&mut h, &b1, 1, 10);
        assert!(values_equal(&a, &b));
        assert_eq!(hash_value(&a), hash_value(&b));

        // A differing value breaks equality.
        let c = set(&mut h, &a, 1, 11);
        assert!(!values_equal(&a, &c));
    }

    // NOTE: the `HamtCollision` path (two distinct keys whose full 64-bit
    // `hash_value` are equal) is not unit-tested here: `insert`/`get` require
    // the passed `hash` to be `hash_value(key)`, and no deterministic pair of
    // small-int keys collides, so a faithful collision cannot be constructed
    // through the public API. The path is short and exercised only by genuine
    // 64-bit hash collisions in the field.
}
