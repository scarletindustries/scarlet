//! A process's heap: where values too big for one word live.
//!
//! The heap is a list of chunks, and a chunk is one block of memory carved
//! into cells, one value per cell. `docs/vm-design.md` ("how a heap grows")
//! has the rules and where they come from:
//!
//! - no chunk until the first cell is needed;
//! - small cells fill chunks that start at 2 KB and grow about 1.6 times;
//! - a cell bigger than the current chunk size gets a chunk of exactly its
//!   size, which goes back to the system when the cell is freed.
//!
//! Every cell starts with one header word: its reference count, its kind, and
//! its size in words. A cell is freed when its count reaches zero, and its
//! space is kept on a free list for the next cell of the same size. Freeing a
//! constructor, tuple, closure or array node gives up the references it holds
//! too, and so on down.
//!
//! Chunks are words, not bytes, and cells are named by chunk and word rather
//! than by address, so the heap needs no `unsafe` and moves with its process
//! as plain data.

use std::collections::HashMap;

use num_bigint::{BigInt, Sign};
use scarlet_ir::TypeId;
use scarlet_ir::core_ir::{FuncIdx, VariantRef};

use crate::value::Value;

/// Words in the first chunk: 2 KB.
const FIRST_CHUNK_WORDS: usize = 256;

/// How many chunks and words a [`Cell`] can name: 20 bits of chunk and 28 of
/// word, so it fits the 48 bits a value word has for it.
const CHUNK_BITS: u32 = 20;
const WORD_BITS: u32 = 28;

/// The most words one cell can take, its header's among them.
pub(crate) const MAX_CELL_WORDS: usize = 1 << WORD_BITS;

/// A cell: which chunk, and which word the cell's header is at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Cell {
    chunk: u32,
    word: u32,
}

impl Cell {
    /// The cell as 48 bits, for a value word's payload.
    pub(crate) fn bits(self) -> u64 {
        u64::from(self.chunk) << WORD_BITS | u64::from(self.word)
    }

    pub(crate) fn from_bits(bits: u64) -> Cell {
        Cell {
            chunk: (bits >> WORD_BITS) as u32 & ((1 << CHUNK_BITS) - 1),
            word: bits as u32 & ((1 << WORD_BITS) - 1),
        }
    }
}

/// What a cell holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// UTF-8 text: a word holding its length in bytes, then the bytes.
    String = 1,
    /// An Int too big for a value word: a word holding its sign (bit 63) and
    /// how many 64-bit digits follow, then the digits, least significant
    /// first.
    BigInt = 2,
    /// A constructor with fields: a word holding its type (bits 0 to 31) and
    /// variant (bits 32 to 47), then each field's value word, in order. Each
    /// field holds one reference.
    Ctor = 3,
    /// A function with captures: a word holding its `FuncIdx`, then each
    /// capture's value word, in order. Each capture holds one reference. A
    /// function with none is a value word of its own, and needs no cell.
    Closure = 4,
    /// A tuple: each element's value word, in order, straight after the
    /// header. Each element holds one reference.
    Tuple = 5,
    /// An array's root, one of its leaves, or one of its branches.
    /// [`crate::array`] has their layout; the heap only knows which of their
    /// words hold references.
    ArrayRoot = 6,
    ArrayLeaf = 7,
    ArrayBranch = 8,
    /// A range `start..end` of Ints: its two ends, as two's complement
    /// words. It holds no references.
    Range = 9,
    /// A binary's bits, or a slice of another binary's
    /// ([`crate::binary`]). A slice holds a reference to the binary it is a
    /// slice of, in its first word.
    Binary = 10,
    BinarySlice = 11,
    /// A map's root, one of its nodes, or a list of keys whose whole hashes
    /// are equal. [`crate::map`] has their layout. Each holds references in
    /// every word after its first.
    Map = 12,
    MapNode = 13,
    MapCollision = 14,
}

/// A heap that has run out of the cells a [`Cell`] can name. It is a limit of
/// the machine, not a bug in the program.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Full;

struct Chunk {
    words: Box<[u64]>,
    /// Words handed out so far, from the front.
    used: usize,
    /// Made for one big cell. It goes back to the system when that cell does.
    own: bool,
}

#[derive(Default)]
pub(crate) struct Heap {
    /// Indexed by [`Cell::chunk`]. A chunk given back leaves `None`, and its
    /// index is reused by the next chunk made.
    chunks: Vec<Option<Chunk>>,
    spare: Vec<u32>,
    /// The chunk small cells are carved from now.
    current: Option<u32>,
    /// Words in the next small-cell chunk.
    next_chunk: usize,
    /// Freed small cells, by size in words, ready to be reused.
    free: HashMap<usize, Vec<Cell>>,
    live: usize,
    /// Cells taken from a chunk or a free list over this heap's life, and
    /// cells a constructor overwrote in place instead (`internal.scrl`).
    made: usize,
    reused: usize,
    /// The cells a [`Heap::release`] still has to give a reference up for.
    /// Kept between calls so a release does not allocate.
    dying: Vec<Cell>,
}

impl Heap {
    /// A new cell of `kind` with `data` words after its header, and a count
    /// of 1: the caller holds the one reference.
    fn alloc(&mut self, kind: Kind, data: usize) -> Result<Cell, Full> {
        let size = data + 1;
        let cell = if let Some(cell) = self.free.get_mut(&size).and_then(Vec::pop) {
            cell
        } else if size > self.chunk_size() {
            self.own_chunk(size)?
        } else {
            self.carve(size)?
        };
        self.set_word(cell, 0, header(1, kind, size));
        self.live += 1;
        self.made += 1;
        Ok(cell)
    }

    fn chunk_size(&self) -> usize {
        self.next_chunk.max(FIRST_CHUNK_WORDS)
    }

    fn own_chunk(&mut self, size: usize) -> Result<Cell, Full> {
        let chunk = self.new_chunk(size, true)?;
        Ok(Cell { chunk, word: 0 })
    }

    /// `size` words from the current chunk, starting a new, bigger one when
    /// it has no room.
    fn carve(&mut self, size: usize) -> Result<Cell, Full> {
        if let Some(i) = self.current
            && let Some(Some(c)) = self.chunks.get_mut(i as usize)
            && c.words.len() - c.used >= size
        {
            let word = c.used as u32;
            c.used += size;
            return Ok(Cell { chunk: i, word });
        }
        let words = self.chunk_size();
        self.next_chunk = words * 8 / 5;
        let i = self.new_chunk(words, false)?;
        self.current = Some(i);
        self.carve(size)
    }

    fn new_chunk(&mut self, words: usize, own: bool) -> Result<u32, Full> {
        if words > 1 << WORD_BITS {
            return Err(Full);
        }
        let chunk = Chunk {
            words: vec![0; words].into_boxed_slice(),
            used: if own { words } else { 0 },
            own,
        };
        if let Some(i) = self.spare.pop() {
            if let Some(slot) = self.chunks.get_mut(i as usize) {
                *slot = Some(chunk);
            }
            return Ok(i);
        }
        if self.chunks.len() >= 1 << CHUNK_BITS {
            return Err(Full);
        }
        self.chunks.push(Some(chunk));
        Ok((self.chunks.len() - 1) as u32)
    }

    /// Add one reference to `cell`.
    pub(crate) fn share(&mut self, cell: Cell) {
        let h = self.word(cell, 0);
        self.set_word(cell, 0, h + 1);
    }

    /// Drop one reference to `cell`, freeing it when that was the last.
    ///
    /// A freed constructor's fields each lose a reference in turn. They are
    /// kept on a list rather than released by recursion, so freeing a list a
    /// million cells long takes no more stack than freeing one cell.
    pub(crate) fn release(&mut self, cell: Cell) {
        let mut dying = std::mem::take(&mut self.dying);
        dying.push(cell);
        while let Some(cell) = dying.pop() {
            let h = self.word(cell, 0);
            if count(h) > 1 {
                self.set_word(cell, 0, h - 1);
                continue;
            }
            for i in held(kind_bits(h), size(h)) {
                if let Some(held) = Value::from_bits(self.word(cell, i)).as_cell() {
                    dying.push(held);
                }
            }
            self.free_cell(cell, h);
        }
        self.dying = dying;
    }

    /// Perceus's drop of a last reference: when nothing else holds `cell`,
    /// give up everything it holds and keep the allocation, header and all,
    /// for a constructor of the same size to overwrite. `false` when
    /// something else holds it, which leaves it to an ordinary `release`.
    ///
    /// A field is emptied before it is released, so a hollowed cell is safe to
    /// release again: it holds nothing. Emptying here rather than at the
    /// constructor is what carries reuse down a chain — a callee sees its
    /// argument as the last reference only because its caller gave up its own
    /// first.
    pub(crate) fn hollow(&mut self, cell: Cell) -> bool {
        let h = self.word(cell, 0);
        if count(h) != 1 {
            return false;
        }
        for i in held(kind_bits(h), size(h)) {
            let held = Value::from_bits(self.word(cell, i));
            self.set_word(cell, i, Value::NIL.bits());
            if let Some(c) = held.as_cell() {
                self.release(c);
            }
        }
        true
    }

    /// Whether a constructor of `fields` fields may be written over `cell`:
    /// it must be a constructor cell of exactly that size, and nothing else
    /// may hold it. Both are true of every cell [`Self::hollow`] kept, and the
    /// count is read again here because a register holds its cell until
    /// something overwrites it, so what a constructor finds there need not be
    /// the one parked for it.
    pub(crate) fn fits_ctor(&self, cell: Cell, fields: usize) -> bool {
        let h = self.word(cell, 0);
        count(h) == 1 && kind_bits(h) == Kind::Ctor as u64 && size(h) == fields + 2
    }

    fn free_cell(&mut self, cell: Cell, h: u64) {
        self.live -= 1;
        let own = matches!(self.chunks.get(cell.chunk as usize), Some(Some(c)) if c.own);
        if own {
            if let Some(slot) = self.chunks.get_mut(cell.chunk as usize) {
                *slot = None;
            }
            self.spare.push(cell.chunk);
        } else {
            self.set_word(cell, 0, 0);
            self.free.entry(size(h)).or_default().push(cell);
        }
    }

    pub(crate) fn kind(&self, cell: Cell) -> Option<Kind> {
        match kind_bits(self.word(cell, 0)) {
            1 => Some(Kind::String),
            2 => Some(Kind::BigInt),
            3 => Some(Kind::Ctor),
            4 => Some(Kind::Closure),
            5 => Some(Kind::Tuple),
            6 => Some(Kind::ArrayRoot),
            7 => Some(Kind::ArrayLeaf),
            8 => Some(Kind::ArrayBranch),
            9 => Some(Kind::Range),
            10 => Some(Kind::Binary),
            11 => Some(Kind::BinarySlice),
            12 => Some(Kind::Map),
            13 => Some(Kind::MapNode),
            14 => Some(Kind::MapCollision),
            _ => None,
        }
    }

    /// A new string cell holding `text`.
    pub(crate) fn string(&mut self, text: &[u8]) -> Result<Cell, Full> {
        let cell = self.alloc(Kind::String, 1 + text.len().div_ceil(8))?;
        self.set_word(cell, 1, text.len() as u64);
        for (i, bytes) in text.chunks(8).enumerate() {
            let mut word = [0u8; 8];
            word[..bytes.len()].copy_from_slice(bytes);
            self.set_word(cell, 2 + i, u64::from_le_bytes(word));
        }
        Ok(cell)
    }

    /// A string cell's length in bytes.
    pub(crate) fn string_len(&self, cell: Cell) -> usize {
        self.word(cell, 1) as usize
    }

    /// Whether a string cell holds exactly `text`.
    pub(crate) fn string_is(&self, cell: Cell, text: &[u8]) -> bool {
        self.string_len(cell) == text.len()
            && text.chunks(8).enumerate().all(|(i, bytes)| {
                let mut word = [0u8; 8];
                word[..bytes.len()].copy_from_slice(bytes);
                self.word(cell, 2 + i) == u64::from_le_bytes(word)
            })
    }

    /// A string cell's bytes, appended to `out`.
    pub(crate) fn read_string(&self, cell: Cell, out: &mut Vec<u8>) {
        let len = self.word(cell, 1) as usize;
        for i in 0..len.div_ceil(8) {
            let bytes = self.word(cell, 2 + i).to_le_bytes();
            let n = (len - i * 8).min(8);
            out.extend_from_slice(&bytes[..n]);
        }
    }

    /// A new big-int cell holding `n`.
    pub(crate) fn big_int(&mut self, n: &BigInt) -> Result<Cell, Full> {
        let (sign, digits) = n.to_u64_digits();
        let cell = self.alloc(Kind::BigInt, 1 + digits.len())?;
        let negative = u64::from(sign == Sign::Minus) << 63;
        self.set_word(cell, 1, negative | digits.len() as u64);
        for (i, d) in digits.iter().enumerate() {
            self.set_word(cell, 2 + i, *d);
        }
        Ok(cell)
    }

    pub(crate) fn read_big_int(&self, cell: Cell) -> BigInt {
        let w = self.word(cell, 1);
        let sign = if w >> 63 == 1 {
            Sign::Minus
        } else {
            Sign::Plus
        };
        let len = (w & 0xFFFF_FFFF) as usize;
        let bytes: Vec<u8> = (0..len)
            .flat_map(|i| self.word(cell, 2 + i).to_le_bytes())
            .collect();
        BigInt::from_bytes_le(sign, &bytes)
    }

    /// A new cell for constructor `v`, holding `fields`. Each field's
    /// reference passes to the cell.
    pub(crate) fn ctor(&mut self, v: VariantRef, fields: &[Value]) -> Result<Cell, Full> {
        let cell = self.alloc(Kind::Ctor, 1 + fields.len())?;
        self.write_ctor(cell, v, fields);
        Ok(cell)
    }

    /// Perceus reuse: the constructor `ctor` would build, written over a cell
    /// [`Self::hollow`] emptied, which [`Self::fits_ctor`] says is the right
    /// size. Its one reference stays, so it is the new constructor's.
    pub(crate) fn ctor_in(&mut self, cell: Cell, v: VariantRef, fields: &[Value]) {
        self.reused += 1;
        self.write_ctor(cell, v, fields);
    }

    fn write_ctor(&mut self, cell: Cell, v: VariantRef, fields: &[Value]) {
        let tag = u64::from(v.variant_idx) << 32 | u64::from(v.type_id.0 as u32);
        self.set_word(cell, 1, tag);
        for (i, f) in fields.iter().enumerate() {
            self.set_word(cell, 2 + i, f.bits());
        }
    }

    /// Which constructor a constructor cell is.
    pub(crate) fn variant(&self, cell: Cell) -> VariantRef {
        let tag = self.word(cell, 1);
        VariantRef {
            type_id: TypeId(tag as u32 as i32),
            variant_idx: (tag >> 32) as u16,
        }
    }

    /// Field `i` of a constructor cell, or `None` past its last. Reading it
    /// adds no reference.
    pub(crate) fn field(&self, cell: Cell, i: usize) -> Option<Value> {
        self.held(cell, i)
    }

    /// A new cell for function `func` over `captures`. Each capture's
    /// reference passes to the cell.
    pub(crate) fn closure(&mut self, func: FuncIdx, captures: &[Value]) -> Result<Cell, Full> {
        let cell = self.alloc(Kind::Closure, 1 + captures.len())?;
        self.set_word(cell, 1, u64::from(func.0));
        for (i, c) in captures.iter().enumerate() {
            self.set_word(cell, 2 + i, c.bits());
        }
        Ok(cell)
    }

    /// The function a closure cell runs.
    pub(crate) fn closure_func(&self, cell: Cell) -> FuncIdx {
        FuncIdx(self.word(cell, 1) as u32)
    }

    /// Capture `i` of a closure cell, or `None` past its last. Reading it adds
    /// no reference.
    pub(crate) fn capture(&self, cell: Cell, i: usize) -> Option<Value> {
        self.held(cell, i)
    }

    /// A new cell of `kind` holding `data` after its header, with one
    /// reference, the caller's. Any reference in `data` passes to the cell.
    /// For a kind whose layout lives outside the heap, like an array's nodes.
    pub(crate) fn make(&mut self, kind: Kind, data: &[u64]) -> Result<Cell, Full> {
        let cell = self.alloc(kind, data.len())?;
        for (i, w) in data.iter().enumerate() {
            self.set_word(cell, 1 + i, *w);
        }
        Ok(cell)
    }

    /// The words of `cell` after its header. Empty for a cell the heap does
    /// not have.
    pub(crate) fn data(&self, cell: Cell) -> &[u64] {
        let n = size(self.word(cell, 0)).saturating_sub(1);
        let start = cell.word as usize + 1;
        match self.chunks.get(cell.chunk as usize) {
            Some(Some(c)) => c.words.get(start..start + n).unwrap_or(&[]),
            _ => &[],
        }
    }

    /// A new tuple cell holding `elements`. Each element's reference passes
    /// to the cell.
    pub(crate) fn tuple(&mut self, elements: &[Value]) -> Result<Cell, Full> {
        let cell = self.alloc(Kind::Tuple, elements.len())?;
        for (i, e) in elements.iter().enumerate() {
            self.set_word(cell, 1 + i, e.bits());
        }
        Ok(cell)
    }

    /// Element `i` of a tuple cell, or `None` past its last. Reading it adds
    /// no reference.
    pub(crate) fn element(&self, cell: Cell, i: usize) -> Option<Value> {
        let n = size(self.word(cell, 0)).saturating_sub(1);
        (i < n).then(|| Value::from_bits(self.word(cell, 1 + i)))
    }

    /// A tuple cell's elements, in order. Reading one adds no reference.
    pub(crate) fn elements(&self, cell: Cell) -> impl Iterator<Item = Value> + '_ {
        let n = size(self.word(cell, 0)).saturating_sub(1);
        (0..n).map(move |i| Value::from_bits(self.word(cell, 1 + i)))
    }

    /// Value `i` of the ones a constructor or closure cell holds after its
    /// tag word.
    fn held(&self, cell: Cell, i: usize) -> Option<Value> {
        let n = size(self.word(cell, 0)).saturating_sub(2);
        (i < n).then(|| Value::from_bits(self.word(cell, 2 + i)))
    }

    /// A constructor cell's fields, in order. Reading one adds no reference.
    pub(crate) fn fields(&self, cell: Cell) -> impl Iterator<Item = Value> + '_ {
        let n = size(self.word(cell, 0)).saturating_sub(2);
        (0..n).map(move |i| Value::from_bits(self.word(cell, 2 + i)))
    }

    /// Cells not yet freed.
    #[cfg(test)]
    pub(crate) fn live(&self) -> usize {
        self.live
    }

    /// Cells this heap has allocated, and cells Perceus reuse saved it from
    /// allocating. Both count over the heap's whole life.
    pub(crate) fn made(&self) -> usize {
        self.made
    }

    pub(crate) fn reused(&self) -> usize {
        self.reused
    }

    fn word(&self, cell: Cell, i: usize) -> u64 {
        match self.chunks.get(cell.chunk as usize) {
            Some(Some(c)) => c.words.get(cell.word as usize + i).copied().unwrap_or(0),
            _ => 0,
        }
    }

    fn set_word(&mut self, cell: Cell, i: usize, w: u64) {
        if let Some(Some(c)) = self.chunks.get_mut(cell.chunk as usize)
            && let Some(slot) = c.words.get_mut(cell.word as usize + i)
        {
            *slot = w;
        }
    }
}

/// A header word: the count in the low 32 bits, then the kind, then the
/// cell's size in words.
fn header(count: u32, kind: Kind, size: usize) -> u64 {
    u64::from(count) | (kind as u64) << 32 | (size as u64) << 40
}

fn count(h: u64) -> u64 {
    h & 0xFFFF_FFFF
}

fn kind_bits(h: u64) -> u64 {
    h >> 32 & 0xFF
}

/// The words of a cell of this kind and size that hold a value, and so a
/// reference: after a constructor's or closure's tag word, all of a tuple or
/// an array leaf, an array root's three parts (after its length and height),
/// an array branch's children (after its height and size table), and a map
/// cell's every word after its first.
fn held(kind: u64, size: usize) -> std::ops::Range<usize> {
    let k = |want: Kind| kind == want as u64;
    if k(Kind::Ctor)
        || k(Kind::Closure)
        || k(Kind::Map)
        || k(Kind::MapNode)
        || k(Kind::MapCollision)
    {
        2..size
    } else if k(Kind::Tuple) || k(Kind::ArrayLeaf) {
        1..size
    } else if k(Kind::ArrayRoot) {
        4..size
    } else if k(Kind::ArrayBranch) {
        2 + size.saturating_sub(2) / 2..size
    } else if k(Kind::BinarySlice) {
        1..2
    } else {
        0..0
    }
}

fn size(h: u64) -> usize {
    (h >> 40) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(heap: &Heap, cell: Cell) -> String {
        let mut out = Vec::new();
        heap.read_string(cell, &mut out);
        String::from_utf8(out).expect("UTF-8")
    }

    #[test]
    fn there_is_no_chunk_until_the_first_cell() {
        let mut heap = Heap::default();
        assert!(heap.chunks.is_empty());
        heap.string(b"hi").expect("room");
        assert_eq!(heap.chunks.len(), 1);
    }

    #[test]
    fn a_string_reads_back_at_every_length() {
        let mut heap = Heap::default();
        for len in [0, 1, 7, 8, 9, 16, 100] {
            let s: String = "abcdefghij".chars().cycle().take(len).collect();
            let cell = heap.string(s.as_bytes()).expect("room");
            assert_eq!(text(&heap, cell), s);
            assert_eq!(heap.kind(cell), Some(Kind::String));
        }
    }

    #[test]
    fn a_freed_cell_is_reused_by_the_next_of_its_size() {
        let mut heap = Heap::default();
        let a = heap.string(b"one").expect("room");
        heap.release(a);
        assert_eq!(heap.live(), 0);
        let b = heap.string(b"two").expect("room");
        assert_eq!(a, b);
    }

    #[test]
    fn a_shared_cell_lives_until_its_last_reference_goes() {
        let mut heap = Heap::default();
        let a = heap.string(b"kept").expect("room");
        heap.share(a);
        heap.release(a);
        assert_eq!(heap.live(), 1);
        assert_eq!(text(&heap, a), "kept");
        heap.release(a);
        assert_eq!(heap.live(), 0);
    }

    /// Each new small-cell chunk is 1.6 times the one before, from 2 KB.
    #[test]
    fn chunks_grow_by_steps() {
        let mut heap = Heap::default();
        for _ in 0..2000 {
            heap.string(b"a string of twenty-four!").expect("room");
        }
        let sizes: Vec<usize> = heap
            .chunks
            .iter()
            .flatten()
            .map(|c| c.words.len())
            .collect();
        assert_eq!(sizes[..4], [256, 409, 654, 1046]);
    }

    /// A cell too big for the current chunk gets its own, and the chunk goes
    /// back when the cell does. Its index is reused.
    #[test]
    fn a_big_cell_gets_a_chunk_of_its_own_size() {
        let mut heap = Heap::default();
        let big = vec![b'x'; 10_000];
        let cell = heap.string(&big).expect("room");
        let own = heap.chunks[cell.chunk as usize].as_ref().expect("made");
        assert!(own.own);
        assert_eq!(own.words.len(), 2 + 10_000usize.div_ceil(8));
        assert_eq!(text(&heap, cell).len(), 10_000);
        heap.release(cell);
        assert!(heap.chunks[cell.chunk as usize].is_none());
        let again = heap.string(&big).expect("room");
        assert_eq!(again.chunk, cell.chunk);
    }

    #[test]
    fn a_big_int_reads_back_with_its_sign() {
        let mut heap = Heap::default();
        let big: BigInt = "-123456789012345678901234567890".parse().expect("a number");
        let cell = heap.big_int(&big).expect("room");
        assert_eq!(heap.kind(cell), Some(Kind::BigInt));
        assert_eq!(heap.read_big_int(cell), big);
        let positive = -big;
        let cell = heap.big_int(&positive).expect("room");
        assert_eq!(heap.read_big_int(cell), positive);
    }

    #[test]
    fn a_string_is_only_its_own_text() {
        let mut heap = Heap::default();
        for text in ["", "a", "eight ch", "nine char", "a longer piece of text"] {
            let cell = heap.string(text.as_bytes()).expect("room");
            assert!(heap.string_is(cell, text.as_bytes()), "{text:?}");
            let mut other = text.as_bytes().to_vec();
            other.push(b'!');
            assert!(!heap.string_is(cell, &other), "{text:?} is not {other:?}");
            if let Some(last) = other.len().checked_sub(2) {
                other.truncate(last + 1);
                other[last] ^= 1;
                assert!(!heap.string_is(cell, &other), "{text:?} is not {other:?}");
            }
        }
    }

    #[test]
    fn a_constructor_reads_back_its_variant_and_fields() {
        let mut heap = Heap::default();
        let v = VariantRef {
            type_id: TypeId(263),
            variant_idx: 0,
        };
        let one = Value::int(1).expect("small");
        let cell = heap.ctor(v, &[one, Value::NIL]).expect("room");
        assert_eq!(heap.kind(cell), Some(Kind::Ctor));
        assert_eq!(heap.variant(cell), v);
        let fields: Vec<_> = heap.fields(cell).map(Value::view).collect();
        assert_eq!(fields, [one.view(), Value::NIL.view()]);
    }

    /// A chain of cells, each holding the next, is freed by one release, and
    /// without a Rust call per link.
    #[test]
    fn freeing_a_constructor_frees_what_it_holds() {
        let mut heap = Heap::default();
        let v = VariantRef {
            type_id: TypeId(1),
            variant_idx: 0,
        };
        let mut tail = Value::NIL;
        for _ in 0..1_000_000 {
            tail = Value::cell(heap.ctor(v, &[tail]).expect("room"));
        }
        let shared = heap.string(b"shared").expect("room");
        heap.share(shared);
        let head = heap.ctor(v, &[tail, Value::cell(shared)]).expect("room");
        assert_eq!(heap.live(), 1_000_002);
        heap.release(head);
        assert_eq!(heap.live(), 1, "only the string the test still holds");
        heap.release(shared);
        assert_eq!(heap.live(), 0);
    }

    /// A closure holds its captures the way a constructor holds its fields,
    /// and freeing it frees them.
    #[test]
    fn a_closure_holds_its_captures() {
        let mut heap = Heap::default();
        let s = heap.string(b"captured").expect("room");
        let cell = heap
            .closure(FuncIdx(9), &[Value::int(4).expect("small"), Value::cell(s)])
            .expect("room");
        assert_eq!(heap.kind(cell), Some(Kind::Closure));
        assert_eq!(heap.closure_func(cell), FuncIdx(9));
        assert_eq!(
            heap.capture(cell, 0).map(Value::view),
            Value::int(4).map(Value::view)
        );
        assert!(heap.capture(cell, 2).is_none());
        assert_eq!(heap.live(), 2);
        heap.release(cell);
        assert_eq!(heap.live(), 0);
    }

    #[test]
    fn a_tuple_holds_its_elements() {
        let mut heap = Heap::default();
        let s = heap.string(b"second").expect("room");
        let one = Value::int(1).expect("small");
        let cell = heap.tuple(&[one, Value::cell(s)]).expect("room");
        assert_eq!(heap.kind(cell), Some(Kind::Tuple));
        assert_eq!(heap.element(cell, 0).map(Value::view), Some(one.view()));
        assert!(heap.element(cell, 2).is_none());
        assert_eq!(heap.elements(cell).count(), 2);
        let empty = heap.tuple(&[]).expect("room");
        assert_eq!(heap.elements(empty).count(), 0);
        heap.release(empty);
        heap.release(cell);
        assert_eq!(heap.live(), 0);
    }

    #[test]
    fn a_cell_round_trips_through_48_bits() {
        let cell = Cell {
            chunk: (1 << CHUNK_BITS) - 1,
            word: (1 << WORD_BITS) - 1,
        };
        assert!(cell.bits() < 1 << 48);
        assert_eq!(Cell::from_bits(cell.bits()), cell);
    }
}
