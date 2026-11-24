use super::*;

impl Heap {
    /// A heap that holds at most `limit` words.
    fn limited(limit: usize) -> Heap {
        Heap {
            limit: limit.min(LIMIT_WORDS),
            ..Heap::default()
        }
    }

    /// How many cells are not yet freed.
    pub(crate) fn live(&self) -> usize {
        self.live_cells().expect("an arena of whole cells")
    }
}

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

/// A cell of up to [`ARENA_CELL_WORDS`] words is in the arena. One word
/// more and it is a stub and a box.
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
    heap.share(held).expect("a live cell");
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
    assert_eq!(heap.blank(Kind::Binary, MAX_CELL_WORDS), Err(Fault::Full));
    assert_eq!(heap.blank(Kind::Binary, usize::MAX), Err(Fault::Full));
    assert!(heap.arena.is_empty());
    assert_eq!((heap.live(), heap.made()), (0, 0));
}

/// A name the heap never gave out reads as a freed cell: index 0, an
/// index past the arena's end, and the largest index a value word
/// holds. Nothing panics, and no word of the arena changes.
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
        assert_eq!(heap.share(cell), Err(DeadCell));
        assert_eq!(heap.release(cell), Err(DeadCell));
        assert_eq!(heap.hollow(cell), Err(DeadCell));
        assert_eq!(heap.discard(cell), Err(DeadCell));
        assert_eq!(heap.copy(cell), Err(Fault::DeadCell));
        assert!(!heap.fits_ctor(cell, 0));
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
    heap.share(shared).expect("a live cell");
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
        heap.share(held).expect("a live cell");
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
        assert_eq!(heap.hollow(cell), Err(DeadCell));
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

/// A copy shares what the node holds, and stops when one of those cells has
/// been freed behind the node's back.
#[test]
fn a_copy_of_a_node_that_holds_a_freed_cell_is_a_dead_cell() {
    let mut heap = Heap::default();
    let held = heap.string(b"held").expect("room");
    let leaf = heap
        .make(Kind::ArrayLeaf, &[Value::cell(held).bits()])
        .expect("room");
    heap.release(held).expect("a live cell");
    assert_eq!(heap.copy(leaf), Err(Fault::DeadCell));
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
    assert_eq!(heap.free, [0, 0, 0, cells[2].0]);
    let word = |cell: Cell, i: usize| heap.arena[cell.index() + i];
    for (cell, next) in [
        (cells[2], cells[1].0),
        (cells[1], cells[0].0),
        (cells[0], 0),
    ] {
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
    assert_eq!((a, b), (Cell(1), Cell(3)));
    assert_eq!(heap.arena.len(), 1 + 2 + 2);
    assert_eq!(heap.elements(a).count(), 0);
    heap.release(a).expect("a live cell");
    heap.release(b).expect("a live cell");
    assert_eq!(heap.tuple(&[]), Ok(b));
    assert_eq!(heap.tuple(&[]), Ok(a));
    // A tuple of one element is two words too, and shares the list.
    heap.release(a).expect("a live cell");
    assert_eq!(heap.tuple(&[int(7)]), Ok(a));
    assert_eq!(heap.element(a, 0).map(Value::view), Some(int(7).view()));
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
        let b = heap.make(Kind::ArrayLeaf, &second).expect("room");
        assert_eq!(a, b);
        assert_eq!(heap.kind(b), Some(Kind::ArrayLeaf));
        assert_eq!(heap.data(b), second.as_slice());
        assert_eq!(heap.unique(b), Ok(true));
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
    assert_eq!(count(heap.header(cell)), i32::MAX);
    heap.share(cell).expect("a live cell");
    assert_eq!(count(heap.header(cell)), UNCOUNTED);
    // The kind and the size are what they were.
    assert_eq!(text(&heap, cell), "held by very many");
    for _ in 0..3 {
        assert_eq!(heap.share(cell), Ok(()));
        assert_eq!(heap.release(cell), Ok(()));
        assert_eq!(heap.release(cell), Ok(()));
        assert_eq!(heap.unique(cell), Ok(false));
        assert_eq!(heap.hollow(cell), Ok(false));
    }
    assert_eq!(count(heap.header(cell)), UNCOUNTED);
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
        cell: new,
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
    let mut heap = Heap::default();
    let cell = heap.ctor(v(), &[Value::NIL]).expect("room");
    for count in [-1, -5, i32::MIN + 1] {
        heap.recount(cell, count);
        let arena = heap.arena.clone();
        assert_eq!(heap.share(cell), Err(DeadCell), "{count}");
        assert_eq!(heap.release(cell), Err(DeadCell), "{count}");
        assert_eq!(heap.unique(cell), Err(DeadCell), "{count}");
        assert_eq!(heap.hollow(cell), Err(DeadCell), "{count}");
        assert_eq!(heap.discard(cell), Err(DeadCell), "{count}");
        assert!(!heap.fits_ctor(cell, 1), "{count}");
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
            let outer = heap.ctor(v(), &values).expect("room");
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

/// `release` frees a kind on the `RAW` list without looking at its words, so
/// the list is the kinds that hold no value, and never a handle, whose number
/// the host has to be given.
#[test]
fn the_raw_kinds_are_the_ones_with_nothing_to_give_up() {
    for bits in 0..16 {
        let raw = match kind_of(bits << 32) {
            None | Some(Kind::Handle) => false,
            Some(_) => held(bits, 10).is_empty(),
        };
        assert_eq!(RAW >> bits & 1 == 1, raw, "{bits}");
    }
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
fn discarding_or_hollowing_a_freed_cell_is_a_dead_cell() {
    let mut heap = Heap::default();
    let v = VariantRef {
        type_id: TypeId(1),
        variant_idx: 0,
    };
    let cell = heap.ctor(v, &[Value::NIL]).expect("room");
    heap.share(cell).expect("a live cell");
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
