//! A map is a persistent hash array mapped trie in the process's heap. It
//! takes the CHAMP form (Steindorfer & Vinju, 2015), which Scala's and
//! Kotlin's immutable maps use.
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
//! A key's hash (`hash.rs`) picks its slot. Its lowest 5 bits pick the slot
//! in the top node, the next 5 pick it one level down, and so on. An entry
//! sits in the first node where no other key shares its slot, so a map of a
//! million entries is about 4 levels deep. When the bits run out, keys whose
//! whole 64-bit hashes are equal share a collision cell, which is a plain
//! list.
//!
//! Nothing is changed in place. `set` and `delete` build new nodes along the
//! one path to the key and share every other node with the map they came
//! from, which stays as it was. A node shared this way holds one more
//! reference.
//!
//! What CHAMP adds to the older trie is that its shape depends only on what
//! the map holds, never on the order it was built in. A delete that leaves a
//! node with one entry moves that entry up into the node above, where an
//! insert would have put it. So two equal maps have the same nodes, and
//! `==`, the hash and the order entries come out in all follow from that.
//!
//! This is the layout, in words after each cell's header:
//!
//! - A map holds `size`, then the top node, or `Nil` when it is empty.
//! - A node holds the two 32-bit slot masks in one word, with the entry mask
//!   low. Then come each entry's key and value, then each child, both in
//!   slot order.
//! - A collision holds the keys' shared hash, then each entry's key and value.

use crate::eq;
use crate::hash;
use crate::heap::{Cell, DeadCell, Edit, Fault, Heap, Kind};
use crate::value::Value;
use crate::word::{high_u32, low_u32};

/// A node's or collision's words, for building a new cell from. A node has
/// at most 32 slots of up to two words each, after its mask, so it never
/// spills to the heap. A long collision can.
type Words = smallvec::SmallVec<[u64; 65]>;

/// What places a key. It is always [`hash::hash`], except in tests, which
/// pass a poor one to reach the paths a good one almost never takes.
type Hasher = fn(&Heap, Value) -> u64;

/// Bits of the hash each level takes, which is `log2` of the 32 slots.
const BITS: u32 = 5;
/// The first shift with none of a 64-bit hash left.
const END: u32 = 64;

/// A new empty map.
pub(crate) fn empty(heap: &mut Heap) -> Result<Cell, Fault> {
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
/// `bit` is never 0, so the bits below it are exact.
fn index(mask: u32, bit: u32) -> usize {
    (mask & bit.saturating_sub(1)).count_ones() as usize
}

/// The shift one level down. A shift past [`END`] has no hash left to read,
/// which [`slot`] reads as slot 0 and [`pair`] turns into a collision first.
fn deeper(shift: u32) -> u32 {
    shift.saturating_add(BITS)
}

// Where things are in a node's or a collision's words: entry `i`'s key and
// value, the words of the whole entry, and child `i` after `entries` entries.
// A cell has at most `MAX_CELL_WORDS` words, so for anything it holds these
// are exact. One that saturates is past every cell's end, and reads as not
// there.

fn key_at(i: usize) -> usize {
    i.saturating_mul(2).saturating_add(1)
}

fn value_at(i: usize) -> usize {
    key_at(i).saturating_add(1)
}

fn entry_words(i: usize) -> std::ops::Range<usize> {
    key_at(i)..key_at(i).saturating_add(2)
}

fn child_at(entries: usize, i: usize) -> usize {
    key_at(entries).saturating_add(i)
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
            .map(|i| (word(d, key_at(i)), word(d, value_at(i))))
            .collect(),
        children: (0..used.children.count_ones() as usize)
            .map(|i| word(d, child_at(n, i)))
            .collect(),
    }
}

/// A collision cell's hash and entries. Reading them adds no reference.
fn collision(heap: &Heap, cell: Cell) -> (u64, Vec<(Value, Value)>) {
    let d = heap.data(cell);
    let n = d.len().saturating_sub(1) / 2;
    let entries = (0..n)
        .map(|i| (word(d, key_at(i)), word(d, value_at(i))))
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
) -> Result<Value, Fault> {
    let words: Words = std::iter::once(used.word())
        .chain(entries.iter().flat_map(|(k, v)| [k.bits(), v.bits()]))
        .chain(children.iter().map(|c| c.bits()))
        .collect();
    Ok(Value::cell(heap.make(Kind::MapNode, &words)?))
}

/// A new collision cell. The references in `entries` pass to it.
fn make_collision(heap: &mut Heap, hash: u64, entries: &[(Value, Value)]) -> Result<Value, Fault> {
    let words: Words = std::iter::once(hash)
        .chain(entries.iter().flat_map(|(k, v)| [k.bits(), v.bits()]))
        .collect();
    Ok(Value::cell(heap.make(Kind::MapCollision, &words)?))
}

/// Whether two keys are `==`. A word that is neither a cell nor a Float has
/// one form, so for two of those the words alone decide, without a walk.
fn same_key(heap: &Heap, a: Value, b: Value) -> bool {
    let deep = |v: Value| v.as_cell().is_some() || matches!(v.view(), crate::value::View::Float(_));
    a.bits() == b.bits() || ((deep(a) || deep(b)) && eq::equal(heap, a, b))
}

/// `v` with one more reference, for keeping it in a new cell.
fn own(heap: &mut Heap, v: Value) -> Value {
    if let Some(cell) = v.as_cell() {
        heap.share(cell);
    }
    v
}

fn release(heap: &mut Heap, w: u64) -> Result<(), DeadCell> {
    match Value::from_bits(w).as_cell() {
        Some(cell) => heap.release(cell),
        None => Ok(()),
    }
}

/// The words of node or collision `at`, for a new cell to start from. Each
/// value in them gets one more reference, which the copy holds. An edit that
/// takes a word out gives its reference back.
fn copy(heap: &mut Heap, at: Cell) -> Words {
    let words = Words::from_slice(heap.data(at));
    for &w in words.iter().skip(1) {
        own(heap, Value::from_bits(w));
    }
    words
}

/// Take words `range` out of `words`, giving back their references.
fn cut(heap: &mut Heap, words: &mut Words, range: std::ops::Range<usize>) -> Result<(), DeadCell> {
    let end = range.end.min(words.len());
    for w in words.drain(range.start.min(end)..end) {
        release(heap, w)?;
    }
    Ok(())
}

/// Put `new` into `words` before word `at`, whose references pass in.
fn place(words: &mut Words, at: usize, new: &[u64]) {
    let at = at.min(words.len());
    words.insert_from_slice(at, new);
}

/// Say which slots `words` now uses.
fn set_used(words: &mut [u64], used: Used) {
    if let Some(w) = words.first_mut() {
        *w = used.word();
    }
}

/// Put `w` at `i` in `words`, giving back the reference the word there held.
fn put(heap: &mut Heap, words: &mut [u64], i: usize, w: u64) -> Result<(), DeadCell> {
    match words.get_mut(i) {
        Some(slot) => release(heap, std::mem::replace(slot, w)),
        None => Ok(()),
    }
}

/// Which of a node's 32 slots hold an entry and which hold a child, with one
/// bit per slot. A slot holds one, the other or neither.
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
            entries: low_u32(m),
            children: high_u32(m),
        }
    }

    /// How many words a node using these slots holds, its mask among them.
    /// A node's cell may have room past that, holding `Nil`.
    fn len(self) -> usize {
        let e = self.entries.count_ones() as usize;
        let c = self.children.count_ones() as usize;
        e.saturating_mul(2).saturating_add(c).saturating_add(1)
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
        let (kind, d) = heap.kind_and_data(at);
        if kind == Some(Kind::MapCollision) {
            return (0..d.len() / 2)
                .find(|&i| same_key(heap, word(d, key_at(i)), key))
                .map(|i| word(d, value_at(i)));
        }
        let used = Used::of(d);
        let b = bit(h, shift);
        if used.entries & b != 0 {
            let i = index(used.entries, b);
            return same_key(heap, word(d, key_at(i)), key).then(|| word(d, value_at(i)));
        }
        if used.children & b == 0 {
            return None;
        }
        let n = used.entries.count_ones() as usize;
        at = word(d, child_at(n, index(used.children, b))).as_cell()?;
        shift = deeper(shift);
    }
}

/// `map` with `key` bound to `value`. All three are borrowed. The new map
/// holds references of its own, and shares every node off the path to `key`
/// with `map`.
pub(crate) fn set(heap: &mut Heap, map: Cell, key: Value, value: Value) -> Result<Cell, Fault> {
    set_by(heap, map, key, value, hash::hash)
}

fn set_by(
    heap: &mut Heap,
    map: Cell,
    key: Value,
    value: Value,
    hash: Hasher,
) -> Result<Cell, Fault> {
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
    let size = size(heap, map)
        .checked_add(u64::from(added))
        .ok_or(Fault::Full)?;
    heap.make(Kind::Map, &[size, top.bits()])
}

/// [`set`] for a caller that gives up its reference to `map`. When nothing
/// else holds the map, it changes in place, and so does each node on the path
/// to `key` that nothing else holds either. `key` and `value` are borrowed.
pub(crate) fn set_owned(
    heap: &mut Heap,
    map: Cell,
    key: Value,
    value: Value,
) -> Result<Cell, Fault> {
    set_owned_by(heap, map, key, value, hash::hash)
}

fn set_owned_by(
    heap: &mut Heap,
    map: Cell,
    key: Value,
    value: Value,
    hash: Hasher,
) -> Result<Cell, Fault> {
    if !heap.unique(map)? {
        let made = set_by(heap, map, key, value, hash);
        heap.release(map)?;
        return made;
    }
    let h = hash(heap, key);
    // The map's reference to its top node passes to `insert_owned`, and the
    // node that comes back takes its place.
    let (top, added) = match top(heap, map).as_cell() {
        Some(t) => insert_owned(heap, t, (key, value, h), 0, hash)?,
        None => {
            let entry = (own(heap, key), own(heap, value));
            let used = Used {
                entries: bit(h, 0),
                children: 0,
            };
            (make_node(heap, used, &[entry], &[])?, true)
        }
    };
    let size = size(heap, map)
        .checked_add(u64::from(added))
        .ok_or(Fault::Full)?;
    if let [s, t, ..] = heap.data_mut(map) {
        *s = size;
        *t = top.bits();
    }
    Ok(map)
}

/// [`insert`] for a caller that gives up its reference to `at`. A node
/// nothing else holds is changed where it is, or, when it has to grow, its
/// words move to a bigger cell without the references in them changing.
fn insert_owned(
    heap: &mut Heap,
    at: Cell,
    (key, value, h): (Value, Value, u64),
    shift: u32,
    hash: Hasher,
) -> Result<(Value, bool), Fault> {
    if !heap.unique(at)? {
        let made = insert(heap, at, (key, value, h), shift, hash);
        heap.release(at)?;
        return made;
    }
    let d = heap.data(at);
    if is_collision(heap, at) {
        let found = (0..d.len() / 2).find(|&i| same_key(heap, word(d, key_at(i)), key));
        return match found {
            Some(i) => {
                let v = own(heap, value).bits();
                put_in(heap, at, value_at(i), v)?;
                Ok((Value::cell(at), false))
            }
            None => {
                let n = d.len();
                let entry = [own(heap, key).bits(), own(heap, value).bits()];
                let made = heap.splice(
                    Kind::MapCollision,
                    at,
                    n,
                    Edit {
                        cut: n..n,
                        at: n,
                        put: &entry,
                    },
                    0,
                )?;
                Ok((Value::cell(made), true))
            }
        };
    }
    let used = Used::of(d);
    let n = used.entries.count_ones() as usize;
    let b = bit(h, shift);
    if used.entries & b != 0 {
        let i = index(used.entries, b);
        let (k, v) = (word(d, key_at(i)), word(d, value_at(i)));
        if same_key(heap, k, key) {
            let v = own(heap, value).bits();
            put_in(heap, at, value_at(i), v)?;
            return Ok((Value::cell(at), false));
        }
        // The entry already here moves down into the new child with ours,
        // taking its references with it.
        let theirs = (k, v, hash(heap, k));
        let ours = (own(heap, key), own(heap, value), h);
        let child = pair(heap, theirs, ours, deeper(shift))?;
        let to = child_at(n.saturating_sub(1), index(used.children, b));
        let after = Used {
            entries: used.entries & !b,
            children: used.children | b,
        };
        let made = reshape(
            heap,
            at,
            used,
            after,
            Edit {
                cut: entry_words(i),
                at: to,
                put: &[child.bits()],
            },
        )?;
        return Ok((Value::cell(made), true));
    }
    if used.children & b != 0 {
        let j = child_at(n, index(used.children, b));
        let Some(child) = word(d, j).as_cell() else {
            return Ok((Value::cell(at), false));
        };
        // The node's reference to the child passes down, and what comes back
        // takes its place, so nothing is released here.
        let (child, added) = insert_owned(heap, child, (key, value, h), deeper(shift), hash)?;
        if let Some(slot) = heap.data_mut(at).get_mut(j) {
            *slot = child.bits();
        }
        return Ok((Value::cell(at), added));
    }
    let i = key_at(index(used.entries, b));
    let entry = [own(heap, key).bits(), own(heap, value).bits()];
    let after = Used {
        entries: used.entries | b,
        ..used
    };
    let made = reshape(
        heap,
        at,
        used,
        after,
        Edit {
            cut: i..i,
            at: i,
            put: &entry,
        },
    )?;
    Ok((Value::cell(made), true))
}

/// Put `w` at word `i` of `cell`, which the caller alone holds, giving back
/// the reference the word there held.
fn put_in(heap: &mut Heap, cell: Cell, i: usize, w: u64) -> Result<(), DeadCell> {
    let old = heap
        .data_mut(cell)
        .get_mut(i)
        .map(|slot| std::mem::replace(slot, w));
    match old {
        Some(old) => release(heap, old),
        None => Ok(()),
    }
}

/// The node `at`, at `shift`, with `key` bound to `value`, and whether `key`
/// is new. `h` is `key`'s hash. Everything passed is borrowed, and the node
/// made is owned.
fn insert(
    heap: &mut Heap,
    at: Cell,
    (key, value, h): (Value, Value, u64),
    shift: u32,
    hash: Hasher,
) -> Result<(Value, bool), Fault> {
    let d = heap.data(at);
    if is_collision(heap, at) {
        let found = (0..d.len() / 2).find(|&i| same_key(heap, word(d, key_at(i)), key));
        let mut words = copy(heap, at);
        let v = own(heap, value).bits();
        match found {
            Some(i) => put(heap, &mut words, value_at(i), v)?,
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
        let (k, v) = (word(d, key_at(i)), word(d, value_at(i)));
        if same_key(heap, k, key) {
            let mut words = copy(heap, at);
            let v = own(heap, value).bits();
            put(heap, &mut words, value_at(i), v)?;
            return Ok((Value::cell(heap.make(Kind::MapNode, &words)?), false));
        }
        // Two keys want one slot, so both go one level down into a node of
        // their own, and the slot holds that node instead.
        let theirs = (own(heap, k), own(heap, v), hash(heap, k));
        let ours = (own(heap, key), own(heap, value), h);
        let child = pair(heap, theirs, ours, deeper(shift))?;
        let mut words = copy(heap, at);
        cut(heap, &mut words, entry_words(i))?;
        place(
            &mut words,
            child_at(n.saturating_sub(1), index(used.children, b)),
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
        let j = child_at(n, index(used.children, b));
        let Some(child) = word(d, j).as_cell() else {
            return Ok((Value::NIL, false));
        };
        let (child, added) = insert(heap, child, (key, value, h), deeper(shift), hash)?;
        let mut words = copy(heap, at);
        put(heap, &mut words, j, child.bits())?;
        return Ok((Value::cell(heap.make(Kind::MapNode, &words)?), added));
    }
    let i = key_at(index(used.entries, b));
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
) -> Result<Value, Fault> {
    if shift >= END {
        return make_collision(heap, a.2, &[(a.0, a.1), (b.0, b.1)]);
    }
    let (sa, sb) = (slot(a.2, shift), slot(b.2, shift));
    if sa == sb {
        let child = pair(heap, a, b, deeper(shift))?;
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
pub(crate) fn delete(heap: &mut Heap, map: Cell, key: Value) -> Result<Cell, Fault> {
    delete_by(heap, map, key, hash::hash)
}

fn delete_by(heap: &mut Heap, map: Cell, key: Value, hash: Hasher) -> Result<Cell, Fault> {
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

/// [`delete`] for a caller that gives up its reference to `map`. Like
/// [`set_owned`], it changes the map and each node on the path that nothing
/// else holds in place. `key` is borrowed.
pub(crate) fn delete_owned(heap: &mut Heap, map: Cell, key: Value) -> Result<Cell, Fault> {
    delete_owned_by(heap, map, key, hash::hash)
}

fn delete_owned_by(heap: &mut Heap, map: Cell, key: Value, hash: Hasher) -> Result<Cell, Fault> {
    if !heap.unique(map)? {
        let made = delete_by(heap, map, key, hash);
        heap.release(map)?;
        return made;
    }
    let Some(t) = top(heap, map).as_cell() else {
        return Ok(map);
    };
    let h = hash(heap, key);
    let top = match remove_owned(heap, t, key, h, 0)? {
        Gone::Missing => return Ok(map),
        Gone::Node(n) => n,
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
    if let [s, t, ..] = heap.data_mut(map) {
        *s = size;
        *t = top.bits();
    }
    Ok(map)
}

/// [`remove`] on a node whose holder's slot will take what comes back. When
/// that is [`Gone::Missing`], nothing changed and the slot keeps `at`.
/// Otherwise the slot's reference to `at` has been given up, and a node that
/// nothing else held was changed in place or had its words moved on.
fn remove_owned(heap: &mut Heap, at: Cell, key: Value, h: u64, shift: u32) -> Result<Gone, Fault> {
    if !heap.unique(at)? {
        let gone = remove(heap, at, key, h, shift)?;
        if !matches!(gone, Gone::Missing) {
            heap.release(at)?;
        }
        return Ok(gone);
    }
    let d = heap.data(at);
    if is_collision(heap, at) {
        let Some(i) = (0..d.len() / 2).find(|&i| same_key(heap, word(d, key_at(i)), key)) else {
            return Ok(Gone::Missing);
        };
        let mut words = Words::from_slice(d);
        cut(heap, &mut words, entry_words(i))?;
        let gone = match words.len() {
            0 | 1 => Gone::Empty,
            3 => Gone::Lone(word(&words, 1), word(&words, 2)),
            _ => Gone::Node(Value::cell(heap.make(Kind::MapCollision, &words)?)),
        };
        heap.discard(at)?;
        return Ok(gone);
    }
    let used = Used::of(d);
    let n = used.entries.count_ones() as usize;
    let b = bit(h, shift);
    if used.entries & b != 0 {
        let i = index(used.entries, b);
        if !same_key(heap, word(d, key_at(i)), key) {
            return Ok(Gone::Missing);
        }
        let (k, v) = (word(d, key_at(i)), word(d, value_at(i)));
        let after = Used {
            entries: used.entries & !b,
            ..used
        };
        let gone = settle_cut(
            heap,
            at,
            shift,
            after,
            Edit {
                cut: entry_words(i),
                at: 0,
                put: &[],
            },
        )?;
        release(heap, k.bits())?;
        release(heap, v.bits())?;
        return Ok(gone);
    }
    if used.children & b == 0 {
        return Ok(Gone::Missing);
    }
    let j = child_at(n, index(used.children, b));
    let Some(child) = word(d, j).as_cell() else {
        return Ok(Gone::Missing);
    };
    // The child's slot is the holder `remove_owned` speaks of, so what comes
    // back goes there, and the child's own reference was given up already.
    match remove_owned(heap, child, key, h, deeper(shift))? {
        Gone::Missing => Ok(Gone::Missing),
        Gone::Node(c) => {
            if let Some(slot) = heap.data_mut(at).get_mut(j) {
                *slot = c.bits();
            }
            Ok(Gone::Node(Value::cell(at)))
        }
        // The child is down to one entry, which moves up into its slot here.
        Gone::Lone(k, v) => {
            let after = Used {
                entries: used.entries | b,
                children: used.children & !b,
            };
            let i = key_at(index(used.entries, b));
            let put = [k.bits(), v.bits()];
            let cut = j..j.saturating_add(1);
            settle_cut(
                heap,
                at,
                shift,
                after,
                Edit {
                    cut,
                    at: i,
                    put: &put,
                },
            )
        }
        Gone::Empty => {
            let after = Used {
                children: used.children & !b,
                ..used
            };
            settle_cut(
                heap,
                at,
                shift,
                after,
                Edit {
                    cut: j..j.saturating_add(1),
                    at: 0,
                    put: &[],
                },
            )
        }
    }
}

/// Spare words a node is given when it has to move to a bigger cell, so the
/// next few entries fit where it is. Two entries' worth.
const ROOM: usize = 4;

/// The most words a node can hold: its mask and 32 slots of two words.
const NODE_MAX: usize = 65;

/// Node `at`, which its one holder gives up, now using slots `after` instead
/// of `before`, with `edit` made to its words. When that fits the cell, it changes in place, and any words
/// it no longer needs go back to `Nil`. Otherwise its words move to a cell
/// with some room. Either way the references move without changing.
fn reshape(
    heap: &mut Heap,
    at: Cell,
    before: Used,
    after: Used,
    edit: Edit<'_>,
) -> Result<Cell, Fault> {
    let (old, new) = (before.len(), after.len());
    let Edit {
        cut,
        at: put_at,
        put,
    } = edit;
    let d = heap.data_mut(at);
    // Both bounds hold for any node this module made, and they keep every
    // shift below inside the cell.
    if new <= d.len() && old <= d.len() {
        // Close the gap `cut` leaves, then open one for `put`. Both are
        // shifts inside the cell, of the words after the change.
        let end = cut.end.min(old);
        let start = cut.start.min(end);
        d.copy_within(end..old, start);
        let left = old.saturating_sub(end.saturating_sub(start));
        let put_at = put_at.min(left);
        let to = put_at.saturating_add(put.len());
        if to <= d.len() && left <= d.len() && new <= d.len() {
            d.copy_within(put_at..left, to);
        }
        if let Some(slot) = d.get_mut(put_at..to) {
            slot.copy_from_slice(put);
        }
        if let Some(spare) = d.get_mut(new..old) {
            spare.fill(Value::NIL.bits());
        }
        set_used(d, after);
        return Ok(at);
    }
    let room = ROOM.min(NODE_MAX.saturating_sub(new));
    let edit = Edit {
        cut,
        at: put_at,
        put,
    };
    let made = heap.splice(Kind::MapNode, at, old, edit, room)?;
    set_used(heap.data_mut(made), after);
    Ok(made)
}

/// [`settle`] for node `at`, which its one holder gives up, once `edit` is
/// made to its words, so that it uses the slots `after` says. The words move on without
/// their references changing, and `at` is freed. Whatever `cut` takes, the
/// caller sees to.
fn settle_cut(
    heap: &mut Heap,
    at: Cell,
    shift: u32,
    after: Used,
    edit: Edit<'_>,
) -> Result<Gone, Fault> {
    match (after.entries.count_ones(), after.children.count_ones()) {
        (0, 0) => {
            heap.discard(at)?;
            Ok(Gone::Empty)
        }
        // One entry is left, which moves up into the node above. The node is
        // three words then, so building them here costs next to nothing.
        (1, 0) if shift > 0 => {
            let mut words = Words::from_slice(heap.data(at));
            let end = edit.cut.end.min(words.len());
            words.drain(edit.cut.start.min(end)..end);
            place(&mut words, edit.at, edit.put);
            let gone = Gone::Lone(word(&words, 1), word(&words, 2));
            heap.discard(at)?;
            Ok(gone)
        }
        _ => {
            let before = Used::of(heap.data(at));
            let made = reshape(heap, at, before, after, edit)?;
            Ok(Gone::Node(Value::cell(made)))
        }
    }
}
/// The node `at`, at `shift`, without `key`, whose hash is `h`. Everything
/// passed is borrowed.
fn remove(heap: &mut Heap, at: Cell, key: Value, h: u64, shift: u32) -> Result<Gone, Fault> {
    let d = heap.data(at);
    if is_collision(heap, at) {
        let Some(i) = (0..d.len() / 2).find(|&i| same_key(heap, word(d, key_at(i)), key)) else {
            return Ok(Gone::Missing);
        };
        let mut words = copy(heap, at);
        cut(heap, &mut words, entry_words(i))?;
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
        if !same_key(heap, word(d, key_at(i)), key) {
            return Ok(Gone::Missing);
        }
        let mut words = copy(heap, at);
        cut(heap, &mut words, entry_words(i))?;
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
    let j = child_at(n, index(used.children, b));
    let Some(child) = word(d, j).as_cell() else {
        return Ok(Gone::Missing);
    };
    let gone = remove(heap, child, key, h, deeper(shift))?;
    let mut words = match gone {
        Gone::Missing => return Ok(Gone::Missing),
        Gone::Node(_) | Gone::Lone(..) | Gone::Empty => copy(heap, at),
    };
    match gone {
        Gone::Missing => {}
        Gone::Node(c) => put(heap, &mut words, j, c.bits())?,
        // The child is down to one entry, which moves up into its slot here.
        Gone::Lone(k, v) => {
            cut(heap, &mut words, j..j.saturating_add(1))?;
            let i = key_at(index(used.entries, b));
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
            cut(heap, &mut words, j..j.saturating_add(1))?;
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
fn settle(heap: &mut Heap, shift: u32, words: Words) -> Result<Gone, Fault> {
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

/// `map`'s pieces in its order, which is each node's entries, then each
/// child's pieces, in slot order. Two equal maps give the same pieces, apart
/// from the order inside a collision. Reading them adds no reference.
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

/// What decides whether two map cells of the same kind are `==`. It is `None`
/// when they differ in shape, and otherwise the pairs of values that have to
/// be equal.
///
/// Equal maps have the same nodes, so two nodes compare slot by slot. A
/// collision's entries can be in either order, so its keys are matched up
/// one by one.
pub(crate) fn equal_parts(heap: &Heap, x: Cell, y: Cell) -> Option<Vec<(Value, Value)>> {
    let (dx, dy) = (heap.data(x), heap.data(y));
    match heap.kind(x)? {
        Kind::Map => (size(heap, x) == size(heap, y)).then(|| vec![(top(heap, x), top(heap, y))]),
        // Equal masks mean equal lengths, whatever room either cell has.
        Kind::MapNode => (dx.first() == dy.first()).then(|| {
            (1..Used::of(dx).len())
                .map(|i| (word(dx, i), word(dy, i)))
                .collect()
        }),
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

    /// A key is the string `k<n>`, so keys are cells whose references count.
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

    /// A hash where keys differ only in the top bits, which makes long paths
    /// of one-child nodes.
    fn deep(heap: &Heap, v: Value) -> u64 {
        (number(heap, v) % 13) << 60 | 0x0555_5555_5555_5555
    }

    /// A hash that puts every key in one of three groups whose whole hashes
    /// are equal, which makes collisions.
    fn clashing(heap: &Heap, v: Value) -> u64 {
        number(heap, v) % 3
    }

    fn release(heap: &mut Heap, v: Value) {
        if let Some(cell) = v.as_cell() {
            heap.release(cell).expect("a live cell");
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

    /// Run `set` and `delete` at random, then delete until the map is empty,
    /// and check it against a plain list after each step. Every key must find
    /// what the list says, the size must agree, and the map must be `==` to
    /// one built fresh from the list in another order. That last check holds
    /// only if the shape depends on the entries alone. Some versions are kept
    /// and checked again at the end, then all are freed, leaving nothing.
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
            // A version kept for later shares its nodes with every map made
            // from it, so a set that gives its map up must not change them.
            let keep = step % 50 == 0;
            let next = if rng.below(3) == 0 {
                model[k as usize] = None;
                if keep {
                    delete_by(&mut heap, map, kv, hash).expect("room")
                } else {
                    delete_owned_by(&mut heap, map, kv, hash).expect("room")
                }
            } else {
                let v = rng.below(1000);
                model[k as usize] = Some(v);
                let vv = Value::cell(heap.string(format!("v{v}").as_bytes()).expect("room"));
                let next = if keep {
                    set_by(&mut heap, map, kv, vv, hash)
                } else {
                    set_owned_by(&mut heap, map, kv, vv, hash)
                };
                release(&mut heap, vv);
                next.expect("room")
            };
            release(&mut heap, kv);
            if keep {
                versions.push((map, before));
            }
            map = next;
            check(&mut heap, map, &model, hash);
        }
        // Then empty it, in a random order, so every node shrinks away.
        let mut left: Vec<u64> = (0..KEYS).filter(|&k| model[k as usize].is_some()).collect();
        while !left.is_empty() {
            let k = left.swap_remove(rng.below(left.len() as u64) as usize);
            let kv = key(&mut heap, k);
            let next = delete_owned_by(&mut heap, map, kv, hash).expect("room");
            release(&mut heap, kv);
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
            heap.release(map).expect("a live cell");
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
                heap.release(fresh).expect("a live cell");
                fresh = next;
            }
        }
        assert!(
            eq::equal(heap, Value::cell(map), Value::cell(fresh)),
            "equal entries, different shapes"
        );
        heap.release(fresh).expect("a live cell");
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
            heap.release(map).expect("a live cell");
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
        heap.release(next).expect("a live cell");
        heap.release(map).expect("a live cell");
        assert_eq!(heap.live(), 0);
    }

    /// The owned forms give up the caller's reference, so on a map that was
    /// already freed they stop instead of freeing it again.
    #[test]
    fn an_owned_edit_of_a_freed_map_is_a_dead_cell() {
        let mut heap = Heap::default();
        let map = empty(&mut heap).expect("room");
        heap.release(map).expect("a live cell");
        let k = Value::int(1).expect("small");
        assert_eq!(set_owned(&mut heap, map, k, k), Err(Fault::DeadCell));
        assert_eq!(delete_owned(&mut heap, map, k), Err(Fault::DeadCell));
    }

    /// A set over a key gives up the value that was there. When that value
    /// was freed behind the map's back, the set stops at it.
    #[test]
    fn a_set_over_a_freed_value_is_a_dead_cell() {
        let mut heap = Heap::default();
        let map = empty(&mut heap).expect("room");
        let k = Value::int(1).expect("small");
        let v = key(&mut heap, 9);
        let map = set_owned(&mut heap, map, k, v).expect("room");
        // One for the test's own reference, and one that was the map's.
        release(&mut heap, v);
        release(&mut heap, v);
        assert_eq!(set_owned(&mut heap, map, k, k), Err(Fault::DeadCell));
    }
}
