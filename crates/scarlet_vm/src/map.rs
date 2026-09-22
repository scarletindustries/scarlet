//! Maps: a persistent hash array mapped trie in the process's heap, in the
//! CHAMP form (Steindorfer & Vinju, 2015), which Scala's and Kotlin's
//! immutable maps use.
//!
//! ```text
//!   map [ size | top ]
//!                 |
//!   node [ which slots hold an entry | which a child ]  32 slots
//!        [ key value  key value ... ][ child child ... ]
//!                                       |
//!                     node ...          the next 5 bits of the hash
//!                                       pick the slot, one level down
//!
//!   collision [ hash | key value  key value ... ]
//! ```
//!
//! A key's hash (`hash.rs`) picks its slot: its lowest 5 bits at the top
//! node, the next 5 one level down, and so on. An entry sits in the first
//! node where no other key shares its slot, so a map of a million entries is
//! about 4 levels deep. Keys whose whole 64-bit hashes are equal, once the
//! bits run out, share a collision cell, a plain list.
//!
//! Nothing is changed in place. `set` and `delete` build new nodes along the
//! one path to the key and share every other node with the map they came
//! from, which stays as it was. A node shared this way holds one more
//! reference.
//!
//! What CHAMP adds to the older trie is that its shape depends only on what
//! the map holds, never on the order it was built in: a delete that leaves a
//! node one entry moves that entry up into the node above, where an insert
//! would have put it. So two equal maps have the same nodes, and `==`, the
//! hash and the order entries come out in all follow from that.
//!
//! Layout, in words after each cell's header:
//!
//! - map: `size`, then the top node, or `Nil` when empty;
//! - node: the two 32-bit slot masks in one word, the entry mask low, then
//!   each entry's key and value, then each child, both in slot order;
//! - collision: the keys' shared hash, then each entry's key and value.

use crate::eq;
use crate::hash;
use crate::heap::{Cell, Full, Heap, Kind};
use crate::value::Value;

/// What places a key. Always [`hash::hash`], but for tests, which pass a
/// poor one to reach the paths a good one almost never takes.
type Hasher = fn(&Heap, Value) -> u64;

/// Bits of the hash each level takes: `log2` of the 32 slots.
const BITS: u32 = 5;
/// The first shift with none of a 64-bit hash left.
const END: u32 = 64;

/// A new empty map.
pub(crate) fn empty(heap: &mut Heap) -> Result<Cell, Full> {
    heap.make(Kind::Map, &[0, Value::NIL.bits()])
}

/// How many entries `map` holds.
pub(crate) fn size(heap: &Heap, map: Cell) -> u64 {
    heap.data(map).first().copied().unwrap_or(0)
}

/// The top node, or `Nil` when the map is empty.
fn top(heap: &Heap, map: Cell) -> Value {
    word(heap.data(map), 1)
}

fn word(d: &[u64], i: usize) -> Value {
    Value::from_bits(d.get(i).copied().unwrap_or(Value::NIL.bits()))
}

/// The slot `hash` takes at `shift`.
fn slot(hash: u64, shift: u32) -> u32 {
    (hash.checked_shr(shift).unwrap_or(0) & 0x1F) as u32
}

fn bit(hash: u64, shift: u32) -> u32 {
    1 << slot(hash, shift)
}

/// Where the one set bit `bit` of `mask` goes among the ones `mask` sets.
fn index(mask: u32, bit: u32) -> usize {
    (mask & (bit - 1)).count_ones() as usize
}

/// A node's entries and children, read out, each in slot order. Reading them
/// adds no reference.
struct Node {
    entries: Vec<(Value, Value)>,
    children: Vec<Value>,
}

fn node(heap: &Heap, cell: Cell) -> Node {
    let d = heap.data(cell);
    let used = Used::of(d);
    let n = used.entries.count_ones() as usize;
    Node {
        entries: (0..n)
            .map(|i| (word(d, 1 + 2 * i), word(d, 2 + 2 * i)))
            .collect(),
        children: (0..used.children.count_ones() as usize)
            .map(|i| word(d, 1 + 2 * n + i))
            .collect(),
    }
}

/// A collision cell's hash and entries. Reading them adds no reference.
fn collision(heap: &Heap, cell: Cell) -> (u64, Vec<(Value, Value)>) {
    let d = heap.data(cell);
    let n = d.len().saturating_sub(1) / 2;
    let entries = (0..n)
        .map(|i| (word(d, 1 + 2 * i), word(d, 2 + 2 * i)))
        .collect();
    (d.first().copied().unwrap_or(0), entries)
}

fn is_collision(heap: &Heap, cell: Cell) -> bool {
    heap.kind(cell) == Some(Kind::MapCollision)
}

/// A new node. The references in `entries` and `children` pass to it.
fn make_node(
    heap: &mut Heap,
    used: Used,
    entries: &[(Value, Value)],
    children: &[Value],
) -> Result<Value, Full> {
    let mut words = Vec::with_capacity(1 + 2 * entries.len() + children.len());
    words.push(used.word());
    for (k, v) in entries {
        words.extend([k.bits(), v.bits()]);
    }
    words.extend(children.iter().map(|c| c.bits()));
    Ok(Value::cell(heap.make(Kind::MapNode, &words)?))
}

/// A new collision cell. The references in `entries` pass to it.
fn make_collision(heap: &mut Heap, hash: u64, entries: &[(Value, Value)]) -> Result<Value, Full> {
    let mut words = Vec::with_capacity(1 + 2 * entries.len());
    words.push(hash);
    for (k, v) in entries {
        words.extend([k.bits(), v.bits()]);
    }
    Ok(Value::cell(heap.make(Kind::MapCollision, &words)?))
}

/// `v`, with one more reference: for keeping it in a new cell.
fn own(heap: &mut Heap, v: Value) -> Value {
    if let Some(cell) = v.as_cell() {
        heap.share(cell);
    }
    v
}

fn release(heap: &mut Heap, w: u64) {
    if let Some(cell) = Value::from_bits(w).as_cell() {
        heap.release(cell);
    }
}

/// The words of node or collision `at`, for a new cell to start from: each
/// value in them has one more reference, which the copy holds. An edit that
/// takes a word out gives its reference back.
fn copy(heap: &mut Heap, at: Cell) -> Vec<u64> {
    let words = heap.data(at).to_vec();
    for &w in words.iter().skip(1) {
        own(heap, Value::from_bits(w));
    }
    words
}

/// Take words `range` out of `words`, giving back their references.
fn cut(heap: &mut Heap, words: &mut Vec<u64>, range: std::ops::Range<usize>) {
    let end = range.end.min(words.len());
    for w in words.drain(range.start.min(end)..end) {
        release(heap, w);
    }
}

/// Put `new` into `words` before word `at`, whose references pass in.
fn place(words: &mut Vec<u64>, at: usize, new: &[u64]) {
    let at = at.min(words.len());
    words.splice(at..at, new.iter().copied());
}

/// Say which slots `words` now uses.
fn set_used(words: &mut [u64], used: Used) {
    if let Some(w) = words.first_mut() {
        *w = used.word();
    }
}

/// Put `w` at `i` in `words`, giving back the reference the word there held.
fn put(heap: &mut Heap, words: &mut [u64], i: usize, w: u64) {
    if let Some(slot) = words.get_mut(i) {
        let old = std::mem::replace(slot, w);
        release(heap, old);
    }
}

/// Which of a node's 32 slots hold an entry and which a child, a bit a slot.
/// A slot holds one, the other or neither.
#[derive(Clone, Copy)]
struct Used {
    entries: u32,
    children: u32,
}

impl Used {
    /// The slots node words `d` use.
    fn of(d: &[u64]) -> Used {
        let m = d.first().copied().unwrap_or(0);
        Used {
            entries: m as u32,
            children: (m >> 32) as u32,
        }
    }

    /// As a node's first word.
    fn word(self) -> u64 {
        u64::from(self.entries) | u64::from(self.children) << 32
    }
}

/// The value `key` is bound to in `map`, or `None`. Reading it adds no
/// reference.
pub(crate) fn get(heap: &Heap, map: Cell, key: Value) -> Option<Value> {
    get_by(heap, map, key, hash::hash)
}

fn get_by(heap: &Heap, map: Cell, key: Value, hash: Hasher) -> Option<Value> {
    let mut at = top(heap, map).as_cell()?;
    let h = hash(heap, key);
    let mut shift = 0;
    loop {
        let d = heap.data(at);
        if is_collision(heap, at) {
            return (0..d.len() / 2)
                .find(|&i| eq::equal(heap, word(d, 1 + 2 * i), key))
                .map(|i| word(d, 2 + 2 * i));
        }
        let used = Used::of(d);
        let b = bit(h, shift);
        if used.entries & b != 0 {
            let i = index(used.entries, b);
            return eq::equal(heap, word(d, 1 + 2 * i), key).then(|| word(d, 2 + 2 * i));
        }
        if used.children & b == 0 {
            return None;
        }
        let n = used.entries.count_ones() as usize;
        at = word(d, 1 + 2 * n + index(used.children, b)).as_cell()?;
        shift += BITS;
    }
}

/// `map` with `key` bound to `value`. All three are borrowed: the new map
/// holds references of its own, and shares every node off the path to `key`
/// with `map`.
pub(crate) fn set(heap: &mut Heap, map: Cell, key: Value, value: Value) -> Result<Cell, Full> {
    set_by(heap, map, key, value, hash::hash)
}

fn set_by(
    heap: &mut Heap,
    map: Cell,
    key: Value,
    value: Value,
    hash: Hasher,
) -> Result<Cell, Full> {
    let h = hash(heap, key);
    let (top, added) = match top(heap, map).as_cell() {
        Some(t) => insert(heap, t, (key, value, h), 0, hash)?,
        None => {
            let entry = (own(heap, key), own(heap, value));
            let used = Used {
                entries: bit(h, 0),
                children: 0,
            };
            (make_node(heap, used, &[entry], &[])?, true)
        }
    };
    let size = size(heap, map) + u64::from(added);
    heap.make(Kind::Map, &[size, top.bits()])
}

/// The node `at`, at `shift`, with `key` bound to `value`, and whether `key`
/// is new. `h` is `key`'s hash. Everything passed is borrowed; the node made
/// is owned.
fn insert(
    heap: &mut Heap,
    at: Cell,
    (key, value, h): (Value, Value, u64),
    shift: u32,
    hash: Hasher,
) -> Result<(Value, bool), Full> {
    let d = heap.data(at);
    if is_collision(heap, at) {
        let found = (0..d.len() / 2).find(|&i| eq::equal(heap, word(d, 1 + 2 * i), key));
        let mut words = copy(heap, at);
        let v = own(heap, value).bits();
        match found {
            Some(i) => put(heap, &mut words, 2 + 2 * i, v),
            None => words.extend([own(heap, key).bits(), v]),
        }
        let made = heap.make(Kind::MapCollision, &words)?;
        return Ok((Value::cell(made), found.is_none()));
    }
    let used = Used::of(d);
    let n = used.entries.count_ones() as usize;
    let b = bit(h, shift);
    if used.entries & b != 0 {
        let i = index(used.entries, b);
        let (k, v) = (word(d, 1 + 2 * i), word(d, 2 + 2 * i));
        if eq::equal(heap, k, key) {
            let mut words = copy(heap, at);
            let v = own(heap, value).bits();
            put(heap, &mut words, 2 + 2 * i, v);
            return Ok((Value::cell(heap.make(Kind::MapNode, &words)?), false));
        }
        // Two keys in one slot: both go one level down, into a node of
        // their own, and the slot holds that instead.
        let theirs = (own(heap, k), own(heap, v), hash(heap, k));
        let ours = (own(heap, key), own(heap, value), h);
        let child = pair(heap, theirs, ours, shift + BITS)?;
        let mut words = copy(heap, at);
        cut(heap, &mut words, 1 + 2 * i..3 + 2 * i);
        place(
            &mut words,
            1 + 2 * (n - 1) + index(used.children, b),
            &[child.bits()],
        );
        set_used(
            &mut words,
            Used {
                entries: used.entries & !b,
                children: used.children | b,
            },
        );
        return Ok((Value::cell(heap.make(Kind::MapNode, &words)?), true));
    }
    if used.children & b != 0 {
        let j = 1 + 2 * n + index(used.children, b);
        let Some(child) = word(d, j).as_cell() else {
            return Ok((Value::NIL, false));
        };
        let (child, added) = insert(heap, child, (key, value, h), shift + BITS, hash)?;
        let mut words = copy(heap, at);
        put(heap, &mut words, j, child.bits());
        return Ok((Value::cell(heap.make(Kind::MapNode, &words)?), added));
    }
    let i = 1 + 2 * index(used.entries, b);
    let mut words = copy(heap, at);
    let entry = [own(heap, key).bits(), own(heap, value).bits()];
    place(&mut words, i, &entry);
    set_used(
        &mut words,
        Used {
            entries: used.entries | b,
            ..used
        },
    );
    Ok((Value::cell(heap.make(Kind::MapNode, &words)?), true))
}

/// A node holding two entries whose keys differ and whose hashes agree below
/// `shift`, each as `(key, value, hash)`. Their references pass to it.
fn pair(
    heap: &mut Heap,
    a: (Value, Value, u64),
    b: (Value, Value, u64),
    shift: u32,
) -> Result<Value, Full> {
    if shift >= END {
        return make_collision(heap, a.2, &[(a.0, a.1), (b.0, b.1)]);
    }
    let (sa, sb) = (slot(a.2, shift), slot(b.2, shift));
    if sa == sb {
        let child = pair(heap, a, b, shift + BITS)?;
        let used = Used {
            entries: 0,
            children: 1 << sa,
        };
        return make_node(heap, used, &[], &[child]);
    }
    let (first, second) = if sa < sb { (a, b) } else { (b, a) };
    let entries = [(first.0, first.1), (second.0, second.1)];
    let used = Used {
        entries: 1 << sa | 1 << sb,
        children: 0,
    };
    make_node(heap, used, &entries, &[])
}

/// What a delete leaves of a node.
enum Gone {
    /// The key was not there.
    Missing,
    /// A node, owned.
    Node(Value),
    /// One entry and no children, owned, for the node above to hold in the
    /// slot the node was in. A node may only be this small at the top.
    Lone(Value, Value),
    /// Nothing at all. Only the top node can be emptied.
    Empty,
}

/// `map` without `key`. Both are borrowed, as in [`set`].
pub(crate) fn delete(heap: &mut Heap, map: Cell, key: Value) -> Result<Cell, Full> {
    delete_by(heap, map, key, hash::hash)
}

fn delete_by(heap: &mut Heap, map: Cell, key: Value, hash: Hasher) -> Result<Cell, Full> {
    let Some(t) = top(heap, map).as_cell() else {
        heap.share(map);
        return Ok(map);
    };
    let h = hash(heap, key);
    let top = match remove(heap, t, key, h, 0)? {
        Gone::Missing => {
            heap.share(map);
            return Ok(map);
        }
        Gone::Node(n) => n,
        // `settle` never leaves the top node lone.
        Gone::Lone(k, v) => {
            let used = Used {
                entries: bit(h, 0),
                children: 0,
            };
            make_node(heap, used, &[(k, v)], &[])?
        }
        Gone::Empty => Value::NIL,
    };
    let size = size(heap, map).saturating_sub(1);
    heap.make(Kind::Map, &[size, top.bits()])
}

/// The node `at`, at `shift`, without `key`, whose hash is `h`. Everything
/// passed is borrowed.
fn remove(heap: &mut Heap, at: Cell, key: Value, h: u64, shift: u32) -> Result<Gone, Full> {
    let d = heap.data(at);
    if is_collision(heap, at) {
        let Some(i) = (0..d.len() / 2).find(|&i| eq::equal(heap, word(d, 1 + 2 * i), key)) else {
            return Ok(Gone::Missing);
        };
        let mut words = copy(heap, at);
        cut(heap, &mut words, 1 + 2 * i..3 + 2 * i);
        return Ok(match words.len() {
            0 | 1 => Gone::Empty,
            3 => Gone::Lone(word(&words, 1), word(&words, 2)),
            _ => Gone::Node(Value::cell(heap.make(Kind::MapCollision, &words)?)),
        });
    }
    let used = Used::of(d);
    let n = used.entries.count_ones() as usize;
    let b = bit(h, shift);
    if used.entries & b != 0 {
        let i = index(used.entries, b);
        if !eq::equal(heap, word(d, 1 + 2 * i), key) {
            return Ok(Gone::Missing);
        }
        let mut words = copy(heap, at);
        cut(heap, &mut words, 1 + 2 * i..3 + 2 * i);
        set_used(
            &mut words,
            Used {
                entries: used.entries & !b,
                ..used
            },
        );
        return settle(heap, shift, words);
    }
    if used.children & b == 0 {
        return Ok(Gone::Missing);
    }
    let j = 1 + 2 * n + index(used.children, b);
    let Some(child) = word(d, j).as_cell() else {
        return Ok(Gone::Missing);
    };
    let gone = remove(heap, child, key, h, shift + BITS)?;
    let mut words = match gone {
        Gone::Missing => return Ok(Gone::Missing),
        Gone::Node(_) | Gone::Lone(..) | Gone::Empty => copy(heap, at),
    };
    match gone {
        Gone::Missing => {}
        Gone::Node(c) => put(heap, &mut words, j, c.bits()),
        // The child is down to one entry, which moves up into its slot here.
        Gone::Lone(k, v) => {
            cut(heap, &mut words, j..j + 1);
            let i = 1 + 2 * index(used.entries, b);
            place(&mut words, i, &[k.bits(), v.bits()]);
            set_used(
                &mut words,
                Used {
                    entries: used.entries | b,
                    children: used.children & !b,
                },
            );
        }
        Gone::Empty => {
            cut(heap, &mut words, j..j + 1);
            set_used(
                &mut words,
                Used {
                    children: used.children & !b,
                    ..used
                },
            );
        }
    }
    settle(heap, shift, words)
}

/// A node at `shift` holding `words`, whose references pass on, unless that
/// is too little for a node to hold there.
fn settle(heap: &mut Heap, shift: u32, words: Vec<u64>) -> Result<Gone, Full> {
    let used = Used::of(&words);
    Ok(
        match (used.entries.count_ones(), used.children.count_ones()) {
            (0, 0) => Gone::Empty,
            (1, 0) if shift > 0 => Gone::Lone(word(&words, 1), word(&words, 2)),
            _ => Gone::Node(Value::cell(heap.make(Kind::MapNode, &words)?)),
        },
    )
}

/// A piece of a map, in the map's order.
pub(crate) enum Piece {
    Entry(Value, Value),
    /// The entries of keys whose whole hashes are equal, in the order they
    /// went in. That order is the only thing about a map its history decides.
    Collision(Vec<(Value, Value)>),
}

/// `map`'s pieces, in its order: each node's entries, then each child's
/// pieces, in slot order. Two equal maps give the same pieces, but for the
/// order inside a collision. Reading them adds no reference.
pub(crate) fn pieces(heap: &Heap, map: Cell) -> Vec<Piece> {
    let mut out = Vec::new();
    let mut todo: Vec<Cell> = top(heap, map).as_cell().into_iter().collect();
    while let Some(at) = todo.pop() {
        if is_collision(heap, at) {
            out.push(Piece::Collision(collision(heap, at).1));
            continue;
        }
        let n = node(heap, at);
        out.extend(n.entries.into_iter().map(|(k, v)| Piece::Entry(k, v)));
        todo.extend(n.children.iter().rev().filter_map(|c| c.as_cell()));
    }
    out
}

/// Every entry of `map`, in its order. Reading them adds no reference.
pub(crate) fn entries(heap: &Heap, map: Cell) -> Vec<(Value, Value)> {
    let mut out = Vec::new();
    for piece in pieces(heap, map) {
        match piece {
            Piece::Entry(k, v) => out.push((k, v)),
            Piece::Collision(entries) => out.extend(entries),
        }
    }
    out
}

/// What decides whether two map cells of the same kind are `==`: `None` when
/// they differ in shape, else the pairs of values that have to be equal.
///
/// Equal maps have the same nodes, so two nodes compare slot by slot. A
/// collision's entries can be in either order, so its keys are matched up
/// one by one.
pub(crate) fn equal_parts(heap: &Heap, x: Cell, y: Cell) -> Option<Vec<(Value, Value)>> {
    let (dx, dy) = (heap.data(x), heap.data(y));
    match heap.kind(x)? {
        Kind::Map => (size(heap, x) == size(heap, y)).then(|| vec![(top(heap, x), top(heap, y))]),
        Kind::MapNode => (dx.first() == dy.first() && dx.len() == dy.len())
            .then(|| (1..dx.len()).map(|i| (word(dx, i), word(dy, i))).collect()),
        Kind::MapCollision => {
            let ((hx, ex), (hy, mut ey)) = (collision(heap, x), collision(heap, y));
            if hx != hy || ex.len() != ey.len() {
                return None;
            }
            let mut pairs = Vec::with_capacity(ex.len());
            for (k, v) in ex {
                let at = ey.iter().position(|&(ky, _)| eq::equal(heap, k, ky))?;
                let (_, vy) = ey.swap_remove(at);
                pairs.push((v, vy));
            }
            Some(pairs)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A key: the string `k<n>`, so keys are cells whose references count.
    fn key(heap: &mut Heap, n: u64) -> Value {
        Value::cell(heap.string(format!("k{n}").as_bytes()).expect("room"))
    }

    /// The `n` a [`key`] was made from.
    fn number(heap: &Heap, v: Value) -> u64 {
        let mut text = Vec::new();
        if let Some(cell) = v.as_cell() {
            heap.read_string(cell, &mut text);
        }
        String::from_utf8_lossy(&text[1..]).parse().expect("a key")
    }

    /// Keys that differ only in the top bits: long paths of one-child nodes.
    fn deep(heap: &Heap, v: Value) -> u64 {
        (number(heap, v) % 13) << 60 | 0x0555_5555_5555_5555
    }

    /// Keys whose whole hashes are equal, three ways: collisions.
    fn clashing(heap: &Heap, v: Value) -> u64 {
        number(heap, v) % 3
    }

    fn release(heap: &mut Heap, v: Value) {
        if let Some(cell) = v.as_cell() {
            heap.release(cell);
        }
    }

    /// A small random number generator, the same every run.
    struct Lcg(u64);

    impl Lcg {
        fn below(&mut self, n: u64) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            (self.0 >> 33) % n
        }
    }

    /// `set` and `delete` at random, then deletes until it is empty, checked
    /// against a plain list after each: every key finds what the list says,
    /// the size agrees, and the map is `==` to one built fresh from the list
    /// in another order, which holds only if the shape depends on the
    /// entries alone. Some versions are kept and checked again at the end,
    /// then all are freed, leaving nothing.
    fn against_a_list(hash: Hasher) {
        const KEYS: u64 = 120;
        let mut heap = Heap::default();
        let mut rng = Lcg(7);
        let mut model: Vec<Option<u64>> = vec![None; KEYS as usize];
        let mut map = empty(&mut heap).expect("room");
        let mut versions = Vec::new();
        for step in 0..1500 {
            let k = rng.below(KEYS);
            let kv = key(&mut heap, k);
            let before = model.clone();
            let next = if rng.below(3) == 0 {
                model[k as usize] = None;
                delete_by(&mut heap, map, kv, hash).expect("room")
            } else {
                let v = rng.below(1000);
                model[k as usize] = Some(v);
                let vv = Value::cell(heap.string(format!("v{v}").as_bytes()).expect("room"));
                let next = set_by(&mut heap, map, kv, vv, hash).expect("room");
                release(&mut heap, vv);
                next
            };
            release(&mut heap, kv);
            if step % 50 == 0 {
                versions.push((map, before));
            } else {
                heap.release(map);
            }
            map = next;
            check(&mut heap, map, &model, hash);
        }
        // Then empty it, in a random order, so every node shrinks away.
        let mut left: Vec<u64> = (0..KEYS).filter(|&k| model[k as usize].is_some()).collect();
        while !left.is_empty() {
            let k = left.swap_remove(rng.below(left.len() as u64) as usize);
            let kv = key(&mut heap, k);
            let next = delete_by(&mut heap, map, kv, hash).expect("room");
            release(&mut heap, kv);
            heap.release(map);
            map = next;
            model[k as usize] = None;
            check(&mut heap, map, &model, hash);
        }
        assert_eq!(top(&heap, map).bits(), Value::NIL.bits());
        versions.push((map, model));
        for (map, model) in &versions {
            check(&mut heap, *map, model, hash);
        }
        for (map, _) in versions {
            heap.release(map);
        }
        assert_eq!(heap.live(), 0, "a map lost track of a reference");
    }

    fn check(heap: &mut Heap, map: Cell, model: &[Option<u64>], hash: Hasher) {
        let mut text = Vec::new();
        for (k, want) in model.iter().enumerate() {
            let kv = key(heap, k as u64);
            let got = get_by(heap, map, kv, hash).map(|v| {
                text.clear();
                heap.read_string(v.as_cell().expect("a string"), &mut text);
                String::from_utf8_lossy(&text).into_owned()
            });
            assert_eq!(got, want.map(|v| format!("v{v}")), "key k{k}");
            release(heap, kv);
        }
        let present = model.iter().filter(|v| v.is_some()).count() as u64;
        assert_eq!(size(heap, map), present);
        assert_eq!(entries(heap, map).len() as u64, present);

        let mut fresh = empty(heap).expect("room");
        for (k, v) in model.iter().enumerate().rev() {
            if let Some(v) = v {
                let kv = key(heap, k as u64);
                let vv = Value::cell(heap.string(format!("v{v}").as_bytes()).expect("room"));
                let next = set_by(heap, fresh, kv, vv, hash).expect("room");
                release(heap, kv);
                release(heap, vv);
                heap.release(fresh);
                fresh = next;
            }
        }
        assert!(
            eq::equal(heap, Value::cell(map), Value::cell(fresh)),
            "equal entries, different shapes"
        );
        heap.release(fresh);
    }

    #[test]
    fn a_map_agrees_with_a_list() {
        against_a_list(hash::hash);
    }

    #[test]
    fn a_map_agrees_with_a_list_down_long_paths() {
        against_a_list(deep);
    }

    #[test]
    fn a_map_agrees_with_a_list_when_whole_hashes_clash() {
        against_a_list(clashing);
    }

    /// A set copies only the path to its key: one new node a level, and the
    /// new map's root. Every other node is shared with the old map.
    #[test]
    fn a_set_copies_only_its_path() {
        let mut heap = Heap::default();
        let mut map = empty(&mut heap).expect("room");
        for n in 0..5000 {
            let kv = key(&mut heap, n);
            let next = set(&mut heap, map, kv, Value::NIL).expect("room");
            release(&mut heap, kv);
            heap.release(map);
            map = next;
        }
        let before = heap.live();
        let kv = key(&mut heap, 5000);
        let next = set(&mut heap, map, kv, Value::NIL).expect("room");
        release(&mut heap, kv);
        // 5,000 entries is three levels, so at most three nodes and a root,
        // and the new key's string is the one other cell.
        assert!(
            heap.live() - before <= 5,
            "{} new cells",
            heap.live() - before
        );
        heap.release(next);
        heap.release(map);
        assert_eq!(heap.live(), 0);
    }
}
