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
//! kind and its size in words. A cell is freed when its last reference goes,
//! and its space is kept for the next cell of the same size, on a list that
//! runs through the freed cells themselves: each keeps the next one's index
//! in its second word, so no cell takes less than [`LEAST_WORDS`]. Freeing a
//! cell also gives up the references it holds, like a constructor's fields or
//! an array node's children, and so on down.
//!
//! A count is a signed number, and its sign says what it means:
//!
//! | count            | the cell                                            |
//! |------------------|-----------------------------------------------------|
//! | 1 and up         | has that many holders                               |
//! | 0                | is free, or is not one the heap has                 |
//! | [`UNCOUNTED`]    | is never freed, whoever holds it                    |
//! | any other below 0| is waiting to be freed, and names the next that is  |
//!
//! A share adds one to a count above 0, and a release takes one from a count
//! above 1 and frees at 1. Any other count is looked at apart, so finding a
//! freed cell costs those paths nothing: a holder of one gets [`DeadCell`],
//! whether it shares the cell, gives it up or asks if it is the only holder.
//! That holds until the space is handed out again as a new cell. From then on
//! the stale holder's cell is the new one, and only [`audit`] can tell. An
//! index past the arena's end, and index 0, read as a freed cell too.
//!
//! The arena is words, not bytes, and cells are named by index rather than
//! by address, so the heap needs no `unsafe`, a cell's name stays good when
//! the arena grows, and the heap moves with its process as plain data.

use num_bigint::{BigInt, Sign};
use scarlet_ir::TypeId;
use scarlet_ir::core_ir::{ConstId, FuncIdx, VariantRef};
use scarlet_ir::tivec::Idx;

use crate::value::Value;
use crate::word::{low_u16, low_u32, to_usize};

mod audit;

pub(crate) use audit::{AuditFailure, AuditFault};

/// The most words a heap holds: the arena's and the boxes' together, which
/// is 16 GiB. A cell's index then fits in the 31 bits below [`DYING`].
const LIMIT_WORDS: usize = DYING as usize;

/// The most words a cell takes in the arena, its header's among them. A
/// longer cell lives in a box.
const ARENA_CELL_WORDS: usize = 254;

/// The words a long cell takes in the arena: its header, then the number of
/// its box.
const STUB_WORDS: usize = 2;

/// The fewest words a cell takes in the arena, whatever its size: room for a
/// header and, once it is freed, the index of the next free cell.
const LEAST_WORDS: usize = 2;

/// How many words of a dying cell the free loop looks up at once, when there
/// are that many.
const STRIDE: usize = 8;

/// A header holds a cell's size in this many bits.
const SIZE_BITS: u32 = 28;

/// The most words one cell can take, its header's among them, in the arena
/// or in a box.
pub(crate) const MAX_CELL_WORDS: usize = (1 << SIZE_BITS) - 1;

/// The bits of a header that hold its count.
const COUNT: u64 = 0xFFFF_FFFF;

/// The count of a cell that is never freed and whose count never changes. A
/// constant's cell is made so ([`Heap::keep_constant`]). A count that
/// overflows lands here too, and stays, because a count that wrapped would
/// free a cell still in use. This is what Koka and Lean do with one that
/// overflows.
const UNCOUNTED: i32 = i32::MIN;

/// Set in the count of a cell waiting to be freed. The bits below are the
/// index of the next cell waiting, or [`NO_MORE`].
const DYING: u32 = 1 << 31;

/// The end of the list of cells waiting to be freed. It is the last index
/// the arena can have, and no cell is there: a cell takes two words. So the
/// count of a cell waiting is never [`UNCOUNTED`], which would name word 0.
const NO_MORE: u32 = DYING - 1;

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

/// A freed cell found where a holder takes another reference to it, gives its
/// reference up or asks whether it is the only holder. Something still held
/// the cell after its last reference went. The heap stops there instead of
/// counting in a cell that is gone, or freeing it a second time.
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
    /// The first free cell of each size, by the words it takes in the arena,
    /// or 0 when there is none. It grows when a size is first freed.
    free: Vec<u32>,
    /// The cell of each big Int constant this heap has been asked for, by the
    /// constant's number in the program, or 0.
    constants: Vec<u32>,
    /// Cells taken from the arena's end or a free list over this heap's
    /// life, and cells a constructor overwrote in place instead
    /// (`internal.sca`).
    made: usize,
    reused: usize,
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
            free: Vec::new(),
            constants: Vec::new(),
            made: 0,
            reused: 0,
            released: Vec::new(),
        }
    }
}

impl Heap {
    /// A new cell of `kind` with `first` and then `rest` after its header.
    /// Its count is 1, and the caller holds that one reference.
    // The way off a free list goes inside every maker of cells, so that a
    // cell made where one was freed costs no call.
    #[inline(always)]
    fn cell<const N: usize>(
        &mut self,
        kind: Kind,
        first: [u64; N],
        rest: impl ExactSizeIterator<Item = u64>,
    ) -> Result<Cell, Fault> {
        let n = N.saturating_add(rest.len());
        let Some((cell, words)) = self.pop(space(n.saturating_add(1))) else {
            return self.append(kind, first, rest);
        };
        if let Some((h, words)) = words.split_first_mut()
            && let Some((slots, words)) = words.split_first_chunk_mut::<N>()
        {
            *h = header(kind, n.saturating_add(1));
            *slots = first;
            for (slot, w) in words.iter_mut().zip(rest) {
                *slot = w;
            }
        }
        self.made = self.made.wrapping_add(1);
        Ok(cell)
    }

    /// [`Self::cell`] for a maker that writes the `n` words itself. Until it
    /// does they hold whatever was there.
    fn blank(&mut self, kind: Kind, n: usize) -> Result<Cell, Fault> {
        let size = n.saturating_add(1);
        let Some((cell, words)) = self.pop(space(size)) else {
            return self.append(kind, [], std::iter::repeat_n(0, n));
        };
        if let Some(h) = words.first_mut() {
            *h = header(kind, size);
        }
        self.made = self.made.wrapping_add(1);
        Ok(cell)
    }

    /// The first free cell of `space` words, taken off its list, and those
    /// words, when there is one.
    #[inline(always)]
    fn pop(&mut self, space: usize) -> Option<(Cell, &mut [u64])> {
        let head = self.free.get_mut(space).filter(|head| **head != 0)?;
        let cell = Cell(*head);
        let words = self
            .arena
            .get_mut(cell.index()..cell.index().wrapping_add(space))?;
        *head = low_u32(*words.get(1)?);
        Some((cell, words))
    }

    /// [`Self::cell`] when no freed cell has the size. The cell goes at the
    /// arena's end, and each of its words is written once.
    #[inline(never)]
    fn append<const N: usize>(
        &mut self,
        kind: Kind,
        first: [u64; N],
        rest: impl ExactSizeIterator<Item = u64>,
    ) -> Result<Cell, Fault> {
        let size = N.saturating_add(rest.len()).saturating_add(1);
        if size > ARENA_CELL_WORDS {
            let words = self.gather(first, rest)?;
            return self.long(kind, words);
        }
        let at = self.room(space(size))?;
        let cell = Cell::at(at).ok_or(Fault::Full)?;
        let words = std::iter::once(header(kind, size)).chain(first);
        self.arena.extend(words.chain(rest));
        if size < LEAST_WORDS {
            self.arena.push(0);
        }
        self.made = self.made.wrapping_add(1);
        Ok(cell)
    }

    /// The index of the arena's end, where there is room for `space` words.
    fn room(&mut self, space: usize) -> Result<usize, Fault> {
        let at = self.arena.len();
        let end = at.saturating_add(space);
        if end > self.arena.capacity() || end.saturating_add(self.boxed) > self.limit {
            return self.grow(space);
        }
        Ok(at)
    }

    /// [`Self::room`] when the arena has to ask the system for it, or has
    /// reached the limit. The first cell of all comes here, because an empty
    /// arena has no room, and is put after word 0.
    #[cold]
    #[inline(never)]
    fn grow(&mut self, space: usize) -> Result<usize, Fault> {
        let len = self.arena.len();
        let more = space.saturating_add(usize::from(len == 0));
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
        if len == 0 {
            self.arena.push(0);
        }
        Ok(end.saturating_sub(space))
    }

    /// The words of a cell too long for the arena, in an allocation of
    /// exactly their size.
    #[cold]
    #[inline(never)]
    fn gather<const N: usize>(
        &self,
        first: [u64; N],
        rest: impl ExactSizeIterator<Item = u64>,
    ) -> Result<Vec<u64>, Fault> {
        let n = N.saturating_add(rest.len());
        let held = self.arena.len().saturating_add(self.boxed);
        if n >= MAX_CELL_WORDS || held.saturating_add(n) > self.limit {
            return Err(Fault::Full);
        }
        let mut words = Vec::new();
        words.try_reserve_exact(n).map_err(|_| Fault::Full)?;
        words.extend_from_slice(&first);
        words.extend(rest);
        Ok(words)
    }

    /// [`Self::cell`] for one too long for the arena, with `words` after its
    /// header. They go in a box, and the arena gets a stub: a header that
    /// gives the size as 0, then the box's number.
    #[cold]
    #[inline(never)]
    fn long(&mut self, kind: Kind, words: Vec<u64>) -> Result<Cell, Fault> {
        // The box is kept first, so that the stub's two words are tested
        // against the limit with it. A stub that is refused gives the box
        // back, and no cell is left half made.
        let number = self.keep(words.into_boxed_slice())?;
        let stub = [header(kind, 0), u64::from(number)];
        let cell = match self.pop(STUB_WORDS) {
            Some((cell, words)) => {
                words.copy_from_slice(&stub);
                cell
            }
            None => match self.room(STUB_WORDS) {
                Ok(at) => {
                    self.arena.extend_from_slice(&stub);
                    Cell::at(at).ok_or(Fault::Full)?
                }
                Err(full) => {
                    self.unbox(number);
                    return Err(full);
                }
            },
        };
        self.made = self.made.wrapping_add(1);
        Ok(cell)
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

    /// Take box `number` out of the table, and keep its place for the next
    /// box.
    fn unbox(&mut self, number: u32) -> Option<Box<[u64]>> {
        let words = self
            .outside
            .get_mut(number as usize)
            .and_then(Option::take)?;
        self.boxed = self.boxed.saturating_sub(words.len());
        self.spare.push(number);
        Some(words)
    }

    /// Add one reference to `cell`. A freed cell, and a cell the heap does
    /// not have, is [`DeadCell`].
    #[inline(always)]
    pub(crate) fn share(&mut self, cell: Cell) -> Result<(), DeadCell> {
        if let Some(h) = self.arena.get_mut(cell.index())
            && count(*h) > 0
        {
            // The count is the header's low bits, and one past the most it
            // can be is `UNCOUNTED`, so this carries into nothing else.
            *h = h.wrapping_add(1);
            return Ok(());
        }
        self.uncounted(cell)
    }

    /// `v` with one more reference, for keeping it somewhere new.
    pub(crate) fn own(&mut self, v: Value) -> Result<Value, DeadCell> {
        if let Some(cell) = v.as_cell() {
            self.share(cell)?;
        }
        Ok(v)
    }

    /// Drop one reference to `cell`, freeing it when that was the last.
    ///
    /// A freed cell, whether it is `cell` or something a dying cell held, is
    /// [`DeadCell`], and so is a cell the heap does not have. One whose
    /// space has been handed out again is not found: this takes a reference
    /// from the new cell there.
    // The interpreter releases the old value of every register it writes, and
    // most are not cells. Inside its loop this took registers the loop needs,
    // and cost more at every instruction than the call costs here. Marked
    // cold, the call takes no register from the paths that do not make it.
    #[cold]
    #[inline(never)]
    pub(crate) fn release(&mut self, cell: Cell) -> Result<(), DeadCell> {
        let Some(slot) = self.arena.get_mut(cell.index()) else {
            return Err(DeadCell);
        };
        let h = *slot;
        if count(h) > 1 {
            *slot = h.wrapping_sub(1);
            return Ok(());
        }
        if count(h) != 1 {
            return self.uncounted(cell);
        }
        // A string, a binary or a big Int holds nothing, and freeing one is
        // only putting it on a list.
        if RAW >> kind_bits(h) & 1 == 1 && size(h) != 0 {
            self.shelve(cell, space(size(h)));
            return Ok(());
        }
        self.free(cell, h)
    }

    /// What a share or a release comes to when `cell`'s count is not above 0:
    /// nothing for a cell that is never counted, and [`DeadCell`] for any
    /// other.
    #[cold]
    #[inline(never)]
    fn uncounted(&self, cell: Cell) -> Result<(), DeadCell> {
        if count(self.header(cell)) == UNCOUNTED {
            Ok(())
        } else {
            Err(DeadCell)
        }
    }

    /// Free `cell`, whose header is `h` and whose last reference is given up,
    /// and every cell that then has no holder.
    ///
    /// A cell whose last reference goes on the way waits on a list that runs
    /// through the waiting cells' own counts. So freeing a list of a million
    /// cells takes no more stack than freeing one, and no storage at all.
    /// This is Lean's `lean_del_core`.
    #[inline(never)]
    fn free(&mut self, cell: Cell, h: u64) -> Result<(), DeadCell> {
        let (mut cell, mut h, mut todo) = (cell, h, NO_MORE);
        loop {
            match size(h).checked_sub(1) {
                Some(len) => {
                    let held = held(kind_bits(h), len);
                    if held.is_empty() && kind_bits(h) == Kind::Handle as u64 {
                        self.released.push(self.word(cell, 1));
                    }
                    let first = cell.index().wrapping_add(1);
                    let mut at = first.wrapping_add(held.start);
                    let end = first.wrapping_add(held.end);
                    // Giving one word up may write anywhere in the arena, so
                    // each lookup has to be made again after it. A wide cell
                    // is copied out a stride at a time to make fewer.
                    while let Some(words) = self
                        .arena
                        .get(at..end)
                        .and_then(|rest| rest.first_chunk::<STRIDE>().copied())
                    {
                        for w in words {
                            todo = self.let_go(w, todo)?;
                        }
                        at = at.wrapping_add(STRIDE);
                    }
                    while at < end
                        && let Some(w) = self.arena.get(at)
                    {
                        todo = self.let_go(*w, todo)?;
                        at = at.wrapping_add(1);
                    }
                }
                None => todo = self.empty_box(cell, h, todo)?,
            }
            self.shelve(cell, space(size(h)));
            if todo == NO_MORE {
                return Ok(());
            }
            cell = Cell(todo);
            h = self.header(cell);
            // Only a cell that holds itself is here and freed already.
            if count(h) >= 0 {
                return Err(DeadCell);
            }
            todo = low_u32(h) & !DYING;
        }
    }

    /// Give up the reference in `w`, a word a dying cell held, if it has one.
    /// When that was the last to its cell, the cell goes on the list of the
    /// dying, whose first is `todo`. Gives the list's first cell.
    #[inline(always)]
    fn let_go(&mut self, w: u64, todo: u32) -> Result<u32, DeadCell> {
        let Some(child) = Value::from_bits(w).as_cell() else {
            return Ok(todo);
        };
        if let Some(h) = self.arena.get_mut(child.index()) {
            let c = count(*h);
            if c > 1 {
                *h = h.wrapping_sub(1);
                return Ok(todo);
            }
            if c == 1 {
                *h = *h & !COUNT | u64::from(DYING | todo);
                return Ok(child.0);
            }
        }
        self.uncounted(child)?;
        Ok(todo)
    }

    /// [`Self::free`]'s part for the long cell `cell`, whose header is `h`:
    /// give up what is in its box, and the box.
    #[cold]
    #[inline(never)]
    fn empty_box(&mut self, cell: Cell, h: u64, todo: u32) -> Result<u32, DeadCell> {
        let words = self.slot(cell, h).and_then(|number| self.unbox(number));
        let words = words.unwrap_or_default();
        let held = held(kind_bits(h), words.len());
        let mut todo = todo;
        for w in words.get(held).unwrap_or(&[]) {
            todo = self.let_go(*w, todo)?;
        }
        Ok(todo)
    }

    /// Put the freed cell `cell`, which takes `space` words, at the front of
    /// its free list.
    #[inline(always)]
    fn shelve(&mut self, cell: Cell, space: usize) {
        let words = cell.index()..cell.index().wrapping_add(LEAST_WORDS);
        if let Some(head) = self.free.get_mut(space)
            && let Some([h, next]) = self.arena.get_mut(words)
        {
            *h = 0;
            *next = free_link(space, *head);
            *head = cell.0;
        } else {
            self.shelve_first(cell, space);
        }
    }

    /// [`Self::shelve`] for a size the table of free lists does not reach
    /// yet.
    #[cold]
    #[inline(never)]
    fn shelve_first(&mut self, cell: Cell, space: usize) {
        if self.free.len() <= space && self.arena.len() > cell.index() {
            self.free.resize(space.saturating_add(1), 0);
            self.shelve(cell, space);
        }
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
        if !self.unique(cell)? {
            return Ok(false);
        }
        let h = self.header(cell);
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

    /// The cell of big Int constant `id`, when [`Self::keep_constant`] has
    /// been given one.
    pub(crate) fn constant(&self, id: ConstId) -> Option<Cell> {
        let cell = self.constants.get(id.index()).copied()?;
        (cell != 0).then_some(Cell(cell))
    }

    /// Keep `cell` as the cell of big Int constant `id`. The heap takes the
    /// caller's one reference, and from here on `cell` is never freed, so any
    /// number of holders may come and go without a count.
    pub(crate) fn keep_constant(&mut self, id: ConstId, cell: Cell) -> Result<(), Fault> {
        let len = id.index().saturating_add(1);
        if self.constants.len() < len {
            let more = len.saturating_sub(self.constants.len());
            self.constants.try_reserve(more).map_err(|_| Fault::Full)?;
            self.constants.resize(len, 0);
        }
        if let Some(kept) = self.constants.get_mut(id.index()) {
            *kept = cell.0;
        }
        self.recount(cell, UNCOUNTED);
        Ok(())
    }

    /// Give up the heap's reference to each constant it kept. The run is
    /// over, so nothing else holds one.
    pub(crate) fn let_go_of_constants(&mut self) -> Result<(), DeadCell> {
        for cell in std::mem::take(&mut self.constants) {
            if cell != 0 {
                self.recount(Cell(cell), 1);
                self.release(Cell(cell))?;
            }
        }
        Ok(())
    }

    /// Make `cell`'s count `c`.
    fn recount(&mut self, cell: Cell, c: i32) {
        if let Some(h) = self.arena.get_mut(cell.index()) {
            *h = *h & !COUNT | u64::from(c.cast_unsigned());
        }
    }

    /// The handle numbers freed since this was last asked.
    pub(crate) fn take_released(&mut self) -> Vec<u64> {
        std::mem::take(&mut self.released)
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
        let words = text.chunks(8).map(padded_word);
        self.cell(Kind::String, [text.len() as u64], words)
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
        let negative = u64::from(sign == Sign::Minus) << 63;
        let first = [negative | digits.len() as u64];
        self.cell(Kind::BigInt, first, digits.into_iter())
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
        let negative = u64::from(n < 0) << 63;
        let first = [negative | digits.len() as u64];
        self.cell(Kind::BigInt, first, digits.iter().copied())
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
        self.cell(Kind::Ctor, ctor_tag(v), words_of(fields))
    }

    /// Perceus reuse. This writes the constructor `ctor` would build over a
    /// cell that [`Self::hollow`] emptied and [`Self::fits_ctor`] says is the
    /// right size. The cell's one reference stays and becomes the new
    /// constructor's.
    pub(crate) fn ctor_in(&mut self, cell: Cell, v: VariantRef, fields: &[Value]) {
        self.reused = self.reused.saturating_add(1);
        let [tag] = ctor_tag(v);
        self.set_word(cell, 1, tag);
        for (slot, w) in self.words_mut(cell, 2).iter_mut().zip(words_of(fields)) {
            *slot = w;
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
        self.cell(Kind::Closure, [u64::from(func.0)], words_of(captures))
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
        self.cell(kind, [], data.iter().copied())
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
        let c = count(self.header(cell));
        if c <= 0 {
            self.uncounted(cell)?;
        }
        Ok(c == 1)
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
        let new = self.blank(kind, len.saturating_add(room))?;
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
            let Some(mut target) = self.outside.get_mut(slot as usize).and_then(Option::take)
            else {
                return;
            };
            if let Some(source) = self.words(from, 1).get(src)
                && let Some(room) = target.get_mut(dst..dst.saturating_add(len))
            {
                room.copy_from_slice(source);
            }
            if let Some(place) = self.outside.get_mut(slot as usize) {
                *place = Some(target);
            }
        } else if let Some(slot) = self.slot(from, self.header(from)) {
            let Some(source) = self.outside.get_mut(slot as usize).and_then(Option::take) else {
                return;
            };
            if let Some(source) = source.get(src)
                && let Some(room) = self.words_mut(to, 1).get_mut(dst..dst.saturating_add(len))
            {
                room.copy_from_slice(source);
            }
            if let Some(place) = self.outside.get_mut(slot as usize) {
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
        let new = self.blank(kind, words)?;
        self.copy_words(cell, 0..words, new, 0);
        for i in held(kind_bits(h), words) {
            let Some(w) = self.words(new, 1).get(i) else {
                break;
            };
            if let Some(held) = Value::from_bits(*w).as_cell() {
                self.share(held)?;
            }
        }
        Ok(new)
    }

    /// Free `cell`, whose one holder has moved what it held somewhere else,
    /// so nothing it holds is released. A cell freed already is
    /// [`DeadCell`], and so is one waiting to be freed or never counted,
    /// which has no one holder.
    pub(crate) fn discard(&mut self, cell: Cell) -> Result<(), DeadCell> {
        let h = self.header(cell);
        if count(h) <= 0 {
            return Err(DeadCell);
        }
        if size(h) == 0
            && let Some(number) = self.slot(cell, h)
        {
            self.unbox(number);
        }
        self.shelve(cell, space(size(h)));
        Ok(())
    }

    /// A new tuple cell holding `elements`. Each element's reference passes
    /// to the cell.
    pub(crate) fn tuple(&mut self, elements: &[Value]) -> Result<Cell, Fault> {
        self.cell(Kind::Tuple, [], words_of(elements))
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

    /// How many words follow the header `h` of `cell`.
    fn data_len(&self, cell: Cell, h: u64) -> usize {
        match size(h).checked_sub(1) {
            Some(n) => n,
            None => self.long_words(cell, h, 1).len(),
        }
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
    fn slot(&self, cell: Cell, h: u64) -> Option<u32> {
        if size(h) != 0 || freed(h) {
            return None;
        }
        let number = self.arena.get(cell.index().saturating_add(1))?;
        Some(low_u32(*number))
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
            .and_then(|slot| self.outside.get(slot as usize)?.as_deref())
            .unwrap_or(&[])
    }

    #[cold]
    #[inline(never)]
    fn long_words_mut(&mut self, cell: Cell, h: u64, from: usize) -> &mut [u64] {
        self.slot(cell, h)
            .and_then(|slot| self.outside.get_mut(slot as usize)?.as_deref_mut())
            .and_then(|words| words.get_mut(from.saturating_sub(1)..))
            .unwrap_or(&mut [])
    }
}

/// The header of a new cell, whose count is 1. A header holds the count in
/// the low 32 bits, then the kind in 4 bits, then in the top [`SIZE_BITS`]
/// the cell's size in words, or 0 for a long cell, whose size is its box's.
fn header(kind: Kind, size: usize) -> u64 {
    1 | (kind as u64) << 32 | (size as u64) << 36
}

fn count(h: u64) -> i32 {
    low_u32(h).cast_signed()
}

/// The words a cell of `size` words takes in the arena, 0 being a long
/// cell's.
fn space(size: usize) -> usize {
    size.max(LEAST_WORDS)
}

/// The second word of a free cell of `space` words, when `next` is the next
/// one of that size. Nothing but an audit reads the size.
fn free_link(space: usize, next: u32) -> u64 {
    (space as u64) << 32 | u64::from(next)
}

/// What a constructor's cell holds before its fields.
fn ctor_tag(v: VariantRef) -> [u64; 1] {
    [u64::from(v.variant_idx) << 32 | u64::from(v.type_id.0.cast_unsigned())]
}

fn words_of(values: &[Value]) -> impl ExactSizeIterator<Item = u64> + '_ {
    values.iter().map(|v| v.bits())
}

fn kind_bits(h: u64) -> u64 {
    h >> 32 & 0xF
}

/// Whether `h` is the header of a freed cell, or of a cell the heap does not
/// have. Freeing zeroes the whole header, and a live cell's count is not 0.
fn freed(h: u64) -> bool {
    h & COUNT == 0
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

/// The kinds that hold no references and are no handle, a bit each.
const RAW: u64 = 1 << Kind::String as u64
    | 1 << Kind::BigInt as u64
    | 1 << Kind::Range as u64
    | 1 << Kind::Binary as u64;

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
    if let Ok(word) = bytes.try_into() {
        return u64::from_le_bytes(word);
    }
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
mod tests;
