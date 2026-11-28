use scarlet_ir::TypeId;

use super::*;
use crate::value::{Tag, View};

/// A heap for a program with no constructors.
impl Default for Heap {
    fn default() -> Heap {
        Heap::new(Numbering::default().finish())
    }
}

impl Heap {
    /// A heap for a program with these constructors, each with this many
    /// fields, and what each is made with.
    pub(crate) fn knowing(ctors: &[(VariantRef, usize)]) -> (Heap, Vec<Ctor>) {
        let mut numbering = Numbering::default();
        let made = ctors
            .iter()
            .map(|(v, fields)| numbering.add(*v, *fields).expect("few constructors"))
            .collect();
        (Heap::new(numbering.finish()), made)
    }

    /// A heap that holds at most `limit` words.
    pub(crate) fn limited(limit: usize) -> Heap {
        Heap::limited_for(limit, Numbering::default().finish())
    }

    /// The same, for a program with these constructors.
    pub(crate) fn limited_for(limit: usize, ctors: Ctors) -> Heap {
        Heap {
            limit: limit.min(LIMIT_WORDS),
            ..Heap::new(ctors)
        }
    }

    /// Hold no more than `limit` words from here on.
    pub(crate) fn limit_to(&mut self, limit: usize) {
        self.limit = limit.min(LIMIT_WORDS);
    }

    /// A new cell of `shape`, given `values`, wherever there is room.
    fn given<const N: usize>(&mut self, shape: Shape<N>, values: &[Value]) -> Result<Cell, Fault> {
        match self.in_freed(shape, values.iter().copied()) {
            Some(cell) => Ok(cell),
            None => self.at_end(shape, values.iter().copied()),
        }
    }

    pub(crate) fn ctor(&mut self, ctor: Ctor, fields: &[Value]) -> Result<Cell, Fault> {
        self.given(Shape::ctor(ctor), fields)
    }

    pub(crate) fn tuple(&mut self, elements: &[Value]) -> Result<Cell, Fault> {
        self.given(Shape::TUPLE, elements)
    }

    pub(crate) fn closure(&mut self, func: FuncIdx, captures: &[Value]) -> Result<Cell, Fault> {
        self.given(Shape::closure(func), captures)
    }

    /// Say that `cell` has `holders` holders.
    pub(crate) fn say_held(&mut self, cell: Cell, holders: i32) {
        self.recount(cell, holders);
    }

    /// How many holders `cell` says it has, or `None` when it is never counted.
    pub(crate) fn held_by(&self, cell: Cell) -> Option<i32> {
        let c = count(self.header(cell.index()));
        (c != UNCOUNTED).then_some(c)
    }

    /// How many words count against the limit.
    pub(crate) fn words_used(&self) -> usize {
        self.arena.len() + self.boxed
    }

    /// How many words the arena has asked the system for.
    pub(crate) fn arena_room(&self) -> usize {
        self.arena.capacity()
    }

    /// Have every new cell made at the arena's end.
    pub(crate) fn forget_freed(&mut self) {
        self.free.fill(0);
    }

    /// How many cells are not yet freed.
    pub(crate) fn live(&self) -> usize {
        self.live_cells().expect("an arena of whole cells")
    }
}

impl Cell {
    /// Where its header is.
    pub(crate) fn place(self) -> Index {
        self.index()
    }
}

fn text(heap: &Heap, cell: Cell) -> String {
    let mut out = Vec::new();
    heap.read_string(cell, &mut out);
    String::from_utf8(out).expect("UTF-8")
}

fn variant(type_id: i32, variant_idx: u16) -> VariantRef {
    VariantRef {
        type_id: TypeId(type_id),
        variant_idx,
    }
}

/// The cell at `index` that is no constructor's.
fn plain(index: usize) -> Cell {
    Cell::new(Index::new(index).expect("an index"), 0)
}

fn fields(heap: &Heap, cell: Cell) -> Vec<View> {
    heap.values(cell).map(Value::view).collect()
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
    assert_eq!(cell, plain(1));
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
    heap.share(a).expect("a live cell");
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
    assert_eq!(heap.words_carved(), 1 + 2000 * 5);
    for (i, cell) in cells.iter().enumerate() {
        assert_eq!(cell.index().at(), 1 + 5 * i);
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

/// A cell of up to [`ARENA_CELL_WORDS`] words is in the arena. One word
/// more and it is a stub and a box.
#[test]
fn the_longest_arena_cell_is_254_words() {
    let mut heap = Heap::default();
    let values: Vec<u64> = (0..ARENA_CELL_WORDS).map(|i| int(i).bits()).collect();
    let short = heap
        .make(Kind::ArrayBranch, &values[..ARENA_CELL_WORDS - 1])
        .expect("room");
    assert_eq!(heap.arena.len(), 1 + ARENA_CELL_WORDS);
    assert_eq!((heap.boxed, heap.outside.len()), (0, 0));
    let long = heap.make(Kind::ArrayBranch, &values).expect("room");
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
        .make(Kind::ArrayBranch, &values[..ARENA_CELL_WORDS - 1])
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
    heap.share(cell).expect("a live cell");
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

/// A constructor too wide for the arena reads and frees what it holds, the same
/// as a narrow one.
#[test]
fn a_long_constructor_is_a_constructor() {
    let (mut heap, ctors) = Heap::knowing(&[(variant(1, 2), 300)]);
    let held = heap.string(b"held").expect("room");
    let mut fields: Vec<Value> = (0..300).map(int).collect();
    fields[299] = Value::cell(held);
    let cell = heap.ctor(ctors[0], &fields).expect("room");
    assert_eq!(heap.words_carved(), 1 + 3 + STUB_WORDS);
    assert_eq!(heap.kind(cell), Some(Kind::Ctor));
    assert_eq!(heap.ctor_of(cell), Some(ctors[0]));
    assert_eq!(heap.variant(cell), Some(variant(1, 2)));
    assert_eq!(Value::cell(cell).tag(), Tag::of_cell(2));
    assert_eq!(heap.values(cell).count(), 300);
    for (i, f) in fields.iter().enumerate() {
        assert_eq!(heap.field(cell, i).map(Value::view), Some(f.view()));
        assert!(heap.near_field(cell, i).is_none());
    }
    assert!(heap.field(cell, 300).is_none());
    assert_eq!(heap.audit([Value::cell(cell)].into_iter()), Ok(()));
    heap.release(cell).expect("a live cell");
    assert_eq!((heap.live(), heap.boxed), (0, 0));
}

/// A long constructor that is freed, or that gives its fields up, leaves its box's
/// place and its two words of the arena to the next. So a thousand made one after
/// another need no more room than one, and nothing is left when the last is gone.
#[test]
fn a_long_constructor_takes_the_space_of_the_last() {
    let (mut heap, ctors) = Heap::knowing(&[(variant(1, 2), 254), (variant(1, 3), 300)]);
    let mut words = None;
    for turn in 0..1000 {
        let n = [254, 300][turn % 2];
        let cell = heap.ctor(ctors[turn % 2], &vec![int(7); n]).expect("room");
        assert_eq!(
            heap.field(cell, n - 1).map(Value::view),
            Some(int(7).view())
        );
        assert_eq!(heap.boxed, n);
        assert_eq!((heap.outside.len(), heap.spare.len()), (1, 0));
        assert_eq!(
            *words.get_or_insert(heap.words_carved()),
            heap.words_carved()
        );
        let gone = if turn / 2 % 2 == 0 {
            heap.release(cell)
        } else {
            heap.discard(cell)
        };
        gone.expect("a live cell");
        assert_eq!((heap.live(), heap.boxed), (0, 0));
        assert_eq!((heap.outside.len(), heap.spare.len()), (1, 1));
    }
}

#[test]
fn a_long_tuple_and_a_long_closure_hold_what_they_were_given() {
    let mut heap = Heap::default();
    let held = heap.string(b"held").expect("room");
    heap.share(held).expect("a live cell");
    let mut values: Vec<Value> = (0..400).map(int).collect();
    values[0] = Value::cell(held);
    let tuple = heap.tuple(&values).expect("room");
    let closure = heap.closure(FuncIdx(9), &values).expect("room");
    assert_eq!(heap.outside.len(), 2);
    assert_eq!(heap.values(tuple).count(), 400);
    assert_eq!(
        heap.field(tuple, 399).map(Value::view),
        Some(int(399).view())
    );
    assert!(heap.field(tuple, 400).is_none());
    assert_eq!(heap.captures(closure).count(), 400);
    assert_eq!(heap.closure_func(closure), Some(FuncIdx(9)));
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
    let (is, data) = heap.is_with_data(cell, Kind::Binary);
    assert_eq!((is, data.len()), (true, 1000));
    assert!(!heap.is_with_data(cell, Kind::String).0);
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
    for cell in [short, long, gone, plain(0), plain(u32::MAX as usize)] {
        assert_eq!(heap.binary_data(cell), heap.data(cell));
    }
    assert_eq!(heap.binary_data(short), &[16, 7]);
    assert_eq!(heap.binary_data(long), &[3; 400]);
    assert!(heap.binary_data(gone).is_empty());
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

/// A refused long cell makes spare again the place it took and no other, and
/// the next two long cells get a place each, in the order they would have.
#[test]
fn a_refused_long_cell_leaves_the_spare_places_in_order() {
    let mut heap = Heap::limited(1000);
    let cells = [1, 2, 3].map(|w| heap.make(Kind::Binary, &[w; 300]).expect("room"));
    heap.release(cells[2]).expect("a live cell");
    heap.release(cells[0]).expect("a live cell");
    assert_eq!(heap.spare, [2, 0]);
    // Two two-word cells take the freed stubs, so the next stub is carved, and
    // the arena's 7 words leave room for a box of 693 and no stub.
    let small = [1, 2].map(|_| heap.tuple(&[Value::NIL]).expect("room"));
    assert_eq!(heap.arena.len() + heap.boxed, 307);
    assert_eq!(heap.make(Kind::Binary, &[4; 693]), Err(Fault::Full));
    assert_eq!((&heap.spare[..], heap.outside.len()), (&[2, 0][..], 3));
    let held = [cells[1], small[0], small[1]].map(Value::cell);
    assert_eq!(heap.audit(held.into_iter()), Ok(()));
    let fourth = heap.make(Kind::Binary, &[4; 300]).expect("room");
    let fifth = heap.make(Kind::Binary, &[5; 300]).expect("room");
    assert_eq!((heap.spare.len(), heap.outside.len()), (0, 3));
    for (cell, w, place) in [(cells[1], 2, 1), (fourth, 4, 0), (fifth, 5, 2)] {
        assert_eq!(heap.data(cell), &[w; 300]);
        assert_eq!(heap.outside[place].as_deref(), Some(&[w; 300][..]));
    }
    let held = [cells[1], small[0], small[1], fourth, fifth].map(Value::cell);
    assert_eq!(heap.audit(held.into_iter()), Ok(()));
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
    let binary = Make::of(Kind::Binary);
    assert_eq!(heap.blank(binary, MAX_CELL_WORDS), Err(Fault::Full));
    assert_eq!(heap.blank(binary, usize::MAX), Err(Fault::Full));
    assert!(heap.arena.is_empty());
    assert_eq!((heap.live(), heap.made()), (0, 0));
}

/// A name the heap never gave out reads as a freed cell: index 0, an
/// index past the arena's end, and the largest index a value word
/// holds. Nothing panics, and no word of the arena changes.
#[test]
fn a_cell_the_arena_does_not_have_is_a_dead_cell() {
    let (mut heap, ctors) = Heap::knowing(&[(variant(1, 0), 1)]);
    let real = heap.tuple(&[Value::NIL]).expect("room");
    let arena = heap.arena.clone();
    let len = u64::try_from(heap.arena.len()).expect("small");
    for bits in [0, len, len + 1, 1 << 31, u64::from(u32::MAX)] {
        let cell = Cell::from_bits(bits);
        assert_eq!(heap.kind(cell), None, "{bits}");
        assert!(heap.data(cell).is_empty());
        assert!(heap.data_mut(cell).is_empty());
        assert_eq!(heap.values(cell).count(), 0);
        assert!(heap.field(cell, 0).is_none());
        assert_eq!(heap.ctor_of(cell), None);
        assert_eq!(heap.variant(cell), None);
        assert_eq!(heap.closure_func(cell), None);
        assert_eq!(heap.string_len(cell), 0);
        assert_eq!(heap.unique(cell), Err(DeadCell));
        assert_eq!(heap.share(cell), Err(DeadCell));
        assert_eq!(heap.release(cell), Err(DeadCell));
        assert_eq!(heap.discard(cell), Err(DeadCell));
        assert!(heap.open::<1>(cell, ctors[0]).is_none());
    }
    assert_eq!(heap.arena, arena);
    assert_eq!(heap.live(), 1);
    assert_eq!(heap.unique(real), Ok(true));
}

/// A stub whose box number names no box has no words, and is freed
/// like any other cell.
#[test]
fn a_stub_with_no_box_has_no_words() {
    let mut heap = Heap::default();
    let cell = heap.make(Kind::Binary, &[9; 300]).expect("room");
    heap.arena[cell.index().at() + 1] = 77;
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
    let names = [(variant(263, 0), 2), (variant(263, 65_535), 2)];
    let (mut heap, ctors) = Heap::knowing(&names);
    let one = Value::int(1).expect("small");
    for ((name, _), ctor) in names.into_iter().zip(ctors) {
        let cell = heap.ctor(ctor, &[one, Value::NIL]).expect("room");
        assert_eq!(heap.kind(cell), Some(Kind::Ctor));
        assert_eq!(heap.ctor_of(cell), Some(ctor));
        assert_eq!(heap.variant(cell), Some(name));
        let word = Value::cell(cell);
        assert_eq!(word.tag(), Tag::of_cell(name.variant_idx));
        assert_eq!(word.as_cell(), Some(cell));
        assert_eq!(fields(&heap, cell), [one.view(), Value::NIL.view()]);
    }
}

/// A chain of cells, each holding the next, is freed by one release, and
/// without a Rust call per link.
#[test]
fn freeing_a_constructor_frees_what_it_holds() {
    let (mut heap, ctors) = Heap::knowing(&[(variant(1, 0), 1), (variant(1, 1), 2)]);
    let mut tail = Value::NIL;
    for _ in 0..1_000_000 {
        tail = Value::cell(heap.ctor(ctors[0], &[tail]).expect("room"));
    }
    let shared = heap.string(b"shared").expect("room");
    heap.share(shared).expect("a live cell");
    let held = [tail, Value::cell(shared)];
    let head = heap.ctor(ctors[1], &held).expect("room");
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
    assert_eq!(heap.closure_func(cell), Some(FuncIdx(9)));
    assert_eq!(
        heap.capture(cell, 0).map(Value::view),
        Value::int(4).map(Value::view)
    );
    assert!(heap.capture(cell, 2).is_none());
    assert!(heap.capture(cell, u32::MAX).is_none());
    assert_eq!(heap.captures(cell).count(), 2);
    // A tuple that starts with a function runs none.
    for n in [1, 300] {
        let tuple = heap.tuple(&vec![Value::func(FuncIdx(9)); n]);
        let tuple = tuple.expect("room");
        assert_eq!(heap.closure_func(tuple), None);
        heap.release(tuple).expect("a live cell");
    }
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
    assert_eq!(heap.field(cell, 0).map(Value::view), Some(one.view()));
    assert!(heap.field(cell, 2).is_none());
    assert_eq!(heap.values(cell).count(), 2);
    let empty = heap.tuple(&[]).expect("room");
    assert_eq!(heap.values(empty).count(), 0);
    assert!(heap.field(empty, 0).is_none());
    heap.release(empty).expect("a live cell");
    heap.release(cell).expect("a live cell");
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

/// A share of a freed cell is refused and changes nothing, however often it
/// is tried, so the cell stays on its free list as it was.
#[test]
fn a_share_of_a_freed_cell_is_a_dead_cell() {
    let mut heap = Heap::default();
    let cell = heap.tuple(&[Value::NIL, Value::NIL]).expect("room");
    heap.release(cell).expect("a live cell");
    let arena = heap.arena.clone();
    for _ in 0..3 {
        assert_eq!(heap.share(cell), Err(DeadCell));
        assert_eq!(heap.unique(cell), Err(DeadCell));
        assert_eq!(heap.discard(cell), Err(DeadCell));
        assert_eq!(heap.release(cell), Err(DeadCell));
    }
    assert_eq!(heap.arena, arena);
    // The space went on its free list once.
    let b = heap.tuple(&[Value::NIL, Value::NIL]).expect("room");
    let c = heap.tuple(&[Value::NIL, Value::NIL]).expect("room");
    assert_eq!(cell, b);
    assert_ne!(b, c);
}

/// A cell whose count is too low is met twice while its holder is freed. The
/// second meeting finds it waiting to be freed and stops.
#[test]
fn a_dying_cell_met_again_is_a_dead_cell() {
    let mut heap = Heap::default();
    let twice = heap.string(b"held twice, counted once").expect("room");
    let v = Value::cell(twice);
    let outer = heap.tuple(&[v, v]).expect("room");
    assert_eq!(heap.release(outer), Err(DeadCell));
}

/// The same when it waits at the end of a longer list, where its count holds
/// the list's end and not another cell's index.
#[test]
fn the_last_of_the_dying_met_again_is_a_dead_cell() {
    let mut heap = Heap::default();
    let twice = heap.tuple(&[Value::NIL]).expect("room");
    let other = heap.tuple(&[Value::NIL]).expect("room");
    let v = Value::cell(twice);
    let outer = heap.tuple(&[v, Value::cell(other), v]).expect("room");
    assert_eq!(heap.release(outer), Err(DeadCell));
}

/// A cell that holds itself, which nothing can make, is freed and then found
/// on the list of the dying. The release stops there and does not free it
/// again.
#[test]
fn freeing_a_cell_that_holds_itself_is_a_dead_cell() {
    let mut heap = Heap::default();
    let cell = heap.tuple(&[Value::NIL, Value::NIL]).expect("room");
    heap.data_mut(cell)[1] = Value::cell(cell).bits();
    assert_eq!(heap.release(cell), Err(DeadCell));
    assert_eq!(heap.release(cell), Err(DeadCell));
    let b = heap.tuple(&[Value::NIL, Value::NIL]).expect("room");
    let c = heap.tuple(&[Value::NIL, Value::NIL]).expect("room");
    assert_ne!(b, c, "the space went on its list once");
    // Two that hold each other are found the same way.
    heap.data_mut(b)[0] = Value::cell(c).bits();
    heap.data_mut(c)[0] = Value::cell(b).bits();
    assert_eq!(heap.release(b), Err(DeadCell));
}

/// Freed cells of one size are a list that runs through the cells: each
/// has a zero header, and its second word names the next. The last freed is
/// the first taken.
#[test]
fn a_free_list_runs_through_the_freed_cells() {
    let mut heap = Heap::default();
    let cells: Vec<Cell> = (0..3)
        .map(|i| heap.tuple(&[int(i), int(i)]).expect("room"))
        .collect();
    assert!(heap.free.is_empty(), "nothing has been freed");
    for cell in &cells {
        heap.release(*cell).expect("a live cell");
    }
    let at = cells.iter().map(|c| c.index().0).collect::<Vec<u32>>();
    assert_eq!(heap.free, [0, 0, 0, at[2]]);
    let word = |cell: Cell, i: usize| heap.arena[cell.index().at() + i];
    for (cell, next) in [(cells[2], at[1]), (cells[1], at[0]), (cells[0], 0)] {
        assert_eq!(word(cell, 0), 0);
        assert_eq!(word(cell, 1), 3 << 32 | u64::from(next));
    }
    for cell in cells.iter().rev() {
        assert_eq!(heap.tuple(&[Value::NIL, Value::NIL]), Ok(*cell));
    }
    assert_eq!(heap.free, [0, 0, 0, 0]);
    assert_eq!((heap.live(), heap.made()), (3, 6));
}

/// A cell of one word takes two in the arena, so that a freed one has
/// somewhere to keep the next one's index.
#[test]
fn a_one_word_cell_takes_two_words() {
    let mut heap = Heap::default();
    let a = heap.tuple(&[]).expect("room");
    let b = heap.tuple(&[]).expect("room");
    assert_eq!((a, b), (plain(1), plain(3)));
    assert_eq!(heap.arena.len(), 1 + 2 + 2);
    assert_eq!(heap.values(a).count(), 0);
    heap.release(a).expect("a live cell");
    heap.release(b).expect("a live cell");
    assert_eq!(heap.tuple(&[]), Ok(b));
    assert_eq!(heap.tuple(&[]), Ok(a));
    // A tuple of one element is two words too, and shares the list.
    heap.release(a).expect("a live cell");
    assert_eq!(heap.tuple(&[int(7)]), Ok(a));
    assert_eq!(heap.field(a, 0).map(Value::view), Some(int(7).view()));
}

/// A cell made where one was freed has the words it was given and none of
/// the last one's, whatever the two kinds.
#[test]
fn a_cell_made_in_freed_space_holds_only_its_own_words() {
    let mut heap = Heap::default();
    for (first, second) in [
        (vec![1; 7], vec![2; 7]),
        (vec![3; 253], vec![4; 253]),
        (vec![5; 300], vec![6; 300]),
    ] {
        let a = heap.make(Kind::Binary, &first).expect("room");
        heap.release(a).expect("a live cell");
        let b = heap.make(Kind::ArrayBranch, &second).expect("room");
        assert_eq!(a, b);
        assert_eq!(heap.kind(b), Some(Kind::ArrayBranch));
        assert_eq!(heap.data(b), second.as_slice());
        assert_eq!(heap.unique(b), Ok(true));
    }
}

/// Many references at once are as many as one by one, and stop where a count stops.
#[test]
fn many_references_are_added_at_once() {
    for n in [0, 1, 2, 1024] {
        let mut heap = Heap::default();
        let cell = heap.string(b"held by some").expect("room");
        heap.own_times(Value::cell(cell), n).expect("a live cell");
        assert_eq!(count(heap.header(cell.index())), 1 + n as i32);
        for _ in 0..n {
            assert_eq!(heap.unique(cell), Ok(false));
            heap.release(cell).expect("a live cell");
        }
        assert_eq!(heap.unique(cell), Ok(true));
        assert_eq!(text(&heap, cell), "held by some");
        heap.release(cell).expect("a live cell");
        assert_eq!(heap.live(), 0);
        assert_eq!(heap.own_times(Value::cell(cell), n), Err(DeadCell));
        assert_eq!(heap.own_times(Value::EMPTY_ARRAY, n), Ok(()));
    }
    for (held, n, want) in [
        (i32::MAX - 5, 5, i32::MAX),
        (i32::MAX - 5, 6, UNCOUNTED),
        (i32::MAX, 0, i32::MAX),
        (1, i32::MAX as usize, UNCOUNTED),
        (1, usize::MAX, UNCOUNTED),
        (UNCOUNTED, 1024, UNCOUNTED),
    ] {
        let mut heap = Heap::default();
        let cell = heap.string(b"held by very many").expect("room");
        let rest = heap.header(cell.index()) >> 32;
        heap.recount(cell, held);
        assert_eq!(heap.own_times(Value::cell(cell), n), Ok(()));
        let h = heap.header(cell.index());
        assert_eq!((count(h), h >> 32), (want, rest));
        assert_eq!(text(&heap, cell), "held by very many");
    }
}

/// The most holders a count can say. One more and the cell is never freed,
/// and its count never changes again.
#[test]
fn a_count_that_overflows_stays_where_it_is() {
    let mut heap = Heap::default();
    let cell = heap.string(b"held by very many").expect("room");
    heap.recount(cell, i32::MAX);
    heap.release(cell).expect("a live cell");
    heap.share(cell).expect("a live cell");
    assert_eq!(count(heap.header(cell.index())), i32::MAX);
    heap.share(cell).expect("a live cell");
    assert_eq!(count(heap.header(cell.index())), UNCOUNTED);
    // The kind and the size are what they were.
    assert_eq!(text(&heap, cell), "held by very many");
    let (mut full, ctors) = Heap::knowing(&[(variant(1, 3), 2)]);
    let pair = full.ctor(ctors[0], &[Value::NIL; 2]).expect("room");
    let rest = full.header(pair.index()) >> 32;
    full.recount(pair, i32::MAX);
    full.share(pair).expect("a live cell");
    let h = full.header(pair.index());
    assert_eq!((count(h), h >> 32), (UNCOUNTED, rest));
    assert!(full.open::<2>(pair, ctors[0]).is_none());
    assert_eq!(full.header(pair.index()), h);
    for _ in 0..3 {
        assert_eq!(heap.share(cell), Ok(()));
        assert_eq!(heap.release(cell), Ok(()));
        assert_eq!(heap.release(cell), Ok(()));
        assert_eq!(heap.unique(cell), Ok(false));
    }
    assert_eq!(count(heap.header(cell.index())), UNCOUNTED);
    let holder = heap.tuple(&[Value::cell(cell)]).expect("room");
    heap.release(holder).expect("a live cell");
    assert_eq!(heap.live(), 1);
    assert_eq!(text(&heap, cell), "held by very many");
}

/// A constant's cell is found by its number, and no share or release
/// changes it. The heap gives it up at the end of a run.
#[test]
fn a_constants_cell_is_never_counted() {
    let mut heap = Heap::default();
    let (two, five) = (ConstId(2), ConstId(5));
    assert_eq!(heap.constant(five), None);
    let big: BigInt = BigInt::from(1) << 70usize;
    let cell = heap.big_int(&big).expect("room");
    assert_eq!(heap.keep_constant(five, cell), Ok(()));
    assert_eq!(heap.constant(five), Some(cell));
    assert_eq!(heap.constant(two), None);
    for _ in 0..10 {
        assert_eq!(heap.release(cell), Ok(()));
    }
    heap.share(cell).expect("a live cell");
    assert_eq!(heap.unique(cell), Ok(false));
    let holder = heap.tuple(&[Value::cell(cell)]).expect("room");
    heap.release(holder).expect("a live cell");
    assert_eq!(heap.read_big_int(cell), big);
    let other = heap.big_int(&-big).expect("room");
    assert_eq!(heap.keep_constant(two, other), Ok(()));
    assert_eq!(heap.live(), 2);
    assert_eq!(heap.let_go_of_constants(), Ok(()));
    assert_eq!(heap.live(), 0);
    assert_eq!(heap.constant(five), None);
}

/// The numbers of handles come back in the order their cells were freed,
/// when one release frees several.
#[test]
fn handles_freed_together_come_back_in_order() {
    let mut heap = Heap::default();
    let handles: Vec<Value> = [7, 8, 9]
        .map(|n| Value::cell(heap.make(Kind::Handle, &[n]).expect("room")))
        .to_vec();
    let inner = heap.tuple(&handles[1..]).expect("room");
    let outer = heap.tuple(&[handles[0], Value::cell(inner)]).expect("room");
    heap.release(outer).expect("a live cell");
    // The last cell met is the first freed.
    assert_eq!(heap.take_released(), [9, 8, 7]);
    assert!(heap.take_released().is_empty());
    assert_eq!(heap.live(), 0);
}

/// The limit of the dead-cell test. Once the freed space is a new cell, a
/// stale holder's release takes that cell's reference and frees it, and
/// nothing is reported. Until then an audit that is told of both holders
/// finds them.
#[test]
fn only_an_audit_finds_a_doubled_release_of_space_handed_out_again() {
    let mut heap = Heap::default();
    let twice = heap.tuple(&[]).expect("room");
    assert_eq!(heap.release(twice), Ok(()));
    let new = heap.tuple(&[]).expect("room");
    assert_eq!(new, twice, "the new cell is in the freed one's space");
    let held = [twice, new].map(Value::cell);
    let fault = AuditFault::Miscounted {
        cell: new.index(),
        count: 1,
        holders: 2,
    };
    assert_eq!(
        heap.audit(held.into_iter()),
        Err(AuditFailure::Found(fault))
    );
    assert_eq!(heap.release(twice), Ok(()));
    assert_eq!(heap.live(), 0);
    // The new cell's holder does find it, if it ever lets go.
    assert_eq!(heap.release(new), Err(DeadCell));
}

/// A cell waiting to be freed has no holder, whatever is asked of it. A stop
/// in the middle of a release leaves such cells, and a run ends there.
#[test]
fn a_cell_waiting_to_be_freed_is_a_dead_cell() {
    let (mut heap, ctors) = Heap::knowing(&[(variant(1, 0), 1)]);
    let cell = heap.ctor(ctors[0], &[Value::NIL]).expect("room");
    for count in [-1, -5, i32::MIN + 1] {
        heap.recount(cell, count);
        let arena = heap.arena.clone();
        assert_eq!(heap.share(cell), Err(DeadCell), "{count}");
        assert_eq!(heap.release(cell), Err(DeadCell), "{count}");
        assert_eq!(heap.unique(cell), Err(DeadCell), "{count}");
        assert_eq!(heap.discard(cell), Err(DeadCell), "{count}");
        assert!(heap.open::<1>(cell, ctors[0]).is_none(), "{count}");
        assert_eq!(heap.arena, arena, "{count}");
    }
}

/// A cell that is never counted is never freed either, so nobody is the one
/// holder who may discard it.
#[test]
fn a_cell_never_counted_cannot_be_discarded() {
    let mut heap = Heap::default();
    let cell = heap.string(b"kept").expect("room");
    heap.recount(cell, UNCOUNTED);
    assert_eq!(heap.discard(cell), Err(DeadCell));
    assert_eq!(text(&heap, cell), "kept");
}

/// Freeing a cell gives up each word it holds once, wherever the word is in
/// the cell and however many there are.
#[test]
fn freeing_a_cell_of_any_width_gives_up_every_word() {
    for n in (0..40).chain([253, 254, 300]) {
        // Every word holds a string, or only some of them do.
        for step in [1, 3, 7, 11] {
            let mut heap = Heap::default();
            let mut held = Vec::new();
            let mut values = Vec::new();
            for i in 0..n {
                if i % step == 0 {
                    let cell = heap.string(b"held").expect("room");
                    heap.share(cell).expect("a live cell");
                    held.push(cell);
                    values.push(Value::cell(cell));
                } else {
                    values.push(int(i));
                }
            }
            let outer = heap.tuple(&values).expect("room");
            heap.release(outer).expect("a live cell");
            for cell in held {
                assert_eq!(heap.unique(cell), Ok(true), "{n} {step} {cell:?}");
                heap.release(cell).expect("a live cell");
            }
            assert_eq!(heap.live(), 0, "{n} {step}");
        }
    }
}

/// A freed cell stops the release from any word of any width.
#[test]
fn freeing_a_cell_that_holds_a_freed_cell_in_any_word_is_a_dead_cell() {
    for n in [1, 7, 8, 9, 15, 16, 17, 33, 300] {
        for i in 0..n {
            let mut heap = Heap::default();
            let gone = heap.string(b"gone").expect("room");
            let mut values = vec![Value::NIL; n];
            values[i] = Value::cell(gone);
            let outer = heap.tuple(&values).expect("room");
            assert_eq!(heap.release(gone), Ok(()));
            assert_eq!(heap.release(outer), Err(DeadCell), "{n} {i}");
        }
    }
}

/// `release` frees a raw cell without looking at its words, so the raw
/// classes are the kinds that hold no value, and never a handle, whose number
/// the host has to be given. A stub is never one, because its box has to go.
#[test]
fn the_raw_kinds_are_the_ones_with_nothing_to_give_up() {
    for bits in 0..40 {
        let want = match Class(bits).kind() {
            None | Some(Kind::Handle) => false,
            Some(_) => held(Class(bits), 10).is_empty(),
        };
        let h = 1 | 3 << 40 | u64::from(bits) << 48;
        assert_eq!(raw(h), want, "{bits}");
        assert!(!raw(h & !(0xFF << 40)), "{bits}");
    }
}

/// A long cell's box is freed when the cell is. Its stub's header is
/// zeroed like any other cell's, so a second release and a unique test
/// both find it.
#[test]
fn a_second_release_of_a_long_cell_is_a_dead_cell() {
    let mut heap = Heap::default();
    let cell = heap.string(&vec![b'x'; 10_000]).expect("room");
    assert_eq!(heap.release(cell), Ok(()));
    assert_eq!(heap.release(cell), Err(DeadCell));
    assert_eq!(heap.unique(cell), Err(DeadCell));
    assert_eq!(heap.share(cell), Err(DeadCell));
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
    heap.share(cell).expect("a live cell");
    assert_eq!(heap.unique(cell), Ok(false));
    heap.release(cell).expect("a live cell");
    heap.release(cell).expect("a live cell");
    assert_eq!(heap.unique(cell), Err(DeadCell));
}

#[test]
fn discarding_a_freed_cell_is_a_dead_cell() {
    let (mut heap, ctors) = Heap::knowing(&[(variant(1, 0), 1)]);
    let cell = heap.ctor(ctors[0], &[Value::NIL]).expect("room");
    assert_eq!(heap.discard(cell), Ok(()));
    assert_eq!(heap.live(), 0);
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

/// Every index the arena can have fits in the 31 bits a value word keeps for
/// it, every variant in the 16 above them, and both come back out the same.
#[test]
fn a_cell_round_trips_through_a_value_word() {
    for at in [1, 2, 1 << 20, LIMIT_WORDS - 1] {
        let index = Index::new(at).expect("an index");
        assert!(index.0 < 1 << 31);
        for variant in [0, 1, 255, 256, u16::MAX] {
            let cell = Cell::new(index, variant);
            assert_eq!((cell.index(), cell.variant()), (index, variant));
            assert!(cell.bits() < 1 << 48);
            assert_eq!(Cell::from_bits(cell.bits()), cell);
            let word = Value::cell(cell);
            assert_eq!(word.as_cell(), Some(cell));
            assert_eq!(word.view(), View::Cell(cell));
            assert_eq!(word.tag(), Tag::of_cell(variant));
            assert_eq!(word.tag().parts(), (variant, true));
        }
    }
    assert!(Index::new(1 << 32).is_none());
}

/// The values of `held`, and whether anything asked for one.
struct Asked<'a> {
    held: std::slice::Iter<'a, Value>,
    asked: &'a std::cell::Cell<usize>,
}

impl Iterator for Asked<'_> {
    type Item = Value;

    fn next(&mut self) -> Option<Value> {
        self.asked.set(self.asked.get() + 1);
        self.held.next().copied()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.held.size_hint()
    }
}

impl ExactSizeIterator for Asked<'_> {}

/// Check that `cell` is a new one of `shape`, which starts with `first`, and
/// holds `values` after that. Nothing else holds it.
fn is_filled<const N: usize>(heap: &Heap, cell: Cell, shape: Shape<N>, values: &[Value]) {
    let what = format!("{cell:?} of {} and {}", N, values.len());
    let len = N + values.len();
    let h = heap.header(cell.index());
    assert_eq!((count(h), class(h)), (1, shape.class), "{what}");
    assert_eq!(cell.variant(), shape.variant, "{what}");
    let narrow = len < ARENA_CELL_WORDS;
    let says = if narrow { (len, len + 1) } else { (0, 0) };
    assert_eq!((scan(h), size(h)), says, "{what}");
    let want = shape.first.iter().chain(values).map(|v| v.bits());
    let got: Vec<u64> = heap.data(cell).to_vec();
    assert_eq!(got, want.collect::<Vec<_>>(), "{what}");
}

/// A cell is the same wherever it is made, with the values it is given in order
/// after those its shape starts with: at the arena's end, in the space of a
/// freed cell, and in a box. One that is not made has asked for no value.
#[test]
fn a_cell_is_filled_the_same_wherever_it_is_made() {
    fn check<const N: usize>(heap: &mut Heap, shape: Shape<N>, values: &[Value]) {
        let len = N + values.len();
        let asked = std::cell::Cell::new(0);
        let given = || Asked {
            held: values.iter(),
            asked: &asked,
        };
        let made = heap.made();
        assert_eq!(heap.in_freed(shape, given()), None, "{len}");
        assert_eq!((asked.get(), heap.made()), (0, made), "{len}");
        let end = heap.at_end(shape, given()).expect("room");
        is_filled(heap, end, shape, values);
        assert!(asked.get() >= values.len());
        let roots = |cell| [Value::cell(cell)].into_iter();
        assert_eq!(heap.audit(roots(end)), Ok(()), "{len}");
        // A long cell's box goes with it, and it is made the long way again.
        heap.release(end).expect("a live cell");
        asked.set(0);
        let again = if len < ARENA_CELL_WORDS {
            heap.in_freed(shape, given()).expect("the space is there")
        } else {
            assert_eq!(heap.in_freed(shape, given()), None, "{len}");
            assert_eq!(asked.get(), 0, "{len}");
            heap.at_end(shape, given()).expect("room")
        };
        assert_eq!(again.index(), end.index(), "{len}");
        is_filled(heap, again, shape, values);
        assert_eq!(heap.audit(roots(again)), Ok(()), "{len}");
        // Beside it, wherever there is room.
        let third = heap.given(shape, values).expect("room");
        is_filled(heap, third, shape, values);
        heap.release(third).expect("a live cell");
        let fourth = heap.given(shape, values).expect("room");
        assert_eq!(fourth.index(), third.index(), "{len}");
        is_filled(heap, fourth, shape, values);
        assert_eq!(heap.made(), made + 4, "{len}");
        let both = [again, fourth].map(Value::cell).into_iter();
        assert_eq!(heap.audit(both), Ok(()), "{len}");
        for cell in [again, fourth] {
            heap.release(cell).expect("a live cell");
        }
        assert_eq!((heap.live(), heap.boxed), (0, 0), "{len}");
    }
    let widths = (0..=9).chain([20, 252, 253, 254, 255, 300]);
    for n in widths {
        let values: Vec<Value> = (0..n).map(|i| int(i + 1)).collect();
        let fields = values.len();
        let (mut heap, ctors) = Heap::knowing(&[(variant(1, 3), 1), (variant(4, 9), fields)]);
        if fields > 0 {
            check(&mut heap, Shape::ctor(ctors[1]), &values);
        }
        check(&mut Heap::default(), Shape::TUPLE, &values);
        let closure = Shape::closure(FuncIdx(6));
        assert_eq!(closure.first[0].as_func(), Some(FuncIdx(6)));
        check(&mut Heap::default(), closure, &values);
    }
}

/// A cell the heap has no room for has asked for no value and changed nothing,
/// wherever the room ran out: at the arena's end, for a box, and for the stub
/// beside a box that fits. A place it was given for its box is spare again.
/// With the room it needs, it is made, and that place is the one it takes.
#[test]
fn a_cell_that_is_refused_has_asked_for_no_value() {
    // How many words each needs past word 0, and the words of a long one.
    for (n, words) in [
        (0, 2),
        (1, 2),
        (2, 3),
        (8, 9),
        (253, 254),
        (254, 256),
        (300, 302),
    ] {
        let values: Vec<Value> = (0..n).map(int).collect();
        for limit in [1, words, words + 1] {
            let mut heap = Heap::limited(limit);
            let asked = std::cell::Cell::new(0);
            let given = || Asked {
                held: values.iter(),
                asked: &asked,
            };
            assert_eq!(heap.in_freed(Shape::TUPLE, given()), None);
            let got = heap.at_end(Shape::TUPLE, given());
            if limit > words {
                is_filled(&heap, got.expect("room"), Shape::TUPLE, &values);
                assert_eq!(heap.words_used(), words + 1, "{n}");
                continue;
            }
            assert_eq!((got, asked.get()), (Err(Fault::Full), 0), "{n} in {limit}");
            assert_eq!((heap.made(), heap.boxed, heap.live()), (0, 0, 0));
            let places = u32::from(words > ARENA_CELL_WORDS && limit == words);
            assert!(heap.outside.iter().all(Option::is_none));
            assert_eq!(heap.outside.len(), places as usize, "{n} in {limit}");
            assert_eq!(heap.spare, (0..places).collect::<Vec<_>>());
            assert_eq!(heap.audit([].into_iter()), Ok(()));
            heap.limit = words + 1;
            let cell = heap.given(Shape::TUPLE, &values).expect("room");
            is_filled(&heap, cell, Shape::TUPLE, &values);
            let boxes = usize::from(words > ARENA_CELL_WORDS);
            assert_eq!((heap.outside.len(), heap.spare.len()), (boxes, 0));
            let held = [Value::cell(cell)].into_iter();
            assert_eq!(heap.audit(held), Ok(()), "{n} in {limit}");
        }
    }
    // A box whose stub is refused, with a freed box's number waiting: both are
    // as they were, and the next long cell takes the number.
    let mut heap = Heap::limited(400);
    let long = |n: usize| (0..n).map(int).collect::<Vec<_>>();
    let first = heap.tuple(&long(300)).expect("room");
    heap.release(first).expect("a live cell");
    let small = heap.tuple(&[int(1)]).expect("the freed stub");
    let values = long(396);
    let asked = std::cell::Cell::new(0);
    let refused = heap.at_end(
        Shape::TUPLE,
        Asked {
            held: values.iter(),
            asked: &asked,
        },
    );
    assert_eq!((refused, asked.get()), (Err(Fault::Full), 0));
    assert_eq!((heap.boxed, &heap.spare[..]), (0, &[0][..]));
    assert_eq!(heap.audit([Value::cell(small)].into_iter()), Ok(()));
    let second = heap.tuple(&long(395)).expect("room");
    assert_eq!((heap.boxed, heap.spare.len()), (395, 0));
    assert_eq!(heap.outside.len(), 1);
    assert_eq!(heap.values(second).count(), 395);
    // A stub that a cell was freed for needs no room, and its box still does.
    heap.release(small).expect("a live cell");
    heap.release(second).expect("a live cell");
    let free = heap.free[STUB_WORDS];
    for (n, fits) in [(395, true), (396, false), (4000, false)] {
        let made = heap.tuple(&long(n));
        assert_eq!(made.is_ok(), fits, "{n}");
        if let Ok(cell) = made {
            assert_eq!((cell.index().0, heap.words_used()), (free, 400));
            heap.release(cell).expect("a live cell");
        }
        assert_eq!((heap.boxed, heap.free[STUB_WORDS]), (0, free), "{n}");
    }
}

/// A cell the VM makes of values it has at hand takes the space of a freed one.
#[test]
fn a_cell_of_values_at_hand_takes_freed_space() {
    let (mut heap, ctors) = Heap::knowing(&[(variant(1, 3), 2)]);
    for shape in [Shape::TUPLE, Shape::ctor(ctors[0])] {
        let values = [int(7), int(8)];
        let first = heap.holding(shape, values).expect("room");
        is_filled(&heap, first, shape, &values);
        heap.release(first).expect("a live cell");
        let again = heap.holding(shape, values).expect("room");
        is_filled(&heap, again, shape, &values);
        assert_eq!(again.index(), first.index());
        heap.release(again).expect("a live cell");
        assert_eq!(heap.arena.len(), 1 + 3);
    }
}

/// The kinds whose header says which words are values, and how many it says
/// for each number of words. Nothing else is read as a value by the scan.
#[test]
fn a_header_says_what_its_kind_holds() {
    let (mut heap, ctors) = Heap::knowing(&[(variant(1, 4), 3)]);
    let nil = Value::NIL;
    let n = [nil.bits(); 3];
    let made = [
        (heap.string(b"0123456789abcdef"), 0),
        (heap.big_int_i128(1 << 100), 0),
        (heap.make(Kind::Range, &n), 0),
        (heap.make(Kind::Binary, &n), 0),
        (heap.make(Kind::Handle, &n), 0),
        (heap.make(Kind::BinarySlice, &n), 1),
        (heap.make(Kind::ArrayRoot, &n), 3),
        (heap.make(Kind::ArrayBranch, &n), 3),
        (heap.make(Kind::Map, &n), 0),
        (heap.make(Kind::MapNode, &n), 0),
        (heap.make(Kind::MapCollision, &n), 0),
        (heap.tuple(&[nil; 3]), 3),
        (heap.closure(FuncIdx(1), &[nil; 2]), 3),
        (heap.ctor(ctors[0], &[nil; 3]), 3),
    ];
    for (cell, want) in made {
        let cell = cell.expect("room");
        let h = heap.header(cell.index());
        assert_eq!((count(h), scan(h), size(h)), (1, want, 4), "{cell:?}");
        let all = want == 3 && heap.kind(cell) != Some(Kind::ArrayRoot);
        assert_eq!(heap.values(cell).count(), if all { 3 } else { 0 });
        let read = (0..9).filter_map(|i| heap.field(cell, i)).count();
        assert_eq!(read, want, "{cell:?}");
    }
    // A header with no words after it, and the widest a header can say.
    let unit = heap.tuple(&[]).expect("room");
    let h = heap.header(unit.index());
    assert_eq!((scan(h), size(h)), (0, 1));
    let wide = heap.tuple(&[nil; 253]).expect("room");
    let h = heap.header(wide.index());
    assert_eq!((scan(h), size(h)), (253, 254));
    // One wider is a stub, whose header says neither.
    for long in [
        heap.tuple(&[nil; 254]),
        heap.closure(FuncIdx(2), &[nil; 253]),
    ] {
        let long = long.expect("room");
        let h = heap.header(long.index());
        assert_eq!((scan(h), size(h)), (0, 0));
        assert_eq!(heap.values(long).count(), 254);
        assert!(heap.near_field(long, 0).is_none());
        assert!(heap.field(long, 253).is_some());
        assert!(heap.field(long, 254).is_none());
    }
}

/// A field is read only from a cell that is all values. A binary slice starts
/// with one, so that one can be read, and nothing after it. No other cell
/// gives one up, in the arena or in a box.
#[test]
fn a_cell_that_is_not_all_values_has_no_fields() {
    let mut heap = Heap::default();
    let nil = Value::NIL.bits();
    for n in [1, 3, 253, 254, 400] {
        for kind in [
            Kind::String,
            Kind::BigInt,
            Kind::Range,
            Kind::Binary,
            Kind::Handle,
            Kind::Map,
            Kind::MapNode,
            Kind::MapCollision,
        ] {
            let cell = heap.make(kind, &vec![nil; n]).expect("room");
            for i in [0, 1, n, 254, usize::MAX - 1, usize::MAX] {
                assert!(heap.field(cell, i).is_none(), "{kind:?} {n} {i}");
            }
            assert_eq!(heap.values(cell).count(), 0, "{kind:?} {n}");
        }
    }
}

/// The word behind a short array's slots is no field, and a cell that has one is not read
/// as one of only values. One too long for the arena, which no short array is, has no field.
#[test]
fn the_word_behind_the_slots_of_a_short_array_is_no_value() {
    let mut heap = Heap::default();
    let nil = Value::NIL.bits();
    for n in [5, 9, 253] {
        let cell = heap.make(Kind::ArrayFlat, &vec![nil; n]).expect("room");
        assert!(heap.field(cell, n - 2).is_some(), "{n}");
        assert!(heap.field(cell, n - 1).is_none(), "{n}");
        assert_eq!(heap.values(cell).count(), 0, "{n}");
    }
    let long = heap.make(Kind::ArrayFlat, &vec![nil; 400]).expect("room");
    assert!(heap.field(long, 0).is_none());
    assert_eq!(heap.values(long).count(), 0);
}

/// Values shared together are all shared, or none is where one is a freed cell.
#[test]
fn values_are_shared_together_or_not_at_all() {
    let mut heap = Heap::default();
    let [a, b, gone] = [0, 1, 2].map(|i| heap.tuple(&[int(i)]).expect("room"));
    heap.release(gone).expect("live");
    let held = |heap: &Heap| [a, b].map(|cell| heap.held_by(cell));
    let values = [a, b, gone].map(Value::cell);
    let shared = heap.share_each(&[values[0], int(7), values[1], values[2], values[0]]);
    assert_eq!(shared, Err(DeadCell));
    assert_eq!(held(&heap), [Some(1), Some(1)]);
    assert_eq!(
        heap.share_each(&[values[0], int(7), values[1], values[0]]),
        Ok(())
    );
    assert_eq!(held(&heap), [Some(3), Some(2)]);
}

/// Take apart a constructor of `N` fields, each a cell of its own, that has one
/// holder, then two, then one that is never counted.
fn opens<const N: usize>() {
    let what = format!("{N} fields");
    let (mut heap, ctors) = Heap::knowing(&[(variant(1, 0), 1), (variant(1, 7), N)]);
    let values: Vec<Value> = (0..N)
        .map(|i| Value::cell(heap.string(&[b'a'; 30][..i]).expect("room")))
        .collect();
    let bits = |of: &[Value]| of.iter().map(|v| v.bits()).collect::<Vec<_>>();
    for round in 0..2 {
        let cell = heap.ctor(ctors[1], &values).expect("room");
        heap.share(cell).expect("a live cell");
        for held in [2, 3, i32::MAX] {
            heap.recount(cell, held);
            let arena = heap.arena.clone();
            let Some(Opened::Held(same, fields)) = heap.open::<N>(cell, ctors[1]) else {
                panic!("{what}: it is held");
            };
            assert!(same == cell && bits(&fields) == bits(&values), "{what}");
            // One holder fewer, and nothing else.
            heap.share(cell).expect("a live cell");
            assert_eq!(heap.arena, arena, "{what}");
        }
        for not_counted in [UNCOUNTED, -3, 0] {
            heap.recount(cell, not_counted);
            let arena = heap.arena.clone();
            assert!(heap.open::<N>(cell, ctors[1]).is_none(), "{what}");
            assert_eq!(heap.arena, arena, "{what}");
        }
        heap.recount(cell, 1);
        // The first cell of the size to be freed, and then not the first.
        let Some(Opened::Alone(fields)) = heap.open::<N>(cell, ctors[1]) else {
            panic!("{what}: nothing else holds it");
        };
        assert_eq!(bits(&fields), bits(&values), "{what}");
        assert_eq!(heap.live(), N, "{what}");
        assert_eq!(heap.audit(values.iter().copied()), Ok(()), "{what} {round}");
        assert!(heap.open::<N>(cell, ctors[1]).is_none(), "it is freed");
    }
    // The space is the next cell's of the size, whatever that is.
    let words = heap.words_carved();
    heap.tuple(&vec![Value::NIL; N]).expect("room");
    assert_eq!(heap.words_carved(), words, "{what}");
}

#[test]
fn a_constructor_taken_apart_gives_its_fields_and_is_freed() {
    opens::<1>();
    opens::<2>();
    opens::<3>();
    opens::<4>();
    opens::<5>();
}

/// Nothing but the constructor that was asked for is taken apart: not another
/// of as many fields, not one of another number, not a cell of another kind that
/// is as long, not a long one and not a freed one. Each is left as it was. It is
/// the same for one that something else holds.
#[test]
fn nothing_else_is_taken_apart() {
    let names = [(variant(1, 0), 2), (variant(1, 1), 2), (variant(1, 2), 3)];
    let (mut heap, ctors) = Heap::knowing(&names);
    let pair = [int(1), int(2)];
    let words = pair.map(Value::bits);
    let freed = heap.ctor(ctors[0], &pair).expect("room");
    let kept = heap.ctor(ctors[0], &pair).expect("room");
    let others = [
        heap.ctor(ctors[1], &pair),
        heap.ctor(ctors[2], &[int(3); 3]),
        heap.tuple(&pair),
        heap.closure(FuncIdx(3), &pair[..1]),
        heap.string(b"eight by"),
        heap.make(Kind::ArrayBranch, &words),
        heap.make(Kind::BinarySlice, &words),
        heap.make(Kind::Handle, &[7]),
        heap.ctor(ctors[0], &[int(3); 300]),
        heap.ctor(ctors[0], &[int(3); 1]),
    ];
    let cells: Vec<Cell> = others.map(|cell| cell.expect("room")).into();
    heap.release(freed).expect("a live cell");
    for holders in [1, 2] {
        for cell in cells.iter().chain([&kept]) {
            heap.say_held(*cell, holders);
        }
        let arena = heap.arena.clone();
        for cell in cells.iter().chain([&freed]) {
            assert!(heap.open::<2>(*cell, ctors[0]).is_none(), "{cell:?}");
        }
        assert!(heap.open::<1>(kept, ctors[0]).is_none());
        assert!(heap.open::<3>(kept, ctors[0]).is_none());
        assert_eq!(heap.arena, arena);
    }
    assert!(heap.open::<2>(kept, ctors[0]).is_some());
}

/// A cell taken off a free list is named by what it is now. It had another
/// variant before, or none.
#[test]
fn a_cell_in_freed_space_is_named_by_what_it_is_now() {
    let (mut heap, ctors) = Heap::knowing(&[(variant(1, 7), 2), (variant(1, 8), 2)]);
    let pair = [int(1), int(2)];
    let first = heap.ctor(ctors[0], &pair).expect("room");
    heap.release(first).expect("a live cell");
    let second = heap.ctor(ctors[1], &pair).expect("room");
    assert_eq!(second.index(), first.index());
    assert_eq!((first.variant(), second.variant()), (7, 8));
    heap.release(second).expect("a live cell");
    let tuple = heap.tuple(&pair).expect("room");
    assert_eq!((tuple.index(), tuple.variant()), (first.index(), 0));
    heap.release(tuple).expect("a live cell");
    // The same through a stub's two words.
    let long = heap.ctor(ctors[0], &[int(3); 300]).expect("room");
    assert_eq!(long.variant(), 7);
    heap.release(long).expect("a live cell");
    let stub = heap.ctor(ctors[1], &[int(4); 260]).expect("room");
    assert_eq!((stub.index(), stub.variant()), (long.index(), 8));
}

/// A layout says of a cell what its header does, with any count, and of no cell
/// of another constructor. A cell has the fields that its constructor has.
#[test]
fn a_layout_says_what_the_header_of_a_cell_says() {
    for n in [1, 2, 8, 9, 252, 253, 254, 300, 1000] {
        let named = |variant_idx| VariantRef {
            type_id: TypeId(3),
            variant_idx,
        };
        let (mut heap, ctors) = Heap::knowing(&[(named(0), n), (named(1), n)]);
        let layout = Layout::new(ctors[1], n);
        let other = Layout::new(ctors[0], n);
        assert_ne!(layout, other);
        assert_eq!(layout.is_long(), n > 253, "{n}");
        let cell = heap.ctor(ctors[1], &vec![Value::NIL; n]).expect("room");
        for holders in [1, 2, i32::MAX, i32::MIN] {
            heap.say_held(cell, holders);
            for (field, has) in [(0, true), (n - 1, true), (n, false), (n + 1, false)] {
                assert_eq!(heap.has_field(cell, layout, field), has, "{n} {field}");
                assert!(!heap.has_field(cell, other, field), "{n} {field}");
            }
            let set = heap.set_alone(cell, layout, [0], [Value::bool(true)]);
            assert_eq!(set.is_some(), holders == 1 && n < 254, "{n} {holders}");
            assert!(heap.set_alone(cell, other, [0], [Value::NIL]).is_none());
        }
        heap.say_held(cell, 1);
        assert_eq!(heap.ctors.laid_out(layout), Some(named(1)));
        assert_eq!(heap.set_field(cell, n, Value::NIL).map(Value::bits), None);
        heap.release(cell).expect("live");
        assert!(!heap.has_field(cell, layout, 0), "{n}");
    }
}

/// A field that is never counted is not counted after a copy that kept it was
/// refused either.
#[test]
fn a_refused_copy_leaves_a_field_that_is_never_counted() {
    for n in [2, 8, 9, 17, 253, 254] {
        for wide in [false, true] {
            let named = VariantRef {
                type_id: TypeId(3),
                variant_idx: 0,
            };
            let (mut heap, ctors) = Heap::knowing(&[(named, n)]);
            let field = heap.make(Kind::Binary, &[9]).expect("room");
            let mut fields = vec![Value::NIL; n];
            fields[n - 1] = Value::cell(field);
            let cell = heap.ctor(ctors[0], &fields).expect("room");
            heap.say_held(field, i32::MIN);
            let layout = Layout::new(ctors[0], n);
            heap.limit = heap.words_used();
            let refused = if wide {
                heap.copy_set_wide(cell, layout, &[(0, Value::NIL)])
            } else {
                heap.copy_set(cell, layout, [0], [Value::NIL])
            };
            assert_eq!(refused, Err(NotCopied::Refused(Fault::Full)), "{n} {wide}");
            assert_eq!(heap.held_by(field), None, "{n} {wide}");
            heap.say_held(field, 1);
            heap.release(cell).expect("live");
            assert_eq!(heap.live(), 0);
        }
    }
}

/// A copy goes in a freed cell's space, and at the end of the arena when there is
/// none. It has the variant of what it is a copy of.
#[test]
fn a_copy_goes_where_there_is_room() {
    for n in [1, 2, 3, 4, 5, 6, 7, 8, 9, 16, 17, 253, 254] {
        let named = VariantRef {
            type_id: TypeId(3),
            variant_idx: 7,
        };
        let fields: Vec<Value> = (0..n as i64)
            .map(|i| Value::int(i).expect("small"))
            .collect();
        let new = Value::bool(true);
        let last = u8::try_from(n - 1).expect("a few");
        let mut want: Vec<u64> = fields.iter().map(|v| v.bits()).collect();
        want[n - 1] = new.bits();
        for wide in [false, true] {
            let (mut heap, ctors) = Heap::knowing(&[(named, n)]);
            let layout = Layout::new(ctors[0], n);
            let cell = heap.ctor(ctors[0], &fields).expect("room");
            let copy = |heap: &mut Heap| {
                let copy = if wide {
                    heap.copy_set_wide(cell, layout, &[(u16::from(last), new)])
                } else {
                    heap.copy_set(cell, layout, [last], [new])
                };
                let copy = copy.expect("there is room");
                let have: Vec<u64> = heap.values(copy).map(Value::bits).collect();
                assert_eq!(have, want, "{n} {wide}");
                assert_eq!(heap.ctor_of(copy), Some(ctors[0]), "{n} {wide}");
                assert_eq!(heap.variant(copy), Some(named));
                copy
            };
            let first = copy(&mut heap);
            let carved = heap.words_carved();
            heap.release(first).expect("live");
            let second = copy(&mut heap);
            assert_eq!(second.place(), first.place(), "{n} {wide}");
            assert_eq!(heap.words_carved(), carved, "{n} {wide}");
            let third = copy(&mut heap);
            assert!(heap.words_carved() > carved, "{n} {wide}");
            assert_eq!((heap.made(), heap.live()), (4, 3), "{n} {wide}");
            for cell in [second, third, cell] {
                heap.release(cell).expect("live");
            }
            assert_eq!(heap.live(), 0);
        }
    }
}
