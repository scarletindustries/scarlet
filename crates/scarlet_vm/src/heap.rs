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
//! space is kept on a free list for the next cell of the same size.
//!
//! Chunks are words, not bytes, and cells are named by chunk and word rather
//! than by address, so the heap needs no `unsafe` and moves with its process
//! as plain data.

use std::collections::HashMap;

use num_bigint::{BigInt, Sign};

/// Words in the first chunk: 2 KB.
const FIRST_CHUNK_WORDS: usize = 256;

/// How many chunks and words a [`Cell`] can name: 20 bits of chunk and 28 of
/// word, so it fits the 48 bits a value word has for it.
const CHUNK_BITS: u32 = 20;
const WORD_BITS: u32 = 28;

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
    pub(crate) fn release(&mut self, cell: Cell) {
        let h = self.word(cell, 0);
        if count(h) > 1 {
            self.set_word(cell, 0, h - 1);
            return;
        }
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
        match self.word(cell, 0) >> 32 & 0xFF {
            1 => Some(Kind::String),
            2 => Some(Kind::BigInt),
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

    /// Cells not yet freed.
    #[cfg(test)]
    pub(crate) fn live(&self) -> usize {
        self.live
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
    fn a_cell_round_trips_through_48_bits() {
        let cell = Cell {
            chunk: (1 << CHUNK_BITS) - 1,
            word: (1 << WORD_BITS) - 1,
        };
        assert!(cell.bits() < 1 << 48);
        assert_eq!(Cell::from_bits(cell.bits()), cell);
    }
}
