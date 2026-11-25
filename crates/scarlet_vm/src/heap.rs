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
//! Every cell starts with one header word:
//!
//! | bits     | what it is                                                  |
//! |----------|-------------------------------------------------------------|
//! | 0 to 31  | the cell's count                                            |
//! | 32 to 39 | its scan: how many of the words after the header are values |
//! | 40 to 47 | its size in words, the header's among them, or 0 for a cell |
//! |          | whose words are in a box                                    |
//! | 48 to 63 | its class: one of the kinds ([`Kind`]), or one of the       |
//! |          | program's constructors ([`Ctors`])                          |
//!
//! Where the scan is above 0, that many of the cell's words are values and the
//! rest are not. Where it is 0 the kind says which are ([`held`]): the cell
//! holds none, or they come after words that are no values, or they are in a
//! box.
//!
//! A cell is freed when its last reference goes, and its space is kept for the
//! next cell of the same size, on a list that runs through the freed cells
//! themselves: each keeps the next one's index in its second word, so no cell
//! takes less than [`LEAST_WORDS`]. Freeing a cell also gives up the references
//! it holds, like a constructor's fields or an array node's children, and so
//! on down.
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
use scarlet_ir::core_ir::{ConstId, FuncIdx, VariantRef};
use scarlet_ir::tivec::Idx;

use crate::value::Value;
use crate::word::{low_byte, low_u16, low_u32, to_usize};

mod audit;
mod ctors;

pub(crate) use audit::{AuditFailure, AuditFault};
use ctors::Class;
pub(crate) use ctors::{Ctor, Ctors, Layout, Numbering, TooManyCtors};

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

/// How many words of a cell that is freed or taken apart are looked up at once,
/// when there are that many.
pub(crate) const STRIDE: usize = 8;

/// The most words one cell can take, its header's among them, in the arena
/// or in a box. That is 2 GiB, an eighth of what a heap holds. It is how long
/// an Int and a binary can be ([`crate::bigint::MAX_BITS`],
/// [`crate::binary::MAX_BITS`]), so it says which programs stop.
pub(crate) const MAX_CELL_WORDS: usize = (1 << 28) - 1;

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

/// Where a cell's header is in the arena.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Index(u32);

impl Index {
    /// Word `at` of the arena.
    fn new(at: usize) -> Option<Index> {
        u32::try_from(at).ok().map(Index)
    }

    fn at(self) -> usize {
        self.0 as usize
    }
}

/// A cell, named by where its header is in the arena, and by its variant when
/// it is a constructor's. So which variant a value is can be told from the
/// word that holds it, with no look at the cell.
///
/// A cell that is no constructor's is named with variant 0. Only the heap makes
/// one of these, when it makes the cell or reads the word that holds it, so
/// the variant is always the one the cell was made with. Two that name one
/// cell are therefore the same.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct Cell(u64);

impl Cell {
    fn new(index: Index, variant: u16) -> Cell {
        Cell(u64::from(variant) << 32 | u64::from(index.0))
    }

    /// The cell as a value word's payload, in 48 bits.
    pub(crate) fn bits(self) -> u64 {
        self.0
    }

    /// The payload [`Self::bits`] gave.
    pub(crate) fn from_bits(bits: u64) -> Cell {
        Cell(bits)
    }

    fn index(self) -> Index {
        Index(low_u32(self.0))
    }

    fn variant(self) -> u16 {
        low_u16(self.0 >> 32)
    }
}

impl std::fmt::Debug for Cell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Cell").field(&self.index().0).finish()
    }
}

/// What a cell holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    // The first four hold no values and nothing of the host's, so freeing one
    // is only putting it on a list ([`Kind::RAW`]).
    /// UTF-8 text: a word holding its length in bytes, then the bytes.
    String = 1,
    /// An Int too big for a value word: a word holding its sign (bit 63) and
    /// how many 64-bit digits follow, then the digits, least significant
    /// first.
    BigInt = 2,
    /// A range `start..end` of Ints: its two ends, as two's complement
    /// words.
    Range = 3,
    /// A binary's bits ([`crate::binary`]).
    Binary = 4,
    /// Something the host holds for the program, like a socket: one word,
    /// the host's number for it. When the cell is freed, the number goes on
    /// [`Heap::take_released`]'s list, for the host to let go of.
    Handle = 5,
    /// A slice of another binary ([`crate::binary`]). It holds a reference to
    /// the binary it is a slice of, in its first word.
    BinarySlice = 6,
    /// A tuple: each element's value word, in order. Each element holds one
    /// reference.
    Tuple = 7,
    /// A function with captures: the function as a value word, then each
    /// capture's value word, in order. Each capture holds one reference. A
    /// function with none is a value word of its own, and needs no cell.
    Closure = 8,
    /// An array's root, one of its leaves, or one of its branches.
    /// [`crate::array`] has their layout; the heap only knows which of their
    /// words hold references.
    ArrayLeaf = 9,
    ArrayRoot = 10,
    ArrayBranch = 11,
    /// A map's root, one of its nodes, or a list of keys whose whole hashes
    /// are equal. [`crate::map`] has their layout. Each holds references in
    /// every word after its first.
    Map = 12,
    MapNode = 13,
    MapCollision = 14,
    /// A constructor with fields: each field's value word, in order. Each field
    /// holds one reference. Which constructor it is, is the header's class.
    Ctor = 16,
}

impl Kind {
    /// The classes of the kinds that hold no references and are no handle are
    /// this many, from 1 up.
    const RAW: u16 = 4;

    /// What a cell of this kind says of its values in its header.
    const fn scan(self) -> Scan {
        match self {
            Kind::Ctor | Kind::Tuple | Kind::Closure | Kind::ArrayLeaf => Scan::All,
            Kind::BinarySlice => Scan::First,
            Kind::String
            | Kind::BigInt
            | Kind::Range
            | Kind::Binary
            | Kind::Handle
            | Kind::ArrayRoot
            | Kind::ArrayBranch
            | Kind::Map
            | Kind::MapNode
            | Kind::MapCollision => Scan::Ask,
        }
    }
}

/// Which of the words after a cell's header its scan says are values.
#[derive(Clone, Copy)]
enum Scan {
    All,
    First,
    /// None, so the kind is asked.
    Ask,
}

impl Scan {
    /// The scan of a cell with `n` words after its header.
    #[inline(always)]
    fn of(self, n: usize) -> usize {
        match self {
            Scan::All => n,
            Scan::First => n.min(1),
            Scan::Ask => 0,
        }
    }
}

/// What a new cell's header says, but for its length.
#[derive(Clone, Copy)]
struct Make {
    class: Class,
    scan: Scan,
    /// The variant its name carries.
    variant: u16,
}

impl Make {
    #[inline(always)]
    const fn of(kind: Kind) -> Make {
        Make {
            class: Class::of(kind),
            scan: kind.scan(),
            variant: 0,
        }
    }

    /// The header of a new cell, whose count is 1, with `n` words after it in
    /// the arena.
    #[inline(always)]
    fn header(self, n: usize) -> u64 {
        let size = n.saturating_add(1) as u64;
        self.stub() | (self.scan.of(n) as u64) << 32 | size << 40
    }

    /// The header of a new cell whose words are in a box.
    #[inline(always)]
    fn stub(self) -> u64 {
        with_class(1, self.class)
    }
}

/// What a new cell that holds only values is, and the `N` values that come
/// before those it is given.
#[derive(Clone, Copy)]
pub(crate) struct Shape<const N: usize> {
    class: Class,
    variant: u16,
    first: [Value; N],
}

impl Shape<0> {
    pub(crate) const TUPLE: Shape<0> = Shape {
        class: Class::of(Kind::Tuple),
        variant: 0,
        first: [],
    };

    #[inline(always)]
    pub(crate) fn ctor(ctor: Ctor) -> Shape<0> {
        Shape {
            class: ctor.class(),
            variant: ctor.variant(),
            first: [],
        }
    }
}

impl Shape<1> {
    /// The shape of a closure that runs `func`, which is given its captures.
    #[inline(always)]
    pub(crate) fn closure(func: FuncIdx) -> Shape<1> {
        Shape {
            class: Class::of(Kind::Closure),
            variant: 0,
            first: [Value::func(func)],
        }
    }
}

impl<const N: usize> Shape<N> {
    #[inline(always)]
    fn make(self) -> Make {
        Make {
            class: self.class,
            scan: Scan::All,
            variant: self.variant,
        }
    }

    #[inline(always)]
    fn words(self) -> [u64; N] {
        self.first.map(Value::bits)
    }
}

/// What [`Heap::open`] found.
pub(crate) enum Opened<const N: usize> {
    /// The fields of a cell that nothing else held. It is freed, and their
    /// references are the caller's.
    Alone([Value; N]),
    /// The fields of a cell that something else holds too. It has lost a holder,
    /// and the references of its fields are its own still.
    Held(Cell, [Value; N]),
}

/// A freed cell found where a holder takes another reference to it, gives its
/// reference up or asks whether it is the only holder. Something still held
/// the cell after its last reference went. The heap stops there instead of
/// counting in a cell that is gone, or freeing it a second time.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct DeadCell;

/// Why the heap could not do what it was asked.
// It is as wide as a cell's name and a value, so that a `Result` of either and
// this comes back in two registers. A narrower one puts it in memory.
#[repr(u64)]
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Fault {
    /// It has no room. The heap is at its limit, or the system has no more
    /// memory to give it, or the cell asked for is longer than a cell can
    /// be. This is a limit of the machine, not a bug in the program.
    Full,
    /// See [`DeadCell`]. This one is a bug, in the VM or in the compiler.
    DeadCell,
}

/// Why [`Heap::copy_set`] made no copy.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum NotCopied {
    /// The cell is not laid out as it was said to be, or has no such field.
    Other,
    Refused(Fault),
}

impl From<Fault> for NotCopied {
    fn from(why: Fault) -> NotCopied {
        NotCopied::Refused(why)
    }
}

impl From<DeadCell> for NotCopied {
    fn from(dead: DeadCell) -> NotCopied {
        NotCopied::Refused(dead.into())
    }
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

/// A place in the table of boxes that nothing is in and no one else is given.
struct BoxPlace(u32);

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
    /// constant's number in the program.
    constants: Vec<Option<Cell>>,
    /// Cells taken from the arena's end or a free list over this heap's life
    /// (`internal.sca`).
    made: usize,
    /// The numbers of handle cells freed since the last
    /// [`Heap::take_released`].
    released: Vec<u64>,
    /// The program's constructors, by class.
    ctors: Ctors,
}

impl Heap {
    /// An empty heap for a program with these constructors.
    pub(crate) fn new(ctors: Ctors) -> Heap {
        Heap {
            arena: Vec::new(),
            outside: Vec::new(),
            spare: Vec::new(),
            boxed: 0,
            limit: LIMIT_WORDS,
            free: Vec::new(),
            constants: Vec::new(),
            made: 0,
            released: Vec::new(),
            ctors,
        }
    }

    /// A new cell with `first` and then `rest` after its header. Its count is
    /// 1, and the caller holds that one reference. A cell that is refused has
    /// asked `rest` for nothing.
    // The way off a free list goes inside every maker of cells, so that a
    // cell made where one was freed costs no call.
    #[inline(always)]
    fn cell<const N: usize>(
        &mut self,
        make: Make,
        first: [u64; N],
        rest: impl ExactSizeIterator<Item = u64>,
    ) -> Result<Cell, Fault> {
        let n = N.saturating_add(rest.len());
        match self.freed(make, n) {
            Some((cell, words)) => {
                write(words, first, rest);
                Ok(cell)
            }
            None => self.append(make, first, rest),
        }
    }

    /// [`Self::cell`] for a maker that writes the `n` words itself. Until it
    /// does they hold whatever was there.
    fn blank(&mut self, make: Make, n: usize) -> Result<Cell, Fault> {
        match self.freed(make, n) {
            Some((cell, _)) => Ok(cell),
            None => self.append(make, [], std::iter::repeat_n(0, n)),
        }
    }

    /// The space of a freed cell for a new one with `n` words after its header,
    /// when there is one: the new cell, and those words for the caller to
    /// write.
    #[inline(always)]
    fn freed(&mut self, make: Make, n: usize) -> Option<(Cell, &mut [u64])> {
        let space = space(n.saturating_add(1));
        let (index, words) = pop(&mut self.free, &mut self.arena, space)?;
        let (h, words) = words.split_first_mut()?;
        *h = make.header(n);
        self.made = self.made.wrapping_add(1);
        Some((Cell::new(index, make.variant), words))
    }

    /// A new cell of `shape`, given `values` and their references, in the space
    /// of a freed one. It is `None` when there is none of the size, and `values`
    /// has then been asked for nothing.
    #[inline(always)]
    pub(crate) fn in_freed<const N: usize>(
        &mut self,
        shape: Shape<N>,
        values: impl ExactSizeIterator<Item = Value>,
    ) -> Option<Cell> {
        let (cell, words) = self.freed(shape.make(), N.saturating_add(values.len()))?;
        write(words, shape.words(), values.map(Value::bits));
        Some(cell)
    }

    /// The same at the arena's end, or in a box. One that is refused has asked
    /// `values` for nothing.
    #[inline(always)]
    pub(crate) fn at_end<const N: usize>(
        &mut self,
        shape: Shape<N>,
        values: impl ExactSizeIterator<Item = Value>,
    ) -> Result<Cell, Fault> {
        self.append_here(shape.make(), shape.words(), values.map(Value::bits))
    }

    /// A new cell of `shape`, given `values` and their references, wherever
    /// there is room.
    pub(crate) fn holding<const N: usize>(
        &mut self,
        shape: Shape<0>,
        values: [Value; N],
    ) -> Result<Cell, Fault> {
        self.cell(shape.make(), values.map(Value::bits), std::iter::empty())
    }

    /// [`Self::cell`] when no freed cell has the size. The cell goes at the
    /// arena's end, and each of its words is written once.
    #[inline(never)]
    fn append<const N: usize>(
        &mut self,
        make: Make,
        first: [u64; N],
        rest: impl ExactSizeIterator<Item = u64>,
    ) -> Result<Cell, Fault> {
        self.append_here(make, first, rest)
    }

    /// [`Self::append`], where it is called.
    #[inline(always)]
    fn append_here<const N: usize>(
        &mut self,
        make: Make,
        first: [u64; N],
        rest: impl ExactSizeIterator<Item = u64>,
    ) -> Result<Cell, Fault> {
        let n = N.saturating_add(rest.len());
        let size = n.saturating_add(1);
        if size > ARENA_CELL_WORDS {
            return self.long(make, first, rest);
        }
        let at = self.room(space(size))?;
        let index = Index::new(at).ok_or(Fault::Full)?;
        let words = std::iter::once(make.header(n)).chain(first);
        self.arena.extend(words.chain(rest));
        if size < LEAST_WORDS {
            self.arena.push(0);
        }
        self.made = self.made.wrapping_add(1);
        Ok(Cell::new(index, make.variant))
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

    /// [`Self::cell`] for one too long for the arena. Its words go in a box,
    /// and the arena gets a stub: a header that gives the size as 0, then the
    /// box's number.
    #[cold]
    #[inline(never)]
    fn long<const N: usize>(
        &mut self,
        make: Make,
        first: [u64; N],
        rest: impl ExactSizeIterator<Item = u64>,
    ) -> Result<Cell, Fault> {
        let n = N.saturating_add(rest.len());
        let held = self.arena.len().saturating_add(self.boxed);
        if n >= MAX_CELL_WORDS || held.saturating_add(n) > self.limit {
            return Err(Fault::Full);
        }
        // Everything that may be refused is asked for before the words are, so
        // that no cell is left half made.
        let mut words = Vec::new();
        words.try_reserve_exact(n).map_err(|_| Fault::Full)?;
        let place = self.box_place()?;
        // The stub's two words are tested against the limit with the box's.
        self.boxed = self.boxed.saturating_add(n);
        let index = match self.stub([make.stub(), u64::from(place.0)]) {
            Ok(index) => index,
            Err(refused) => {
                self.boxed = self.boxed.saturating_sub(n);
                self.spare.push(place.0);
                return Err(refused);
            }
        };
        words.extend(first);
        words.extend(rest);
        self.fill(place, words.into_boxed_slice());
        self.made = self.made.wrapping_add(1);
        Ok(Cell::new(index, make.variant))
    }

    /// Put `stub` where a cell was freed, or at the arena's end.
    fn stub(&mut self, stub: [u64; STUB_WORDS]) -> Result<Index, Fault> {
        if let Some((index, words)) = pop(&mut self.free, &mut self.arena, STUB_WORDS) {
            words.copy_from_slice(&stub);
            return Ok(index);
        }
        let at = self.room(STUB_WORDS)?;
        let index = Index::new(at).ok_or(Fault::Full)?;
        self.arena.extend_from_slice(&stub);
        Ok(index)
    }

    /// A place for a box: a spare one, or a new one at the table's end.
    fn box_place(&mut self) -> Result<BoxPlace, Fault> {
        if let Some(number) = self.spare.pop() {
            return Ok(BoxPlace(number));
        }
        let number = u32::try_from(self.outside.len()).map_err(|_| Fault::Full)?;
        self.outside.try_reserve(1).map_err(|_| Fault::Full)?;
        self.outside.push(None);
        Ok(BoxPlace(number))
    }

    fn fill(&mut self, place: BoxPlace, words: Box<[u64]>) {
        if let Some(empty) = self.outside.get_mut(place.0 as usize) {
            *empty = Some(words);
        }
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
        if self.count_up(cell) {
            Ok(())
        } else {
            self.uncounted(cell.index())
        }
    }

    /// [`Self::share`], when `cell` is counted. Nothing changes when it is not,
    /// and it is `false`.
    // A share has a call in it, for a cell that is not counted, and goes on
    // after the call. So what comes after is kept where a call leaves it. An
    // instruction that goes somewhere else when this is `false` need not.
    #[inline(always)]
    pub(crate) fn count_up(&mut self, cell: Cell) -> bool {
        match self.arena.get_mut(cell.index().at()) {
            Some(h) if count(*h) > 0 => {
                // The count is the header's low bits, and one past the most it
                // can be is `UNCOUNTED`, so this carries into nothing else.
                *h = h.wrapping_add(1);
                true
            }
            Some(_) | None => false,
        }
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
    #[inline(always)]
    pub(crate) fn release(&mut self, cell: Cell) -> Result<(), DeadCell> {
        let h = match self.arena.get_mut(cell.index().at()) {
            Some(h) if count(*h) > 1 => {
                *h = h.wrapping_sub(1);
                return Ok(());
            }
            Some(h) => *h,
            None => 0,
        };
        self.release_last(cell.index(), h)
    }

    /// [`Self::release`] out of line, for where there is seldom a cell.
    #[inline(never)]
    pub(crate) fn release_apart(&mut self, cell: Cell) -> Result<(), DeadCell> {
        self.release(cell)
    }

    /// [`Self::release`] of the cell at `index`, whose header is `h` and whose
    /// count is not above 1.
    #[cold]
    #[inline(never)]
    fn release_last(&mut self, index: Index, h: u64) -> Result<(), DeadCell> {
        if count(h) != 1 {
            return self.uncounted(index);
        }
        // A string, a binary or a big Int holds nothing, and freeing one is
        // only putting it on a list.
        if raw(h) {
            self.shelve(index, space(size(h)));
            return Ok(());
        }
        self.free(index, h)
    }

    /// What a share or a release comes to when the count of the cell at `index`
    /// is not above 0: nothing for a cell that is never counted, and
    /// [`DeadCell`] for any other.
    #[cold]
    #[inline(never)]
    fn uncounted(&self, index: Index) -> Result<(), DeadCell> {
        if count(self.header(index)) == UNCOUNTED {
            Ok(())
        } else {
            Err(DeadCell)
        }
    }

    /// Free the cell at `index`, whose header is `h` and whose last reference
    /// is given up, and every cell that then has no holder.
    ///
    /// A cell whose last reference goes on the way waits on a list that runs
    /// through the waiting cells' own counts. So freeing a list of a million
    /// cells takes no more stack than freeing one, and no storage at all.
    /// This is Lean's `lean_del_core`.
    #[inline(never)]
    fn free(&mut self, index: Index, h: u64) -> Result<(), DeadCell> {
        let (mut index, mut h, mut todo) = (index, h, NO_MORE);
        loop {
            let scan = scan(h);
            let held = if scan > 0 {
                0..scan
            } else if raw(h) {
                0..0
            } else if let Some(len) = size(h).checked_sub(1) {
                if class(h).is(Kind::Handle) {
                    self.released.push(self.word(index, 1));
                }
                held(class(h), len)
            } else {
                todo = self.empty_box(index, h, todo)?;
                0..0
            };
            let first = index.at().wrapping_add(1);
            let mut at = first.wrapping_add(held.start);
            let end = first.wrapping_add(held.end);
            // Giving one word up may write anywhere in the arena, so each
            // lookup has to be made again after it. A wide cell is copied out
            // a stride at a time to make fewer.
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
            self.shelve(index, space(size(h)));
            if todo == NO_MORE {
                return Ok(());
            }
            index = Index(todo);
            h = self.header(index);
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
        let child = child.index();
        if let Some(h) = self.arena.get_mut(child.at()) {
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

    /// [`Self::free`]'s part for the long cell at `index`, whose header is `h`:
    /// give up what is in its box, and the box.
    #[cold]
    #[inline(never)]
    fn empty_box(&mut self, index: Index, h: u64, todo: u32) -> Result<u32, DeadCell> {
        let words = self.slot(index, h).and_then(|number| self.unbox(number));
        let words = words.unwrap_or_default();
        let held = held(class(h), words.len());
        let mut todo = todo;
        for w in words.get(held).unwrap_or(&[]) {
            todo = self.let_go(*w, todo)?;
        }
        Ok(todo)
    }

    /// Put the freed cell at `index`, which takes `space` words, at the front
    /// of its free list.
    #[inline(always)]
    fn shelve(&mut self, index: Index, space: usize) {
        let words = index.at()..index.at().wrapping_add(LEAST_WORDS);
        if let Some(head) = self.free.get_mut(space)
            && let Some([h, next]) = self.arena.get_mut(words)
        {
            shelve(head, h, next, index, space);
        } else {
            self.shelve_first(index, space);
        }
    }

    /// [`Self::shelve`] for a size the table of free lists does not reach
    /// yet.
    #[cold]
    #[inline(never)]
    fn shelve_first(&mut self, index: Index, space: usize) {
        if self.free.len() <= space && self.arena.len() > index.at() {
            self.free.resize(space.saturating_add(1), 0);
            self.shelve(index, space);
        }
    }

    /// The `N` fields of `cell`, when it is a `ctor` in the arena and is counted.
    /// The caller gives up its reference to it. One that has no other is freed,
    /// and the caller has the references of its fields. It is `None` for any other
    /// cell, and nothing has then changed.
    #[inline(always)]
    pub(crate) fn open<const N: usize>(&mut self, cell: Cell, ctor: Ctor) -> Option<Opened<N>> {
        let index = cell.index();
        let size = N.saturating_add(1);
        let words = index.at()..index.at().wrapping_add(size);
        let (h, words) = self.arena.get_mut(words)?.split_first_mut()?;
        let fields = words.first_chunk::<N>()?.map(Value::from_bits);
        let like = Shape::ctor(ctor).make().header(N);
        if *h != like {
            if *h >> 32 != like >> 32 || count(*h) <= 1 {
                return None;
            }
            *h = h.wrapping_sub(1);
            return Some(Opened::Held(cell, fields));
        }
        match (self.free.get_mut(space(size)), words.first_mut()) {
            (Some(head), Some(next)) => shelve(head, h, next, index, space(size)),
            _ => self.shelve_first(index, space(size)),
        }
        Some(Opened::Alone(fields))
    }

    /// Put `values` at the fields `at` of `cell`, when it is laid out as `layout`
    /// says and has one holder. The caller gives their references, and gets
    /// those of what was there. It is `None` for any other cell, and nothing has
    /// then changed.
    #[inline(always)]
    pub(crate) fn set_alone<const N: usize>(
        &mut self,
        cell: Cell,
        layout: Layout,
        at: [u8; N],
        values: [Value; N],
    ) -> Option<[Value; N]> {
        let from = cell.index().at();
        let to = from.wrapping_add(layout.scan()).wrapping_add(1);
        let (h, words) = self.arena.get_mut(from..to)?.split_first_mut()?;
        if *h != layout.alone() || at.iter().any(|at| usize::from(*at) >= words.len()) {
            return None;
        }
        let mut old = values;
        for (old, at) in old.iter_mut().zip(at) {
            if let Some(word) = words.get_mut(usize::from(at)) {
                *old = Value::from_bits(std::mem::replace(word, old.bits()));
            }
        }
        Some(old)
    }

    /// A new cell like `cell`, but with `values` at the fields `at`. The caller
    /// gives the references of those, and everything else the new cell holds
    /// gets one more. Nothing has changed when there is no copy.
    #[inline(always)]
    pub(crate) fn copy_set<const N: usize>(
        &mut self,
        cell: Cell,
        layout: Layout,
        at: [u8; N],
        values: [Value; N],
    ) -> Result<Cell, NotCopied> {
        match layout.scan() {
            1 => self.copy_of::<1, N>(cell, layout, at, values),
            2 => self.copy_of::<2, N>(cell, layout, at, values),
            3 => self.copy_of::<3, N>(cell, layout, at, values),
            4 => self.copy_of::<4, N>(cell, layout, at, values),
            5 => self.copy_of::<5, N>(cell, layout, at, values),
            6 => self.copy_of::<6, N>(cell, layout, at, values),
            7 => self.copy_of::<7, N>(cell, layout, at, values),
            8 => self.copy_of::<8, N>(cell, layout, at, values),
            _ => {
                let mut sets = [(0, Value::NIL); N];
                for ((set, at), v) in sets.iter_mut().zip(at).zip(values) {
                    *set = (u16::from(at), v);
                }
                self.copy_set_wide(cell, layout, &sets)
            }
        }
    }

    /// [`Self::copy_set`] of a cell of `K` fields, which are read once and
    /// written once.
    #[inline(always)]
    fn copy_of<const K: usize, const N: usize>(
        &mut self,
        cell: Cell,
        layout: Layout,
        at: [u8; N],
        values: [Value; N],
    ) -> Result<Cell, NotCopied> {
        let of = self.fields_of::<K>(cell, layout);
        let mut fields = of.ok_or(NotCopied::Other)?;
        for at in at {
            let set = fields.get_mut(usize::from(at)).ok_or(NotCopied::Other)?;
            *set = Value::NIL.bits();
        }
        // This comes before there is a cell, so that the space of a freed field
        // is not the one it gets.
        self.count_all(&fields)?;
        for (at, v) in at.into_iter().zip(values) {
            let set = fields.get_mut(usize::from(at)).ok_or(NotCopied::Other)?;
            *set = v.bits();
        }
        let make = layout.make(cell.variant());
        let copy = self.cell(make, fields, std::iter::empty());
        Ok(copy.inspect_err(|_| self.count_down_but(fields, at))?)
    }

    /// The `K` fields of `cell`, which is in the arena and laid out as `layout`
    /// says.
    #[inline(always)]
    fn fields_of<const K: usize>(&self, cell: Cell, layout: Layout) -> Option<[u64; K]> {
        let from = cell.index().at();
        let to = from.wrapping_add(K).wrapping_add(1);
        let (h, fields) = self.arena.get(from..to)?.split_first()?;
        if layout.is_of(*h) {
            fields.first_chunk().copied()
        } else {
            None
        }
    }

    /// [`Self::copy_set`] of a cell of any width, with `sets`, which are fields
    /// in order and what each is to be.
    #[inline(never)]
    pub(crate) fn copy_set_wide(
        &mut self,
        cell: Cell,
        layout: Layout,
        sets: &[(u16, Value)],
    ) -> Result<Cell, NotCopied> {
        let last = sets.last().map_or(0, |(at, _)| usize::from(*at));
        if !self.has_field(cell, layout, last) {
            return Err(NotCopied::Other);
        }
        let new = if layout.is_long() {
            self.copy_long(cell, layout, sets)?
        } else {
            self.copy_near(cell, layout, sets)?
        };
        for (at, v) in sets {
            self.set_field(new, usize::from(*at), *v);
        }
        Ok(new)
    }

    /// The copy of [`Self::copy_set_wide`], of a cell in the arena, with `sets`
    /// still to be written.
    #[inline(always)]
    fn copy_near(
        &mut self,
        cell: Cell,
        layout: Layout,
        sets: &[(u16, Value)],
    ) -> Result<Cell, Fault> {
        let n = layout.scan();
        let mut done = 0usize;
        while done < n {
            let counted = match n.wrapping_sub(done) {
                1 => self.count_run::<1>(cell, done, sets),
                2 => self.count_run::<2>(cell, done, sets),
                3 => self.count_run::<3>(cell, done, sets),
                4 => self.count_run::<4>(cell, done, sets),
                5 => self.count_run::<5>(cell, done, sets),
                6 => self.count_run::<6>(cell, done, sets),
                7 => self.count_run::<7>(cell, done, sets),
                _ => self.count_run::<STRIDE>(cell, done, sets),
            };
            if let Err(dead) = counted {
                self.count_back(cell, done, sets);
                return Err(dead.into());
            }
            done = done.wrapping_add(STRIDE);
        }
        self.twin(cell, layout.make(cell.variant()), n)
            .inspect_err(|_| self.count_back(cell, n, sets))
    }

    /// Add a reference to each of the `N` fields of `cell` from `done` on that
    /// `sets` leaves out, or at a freed cell to none of them.
    #[inline(always)]
    fn count_run<const N: usize>(
        &mut self,
        cell: Cell,
        done: usize,
        sets: &[(u16, Value)],
    ) -> Result<(), DeadCell> {
        let first = cell.index().at().wrapping_add(1).wrapping_add(done);
        let words = self.arena.get(first..first.wrapping_add(N));
        let mut words: [u64; N] = *words.and_then(|w| w.first_chunk()).ok_or(DeadCell)?;
        for (at, _) in sets {
            if let Some(word) = words.get_mut(usize::from(*at).wrapping_sub(done)) {
                *word = Value::NIL.bits();
            }
        }
        self.count_all(&words)
    }

    /// Take back the references of the first `done` fields, which
    /// [`Self::count_run`] added.
    #[cold]
    #[inline(never)]
    fn count_back(&mut self, cell: Cell, done: usize, sets: &[(u16, Value)]) {
        for (run, start) in (0..done).step_by(STRIDE).enumerate() {
            let mut kept = self.run_of_values(cell, run);
            for (at, _) in sets {
                if let Some(v) = kept.get_mut(usize::from(*at).wrapping_sub(start)) {
                    *v = Value::NIL;
                }
            }
            self.count_down(&kept.map(Value::bits));
        }
    }

    /// A new cell with the `n` words of `cell`, which is in the arena. The
    /// caller gives the references of what they hold.
    #[inline(always)]
    fn twin(&mut self, cell: Cell, make: Make, n: usize) -> Result<Cell, Fault> {
        if let Some((new, _)) = self.freed(make, n) {
            self.copy_words(cell, 0..n, new, 0);
            return Ok(new);
        }
        let from = cell.index().at().wrapping_add(1);
        let words = from..from.wrapping_add(n);
        if words.end > self.arena.len() {
            return Err(Fault::DeadCell);
        }
        let at = self.room(space(n.wrapping_add(1)))?;
        let index = Index::new(at).ok_or(Fault::Full)?;
        self.arena.push(make.header(n));
        self.arena.extend_from_within(words);
        self.made = self.made.wrapping_add(1);
        Ok(Cell::new(index, make.variant))
    }

    /// Add a reference to each of `words`, or at a freed cell to none of them.
    #[inline(always)]
    fn count_all(&mut self, words: &[u64]) -> Result<(), DeadCell> {
        let mut counted = true;
        for w in words {
            if let Some(child) = Value::from_bits(*w).as_cell() {
                counted &= self.count_up(child);
            }
        }
        if counted {
            Ok(())
        } else {
            self.counted_but_frozen(words)
        }
    }

    /// The end of a [`Self::count_all`] of `words`, one of which is not counted.
    #[cold]
    #[inline(never)]
    fn counted_but_frozen(&mut self, words: &[u64]) -> Result<(), DeadCell> {
        if words.iter().any(|w| self.is_freed(*w)) {
            self.count_down(words);
            return Err(DeadCell);
        }
        Ok(())
    }

    /// [`Self::count_down`] of `words` but for those at `at`.
    #[cold]
    #[inline(never)]
    fn count_down_but<const K: usize, const N: usize>(&mut self, words: [u64; K], at: [u8; N]) {
        for (i, w) in words.iter().enumerate() {
            if at.iter().all(|at| usize::from(*at) != i) {
                self.count_down(std::slice::from_ref(w));
            }
        }
    }

    /// Take back the references [`Self::count_all`] added.
    #[cold]
    #[inline(never)]
    fn count_down(&mut self, words: &[u64]) {
        for w in words {
            if let Some(child) = Value::from_bits(*w).as_cell()
                && let Some(h) = self.arena.get_mut(child.index().at())
                && count(*h) > 0
            {
                *h = h.wrapping_sub(1);
            }
        }
    }

    /// [`Self::copy_near`] of a long cell.
    #[cold]
    #[inline(never)]
    fn copy_long(
        &mut self,
        cell: Cell,
        layout: Layout,
        sets: &[(u16, Value)],
    ) -> Result<Cell, Fault> {
        let mut words: Vec<u64> = Vec::new();
        let n = self.data(cell).len();
        words.try_reserve_exact(n).map_err(|_| Fault::Full)?;
        words.extend_from_slice(self.data(cell));
        for (at, _) in sets {
            if let Some(word) = words.get_mut(usize::from(*at)) {
                *word = Value::NIL.bits();
            }
        }
        self.count_all(&words)?;
        let make = layout.make(cell.variant());
        self.cell(make, [], words.iter().copied())
            .inspect_err(|_| self.count_down(&words))
    }

    /// Whether `w` is a freed cell.
    fn is_freed(&self, w: u64) -> bool {
        let cell = Value::from_bits(w).as_cell();
        cell.is_some_and(|cell| freed(self.header(cell.index())))
    }

    /// Whether `cell` is laid out as `layout` says, and has field `i`. A long
    /// cell's layout does not say how many it has, so its constructor does.
    pub(crate) fn has_field(&self, cell: Cell, layout: Layout, i: usize) -> bool {
        let n = self.data(cell).len();
        let whole = || self.ctors.shape(layout.class()).map(|(fields, _)| fields) == Some(n);
        i < n && layout.is_of(self.header(cell.index())) && (!layout.is_long() || whole())
    }

    /// Put `v` at field `i` of `cell`, which one holder holds, and give what was
    /// there. Both references are the caller's. It is `None`, and nothing
    /// changes, when the cell has no such field.
    pub(crate) fn set_field(&mut self, cell: Cell, i: usize, v: Value) -> Option<Value> {
        let word = self.data_mut(cell).get_mut(i)?;
        Some(Value::from_bits(std::mem::replace(word, v.bits())))
    }

    /// The cell of big Int constant `id`, when [`Self::keep_constant`] has
    /// been given one.
    pub(crate) fn constant(&self, id: ConstId) -> Option<Cell> {
        self.constants.get(id.index()).copied()?
    }

    /// Keep `cell` as the cell of big Int constant `id`. The heap takes the
    /// caller's one reference, and from here on `cell` is never freed, so any
    /// number of holders may come and go without a count.
    pub(crate) fn keep_constant(&mut self, id: ConstId, cell: Cell) -> Result<(), Fault> {
        let len = id.index().saturating_add(1);
        if self.constants.len() < len {
            let more = len.saturating_sub(self.constants.len());
            self.constants.try_reserve(more).map_err(|_| Fault::Full)?;
            self.constants.resize(len, None);
        }
        if let Some(kept) = self.constants.get_mut(id.index()) {
            *kept = Some(cell);
        }
        self.recount(cell, UNCOUNTED);
        Ok(())
    }

    /// Give up the heap's reference to each constant it kept. The run is
    /// over, so nothing else holds one.
    pub(crate) fn let_go_of_constants(&mut self) -> Result<(), DeadCell> {
        for cell in std::mem::take(&mut self.constants).into_iter().flatten() {
            self.recount(cell, 1);
            self.release(cell)?;
        }
        Ok(())
    }

    /// Make `cell`'s count `c`.
    fn recount(&mut self, cell: Cell, c: i32) {
        if let Some(h) = self.arena.get_mut(cell.index().at()) {
            *h = *h & !COUNT | u64::from(c.cast_unsigned());
        }
    }

    /// The handle numbers freed since this was last asked.
    pub(crate) fn take_released(&mut self) -> Vec<u64> {
        std::mem::take(&mut self.released)
    }

    pub(crate) fn kind(&self, cell: Cell) -> Option<Kind> {
        class(self.header(cell.index())).kind()
    }

    /// Whether `cell` is a cell of `kind`. It is [`Self::kind`] for a caller
    /// that has one in mind.
    #[inline(always)]
    pub(crate) fn is(&self, cell: Cell, kind: Kind) -> bool {
        class(self.header(cell.index())).is(kind)
    }

    /// [`Self::is`], and the words after `cell`'s header, for a walk that
    /// wants both at every step.
    #[inline(always)]
    pub(crate) fn is_with_data(&self, cell: Cell, kind: Kind) -> (bool, &[u64]) {
        (self.is(cell, kind), self.data(cell))
    }

    /// A new string cell holding `text`.
    pub(crate) fn string(&mut self, text: &[u8]) -> Result<Cell, Fault> {
        let words = text.chunks(8).map(padded_word);
        self.cell(Make::of(Kind::String), [text.len() as u64], words)
    }

    /// A string cell's length in bytes.
    pub(crate) fn string_len(&self, cell: Cell) -> usize {
        to_usize(self.word(cell.index(), 1))
    }

    /// Whether a string cell holds exactly `text`.
    pub(crate) fn string_is(&self, cell: Cell, text: &[u8]) -> bool {
        let words = self.words(cell.index(), 2);
        self.string_len(cell) == text.len()
            && words.len() == text.len().div_ceil(8)
            && words
                .iter()
                .zip(text.chunks(8))
                .all(|(w, bytes)| *w == padded_word(bytes))
    }

    /// A string cell's bytes, appended to `out`.
    pub(crate) fn read_string(&self, cell: Cell, out: &mut Vec<u8>) {
        let len = to_usize(self.word(cell.index(), 1));
        out.extend(
            self.words(cell.index(), 2)
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
        self.cell(Make::of(Kind::BigInt), first, digits.into_iter())
    }

    /// The Int in big Int cell `cell`, when it is one word long, as an
    /// `i128`. A wider one is `None`.
    pub(crate) fn big_int_word(&self, cell: Cell) -> Option<i128> {
        let w = self.word(cell.index(), 1);
        if w & 0xFFFF_FFFF != 1 {
            return None;
        }
        let d = i128::from(self.word(cell.index(), 2));
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
        self.cell(Make::of(Kind::BigInt), first, digits.iter().copied())
    }

    pub(crate) fn read_big_int(&self, cell: Cell) -> BigInt {
        let w = self.word(cell.index(), 1);
        let sign = if w >> 63 == 1 {
            Sign::Minus
        } else {
            Sign::Plus
        };
        let len = (w & 0xFFFF_FFFF) as usize;
        let bytes: Vec<u8> = self
            .words(cell.index(), 2)
            .iter()
            .take(len)
            .flat_map(|d| d.to_le_bytes())
            .collect();
        BigInt::from_bytes_le(sign, &bytes)
    }

    /// Which constructor a cell is, to tell it from others. It is `None` for
    /// a cell that is no constructor's.
    pub(crate) fn ctor_of(&self, cell: Cell) -> Option<Ctor> {
        class(self.header(cell.index())).ctor(cell.variant())
    }

    /// Whether `cell` is a `ctor` of `n` fields.
    pub(crate) fn is_ctor(&self, cell: Cell, ctor: Ctor, n: usize) -> bool {
        let index = cell.index();
        let h = self.header(index);
        class(h) == ctor.class() && self.data_len(index, h) == n
    }

    /// The name the program knows a constructor cell by. It is `None` for a
    /// cell that is no constructor's, and for one that is none of the
    /// program's.
    pub(crate) fn variant(&self, cell: Cell) -> Option<VariantRef> {
        self.ctors.variant(class(self.header(cell.index())))
    }

    /// Value `i` of a cell whose values come first, or `None` past its last: a
    /// constructor's field, a tuple's element, an array leaf's item, the binary
    /// a slice is of, or a closure's function and then its captures. Reading it
    /// adds no reference.
    pub(crate) fn field(&self, cell: Cell, i: usize) -> Option<Value> {
        self.near_field(cell, i)
            .or_else(|| self.boxed_field(cell, i))
    }

    /// [`Self::field`] of a cell in the arena. It is `None` for a long cell.
    #[inline(always)]
    pub(crate) fn near_field(&self, cell: Cell, i: usize) -> Option<Value> {
        let at = cell.index().at();
        if i >= scan(*self.arena.get(at)?) {
            return None;
        }
        // A scan is less than a cell's size, so the index is exact.
        let field = self.arena.get(at.wrapping_add(1).wrapping_add(i))?;
        Some(Value::from_bits(*field))
    }

    /// [`Self::field`] of a long cell, which has one only when all of its words
    /// are values. It is `None` for a cell in the arena.
    #[inline]
    fn boxed_field(&self, cell: Cell, i: usize) -> Option<Value> {
        let index = cell.index();
        let h = self.header(index);
        let field = self.boxed(index, h).get(i)?;
        match class(h).scan() {
            Scan::All => Some(Value::from_bits(*field)),
            Scan::First | Scan::Ask => None,
        }
    }

    /// Every word of a cell that holds only values, as [`Self::field`] numbers
    /// them, and nothing of any other cell. Reading one adds no reference.
    pub(crate) fn values(&self, cell: Cell) -> impl Iterator<Item = Value> + '_ {
        let index = cell.index();
        let words: &[u64] = match class(self.header(index)).scan() {
            Scan::All => self.words(index, 1),
            Scan::First | Scan::Ask => &[],
        };
        words.iter().map(|w| Value::from_bits(*w))
    }

    /// The values of `cell` that [`Self::values`] gives from the `run`th run of
    /// [`STRIDE`] on, with `Nil` past its last.
    #[inline]
    pub(crate) fn run_of_values(&self, cell: Cell, run: usize) -> [Value; STRIDE] {
        let from = run.wrapping_mul(STRIDE);
        let words = self.words(cell.index(), 1).get(from..).unwrap_or(&[]);
        if let Some(whole) = words.first_chunk() {
            return whole.map(Value::from_bits);
        }
        let mut values = [Value::NIL; STRIDE];
        for (v, w) in values.iter_mut().zip(words) {
            *v = Value::from_bits(*w);
        }
        values
    }

    /// The function a closure cell runs, or `None` for a cell that is no
    /// closure.
    pub(crate) fn closure_func(&self, cell: Cell) -> Option<FuncIdx> {
        let index = cell.index();
        let h = self.header(index);
        if !class(h).is(Kind::Closure) {
            return None;
        }
        let func = if scan(h) > 0 {
            self.arena.get(index.at().wrapping_add(1))
        } else {
            self.boxed(index, h).first()
        };
        Value::from_bits(*func?).as_func()
    }

    /// Capture `i` of a closure cell, or `None` past its last. Reading it adds
    /// no reference.
    pub(crate) fn capture(&self, cell: Cell, i: u32) -> Option<Value> {
        self.field(cell, capture_at(i))
    }

    /// [`Self::capture`] of a cell in the arena. It is `None` for a long cell.
    #[inline(always)]
    pub(crate) fn near_capture(&self, cell: Cell, i: u32) -> Option<Value> {
        self.near_field(cell, capture_at(i))
    }

    /// Every capture of a closure cell, in order. Reading one adds no
    /// reference.
    pub(crate) fn captures(&self, cell: Cell) -> impl Iterator<Item = Value> + '_ {
        self.values(cell).skip(1)
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
        self.cell(Make::of(kind), [], data.iter().copied())
    }

    /// The words of `cell` after its header. They are empty for a cell the
    /// heap does not have.
    #[inline]
    pub(crate) fn data(&self, cell: Cell) -> &[u64] {
        self.words(cell.index(), 1)
    }

    /// [`Self::data`] for a binary's readers.
    // A binary is as often long as short, and a parser reads one byte of it
    // at a time. So the way to a long cell's box is not marked cold here, as
    // it is in `words` for every other reader. A read of a long binary costs
    // about 25 machine instructions more through the cold one.
    #[inline]
    pub(crate) fn binary_data(&self, cell: Cell) -> &[u64] {
        let index = cell.index();
        let onward = self.arena.get(index.at()..).unwrap_or(&[]);
        let h = onward.first().copied().unwrap_or(FREED);
        match onward.get(1..size(h)) {
            Some(words) => words,
            None => self.boxed(index, h),
        }
    }

    /// Whether `cell` has one holder, who may then change it in place.
    /// Asking about a freed cell is [`DeadCell`].
    #[inline]
    pub(crate) fn unique(&self, cell: Cell) -> Result<bool, DeadCell> {
        let c = count(self.header(cell.index()));
        if c <= 0 {
            self.uncounted(cell.index())?;
        }
        Ok(c == 1)
    }

    /// The words of `cell` after its header, for its one holder to change.
    pub(crate) fn data_mut(&mut self, cell: Cell) -> &mut [u64] {
        self.words_mut(cell.index(), 1)
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
        let new = self.blank(Make::of(kind), len.saturating_add(room))?;
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
        let (from, to) = (from.index(), to.index());
        if size(self.header(from)) == 0 || size(self.header(to)) == 0 {
            return self.copy_long_words(from, src, to, dst);
        }
        let len = src.len();
        let s = from.at().saturating_add(1).saturating_add(src.start);
        let d = to.at().saturating_add(1).saturating_add(dst);
        if s.saturating_add(len) <= self.arena.len() && d.saturating_add(len) <= self.arena.len() {
            self.arena.copy_within(s..s.saturating_add(len), d);
        }
    }

    /// [`Self::copy_words`] when one of the cells is long, or both are. A
    /// long cell's box is taken out of its place while it is read or
    /// written, so that the other cell can be reached wherever it is.
    #[cold]
    #[inline(never)]
    fn copy_long_words(&mut self, from: Index, src: std::ops::Range<usize>, to: Index, dst: usize) {
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
        let index = cell.index();
        let h = self.header(index);
        let kind = class(h).kind().ok_or(Fault::DeadCell)?;
        let words = self.data_len(index, h);
        let make = Make {
            class: class(h),
            scan: kind.scan(),
            variant: cell.variant(),
        };
        let new = self.blank(make, words)?;
        self.copy_words(cell, 0..words, new, 0);
        for i in held(class(h), words) {
            let Some(w) = self.data(new).get(i) else {
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
        let index = cell.index();
        let h = self.header(index);
        if count(h) <= 0 {
            return Err(DeadCell);
        }
        if size(h) == 0
            && let Some(number) = self.slot(index, h)
        {
            self.unbox(number);
        }
        self.shelve(index, space(size(h)));
        Ok(())
    }

    /// Cells this heap has made over its whole life.
    pub(crate) fn made(&self) -> usize {
        self.made
    }

    /// The words of the arena, which only grows.
    pub(crate) fn words_carved(&self) -> usize {
        self.arena.len()
    }

    /// The header at `index`, or a freed cell's when the arena has no such
    /// word.
    fn header(&self, index: Index) -> u64 {
        self.arena.get(index.at()).copied().unwrap_or(FREED)
    }

    /// How many words follow the header `h` of the cell at `index`.
    fn data_len(&self, index: Index, h: u64) -> usize {
        match size(h).checked_sub(1) {
            Some(n) => n,
            None => self.long_words(index, h, 1).len(),
        }
    }

    /// Words `from` to the end of the cell at `index`, counting its header as
    /// word 0. `from` is 1 or 2: the header itself is [`Self::header`]'s to
    /// read.
    // This and `words_mut` are a few instructions each. They go inside their
    // callers, so that a node `make` writes, or one an array reads to copy,
    // costs no call. Left to the optimizer, each array node made paid for
    // two.
    #[inline(always)]
    fn words(&self, index: Index, from: usize) -> &[u64] {
        let onward = self.arena.get(index.at()..).unwrap_or(&[]);
        let h = onward.first().copied().unwrap_or(FREED);
        // A long cell's header gives its size as 0, so it has no such run
        // of words in the arena, and neither has a freed cell.
        match onward.get(from..size(h)) {
            Some(words) => words,
            None => self.long_words(index, h, from),
        }
    }

    #[inline(always)]
    fn words_mut(&mut self, index: Index, from: usize) -> &mut [u64] {
        let h = self.header(index);
        if size(h) == 0 {
            return self.long_words_mut(index, h, from);
        }
        self.arena
            .get_mut(index.at()..)
            .and_then(|onward| onward.get_mut(from..size(h)))
            .unwrap_or(&mut [])
    }

    /// Word `i` of the cell at `index`, counting its header as word 0, for `i`
    /// of 1 or more. It is 0 past the cell's last word.
    fn word(&self, index: Index, i: usize) -> u64 {
        let h = self.header(index);
        let word = if i < size(h) {
            self.arena.get(index.at().saturating_add(i))
        } else {
            self.long_words(index, h, i).first()
        };
        word.copied().unwrap_or(0)
    }

    /// The number of the box that holds the words of the cell at `index`, whose
    /// header is `h`, when it is a long cell.
    fn slot(&self, index: Index, h: u64) -> Option<u32> {
        if size(h) != 0 || freed(h) {
            return None;
        }
        let number = self.arena.get(index.at().saturating_add(1))?;
        Some(low_u32(*number))
    }

    /// [`Self::words`] for a cell whose header `h` gives it no such words in
    /// the arena. For a long cell they are in its box. A freed cell, a cell
    /// the heap does not have and a cell shorter than `from` have none.
    #[cold]
    #[inline(never)]
    fn long_words(&self, index: Index, h: u64, from: usize) -> &[u64] {
        self.boxed(index, h)
            .get(from.saturating_sub(1)..)
            .unwrap_or(&[])
    }

    /// The words in the box of the cell at `index`, whose header is `h`, or
    /// none when it is not a long cell.
    fn boxed(&self, index: Index, h: u64) -> &[u64] {
        self.slot(index, h)
            .and_then(|slot| self.outside.get(slot as usize)?.as_deref())
            .unwrap_or(&[])
    }

    #[cold]
    #[inline(never)]
    fn long_words_mut(&mut self, index: Index, h: u64, from: usize) -> &mut [u64] {
        self.slot(index, h)
            .and_then(|slot| self.outside.get_mut(slot as usize)?.as_deref_mut())
            .and_then(|words| words.get_mut(from.saturating_sub(1)..))
            .unwrap_or(&mut [])
    }
}

/// The header of a freed cell. Every reader of a cell's words finds their
/// number there, so a freed cell reads as one with none.
const FREED: u64 = 0;

fn count(h: u64) -> i32 {
    low_u32(h).cast_signed()
}

/// How far up a header its class is.
const CLASS_SHIFT: u32 = 48;

fn scan(h: u64) -> usize {
    usize::from(low_byte(h >> 32))
}

/// A cell's words in the arena, or 0 for a long cell and a freed one.
fn size(h: u64) -> usize {
    usize::from(low_byte(h >> 40))
}

fn class(h: u64) -> Class {
    Class(low_u16(h >> CLASS_SHIFT))
}

/// The header `h`, of a cell of `class` instead.
fn with_class(h: u64, class: Class) -> u64 {
    h & !(0xFFFF << CLASS_SHIFT) | u64::from(class.0) << CLASS_SHIFT
}

/// Whether `h` is the header of a cell in the arena that holds no values and
/// nothing of the host's.
#[inline(always)]
fn raw(h: u64) -> bool {
    class(h).0.wrapping_sub(1) < Kind::RAW && size(h) != 0
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

/// Put the freed cell at `index`, whose first two words are `h` and `next`, in
/// front of the cell at `head`, the first on the free list of its `space`.
#[inline(always)]
fn shelve(head: &mut u32, h: &mut u64, next: &mut u64, index: Index, space: usize) {
    *h = FREED;
    *next = free_link(space, *head);
    *head = index.0;
}

/// Which of a closure cell's values capture `i` is. The first is its function.
fn capture_at(i: u32) -> usize {
    (i as usize).wrapping_add(1)
}

/// The first freed cell of `space` words, taken off its list, and those words,
/// when there is one.
#[inline(always)]
fn pop<'a>(free: &mut [u32], arena: &'a mut [u64], space: usize) -> Option<(Index, &'a mut [u64])> {
    let head = free.get_mut(space).filter(|head| **head != 0)?;
    let index = Index(*head);
    let words = arena.get_mut(index.at()..index.at().wrapping_add(space))?;
    *head = low_u32(*words.get(1)?);
    Some((index, words))
}

/// Write `first` and then `rest` in `words`.
#[inline(always)]
fn write<const N: usize>(words: &mut [u64], first: [u64; N], rest: impl Iterator<Item = u64>) {
    if let Some((slots, words)) = words.split_first_chunk_mut::<N>() {
        *slots = first;
        for (slot, w) in words.iter_mut().zip(rest) {
            *slot = w;
        }
    }
}

/// Whether `h` is the header of a freed cell, or of a cell the heap does not
/// have. Freeing zeroes the whole header, and a live cell's count is not 0.
fn freed(h: u64) -> bool {
    h & COUNT == 0
}

/// Which of the `len` words after the header of a cell of this class hold a
/// value, and so a reference. The header of a cell in the arena says it of most
/// kinds, as its scan. This says it of the rest, which are an array root's
/// three parts (after its length, height and offset), an array branch's
/// children (after its height and size table) and every word of a map cell
/// after its first. It says it of a long cell of any class too.
fn held(class: Class, len: usize) -> std::ops::Range<usize> {
    match class.kind() {
        Some(Kind::Map | Kind::MapNode | Kind::MapCollision) => 1..len,
        Some(Kind::ArrayRoot) => 3..len,
        // A branch is its height, then a size table and the children, the
        // same number of each.
        Some(Kind::ArrayBranch) => len.saturating_sub(len.saturating_sub(1) / 2)..len,
        Some(
            kind @ (Kind::Ctor
            | Kind::Closure
            | Kind::Tuple
            | Kind::ArrayLeaf
            | Kind::BinarySlice
            | Kind::String
            | Kind::BigInt
            | Kind::Range
            | Kind::Binary
            | Kind::Handle),
        ) => 0..kind.scan().of(len),
        None => 0..0,
    }
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

#[cfg(test)]
mod tests;
