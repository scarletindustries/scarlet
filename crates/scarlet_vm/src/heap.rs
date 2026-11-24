//! A process's heap, which is where values too big for one word live.
//!
//! The heap is one run of words, the arena, carved into cells, one value per
//! cell. A cell is named by the index of its header word in the arena, so
//! reaching one is a single bounds check. `docs/vm-design.md` ("how a heap
//! grows") has the rules and where they come from:
//!
//! - There is no arena until the first cell is needed.
//! - A new cell is taken from the free list for its size, and when that is
//!   empty it is added at the arena's end. The arena only grows.
//! - A cell of more than [`ARENA_CELL_WORDS`] words keeps two words in the
//!   arena, a header and the number of a box of its own that holds the rest.
//!   The box is freed when the cell is.
//!
//! Every cell starts with one header word holding its reference count, its
//! kind and its size in words. A cell is freed when its count reaches zero,
//! and its space is kept on a free list for the next cell of the same size.
//! Freeing a cell also gives up the references it holds, like a constructor's
//! fields or an array node's children, and so on down. Freeing a cell zeroes
//! its header, so a holder that gives up its reference and finds a zeroed
//! header has found a count that went wrong, and gets [`DeadCell`] instead
//! of a second free. That holds until the space is handed out again as a new
//! cell. From then on the stale holder's cell is the new one, and nothing
//! here can tell. An index past the arena's end, and index 0, read as a
//! freed cell too.
//!
//! The arena is words, not bytes, and cells are named by index rather than
//! by address, so the heap needs no `unsafe`, a cell's name stays good when
//! the arena grows, and the heap moves with its process as plain data.

use std::collections::HashMap;

use num_bigint::{BigInt, Sign};
use scarlet_ir::TypeId;
use scarlet_ir::core_ir::{FuncIdx, VariantRef};

use crate::value::Value;
use crate::word::{low_u16, low_u32, to_usize};

/// The most words a heap holds: the arena's and the boxes' together, which
/// is 16 GiB. A cell's index then fits in 31 bits.
const LIMIT_WORDS: usize = 1 << 31;

/// The most words a cell takes in the arena, its header's among them. A
/// longer cell lives in a box.
const ARENA_CELL_WORDS: usize = 254;

/// The words a long cell takes in the arena: its header, then the number of
/// its box.
const STUB_WORDS: usize = 2;

/// A header holds a cell's size in this many bits.
const SIZE_BITS: u32 = 28;

/// The most words one cell can take, its header's among them, in the arena
/// or in a box.
pub(crate) const MAX_CELL_WORDS: usize = (1 << SIZE_BITS) - 1;

/// A reference count that has reached this stays there, and its cell is never
/// freed. A count cannot grow past the 32 bits its header has for it, and a
/// count that wrapped would free a cell still in use. This is what Koka and
/// Lean do with a count that overflows.
const STICKY: u64 = 0xFFFF_FFFF;

/// A cell, named by the index of its header word in the heap's arena.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Cell(u32);

impl Cell {
    /// The cell as the low bits of a value word's payload.
    pub(crate) fn bits(self) -> u64 {
        u64::from(self.0)
    }

    pub(crate) fn from_bits(bits: u64) -> Cell {
        Cell(low_u32(bits))
    }

    /// The cell whose header is word `index` of the arena.
    fn at(index: usize) -> Option<Cell> {
        u32::try_from(index).ok().map(Cell)
    }

    fn index(self) -> usize {
        self.0 as usize
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
    /// Something the host holds for the program, like a socket: one word,
    /// the host's number for it. When the cell is freed, the number goes on
    /// [`Heap::take_released`]'s list, for the host to let go of.
    Handle = 15,
}

/// A freed cell found where a holder gives its reference up or asks whether
/// it is the only one. Something still held the cell after its last
/// reference went. The heap stops there instead of freeing the cell a second
/// time.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct DeadCell;

/// Why the heap could not do what it was asked.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Fault {
    /// It has no room. The heap is at its limit, or the system has no more
    /// memory to give it, or the cell asked for is longer than a cell can
    /// be. This is a limit of the machine, not a bug in the program.
    Full,
    /// See [`DeadCell`]. This one is a bug, in the VM or in the compiler.
    DeadCell,
}

impl From<DeadCell> for Fault {
    // No correct program takes this path. Saying so keeps the optimizer
    // from weighing each check for it as a likely way out of a loop.
    #[cold]
    #[inline(never)]
    fn from(_: DeadCell) -> Fault {
        Fault::DeadCell
    }
}

/// Sizes below this, in words, have their free list in a plain array. Almost
/// every cell is one of these: constructors, tuples, closures, array nodes
/// and short strings.
const DENSE_SIZES: usize = 64;

/// Freed cells, one list per size in words. The size is the key for both
/// halves. Small sizes index an array, so the alloc and free on every hot path
/// do no hashing, and the rare large size goes in a map.
#[derive(Default)]
struct FreeLists {
    dense: Vec<Vec<Cell>>,
    sparse: HashMap<usize, Vec<Cell>>,
}

impl FreeLists {
    fn pop(&mut self, size: usize) -> Option<Cell> {
        if size < DENSE_SIZES {
            self.dense.get_mut(size)?.pop()
        } else {
            self.sparse.get_mut(&size)?.pop()
        }
    }

    fn push(&mut self, size: usize, cell: Cell) {
        if size < DENSE_SIZES {
            if self.dense.len() <= size {
                self.dense.resize_with(DENSE_SIZES, Vec::new);
            }
            if let Some(list) = self.dense.get_mut(size) {
                list.push(cell);
            }
        } else {
            self.sparse.entry(size).or_default().push(cell);
        }
    }
}

pub(crate) struct Heap {
    /// Every cell's header, and after it the cell's other words, or for a
    /// long cell the number of its box. Its length is how much has been
    /// handed out. Word 0 is never a cell.
    arena: Vec<u64>,
    /// The words after the header of each long cell, by the number its stub
    /// holds. A box given back leaves `None`, and its number is reused by
    /// the next long cell made.
    outside: Vec<Option<Box<[u64]>>>,
    spare: Vec<u32>,
    /// Words in `outside`'s boxes. They count against `limit`.
    boxed: usize,
    /// The most words the arena and the boxes may hold between them.
    limit: usize,
    /// Freed cells, by the words they take in the arena, ready to be reused.
    free: FreeLists,
    /// Cells made and not yet freed. Every cell made adds one and every
    /// cell freed takes one off. A cell is only freed while its header is
    /// not the zeroed one freeing leaves ([`freed`]), which is once each time
    /// it is made, so this is exact and never goes below zero.
    live: usize,
    /// Cells taken from the arena's end or a free list over this heap's
    /// life, and cells a constructor overwrote in place instead
    /// (`internal.sca`).
    made: usize,
    reused: usize,
    /// The cells a [`Heap::release`] still has to give a reference up for.
    /// Kept between calls so a release does not allocate.
    dying: Vec<Cell>,
    /// The numbers of handle cells freed since the last
    /// [`Heap::take_released`].
    released: Vec<u64>,
}

impl Default for Heap {
    fn default() -> Heap {
        Heap {
            arena: Vec::new(),
            outside: Vec::new(),
            spare: Vec::new(),
            boxed: 0,
            limit: LIMIT_WORDS,
            free: FreeLists::default(),
            live: 0,
            made: 0,
            reused: 0,
            dying: Vec::new(),
            released: Vec::new(),
        }
    }
}

impl Heap {
    /// A heap that holds at most `limit` words.
    #[cfg(test)]
    fn limited(limit: usize) -> Heap {
        Heap {
            limit: limit.min(LIMIT_WORDS),
            ..Heap::default()
        }
    }

    /// A new cell of `kind` with `fixed` and then `n` words after its header.
    /// Its count is 1, and the caller holds that one reference.
    fn alloc(&mut self, kind: Kind, fixed: usize, n: usize) -> Result<Cell, Fault> {
        let size = n
            .checked_add(fixed)
            .and_then(|data| data.checked_add(1))
            .filter(|size| *size <= MAX_CELL_WORDS)
            .ok_or(Fault::Full)?;
        // No free list has a long cell: its stub is freed as two words.
        let cell = if size > ARENA_CELL_WORDS {
            self.long(kind, size)?
        } else {
            let cell = match self.free.pop(size) {
                Some(cell) => cell,
                None => self.carve(size)?,
            };
            self.set_header(cell, header(1, kind, size));
            cell
        };
        self.live = self.live.wrapping_add(1);
        self.made = self.made.saturating_add(1);
        Ok(cell)
    }

    /// `size` new words at the arena's end.
    fn carve(&mut self, size: usize) -> Result<Cell, Fault> {
        let at = self.arena.len();
        let end = at.saturating_add(size);
        if end > self.arena.capacity() || end.saturating_add(self.boxed) > self.limit {
            return self.grow(size);
        }
        self.arena.resize(end, 0);
        Cell::at(at).ok_or(Fault::Full)
    }

    /// [`Self::carve`] when the arena has to ask the system for room first,
    /// or has reached the limit. The first cell of all comes here, because
    /// an empty arena has no room, and is put after word 0.
    #[cold]
    #[inline(never)]
    fn grow(&mut self, size: usize) -> Result<Cell, Fault> {
        let len = self.arena.len();
        let more = size.saturating_add(usize::from(len == 0));
        let end = len.saturating_add(more);
        if end.saturating_add(self.boxed) > self.limit {
            return Err(Fault::Full);
        }
        let room = self.arena.capacity();
        if end > room
            && !growth(len, room, more, self.limit)
                .any(|extra| self.arena.try_reserve_exact(extra).is_ok())
        {
            return Err(Fault::Full);
        }
        let at = end.saturating_sub(size);
        self.arena.resize(end, 0);
        Cell::at(at).ok_or(Fault::Full)
    }

    /// A new cell of `size` words, which is too many for the arena. Its
    /// words after the header go in a box, zeroed, and the arena gets a stub:
    /// a header that gives the size as 0, then the box's number.
    #[cold]
    #[inline(never)]
    fn long(&mut self, kind: Kind, size: usize) -> Result<Cell, Fault> {
        let n = size.saturating_sub(1);
        let held = self.arena.len().saturating_add(self.boxed);
        if held.saturating_add(n) > self.limit {
            return Err(Fault::Full);
        }
        let mut words = Vec::new();
        words.try_reserve_exact(n).map_err(|_| Fault::Full)?;
        words.resize(n, 0);
        // The box is kept first, so that the stub's two words are tested
        // against the limit with it. A stub that is refused gives the box
        // back, and no cell is left half made.
        let number = self.keep(words.into_boxed_slice())?;
        let stub = match self.free.pop(STUB_WORDS) {
            Some(stub) => stub,
            None => match self.carve(STUB_WORDS) {
                Ok(stub) => stub,
                Err(full) => {
                    self.let_go(number as usize);
                    return Err(full);
                }
            },
        };
        if let Some([h, second]) = self
            .arena
            .get_mut(stub.index()..)
            .and_then(|w| w.get_mut(..2))
        {
            *h = header(1, kind, 0);
            *second = u64::from(number);
        }
        Ok(stub)
    }

    /// Put `words` in the table of boxes, in a place a freed box left or in
    /// a new one, and give that place's number.
    fn keep(&mut self, words: Box<[u64]>) -> Result<u32, Fault> {
        self.boxed = self.boxed.saturating_add(words.len());
        if let Some(number) = self.spare.pop()
            && let Some(place) = self.outside.get_mut(number as usize)
        {
            *place = Some(words);
            return Ok(number);
        }
        let room = self.outside.try_reserve(1);
        let (Ok(number), Ok(())) = (u32::try_from(self.outside.len()), room) else {
            self.boxed = self.boxed.saturating_sub(words.len());
            return Err(Fault::Full);
        };
        self.outside.push(Some(words));
        Ok(number)
    }

    /// Free the box in place `slot`, and keep the place's number for the
    /// next box.
    fn let_go(&mut self, slot: usize) {
        let (Some(words), Ok(number)) = (
            self.outside.get_mut(slot).and_then(Option::take),
            u32::try_from(slot),
        ) else {
            return;
        };
        self.boxed = self.boxed.saturating_sub(words.len());
        self.spare.push(number);
    }

    /// Add one reference to `cell`.
    pub(crate) fn share(&mut self, cell: Cell) {
        let h = self.header(cell);
        let c = count(h);
        if c < STICKY {
            self.set_header(cell, with_count(h, c.saturating_add(1)));
        }
    }

    /// Drop one reference to `cell`, freeing it when that was the last.
    ///
    /// A freed cell's fields each lose a reference in turn. We keep them on a
    /// list instead of releasing them by recursion, so freeing a very long
    /// list takes no more stack than freeing one cell.
    ///
    /// A freed cell, whether it is `cell` or something a dying cell held, is
    /// [`DeadCell`], and so is a cell the heap does not have. One whose
    /// space has been handed out again is not found: this takes a reference
    /// from the new cell there.
    pub(crate) fn release(&mut self, cell: Cell) -> Result<(), DeadCell> {
        // Most releases leave the cell alive, and need no list of the dying.
        let h = self.header(cell);
        let c = count(h);
        if c > 1 {
            if c < STICKY {
                self.set_header(cell, with_count(h, c.saturating_sub(1)));
            }
            return Ok(());
        }
        let mut dying = std::mem::take(&mut self.dying);
        dying.push(cell);
        while let Some(cell) = dying.pop() {
            let h = self.header(cell);
            let c = count(h);
            if c > 1 {
                if c < STICKY {
                    self.set_header(cell, with_count(h, c.saturating_sub(1)));
                }
                continue;
            }
            if freed(h) {
                return Err(DeadCell);
            }
            for w in self.held_words(cell, h) {
                if let Some(held) = Value::from_bits(*w).as_cell() {
                    dying.push(held);
                }
            }
            self.free_cell(cell, h);
        }
        self.dying = dying;
        Ok(())
    }

    /// Perceus's drop of a last reference. When nothing else holds `cell`,
    /// this gives up everything it holds and keeps the allocation, header and
    /// all, for a constructor of the same size to overwrite. It returns
    /// `false` when something else holds `cell`, which leaves it to an
    /// ordinary `release`, and [`DeadCell`] when nothing holds it at all.
    ///
    /// Each field is emptied before it is released, so a hollowed cell holds
    /// nothing and is safe to release again. Emptying here instead of at the
    /// constructor is what carries reuse down a chain. A callee sees its
    /// argument as the last reference only because its caller gave up its own
    /// first.
    pub(crate) fn hollow(&mut self, cell: Cell) -> Result<bool, DeadCell> {
        let h = self.header(cell);
        if freed(h) {
            return Err(DeadCell);
        }
        if count(h) != 1 {
            return Ok(false);
        }
        for i in held(kind_bits(h), self.data_len(cell, h)) {
            let Some(slot) = self.words_mut(cell, 1).get_mut(i) else {
                break;
            };
            let held = Value::from_bits(std::mem::replace(slot, Value::NIL.bits()));
            if let Some(c) = held.as_cell() {
                self.release(c)?;
            }
        }
        Ok(true)
    }

    /// Whether a constructor with `fields` fields may be written over `cell`.
    /// It must be a constructor cell of exactly that size, and nothing else
    /// may hold it. Both are true of every cell [`Self::hollow`] kept. We read
    /// the count again here anyway, because a register holds its cell until
    /// something overwrites it. So the cell a constructor finds there might
    /// not be the one that was kept for it. A freed cell does not fit, and
    /// the release its register then gets is what reports it.
    pub(crate) fn fits_ctor(&self, cell: Cell, fields: usize) -> bool {
        let h = self.header(cell);
        count(h) == 1
            && kind_bits(h) == Kind::Ctor as u64
            && self.data_len(cell, h).checked_sub(1) == Some(fields)
    }

    /// The handle numbers freed since this was last asked.
    pub(crate) fn take_released(&mut self) -> Vec<u64> {
        std::mem::take(&mut self.released)
    }

    /// Give back the space of `cell`, whose header is `h`, which the caller
    /// has checked is not a freed cell's.
    fn free_cell(&mut self, cell: Cell, h: u64) {
        if kind_bits(h) == Kind::Handle as u64 {
            self.released.push(self.word(cell, 1));
        }
        self.live = self.live.wrapping_sub(1);
        let size = match size(h) {
            0 => {
                self.unbox(cell, h);
                STUB_WORDS
            }
            size => size,
        };
        self.set_header(cell, 0);
        self.free.push(size, cell);
    }

    /// Give back the box of the long cell `cell`, whose header is `h`.
    #[cold]
    #[inline(never)]
    fn unbox(&mut self, cell: Cell, h: u64) {
        if let Some(slot) = self.slot(cell, h) {
            self.let_go(slot);
        }
    }

    pub(crate) fn kind(&self, cell: Cell) -> Option<Kind> {
        kind_of(self.header(cell))
    }

    /// `cell`'s kind and the words after its header, for a walk that wants
    /// both at every step.
    pub(crate) fn kind_and_data(&self, cell: Cell) -> (Option<Kind>, &[u64]) {
        (kind_of(self.header(cell)), self.words(cell, 1))
    }

    /// A new string cell holding `text`.
    pub(crate) fn string(&mut self, text: &[u8]) -> Result<Cell, Fault> {
        let cell = self.alloc(Kind::String, 1, text.len().div_ceil(8))?;
        self.set_word(cell, 1, text.len() as u64);
        for (slot, bytes) in self.words_mut(cell, 2).iter_mut().zip(text.chunks(8)) {
            *slot = padded_word(bytes);
        }
        Ok(cell)
    }

    /// A string cell's length in bytes.
    pub(crate) fn string_len(&self, cell: Cell) -> usize {
        to_usize(self.word(cell, 1))
    }

    /// Whether a string cell holds exactly `text`.
    pub(crate) fn string_is(&self, cell: Cell, text: &[u8]) -> bool {
        let words = self.words(cell, 2);
        self.string_len(cell) == text.len()
            && words.len() == text.len().div_ceil(8)
            && words
                .iter()
                .zip(text.chunks(8))
                .all(|(w, bytes)| *w == padded_word(bytes))
    }

    /// A string cell's bytes, appended to `out`.
    pub(crate) fn read_string(&self, cell: Cell, out: &mut Vec<u8>) {
        let len = to_usize(self.word(cell, 1));
        out.extend(
            self.words(cell, 2)
                .iter()
                .flat_map(|w| w.to_le_bytes())
                .take(len),
        );
    }

    /// A new big-int cell holding `n`.
    pub(crate) fn big_int(&mut self, n: &BigInt) -> Result<Cell, Fault> {
        let (sign, digits) = n.to_u64_digits();
        let cell = self.alloc(Kind::BigInt, 1, digits.len())?;
        let negative = u64::from(sign == Sign::Minus) << 63;
        self.set_word(cell, 1, negative | digits.len() as u64);
        for (slot, d) in self.words_mut(cell, 2).iter_mut().zip(&digits) {
            *slot = *d;
        }
        Ok(cell)
    }

    /// The Int in big Int cell `cell`, when it is one word long, as an
    /// `i128`. A wider one is `None`.
    pub(crate) fn big_int_word(&self, cell: Cell) -> Option<i128> {
        let w = self.word(cell, 1);
        if w & 0xFFFF_FFFF != 1 {
            return None;
        }
        let d = i128::from(self.word(cell, 2));
        if w >> 63 == 1 {
            d.checked_neg()
        } else {
            Some(d)
        }
    }

    /// A big Int cell for `n`, laid out as [`Self::big_int`] lays it out.
    /// `n` is past what a small Int holds, so it is never 0.
    pub(crate) fn big_int_i128(&mut self, n: i128) -> Result<Cell, Fault> {
        let m = n.unsigned_abs();
        let lo = u64::try_from(m & u128::from(u64::MAX)).unwrap_or(0);
        let hi = u64::try_from(m >> 64).unwrap_or(0);
        let digits: &[u64] = if hi == 0 { &[lo] } else { &[lo, hi] };
        let cell = self.alloc(Kind::BigInt, 1, digits.len())?;
        let negative = u64::from(n < 0) << 63;
        self.set_word(cell, 1, negative | digits.len() as u64);
        for (slot, d) in self.words_mut(cell, 2).iter_mut().zip(digits) {
            *slot = *d;
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
        let bytes: Vec<u8> = self
            .words(cell, 2)
            .iter()
            .take(len)
            .flat_map(|d| d.to_le_bytes())
            .collect();
        BigInt::from_bytes_le(sign, &bytes)
    }

    /// A new cell for constructor `v`, holding `fields`. Each field's
    /// reference passes to the cell.
    pub(crate) fn ctor(&mut self, v: VariantRef, fields: &[Value]) -> Result<Cell, Fault> {
        let cell = self.alloc(Kind::Ctor, 1, fields.len())?;
        self.write_ctor(cell, v, fields);
        Ok(cell)
    }

    /// Perceus reuse. This writes the constructor `ctor` would build over a
    /// cell that [`Self::hollow`] emptied and [`Self::fits_ctor`] says is the
    /// right size. The cell's one reference stays and becomes the new
    /// constructor's.
    pub(crate) fn ctor_in(&mut self, cell: Cell, v: VariantRef, fields: &[Value]) {
        self.reused = self.reused.saturating_add(1);
        self.write_ctor(cell, v, fields);
    }

    fn write_ctor(&mut self, cell: Cell, v: VariantRef, fields: &[Value]) {
        let tag = u64::from(v.variant_idx) << 32 | u64::from(v.type_id.0.cast_unsigned());
        self.set_word(cell, 1, tag);
        for (slot, f) in self.words_mut(cell, 2).iter_mut().zip(fields) {
            *slot = f.bits();
        }
    }

    /// Which constructor a constructor cell is.
    pub(crate) fn variant(&self, cell: Cell) -> VariantRef {
        let tag = self.word(cell, 1);
        VariantRef {
            type_id: TypeId(low_u32(tag).cast_signed()),
            variant_idx: low_u16(tag >> 32),
        }
    }

    /// Field `i` of a constructor cell, or `None` past its last. Reading it
    /// adds no reference.
    pub(crate) fn field(&self, cell: Cell, i: usize) -> Option<Value> {
        self.held(cell, i)
    }

    /// A new cell for function `func` over `captures`. Each capture's
    /// reference passes to the cell.
    pub(crate) fn closure(&mut self, func: FuncIdx, captures: &[Value]) -> Result<Cell, Fault> {
        let cell = self.alloc(Kind::Closure, 1, captures.len())?;
        self.set_word(cell, 1, u64::from(func.0));
        for (slot, c) in self.words_mut(cell, 2).iter_mut().zip(captures) {
            *slot = c.bits();
        }
        Ok(cell)
    }

    /// The function a closure cell runs.
    pub(crate) fn closure_func(&self, cell: Cell) -> FuncIdx {
        FuncIdx(low_u32(self.word(cell, 1)))
    }

    /// Capture `i` of a closure cell, or `None` past its last. Reading it adds
    /// no reference.
    pub(crate) fn capture(&self, cell: Cell, i: usize) -> Option<Value> {
        self.held(cell, i)
    }

    /// A new cell of `kind` holding `data` after its header, with one
    /// reference, which is the caller's. Any reference in `data` passes to the
    /// cell. This is for kinds whose layout lives outside the heap, like an
    /// array's nodes.
    // The optimizer put this inside every caller until the error type grew a
    // second case, which took it just past the size it will do that for.
    // Left out of line, every array and map node made pays for a call.
    #[inline(always)]
    pub(crate) fn make(&mut self, kind: Kind, data: &[u64]) -> Result<Cell, Fault> {
        let cell = self.alloc(kind, 0, data.len())?;
        for (slot, w) in self.words_mut(cell, 1).iter_mut().zip(data) {
            *slot = *w;
        }
        Ok(cell)
    }

    /// The words of `cell` after its header. They are empty for a cell the
    /// heap does not have.
    #[inline]
    pub(crate) fn data(&self, cell: Cell) -> &[u64] {
        self.words(cell, 1)
    }

    /// [`Self::data`] for a binary's readers.
    // A binary is as often long as short, and a parser reads one byte of it
    // at a time. So the way to a long cell's box is not marked cold here, as
    // it is in `words` for every other reader. A read of a long binary costs
    // about 25 machine instructions more through the cold one.
    #[inline]
    pub(crate) fn binary_data(&self, cell: Cell) -> &[u64] {
        let onward = self.arena.get(cell.index()..).unwrap_or(&[]);
        let h = onward.first().copied().unwrap_or(0);
        match onward.get(1..size(h)) {
            Some(words) => words,
            None => self.boxed(cell, h),
        }
    }

    /// Whether `cell` has one holder, who may then change it in place.
    /// Asking about a freed cell is [`DeadCell`].
    pub(crate) fn unique(&self, cell: Cell) -> Result<bool, DeadCell> {
        let h = self.header(cell);
        if freed(h) {
            return Err(DeadCell);
        }
        Ok(count(h) == 1)
    }

    /// The words of `cell` after its header, for its one holder to change.
    pub(crate) fn data_mut(&mut self, cell: Cell) -> &mut [u64] {
        self.words_mut(cell, 1)
    }

    /// A new cell of `kind` made from the first `n` words after `old`'s
    /// header with `edit` made to them, and `room` spare words after, holding
    /// `Nil`. The words move straight from one cell to the other. `old`'s
    /// one holder gives it up, and it is freed without anything it held
    /// being released: that is in the new cell now, apart from what the edit
    /// cut, which the caller has seen to.
    pub(crate) fn splice(
        &mut self,
        kind: Kind,
        old: Cell,
        n: usize,
        edit: Edit<'_>,
        room: usize,
    ) -> Result<Cell, Fault> {
        let Edit { cut, at, put } = edit;
        let n = n.min(self.data(old).len());
        let cut = cut.start.min(n)..cut.end.min(n).max(cut.start.min(n));
        let kept = n.saturating_sub(cut.len());
        let at = at.min(kept);
        let len = kept.saturating_add(put.len());
        let new = self.alloc(kind, 0, len.saturating_add(room))?;
        if let Some(spare) = self.data_mut(new).get_mut(len..) {
            spare.fill(Value::NIL.bits());
        }
        // What is kept is `old[..cut.start]` then `old[cut.end..]`. Copy it
        // in runs, leaving a gap for `put` at `at`.
        let mut done = 0;
        for run in [0..cut.start, cut.end..n] {
            let mut run = run;
            while !run.is_empty() {
                let gap = if done < at { at } else { usize::MAX };
                let take = run.len().min(gap.saturating_sub(done));
                let dst = if done < at {
                    done
                } else {
                    done.saturating_add(put.len())
                };
                let piece = run.start..run.start.saturating_add(take);
                self.copy_words(old, piece, new, dst);
                done = done.saturating_add(take);
                run = run.start.saturating_add(take)..run.end;
                if take == 0 {
                    break;
                }
            }
        }
        if let Some(slot) = self.data_mut(new).get_mut(at..at.saturating_add(put.len())) {
            slot.copy_from_slice(put);
        }
        self.discard(old)?;
        Ok(new)
    }

    /// Copy words `src` after `from`'s header to the words from `dst` after
    /// `to`'s, two different cells.
    fn copy_words(&mut self, from: Cell, src: std::ops::Range<usize>, to: Cell, dst: usize) {
        if size(self.header(from)) == 0 || size(self.header(to)) == 0 {
            return self.copy_long_words(from, src, to, dst);
        }
        let len = src.len();
        let s = from.index().saturating_add(1).saturating_add(src.start);
        let d = to.index().saturating_add(1).saturating_add(dst);
        if s.saturating_add(len) <= self.arena.len() && d.saturating_add(len) <= self.arena.len() {
            self.arena.copy_within(s..s.saturating_add(len), d);
        }
    }

    /// [`Self::copy_words`] when one of the cells is long, or both are. A
    /// long cell's box is taken out of its place while it is read or
    /// written, so that the other cell can be reached wherever it is.
    #[cold]
    #[inline(never)]
    fn copy_long_words(&mut self, from: Cell, src: std::ops::Range<usize>, to: Cell, dst: usize) {
        let len = src.len();
        if let Some(slot) = self.slot(to, self.header(to)) {
            let Some(mut target) = self.outside.get_mut(slot).and_then(Option::take) else {
                return;
            };
            if let Some(source) = self.words(from, 1).get(src)
                && let Some(room) = target.get_mut(dst..dst.saturating_add(len))
            {
                room.copy_from_slice(source);
            }
            if let Some(place) = self.outside.get_mut(slot) {
                *place = Some(target);
            }
        } else if let Some(slot) = self.slot(from, self.header(from)) {
            let Some(source) = self.outside.get_mut(slot).and_then(Option::take) else {
                return;
            };
            if let Some(source) = source.get(src)
                && let Some(room) = self.words_mut(to, 1).get_mut(dst..dst.saturating_add(len))
            {
                room.copy_from_slice(source);
            }
            if let Some(place) = self.outside.get_mut(slot) {
                *place = Some(source);
            }
        }
    }

    /// A new cell with the kind and words of `cell` and one reference, which
    /// is the caller's. Everything `cell` holds gets one more reference, for
    /// the copy. A freed cell has nothing to copy and is [`Fault::DeadCell`].
    /// This is for an array's nodes, which a write copies when something
    /// else holds them. It is not for a handle, whose number the host would
    /// then be asked to let go of twice.
    pub(crate) fn copy(&mut self, cell: Cell) -> Result<Cell, Fault> {
        let h = self.header(cell);
        let kind = kind_of(h).ok_or(Fault::DeadCell)?;
        let words = self.data_len(cell, h);
        let new = self.alloc(kind, 0, words)?;
        self.copy_words(cell, 0..words, new, 0);
        for i in held(kind_bits(h), words) {
            let Some(w) = self.words(new, 1).get(i) else {
                break;
            };
            if let Some(held) = Value::from_bits(*w).as_cell() {
                self.share(held);
            }
        }
        Ok(new)
    }

    /// Free `cell`, whose one holder has moved what it held somewhere else,
    /// so nothing it holds is released. A cell freed already is
    /// [`DeadCell`].
    // This is one test in front of `free_cell`, so it goes inside the caller,
    // which then makes one call and not two.
    #[inline]
    pub(crate) fn discard(&mut self, cell: Cell) -> Result<(), DeadCell> {
        let h = self.header(cell);
        if freed(h) {
            return Err(DeadCell);
        }
        self.free_cell(cell, h);
        Ok(())
    }

    /// A new tuple cell holding `elements`. Each element's reference passes
    /// to the cell.
    pub(crate) fn tuple(&mut self, elements: &[Value]) -> Result<Cell, Fault> {
        let cell = self.alloc(Kind::Tuple, 0, elements.len())?;
        for (slot, e) in self.words_mut(cell, 1).iter_mut().zip(elements) {
            *slot = e.bits();
        }
        Ok(cell)
    }

    /// Element `i` of a tuple cell, or `None` past its last. Reading it adds
    /// no reference.
    pub(crate) fn element(&self, cell: Cell, i: usize) -> Option<Value> {
        self.words(cell, 1).get(i).map(|w| Value::from_bits(*w))
    }

    /// A tuple cell's elements, in order. Reading one adds no reference.
    pub(crate) fn elements(&self, cell: Cell) -> impl Iterator<Item = Value> + '_ {
        self.words(cell, 1).iter().map(|w| Value::from_bits(*w))
    }

    /// Value `i` of the ones a constructor or closure cell holds after its
    /// tag word.
    fn held(&self, cell: Cell, i: usize) -> Option<Value> {
        self.words(cell, 2).get(i).map(|w| Value::from_bits(*w))
    }

    /// A constructor cell's fields, in order. Reading one adds no reference.
    pub(crate) fn fields(&self, cell: Cell) -> impl Iterator<Item = Value> + '_ {
        self.words(cell, 2).iter().map(|w| Value::from_bits(*w))
    }

    /// Cells not yet freed.
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

    /// `cell`'s header, or 0, a freed cell's, when the arena has no such
    /// word.
    fn header(&self, cell: Cell) -> u64 {
        self.arena.get(cell.index()).copied().unwrap_or(0)
    }

    fn set_header(&mut self, cell: Cell, h: u64) {
        if let Some(slot) = self.arena.get_mut(cell.index()) {
            *slot = h;
        }
    }

    /// How many words follow the header `h` of `cell`.
    fn data_len(&self, cell: Cell, h: u64) -> usize {
        match size(h).checked_sub(1) {
            Some(n) => n,
            None => self.long_words(cell, h, 1).len(),
        }
    }

    /// The words of `cell`, whose header is `h`, that hold values. Which
    /// they are comes from the header alone, so a cell that holds none, like
    /// a string, has none of its words looked up.
    #[inline(always)]
    fn held_words(&self, cell: Cell, h: u64) -> &[u64] {
        let Some(len) = size(h).checked_sub(1) else {
            return self.long_held_words(cell, h);
        };
        let held = held(kind_bits(h), len);
        if held.is_empty() {
            return &[];
        }
        let first = cell.index().saturating_add(1);
        self.arena
            .get(first.saturating_add(held.start)..first.saturating_add(held.end))
            .unwrap_or(&[])
    }

    #[cold]
    #[inline(never)]
    fn long_held_words(&self, cell: Cell, h: u64) -> &[u64] {
        let data = self.boxed(cell, h);
        data.get(held(kind_bits(h), data.len())).unwrap_or(&[])
    }

    /// Words `from` to the end of `cell`, counting its header as word 0.
    /// `from` is 1 or 2: the header itself is [`Self::header`]'s to read.
    // This and `words_mut` are a few instructions each. They go inside their
    // callers, so that a node `make` writes, or one an array reads to copy,
    // costs no call. Left to the optimizer, each array node made paid for
    // two.
    #[inline(always)]
    fn words(&self, cell: Cell, from: usize) -> &[u64] {
        let onward = self.arena.get(cell.index()..).unwrap_or(&[]);
        let h = onward.first().copied().unwrap_or(0);
        // A long cell's header gives its size as 0, so it has no such run
        // of words in the arena, and neither has a freed cell.
        match onward.get(from..size(h)) {
            Some(words) => words,
            None => self.long_words(cell, h, from),
        }
    }

    #[inline(always)]
    fn words_mut(&mut self, cell: Cell, from: usize) -> &mut [u64] {
        let h = self.header(cell);
        if size(h) == 0 {
            return self.long_words_mut(cell, h, from);
        }
        self.arena
            .get_mut(cell.index()..)
            .and_then(|onward| onward.get_mut(from..size(h)))
            .unwrap_or(&mut [])
    }

    /// Word `i` of `cell`, counting its header as word 0, for `i` of 1 or
    /// more. It is 0 past the cell's last word.
    fn word(&self, cell: Cell, i: usize) -> u64 {
        let h = self.header(cell);
        let word = if i < size(h) {
            self.arena.get(cell.index().saturating_add(i))
        } else {
            self.long_words(cell, h, i).first()
        };
        word.copied().unwrap_or(0)
    }

    fn set_word(&mut self, cell: Cell, i: usize, w: u64) {
        let h = self.header(cell);
        let slot = if i < size(h) {
            self.arena.get_mut(cell.index().saturating_add(i))
        } else {
            self.long_words_mut(cell, h, i).first_mut()
        };
        if let Some(slot) = slot {
            *slot = w;
        }
    }

    /// The number of the box that holds the words of `cell`, whose header
    /// is `h`, when it is a long cell.
    fn slot(&self, cell: Cell, h: u64) -> Option<usize> {
        if size(h) != 0 || freed(h) {
            return None;
        }
        let number = self.arena.get(cell.index().saturating_add(1))?;
        Some(to_usize(*number))
    }

    /// [`Self::words`] for a cell whose header `h` gives it no such words in
    /// the arena. For a long cell they are in its box. A freed cell, a cell
    /// the heap does not have and a cell shorter than `from` have none.
    #[cold]
    #[inline(never)]
    fn long_words(&self, cell: Cell, h: u64, from: usize) -> &[u64] {
        self.boxed(cell, h)
            .get(from.saturating_sub(1)..)
            .unwrap_or(&[])
    }

    /// The words in the box of `cell`, whose header is `h`, or none when it
    /// is not a long cell.
    fn boxed(&self, cell: Cell, h: u64) -> &[u64] {
        self.slot(cell, h)
            .and_then(|slot| self.outside.get(slot)?.as_deref())
            .unwrap_or(&[])
    }

    #[cold]
    #[inline(never)]
    fn long_words_mut(&mut self, cell: Cell, h: u64, from: usize) -> &mut [u64] {
        self.slot(cell, h)
            .and_then(|slot| self.outside.get_mut(slot)?.as_deref_mut())
            .and_then(|words| words.get_mut(from.saturating_sub(1)..))
            .unwrap_or(&mut [])
    }
}

/// A header word. It holds the count in the low 32 bits, then the kind in 4
/// bits, then in the top [`SIZE_BITS`] the cell's size in words, or 0 for a
/// long cell, whose size is its box's.
fn header(count: u32, kind: Kind, size: usize) -> u64 {
    u64::from(count) | (kind as u64) << 32 | (size as u64) << 36
}

fn count(h: u64) -> u64 {
    h & STICKY
}

/// `h` with its count replaced by `c`, which is at most [`STICKY`].
fn with_count(h: u64, c: u64) -> u64 {
    h & !STICKY | c & STICKY
}

fn kind_bits(h: u64) -> u64 {
    h >> 32 & 0xF
}

/// Whether `h` is the header of a freed cell, or of a cell the heap does not
/// have. Freeing zeroes the whole header and every live cell has a kind, so
/// the bits above the count say which it is. The count alone does not: a
/// share of a freed cell makes its count 1 again.
fn freed(h: u64) -> bool {
    h >> 32 == 0
}

/// Which of the `len` words after the header of a cell of this kind hold a
/// value, and so a reference. They are the words after a constructor's or
/// closure's tag word, all of a tuple or an array leaf, an array root's three
/// parts (after its length, height and offset), an array branch's children
/// (after its height and size table), every word of a map cell after its
/// first, and a binary slice's first word.
fn held(kind: u64, len: usize) -> std::ops::Range<usize> {
    let k = |want: Kind| kind == want as u64;
    if k(Kind::Ctor)
        || k(Kind::Closure)
        || k(Kind::Map)
        || k(Kind::MapNode)
        || k(Kind::MapCollision)
    {
        1..len
    } else if k(Kind::Tuple) || k(Kind::ArrayLeaf) {
        0..len
    } else if k(Kind::ArrayRoot) {
        3..len
    } else if k(Kind::ArrayBranch) {
        // A branch is its height, then a size table and the children, the
        // same number of each.
        len.saturating_sub(len.saturating_sub(1) / 2)..len
    } else if k(Kind::BinarySlice) {
        0..1
    } else {
        0..0
    }
}

fn size(h: u64) -> usize {
    (h >> 36) as usize
}

/// How many more words to ask the system for when an arena of `len` words
/// with room for `room` needs `more`, in the order to try them. The first
/// doubles the room, so that growing is rare, but never takes it past
/// `limit`. The rest are for when the system refuses: each is half the one
/// before, down to an eighth of the arena. When that is refused too the heap
/// is full, because growing can move the whole arena, and smaller steps
/// would move it once every few cells.
fn growth(len: usize, room: usize, more: usize, limit: usize) -> impl Iterator<Item = usize> {
    let most = room
        .saturating_mul(2)
        .max(4)
        .min(limit)
        .saturating_sub(len)
        .max(more);
    let least = (len / 8).max(more).min(most);
    std::iter::successors(Some(most), move |last| {
        (*last > least).then(|| (last / 2).max(least))
    })
}

/// Up to eight bytes of a string, little-endian, zero-padded to a whole word.
fn padded_word(bytes: &[u8]) -> u64 {
    let mut word = [0u8; 8];
    for (w, b) in word.iter_mut().zip(bytes) {
        *w = *b;
    }
    u64::from_le_bytes(word)
}

/// A change to a run of words: `cut` taken out, then `put` placed before
/// word `at` of what is left.
pub(crate) struct Edit<'a> {
    pub(crate) cut: std::ops::Range<usize>,
    pub(crate) at: usize,
    pub(crate) put: &'a [u64],
}

/// The kind header `h` names, or `None` for a header no cell has.
fn kind_of(h: u64) -> Option<Kind> {
    match kind_bits(h) {
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
        15 => Some(Kind::Handle),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(heap: &Heap, cell: Cell) -> String {
        let mut out = Vec::new();
        heap.read_string(cell, &mut out);
        String::from_utf8(out).expect("UTF-8")
    }

    fn v() -> VariantRef {
        VariantRef {
            type_id: TypeId(1),
            variant_idx: 0,
        }
    }

    /// A value word that is not a cell, and is different for each `i`.
    fn int(i: usize) -> Value {
        Value::int(i64::try_from(i).expect("small")).expect("small")
    }

    /// A heap asks the system for nothing until it has a cell to keep. Word
    /// 0 is kept back, so the first cell is at word 1.
    #[test]
    fn there_is_no_arena_until_the_first_cell() {
        let mut heap = Heap::default();
        assert_eq!(heap.arena.capacity(), 0);
        let cell = heap.string(b"hi").expect("room");
        assert_eq!(cell, Cell(1));
        // A header, the length, and one word of text.
        assert_eq!(heap.arena.len(), 1 + 3);
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
        heap.release(a).expect("a live cell");
        assert_eq!(heap.live(), 0);
        let b = heap.string(b"two").expect("room");
        assert_eq!(a, b);
    }

    #[test]
    fn a_shared_cell_lives_until_its_last_reference_goes() {
        let mut heap = Heap::default();
        let a = heap.string(b"kept").expect("room");
        heap.share(a);
        heap.release(a).expect("a live cell");
        assert_eq!(heap.live(), 1);
        assert_eq!(text(&heap, a), "kept");
        heap.release(a).expect("a live cell");
        assert_eq!(heap.live(), 0);
    }

    /// A cell that no free list has is added at the arena's end, so the
    /// arena is exactly as long as the cells carved from it, and a freed
    /// cell's space is taken again before the arena grows.
    #[test]
    fn the_arena_is_as_long_as_the_cells_carved_from_it() {
        let mut heap = Heap::default();
        let cells: Vec<Cell> = (0..2000)
            .map(|_| heap.string(b"a string of twenty-four!").expect("room"))
            .collect();
        // Each is a header, the length, and three words of text.
        assert_eq!(heap.arena.len(), 1 + 2000 * 5);
        for (i, cell) in cells.iter().enumerate() {
            assert_eq!(cell.index(), 1 + 5 * i);
        }
        for cell in &cells {
            heap.release(*cell).expect("a live cell");
        }
        for _ in 0..2000 {
            heap.string(b"another one of that size").expect("room");
        }
        assert_eq!(heap.arena.len(), 1 + 2000 * 5);
        assert_eq!(heap.made(), 4000);
    }

    /// The arena moves when it grows, and a cell's name is an index, so the
    /// name is still good afterwards.
    #[test]
    fn a_cell_keeps_its_name_when_the_arena_grows() {
        let mut heap = Heap::default();
        let first = heap.string(b"the first cell").expect("room");
        let before = heap.arena.capacity();
        let mut last = first;
        while heap.arena.capacity() < 100 * before {
            last = heap.string(b"more").expect("room");
        }
        assert_eq!(text(&heap, first), "the first cell");
        assert_eq!(text(&heap, last), "more");
        assert_eq!(heap.unique(first), Ok(true));
    }

    /// A cell of up to [`ARENA_CELL_WORDS`] words is in the arena, whichever
    /// half of the free lists its size is in. One word more and it is a
    /// stub and a box.
    #[test]
    fn the_longest_arena_cell_is_254_words() {
        let mut heap = Heap::default();
        let values: Vec<u64> = (0..ARENA_CELL_WORDS).map(|i| int(i).bits()).collect();
        let short = heap
            .make(Kind::ArrayLeaf, &values[..ARENA_CELL_WORDS - 1])
            .expect("room");
        assert_eq!(heap.arena.len(), 1 + ARENA_CELL_WORDS);
        assert_eq!((heap.boxed, heap.outside.len()), (0, 0));
        let long = heap.make(Kind::ArrayLeaf, &values).expect("room");
        assert_eq!(heap.arena.len(), 1 + ARENA_CELL_WORDS + STUB_WORDS);
        assert_eq!((heap.boxed, heap.outside.len()), (ARENA_CELL_WORDS, 1));
        assert_eq!(heap.data(short), &values[..ARENA_CELL_WORDS - 1]);
        assert_eq!(heap.data(long), values.as_slice());
        // Each goes back on the list for the words it took in the arena.
        heap.release(short).expect("a live cell");
        heap.release(long).expect("a live cell");
        assert_eq!(heap.live(), 0);
        let stub = heap.tuple(&[Value::NIL]).expect("room");
        assert_eq!(stub, long, "a tuple of one element is two words too");
        let again = heap
            .make(Kind::ArrayLeaf, &values[..ARENA_CELL_WORDS - 1])
            .expect("room");
        assert_eq!(again, short);
        assert_eq!(heap.arena.len(), 1 + ARENA_CELL_WORDS + STUB_WORDS);
    }

    /// A long cell takes two words of the arena and keeps the rest in a box.
    /// The box is freed when the cell is, and the stub's space and the box's
    /// number are both used again.
    #[test]
    fn a_long_cell_keeps_its_words_in_a_box_of_its_own() {
        let mut heap = Heap::default();
        let big = vec![b'x'; 10_000];
        let cell = heap.string(&big).expect("room");
        assert_eq!(heap.arena.len(), 1 + STUB_WORDS);
        assert_eq!(heap.boxed, 1 + 10_000usize.div_ceil(8));
        assert_eq!(heap.outside.len(), 1);
        assert_eq!(heap.kind(cell), Some(Kind::String));
        assert_eq!(heap.string_len(cell), 10_000);
        assert!(heap.string_is(cell, &big));
        assert_eq!(text(&heap, cell).len(), 10_000);
        heap.share(cell);
        assert_eq!(heap.unique(cell), Ok(false));
        heap.release(cell).expect("a live cell");
        assert_eq!(heap.unique(cell), Ok(true));
        heap.release(cell).expect("a live cell");
        assert_eq!((heap.live(), heap.boxed), (0, 0));
        assert_eq!(heap.outside, [None]);
        assert!(heap.data(cell).is_empty(), "a freed cell has no words");
        let again = heap.string(&big).expect("room");
        assert_eq!(again, cell);
        assert_eq!(heap.arena.len(), 1 + STUB_WORDS);
        assert_eq!(heap.outside.len(), 1);
        assert!(heap.string_is(again, &big));
    }

    /// Two long cells have a box each, and freeing the first leaves the
    /// second's words alone.
    #[test]
    fn each_long_cell_has_its_own_box() {
        let mut heap = Heap::default();
        let a = heap.string(&vec![b'a'; 5_000]).expect("room");
        let b = heap.string(&vec![b'b'; 6_000]).expect("room");
        assert_eq!(heap.outside.len(), 2);
        heap.release(a).expect("a live cell");
        assert!(heap.string_is(b, &vec![b'b'; 6_000]));
        // The next long cell takes the first one's stub and box number.
        let c = heap.string(&vec![b'c'; 7_000]).expect("room");
        assert_eq!(c, a);
        assert_eq!(heap.outside.len(), 2);
        assert!(heap.string_is(b, &vec![b'b'; 6_000]));
        assert!(heap.string_is(c, &vec![b'c'; 7_000]));
        assert_eq!(heap.boxed, 2 + 6_000 / 8 + 7_000 / 8);
    }

    /// A constructor too wide for the arena reads, is reused in place, and
    /// frees what it holds, the same as a narrow one.
    #[test]
    fn a_long_constructor_is_a_constructor() {
        let mut heap = Heap::default();
        let held = heap.string(b"held").expect("room");
        let mut fields: Vec<Value> = (0..300).map(int).collect();
        fields[299] = Value::cell(held);
        let cell = heap.ctor(v(), &fields).expect("room");
        assert_eq!(heap.arena.len(), 1 + 3 + STUB_WORDS);
        assert_eq!(heap.kind(cell), Some(Kind::Ctor));
        assert_eq!(heap.variant(cell), v());
        assert_eq!(heap.fields(cell).count(), 300);
        for (i, f) in fields.iter().enumerate() {
            assert_eq!(heap.field(cell, i).map(Value::view), Some(f.view()));
        }
        assert!(heap.field(cell, 300).is_none());
        // Hollowing gives up the string and keeps the cell, box and all.
        assert!(heap.fits_ctor(cell, 300));
        assert!(!heap.fits_ctor(cell, 299));
        assert_eq!(heap.hollow(cell), Ok(true));
        assert_eq!(heap.live(), 1);
        assert!(heap.fields(cell).all(|f| f.view() == Value::NIL.view()));
        let other = VariantRef {
            type_id: TypeId(7),
            variant_idx: 3,
        };
        heap.ctor_in(cell, other, &fields[..299]);
        assert_eq!(heap.variant(cell), other);
        assert_eq!(
            heap.field(cell, 298).map(Value::view),
            Some(int(298).view())
        );
        heap.release(cell).expect("a live cell");
        assert_eq!((heap.live(), heap.boxed), (0, 0));
    }

    #[test]
    fn a_long_tuple_and_a_long_closure_hold_what_they_were_given() {
        let mut heap = Heap::default();
        let held = heap.string(b"held").expect("room");
        heap.share(held);
        let mut values: Vec<Value> = (0..400).map(int).collect();
        values[0] = Value::cell(held);
        let tuple = heap.tuple(&values).expect("room");
        let closure = heap.closure(FuncIdx(9), &values).expect("room");
        assert_eq!(heap.outside.len(), 2);
        assert_eq!(heap.elements(tuple).count(), 400);
        assert_eq!(
            heap.element(tuple, 399).map(Value::view),
            Some(int(399).view())
        );
        assert!(heap.element(tuple, 400).is_none());
        assert_eq!(heap.closure_func(closure), FuncIdx(9));
        assert_eq!(
            heap.capture(closure, 399).map(Value::view),
            Some(int(399).view())
        );
        assert!(heap.capture(closure, 400).is_none());
        // Each holds the string once, so it is freed with the second.
        heap.release(tuple).expect("a live cell");
        assert_eq!(heap.unique(held), Ok(true));
        heap.release(closure).expect("a live cell");
        assert_eq!((heap.live(), heap.boxed), (0, 0));
    }

    #[test]
    fn a_long_big_int_reads_back() {
        let mut heap = Heap::default();
        let big: BigInt = -(BigInt::from(1) << 19_200usize);
        let cell = heap.big_int(&big).expect("room");
        assert_eq!(heap.outside.len(), 1);
        assert_eq!(heap.read_big_int(cell), big);
        assert_eq!(heap.big_int_word(cell), None);
        heap.release(cell).expect("a live cell");
        assert_eq!(heap.boxed, 0);
    }

    /// A long cell's one holder changes its words where they are.
    #[test]
    fn a_long_cell_is_written_in_its_box() {
        let mut heap = Heap::default();
        let cell = heap.make(Kind::Binary, &[0; 1000]).expect("room");
        heap.data_mut(cell)[999] = 7;
        heap.data_mut(cell)[0] = 5;
        assert_eq!(heap.data(cell).len(), 1000);
        assert_eq!((heap.data(cell)[0], heap.data(cell)[999]), (5, 7));
        let (kind, data) = heap.kind_and_data(cell);
        assert_eq!((kind, data.len()), (Some(Kind::Binary), 1000));
    }

    /// A binary's readers have their own way to its words, and it gives
    /// what `data` gives for a short cell, a long one, a freed one and one
    /// the heap does not have.
    #[test]
    fn binary_data_is_data() {
        let mut heap = Heap::default();
        let short = heap.make(Kind::Binary, &[16, 7]).expect("room");
        let long = heap.make(Kind::Binary, &[3; 400]).expect("room");
        let gone = heap.make(Kind::Binary, &[4; 400]).expect("room");
        heap.release(gone).expect("a live cell");
        for cell in [short, long, gone, Cell(0), Cell(u32::MAX)] {
            assert_eq!(heap.binary_data(cell), heap.data(cell));
        }
        assert_eq!(heap.binary_data(short), &[16, 7]);
        assert_eq!(heap.binary_data(long), &[3; 400]);
        assert!(heap.binary_data(gone).is_empty());
    }

    /// A long node is copied word for word into a box of its own, and what
    /// it holds gets one more reference each, as for a short one.
    #[test]
    fn a_copy_of_a_long_node_shares_what_it_holds() {
        let mut heap = Heap::default();
        let held = heap.string(b"held").expect("room");
        let mut data: Vec<u64> = (0..500).map(|i| int(i).bits()).collect();
        data[499] = Value::cell(held).bits();
        let node = heap.make(Kind::ArrayLeaf, &data).expect("room");
        let copy = heap.copy(node).expect("room");
        assert_ne!(copy, node);
        assert_eq!(heap.outside.len(), 2);
        assert_eq!(heap.data(copy), data.as_slice());
        assert_eq!(heap.unique(held), Ok(false));
        heap.release(node).expect("a live cell");
        assert_eq!(heap.unique(held), Ok(true));
        assert_eq!(heap.data(copy), data.as_slice());
        heap.release(copy).expect("a live cell");
        assert_eq!((heap.live(), heap.boxed), (0, 0));
    }

    /// A splice moves words between a short cell and a long one, either
    /// way, and between two long ones.
    #[test]
    fn a_splice_moves_words_into_and_out_of_a_box() {
        let mut heap = Heap::default();
        let words: Vec<u64> = (0..600).map(|i| int(i).bits()).collect();
        let put = [int(7000).bits(), int(7001).bits()];
        // Short to long: 200 words, two put in at 100, and 300 spare.
        let short = heap.make(Kind::MapNode, &words[..200]).expect("room");
        let edit = Edit {
            cut: 0..0,
            at: 100,
            put: &put,
        };
        let long = heap
            .splice(Kind::MapNode, short, 200, edit, 300)
            .expect("room");
        let mut want = words[..100].to_vec();
        want.extend(put);
        want.extend(&words[100..200]);
        want.resize(502, Value::NIL.bits());
        assert_eq!(heap.data(long), want.as_slice());
        assert_eq!((heap.live(), heap.outside.len()), (1, 1));
        // Long to long: the first 202 words, less two cut from the middle.
        let edit = Edit {
            cut: 100..102,
            at: 0,
            put: &[],
        };
        let longer = heap
            .splice(Kind::MapNode, long, 202, edit, 400)
            .expect("room");
        let mut want = words[..200].to_vec();
        want.resize(600, Value::NIL.bits());
        assert_eq!(heap.data(longer), want.as_slice());
        assert_eq!((heap.live(), heap.boxed), (1, 600));
        // Long to short: its first 50 words.
        let edit = Edit {
            cut: 50..200,
            at: 0,
            put: &[],
        };
        let back = heap
            .splice(Kind::MapNode, longer, 200, edit, 0)
            .expect("room");
        assert_eq!(heap.data(back), &words[..50]);
        assert_eq!((heap.live(), heap.boxed), (1, 0));
    }

    /// Freeing a long cell gives up the cells it holds, long ones among
    /// them, without a Rust call per cell.
    #[test]
    fn freeing_a_long_cell_frees_what_it_holds() {
        let mut heap = Heap::default();
        let mut tail = Value::NIL;
        for _ in 0..1000 {
            let mut values = vec![Value::NIL; 300];
            values[150] = tail;
            tail = Value::cell(heap.tuple(&values).expect("room"));
        }
        assert_eq!((heap.live(), heap.boxed), (1000, 300_000));
        let head = tail.as_cell().expect("a cell");
        heap.release(head).expect("a live cell");
        assert_eq!((heap.live(), heap.boxed), (0, 0));
        assert!(heap.outside.iter().all(Option::is_none));
    }

    /// The limit counts the arena's words and the boxes' together. A cell
    /// that would pass it is refused, and the heap is left as it was.
    #[test]
    fn a_heap_at_its_limit_is_full() {
        // Word 0, then room for four cells of three words.
        let mut heap = Heap::limited(1 + 4 * 3);
        let cells: Vec<Cell> = (0..4)
            .map(|_| heap.string(b"three").expect("room"))
            .collect();
        assert_eq!(heap.string(b"three"), Err(Fault::Full));
        assert_eq!(heap.tuple(&[]), Err(Fault::Full), "not even one word");
        assert_eq!((heap.live(), heap.made(), heap.arena.len()), (4, 4, 13));
        // A freed cell's space is still there to take.
        heap.release(cells[1]).expect("a live cell");
        assert_eq!(heap.string(b"again"), Ok(cells[1]));
        assert_eq!(heap.string(b"three"), Err(Fault::Full));
        assert_eq!(text(&heap, cells[0]), "three");
    }

    #[test]
    fn a_long_cell_counts_against_the_limit() {
        let mut heap = Heap::limited(1000);
        let first = heap.make(Kind::Binary, &[1; 600]).expect("room");
        let before = (heap.live(), heap.made(), heap.arena.len(), heap.boxed);
        assert_eq!(before, (1, 1, 3, 600));
        assert_eq!(heap.make(Kind::Binary, &[2; 600]), Err(Fault::Full));
        assert_eq!(
            (heap.live(), heap.made(), heap.arena.len(), heap.boxed),
            before
        );
        assert_eq!(heap.outside.len(), 1);
        // 397 more words fit beside it: a stub and 395 in a box.
        assert_eq!(heap.make(Kind::Binary, &[2; 396]), Err(Fault::Full));
        let second = heap.make(Kind::Binary, &[2; 395]).expect("room");
        assert_eq!(heap.arena.len() + heap.boxed, 1000);
        assert_eq!(heap.tuple(&[]), Err(Fault::Full));
        // Giving the first back makes room for the cell refused before.
        heap.release(first).expect("a live cell");
        let third = heap.make(Kind::Binary, &[3; 600]).expect("room");
        assert_eq!(heap.data(second), &[2; 395]);
        assert_eq!(heap.data(third), &[3; 600]);
    }

    /// The box fits under the limit and the stub's two words do not. The
    /// refusal leaves no box behind.
    #[test]
    fn a_long_cell_whose_stub_has_no_room_leaves_nothing_behind() {
        let mut heap = Heap::limited(300 + 2);
        assert_eq!(heap.make(Kind::Binary, &[0; 300]), Err(Fault::Full));
        assert_eq!((heap.live(), heap.made(), heap.boxed), (0, 0, 0));
        assert!(heap.arena.is_empty());
        assert!(heap.outside.iter().all(Option::is_none));
        let cell = heap.make(Kind::Binary, &[0; 299]).expect("room");
        assert_eq!(heap.data(cell).len(), 299);
        assert_eq!(heap.arena.len() + heap.boxed, 302);
    }

    /// A long cell is refused at its stub while a freed box's number is
    /// waiting to be used again. The number is still waiting afterwards, and
    /// the next long cell takes it.
    #[test]
    fn a_long_cell_refused_at_its_stub_gives_its_box_number_back() {
        let mut heap = Heap::limited(400);
        let first = heap.make(Kind::Binary, &[1; 300]).expect("room");
        heap.release(first).expect("a live cell");
        assert_eq!(heap.spare, [0]);
        // A two-word cell takes the freed stub, so the next stub is carved.
        let small = heap.tuple(&[Value::NIL]).expect("room");
        assert_eq!(small, first);
        // 3 words of arena and 396 of box fit under 400, and a stub does not.
        assert_eq!(heap.make(Kind::Binary, &[2; 396]), Err(Fault::Full));
        assert_eq!((heap.live(), heap.made(), heap.boxed), (1, 2, 0));
        assert_eq!(heap.spare, [0]);
        assert_eq!(heap.outside, [None]);
        let second = heap.make(Kind::Binary, &[2; 395]).expect("room");
        assert_eq!(heap.data(second), &[2; 395]);
        assert_eq!(heap.outside.len(), 1);
        assert!(heap.spare.is_empty());
        assert_eq!(heap.arena.len() + heap.boxed, 400);
    }

    /// The arena doubles its room until twice as much would pass the limit,
    /// and then takes room up to the limit and no further. Every word under
    /// the limit can still be used.
    #[test]
    fn the_arena_has_no_room_past_the_limit() {
        let mut heap = Heap::limited(1000);
        let mut cells = 0;
        // Each is a header, the length, and three words of text.
        while heap.string(b"three words of it").is_ok() {
            cells += 1;
            assert!(heap.arena.capacity() <= 1000, "after {cells} cells");
        }
        assert_eq!(cells, 199);
        assert_eq!(heap.arena.len(), 1 + 199 * 5);
        assert_eq!(heap.string(b"three words of it"), Err(Fault::Full));
    }

    /// What the arena asks the system for, in order. Only the first is
    /// asked for unless the system refuses it.
    #[test]
    fn the_arena_asks_for_less_when_the_system_refuses() {
        let steps = |len, room, more, limit| growth(len, room, more, limit).collect::<Vec<_>>();
        // The first cell gets room for itself and word 0, 4 words at least.
        assert_eq!(steps(0, 0, 3, LIMIT_WORDS), [4, 3]);
        assert_eq!(steps(0, 0, 4, LIMIT_WORDS), [4]);
        assert_eq!(steps(0, 0, 255, LIMIT_WORDS), [255]);
        // As much again, then half of that each time, down to an eighth of
        // the arena.
        assert_eq!(steps(1024, 1024, 5, LIMIT_WORDS), [1024, 512, 256, 128]);
        // A cell longer than the eighth is the least that will do.
        assert_eq!(steps(1024, 1024, 200, LIMIT_WORDS), [1024, 512, 256, 200]);
        // The doubling stops at the limit.
        assert_eq!(steps(1024, 1024, 5, 1500), [476, 238, 128]);
        assert_eq!(steps(1024, 1024, 5, 1100), [76]);
        // No step is less than what is needed now, or takes the arena past
        // the limit.
        for (len, more, limit) in [
            (1, 2, 3),
            (7, 254, 261),
            (900, 3, 1000),
            (1 << 20, 9, 1 << 21),
        ] {
            for extra in growth(len, len, more, limit) {
                assert!(
                    extra >= more && len + extra <= limit,
                    "{len} {more} {limit}"
                );
            }
        }
    }

    /// A cell longer than a cell can be is refused before anything is asked
    /// of the system.
    #[test]
    fn a_cell_past_the_longest_is_full() {
        let mut heap = Heap::default();
        assert_eq!(
            heap.alloc(Kind::Binary, 0, MAX_CELL_WORDS),
            Err(Fault::Full)
        );
        assert_eq!(heap.alloc(Kind::Binary, 1, usize::MAX), Err(Fault::Full));
        assert!(heap.arena.is_empty());
        assert_eq!((heap.live(), heap.made()), (0, 0));
    }

    /// A name the heap never gave out reads as a freed cell: index 0, an
    /// index past the arena's end, and the largest index a value word
    /// holds. Nothing panics, and no cell changes. A share of word 0 counts
    /// in it, as a share of any freed cell does, and it still reads as
    /// freed.
    #[test]
    fn a_cell_the_arena_does_not_have_is_a_dead_cell() {
        let mut heap = Heap::default();
        let real = heap.tuple(&[Value::NIL]).expect("room");
        let arena = heap.arena.clone();
        let len = u64::try_from(heap.arena.len()).expect("small");
        for bits in [0, len, len + 1, 1 << 31, u64::from(u32::MAX)] {
            let cell = Cell::from_bits(bits);
            assert_eq!(heap.kind(cell), None, "{bits}");
            assert!(heap.data(cell).is_empty());
            assert!(heap.data_mut(cell).is_empty());
            assert_eq!(heap.elements(cell).count(), 0);
            assert!(heap.field(cell, 0).is_none());
            assert_eq!(heap.string_len(cell), 0);
            assert_eq!(heap.unique(cell), Err(DeadCell));
            heap.share(cell);
            assert_eq!(heap.release(cell), Err(DeadCell));
            assert_eq!(heap.hollow(cell), Err(DeadCell));
            assert_eq!(heap.discard(cell), Err(DeadCell));
            assert_eq!(heap.copy(cell), Err(Fault::DeadCell));
            assert!(!heap.fits_ctor(cell, 0));
        }
        assert_eq!(heap.arena[1..], arena[1..]);
        assert!(freed(heap.arena[0]));
        assert_eq!(heap.live(), 1);
        assert_eq!(heap.unique(real), Ok(true));
    }

    /// A stub whose box number names no box has no words, and is freed
    /// like any other cell.
    #[test]
    fn a_stub_with_no_box_has_no_words() {
        let mut heap = Heap::default();
        let cell = heap.make(Kind::Binary, &[9; 300]).expect("room");
        heap.arena[cell.index() + 1] = 77;
        assert!(heap.data(cell).is_empty());
        assert!(heap.data_mut(cell).is_empty());
        assert_eq!(heap.release(cell), Ok(()));
        assert_eq!(heap.live(), 0);
        assert_eq!(heap.release(cell), Err(DeadCell));
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
        heap.release(head).expect("a live cell");
        assert_eq!(heap.live(), 1, "only the string the test still holds");
        heap.release(shared).expect("a live cell");
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
        heap.release(cell).expect("a live cell");
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
        heap.release(empty).expect("a live cell");
        heap.release(cell).expect("a live cell");
        assert_eq!(heap.live(), 0);
    }

    /// A copy of an array node is a new cell with the same kind and words
    /// and one holder. It adds one reference to each cell the node holds and
    /// to nothing else: a root's length and a branch's size table are plain
    /// numbers, even when one has the bits of a cell.
    #[test]
    fn a_copy_of_a_node_shares_what_the_node_holds_and_nothing_else() {
        let mut heap = Heap::default();
        let held = heap.string(b"held").expect("room");
        let number = heap.string(b"only a number's bits").expect("room");
        let (h, n) = (Value::cell(held).bits(), Value::cell(number).bits());
        let nil = Value::NIL.bits();
        for (kind, data) in [
            (Kind::ArrayLeaf, vec![nil, h, nil]),
            // Its height, two sizes, then two children.
            (Kind::ArrayBranch, vec![n, n, n, h, nil]),
            // Its length, height and offset, then the head, tree and tail.
            (Kind::ArrayRoot, vec![n, n, n, nil, h, nil]),
        ] {
            let node = heap.make(kind, &data).expect("room");
            heap.share(held);
            let made = heap.made();
            let copy = heap.copy(node).expect("room");
            assert_ne!(copy, node);
            assert_eq!(heap.made() - made, 1);
            assert_eq!(heap.kind(copy), Some(kind));
            assert_eq!(heap.data(copy), data.as_slice());
            assert_eq!(heap.unique(copy), Ok(true));
            assert_eq!(heap.unique(node), Ok(true));
            // The string has its first holder and the node's. Giving up
            // both nodes leaves just the first, so the copy held it once.
            heap.release(node).expect("a live cell");
            assert_eq!(heap.unique(held), Ok(false), "{kind:?}");
            heap.release(copy).expect("a live cell");
            assert_eq!(heap.unique(held), Ok(true), "{kind:?}");
            assert_eq!(heap.unique(number), Ok(true), "{kind:?}");
            assert_eq!(heap.live(), 2);
        }
        heap.release(held).expect("a live cell");
        heap.release(number).expect("a live cell");
        assert_eq!(heap.live(), 0);
    }

    /// A freed cell has nothing to copy. Asking makes no cell.
    #[test]
    fn a_copy_of_a_freed_cell_is_a_dead_cell() {
        let mut heap = Heap::default();
        let leaf = heap
            .make(Kind::ArrayLeaf, &[Value::NIL.bits()])
            .expect("room");
        heap.release(leaf).expect("a live cell");
        let made = heap.made();
        assert_eq!(heap.copy(leaf), Err(Fault::DeadCell));
        assert_eq!(heap.made(), made);
        assert_eq!(heap.live(), 0);
    }

    /// A cell given up once more than it was held is found at the second
    /// release, which frees nothing and changes no count.
    #[test]
    fn a_second_release_of_a_freed_cell_is_a_dead_cell() {
        let mut heap = Heap::default();
        let kept = heap.string(b"kept").expect("room");
        let a = heap.string(b"a longer string than kept").expect("room");
        assert_eq!(heap.release(a), Ok(()));
        assert_eq!(heap.live(), 1);
        assert_eq!(heap.release(a), Err(DeadCell));
        assert_eq!(heap.release(a), Err(DeadCell));
        assert_eq!(heap.live(), 1, "only the first release freed anything");
        // The cell went on its free list once, so the next two strings of
        // its size are two different cells.
        let b = heap.string(b"a longer string than kept").expect("room");
        let c = heap.string(b"a longer string than kept").expect("room");
        assert_eq!(a, b);
        assert_ne!(b, c);
        assert_eq!(text(&heap, kept), "kept");
    }

    /// A second release takes nothing off the live count, so one cell that
    /// is never freed and one that is released twice do not look like a heap
    /// with nothing left in it. This holds while the freed space has not
    /// been handed out again.
    #[test]
    fn a_doubled_release_leaves_the_live_count_alone() {
        let mut heap = Heap::default();
        let _leaked = heap.string(b"leaked").expect("room");
        let twice = heap.tuple(&[]).expect("room");
        assert_eq!(heap.release(twice), Ok(()));
        assert_eq!(heap.release(twice), Err(DeadCell));
        assert_eq!(heap.live(), 1);
    }

    /// A share of a freed cell does not bring it back. Its count reads 1
    /// again, but its header still says it was freed, so the unique test
    /// and the release that pairs with the share both stop, and nothing is
    /// freed a second time.
    #[test]
    fn a_freed_cell_that_is_shared_is_still_a_dead_cell() {
        let mut heap = Heap::default();
        let cell = heap.tuple(&[Value::NIL, Value::NIL]).expect("room");
        heap.release(cell).expect("a live cell");
        assert_eq!(heap.live(), 0);
        heap.share(cell);
        assert_eq!(heap.unique(cell), Err(DeadCell));
        assert_eq!(heap.hollow(cell), Err(DeadCell));
        assert_eq!(heap.discard(cell), Err(DeadCell));
        assert_eq!(heap.release(cell), Err(DeadCell));
        assert_eq!(heap.live(), 0, "the count did not go below zero");
        // The space went on its free list once.
        let b = heap.tuple(&[Value::NIL, Value::NIL]).expect("room");
        let c = heap.tuple(&[Value::NIL, Value::NIL]).expect("room");
        assert_eq!(cell, b);
        assert_ne!(b, c);
    }

    /// Shared twice, a freed cell passes its first release, which only
    /// counts down, and stops the second.
    #[test]
    fn a_freed_cell_shared_twice_is_found_at_its_last_release() {
        let mut heap = Heap::default();
        let cell = heap.string(b"gone").expect("room");
        heap.release(cell).expect("a live cell");
        heap.share(cell);
        heap.share(cell);
        assert_eq!(heap.release(cell), Ok(()));
        assert_eq!(heap.release(cell), Err(DeadCell));
        assert_eq!(heap.live(), 0);
    }

    /// The limit of the dead-cell test. Once the freed space is a new cell,
    /// a stale holder's release takes that cell's reference and frees it,
    /// and nothing is reported. If the new cell's own holder then never
    /// gives it up, the live count reads 0 with a leak and a doubled
    /// release behind it. `docs/vm-design.md` says what closes this.
    #[test]
    fn a_doubled_release_of_space_handed_out_again_is_not_found() {
        let mut heap = Heap::default();
        let twice = heap.tuple(&[]).expect("room");
        assert_eq!(heap.release(twice), Ok(()));
        let leaked = heap.tuple(&[]).expect("room");
        assert_eq!(leaked, twice, "the new cell is in the freed one's space");
        assert_eq!(heap.release(twice), Ok(()));
        assert_eq!(heap.live(), 0);
        // The new cell's holder does find it, if it ever lets go.
        assert_eq!(heap.release(leaked), Err(DeadCell));
    }

    /// A long cell's box is freed when the cell is. Its stub's header is
    /// zeroed like any other cell's, so a second release, a unique test and
    /// a copy all find it.
    #[test]
    fn a_second_release_of_a_long_cell_is_a_dead_cell() {
        let mut heap = Heap::default();
        let cell = heap.string(&vec![b'x'; 10_000]).expect("room");
        assert_eq!(heap.release(cell), Ok(()));
        assert_eq!(heap.release(cell), Err(DeadCell));
        assert_eq!(heap.unique(cell), Err(DeadCell));
        assert_eq!(heap.copy(cell), Err(Fault::DeadCell));
        heap.share(cell);
        assert_eq!(heap.release(cell), Err(DeadCell));
        assert_eq!((heap.live(), heap.boxed), (0, 0));
        // The stub went on its free list once, and the box number on its
        // list once.
        let b = heap.string(&vec![b'y'; 10_000]).expect("room");
        let c = heap.string(&vec![b'z'; 10_000]).expect("room");
        assert_eq!(b, cell);
        assert_ne!(c, b);
        assert_eq!(heap.outside.len(), 2);
    }

    /// Freeing a long cell that holds a freed cell stops the release, as
    /// it does for a short one.
    #[test]
    fn freeing_a_long_cell_that_holds_a_freed_cell_is_a_dead_cell() {
        let mut heap = Heap::default();
        let inner = heap.string(b"inner").expect("room");
        let mut values = vec![Value::NIL; 300];
        values[299] = Value::cell(inner);
        let outer = heap.tuple(&values).expect("room");
        assert_eq!(heap.release(inner), Ok(()));
        assert_eq!(heap.release(outer), Err(DeadCell));
    }

    /// Freeing a cell gives up what it holds, and a freed cell among those
    /// stops the release.
    #[test]
    fn freeing_a_cell_that_holds_a_freed_cell_is_a_dead_cell() {
        let mut heap = Heap::default();
        let inner = heap.string(b"inner").expect("room");
        let outer = heap.tuple(&[Value::cell(inner)]).expect("room");
        // The tuple holds the string's one reference, and this gives it up
        // behind the tuple's back.
        assert_eq!(heap.release(inner), Ok(()));
        assert_eq!(heap.release(outer), Err(DeadCell));
    }

    #[test]
    fn asking_whether_a_freed_cell_is_unique_is_a_dead_cell() {
        let mut heap = Heap::default();
        let cell = heap.string(b"one").expect("room");
        assert_eq!(heap.unique(cell), Ok(true));
        heap.share(cell);
        assert_eq!(heap.unique(cell), Ok(false));
        heap.release(cell).expect("a live cell");
        heap.release(cell).expect("a live cell");
        assert_eq!(heap.unique(cell), Err(DeadCell));
    }

    #[test]
    fn discarding_or_hollowing_a_freed_cell_is_a_dead_cell() {
        let mut heap = Heap::default();
        let v = VariantRef {
            type_id: TypeId(1),
            variant_idx: 0,
        };
        let cell = heap.ctor(v, &[Value::NIL]).expect("room");
        heap.share(cell);
        assert_eq!(heap.hollow(cell), Ok(false), "something else holds it");
        heap.release(cell).expect("a live cell");
        assert_eq!(heap.hollow(cell), Ok(true));
        assert_eq!(heap.discard(cell), Ok(()));
        assert_eq!(heap.live(), 0);
        assert_eq!(heap.hollow(cell), Err(DeadCell));
        assert_eq!(heap.discard(cell), Err(DeadCell));
        assert_eq!(heap.live(), 0);
    }

    /// A splice frees the cell it moves the words out of, so one of a freed
    /// cell is refused, and it reports that instead of a full heap.
    #[test]
    fn splicing_a_freed_cell_is_a_dead_cell() {
        let mut heap = Heap::default();
        let old = heap.make(Kind::MapNode, &[0, 1, 2]).expect("room");
        heap.release(old).expect("a live cell");
        let edit = Edit {
            cut: 0..0,
            at: 0,
            put: &[],
        };
        assert_eq!(
            heap.splice(Kind::MapNode, old, 3, edit, 0),
            Err(Fault::DeadCell)
        );
    }

    /// Every index the arena can have fits in the 31 bits a value word
    /// keeps for it, and comes back out the same.
    #[test]
    fn a_cell_round_trips_through_a_value_word() {
        for index in [1, 2, 1 << 20, LIMIT_WORDS - 1] {
            let cell = Cell::at(index).expect("an index");
            assert_eq!(cell.index(), index);
            assert!(cell.bits() < 1 << 31);
            assert_eq!(Cell::from_bits(cell.bits()), cell);
            assert_eq!(Value::cell(cell).as_cell(), Some(cell));
        }
        assert!(Cell::at(1 << 32).is_none());
    }
}
