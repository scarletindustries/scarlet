use scarlet_ir::TypeId;
use scarlet_ir::core_ir::{FuncIdx, VariantRef};

use super::*;
use crate::heap::{Class, Ctor, DeadCell, Edit, Kind, STUB_WORDS};

fn tuple(heap: &mut Heap, values: &[Value]) -> Cell {
    heap.tuple(values).expect("room")
}

fn audit(heap: &Heap, roots: &[Cell]) -> Result<(), AuditFault> {
    match heap.audit(roots.iter().copied().map(Value::cell)) {
        Ok(()) => Ok(()),
        Err(AuditFailure::Found(fault)) => Err(fault),
        Err(AuditFailure::NoRoom) => panic!("no memory for a small table"),
    }
}

/// A heap that knows a constructor of two fields, which is variant 5 of its
/// type, and one of 300, which is variant 6.
fn heap() -> (Heap, [Ctor; 2]) {
    let name = |variant_idx| VariantRef {
        type_id: TypeId(40),
        variant_idx,
    };
    let (heap, ctors) = Heap::knowing(&[(name(5), 2), (name(6), 300)]);
    (heap, [ctors[0], ctors[1]])
}

/// A cell's words for each kind there is but a constructor's, with `n` in each
/// that is a plain number and `h` in each that holds a value.
fn each_kind(n: u64, h: u64) -> [(Kind, Vec<u64>); 14] {
    [
        (Kind::String, vec![n, n]),
        (Kind::BigInt, vec![n, n]),
        (Kind::Range, vec![n, n]),
        (Kind::Binary, vec![n, n]),
        (Kind::Handle, vec![n]),
        // The binary it is a slice of, then where it starts and its length.
        (Kind::BinarySlice, vec![h, n, n]),
        (Kind::Tuple, vec![h, h]),
        (Kind::Closure, vec![Value::func(FuncIdx(1)).bits(), h]),
        (Kind::ArrayLeaf, vec![h, h, h]),
        // Its length, height and offset, then the head, tree and tail.
        (Kind::ArrayRoot, vec![n, n, n, h, h, h]),
        // Its height, two sizes, then two children.
        (Kind::ArrayBranch, vec![n, n, n, h, h]),
        (Kind::Map, vec![n, h]),
        (Kind::MapNode, vec![n, h, h, h]),
        (Kind::MapCollision, vec![n, h, h]),
    ]
}

/// The classes below the first constructor's are these kinds and one spare.
#[test]
fn each_kind_is_every_kind() {
    let kinds = each_kind(0, 0).map(|(kind, _)| Some(kind));
    let all: Vec<Option<Kind>> = (1..15).map(|c| Class(c).kind()).collect();
    assert_eq!(kinds.as_slice(), all.as_slice());
    assert_eq!([0, 15].map(|c| Class(c).kind()), [None, None]);
    for c in [16, 17, 255, 256, u16::MAX] {
        assert_eq!(Class(c).kind(), Some(Kind::Ctor));
    }
    for c in 0..40 {
        for kind in kinds.into_iter().flatten().chain([Kind::Ctor]) {
            let same = Class(c).kind() == Some(kind);
            assert_eq!(Class(c).is(kind), same, "{c} {kind:?}");
        }
    }
}

/// [`each_kind`], of cells too long for the arena, for the kinds that can have
/// one.
fn each_long_kind(n: u64, h: u64) -> [(Kind, Vec<u64>); 6] {
    let after = |first: u64, rest: u64| [vec![first], vec![rest; 299]].concat();
    [
        (Kind::String, vec![n; 300]),
        (Kind::BigInt, vec![n; 300]),
        (Kind::Binary, vec![n; 300]),
        (Kind::Tuple, vec![h; 300]),
        (Kind::Closure, after(Value::func(FuncIdx(1)).bits(), h)),
        (Kind::MapCollision, after(n, h)),
    ]
}

/// A cell of each kind, a long one of each that can be, and a constructor's
/// narrow and wide, with every word that holds a value holding `held`, whose
/// count goes up with each.
fn one_of_each(heap: &mut Heap, ctors: [Ctor; 2], held: Cell, n: u64) -> Vec<Cell> {
    let h = Value::cell(held).bits();
    let mut cells = Vec::new();
    for (kind, data) in each_kind(n, h).into_iter().chain(each_long_kind(n, h)) {
        for w in &data {
            if *w == h {
                heap.share(held).expect("a live cell");
            }
        }
        cells.push(heap.make(kind, &data).expect("room"));
    }
    for (ctor, fields) in ctors.into_iter().zip([2, 300]) {
        for _ in 0..fields {
            heap.share(held).expect("a live cell");
        }
        let values = vec![Value::cell(held); fields];
        cells.push(heap.ctor(ctor, &values).expect("room"));
    }
    cells
}

#[test]
fn a_heap_with_nothing_in_it_passes() {
    assert_eq!(audit(&Heap::default(), &[]), Ok(()));
}

/// A heap with every kind of cell in it, long and short, live and free, held
/// by roots and by each other, passes as it is.
#[test]
fn a_heap_whose_counts_are_right_passes() {
    let (mut heap, ctors) = heap();
    let text = heap.string(b"shared").expect("room");
    heap.share(text).expect("a live cell");
    let long = heap.string(&[b'x'; 5000]).expect("room");
    let wide: Vec<Value> = (0..300)
        .map(|i| {
            if i == 150 {
                Value::cell(long)
            } else {
                Value::NIL
            }
        })
        .collect();
    let wide = tuple(&mut heap, &wide);
    let t = Value::cell(text);
    let pair = tuple(&mut heap, &[t, t, Value::cell(wide)]);
    let gone = [1usize, 2, 7, 300].map(|n| {
        let values = vec![Value::NIL; n];
        tuple(&mut heap, &values)
    });
    for cell in gone {
        heap.release(cell).expect("a live cell");
    }
    let unit = tuple(&mut heap, &[]);
    let mut roots = vec![pair, unit];
    roots.extend(one_of_each(&mut heap, ctors, text, 7));
    assert_eq!(audit(&heap, &roots), Ok(()));
    // A hollowed constructor is one whose fields are all `Nil`.
    let hollow = heap.ctor(ctors[0], &[Value::NIL; 2]).expect("room");
    assert_eq!(heap.hollow(hollow), Ok(true));
    roots.push(hollow);
    assert_eq!(audit(&heap, &roots), Ok(()));
    for root in roots {
        heap.release(root).expect("a live cell");
    }
    assert_eq!(audit(&heap, &[]), Ok(()));
    assert_eq!(heap.live(), 0);
}

/// A branch's size table, a slice's offset and the like are plain numbers, so
/// nothing there counts as a holder, even when it has the bits of a cell.
/// Every other word does.
#[test]
fn only_the_words_that_hold_values_are_holders() {
    let (mut heap, ctors) = heap();
    let number = heap.string(b"only a number's bits").expect("room");
    let held = heap.string(b"held").expect("room");
    let n = Value::cell(number).bits();
    let mut cells = one_of_each(&mut heap, ctors, held, n);
    while let Some(cell) = cells.pop() {
        let roots = [cells.as_slice(), &[number, held, cell]].concat();
        assert_eq!(audit(&heap, &roots), Ok(()), "{:?}", heap.kind(cell));
        // Giving the cell up gives up what the audit says it holds.
        heap.release(cell).expect("a live cell");
        let roots = [cells.as_slice(), &[number, held]].concat();
        assert_eq!(audit(&heap, &roots), Ok(()), "{cell:?}");
    }
}

/// A header made wrong by hand, one part at a time: each is found, and named.
#[test]
fn a_header_its_class_does_not_have_is_found() {
    let (mut heap, ctors) = heap();
    let held = heap.string(b"held").expect("room");
    let cells = one_of_each(&mut heap, ctors, held, 0);
    let roots = [cells.as_slice(), &[held]].concat();
    assert_eq!(audit(&heap, &roots), Ok(()));
    for cell in &cells {
        let (at, right) = (cell.index(), heap.header(cell.index()));
        let misshapen = Err(AuditFault::Misshapen { cell: at });
        let mut found = |h: u64| {
            heap.arena[at.at()] = h;
            let fault = audit(&heap, &roots);
            heap.arena[at.at()] = right;
            fault
        };
        // A scan of one more, one less, and the most a header can say.
        let one = 1 << 32;
        assert_eq!(found(right + one), misshapen, "{right:x}");
        assert_eq!(found(right | 0xFF << 32), misshapen, "{right:x}");
        if scan(right) > 0 {
            assert_eq!(found(right - one), misshapen, "{right:x}");
        }
        // A class no cell has: a freed cell's, the spare one, and that of a
        // constructor the program does not have.
        for wrong in [0, 15, 18, u64::from(u16::MAX)] {
            let h = right & !(0xFFFF << 48) | wrong << 48;
            assert_eq!(found(h), misshapen, "{right:x}");
        }
        // A constructor's class on a cell with another number of words.
        for wrong in [16, 17] {
            let h = right & !(0xFFFF << 48) | wrong << 48;
            let fields = if wrong == 16 { 2 } else { 300 };
            if heap.data(*cell).len() != fields {
                heap.arena[at.at()] = h;
                assert_eq!(audit(&heap, &roots), misshapen, "{right:x}");
                heap.arena[at.at()] = right;
            }
        }
    }
    assert_eq!(audit(&heap, &roots), Ok(()));
}

/// The size in a header is where the next cell starts, so a wrong one leaves
/// the arena in pieces that are no cells, and one that is too long for an
/// arena cell is no cell at all.
#[test]
fn a_size_the_cell_does_not_have_is_found() {
    for (wrong, want) in [
        // The next cell starts in this one's last word.
        (3, AuditFault::NoCell { at: 4 }),
        // It starts past the next one's header.
        (5, AuditFault::NoCell { at: 6 }),
        (255, AuditFault::NoCell { at: 1 }),
    ] {
        let mut heap = Heap::default();
        let cell = tuple(&mut heap, &[Value::NIL; 3]);
        let other = tuple(&mut heap, &[Value::NIL; 3]);
        assert_eq!(audit(&heap, &[cell, other]), Ok(()));
        heap.arena[1] = heap.arena[1] & !(0xFF << 40) | wrong << 40;
        let found = audit(&heap, &[cell, other]);
        assert_eq!(found, Err(want), "{wrong}");
    }
    // A cell longer than one in the arena may be, with all its words there.
    let mut heap = Heap::default();
    let cell = heap.make(Kind::Binary, &[9; 253]).expect("room");
    assert_eq!(audit(&heap, &[cell]), Ok(()));
    heap.arena.push(9);
    heap.arena[1] += 1 << 40;
    assert_eq!(audit(&heap, &[cell]), Err(AuditFault::NoCell { at: 1 }));
    // A cell whose words are in the arena called a stub, and a stub whose box
    // has the words of a cell that fits in the arena.
    let mut heap = Heap::default();
    let cell = tuple(&mut heap, &[Value::NIL]);
    heap.arena[1] &= !(0xFF << 40);
    let boxed = audit(&heap, &[cell]);
    assert_eq!(boxed, Err(AuditFault::NoBox { cell: cell.index() }));
    let mut heap = Heap::default();
    let long = heap.make(Kind::Binary, &[9; 300]).expect("room");
    heap.outside[0] = Some(Box::new([9; 253]));
    heap.boxed = 253;
    let short = audit(&heap, &[long]);
    let cell = long.index();
    assert_eq!(short, Err(AuditFault::Misshapen { cell }));
    heap.outside[0] = Some(Box::new([9; 254]));
    heap.boxed = 254;
    assert_eq!(audit(&heap, &[long]), Ok(()));
    // A box with nothing in it is too short as well, and no missing box.
    heap.outside[0] = Some(Box::new([]));
    heap.boxed = 0;
    let empty = audit(&heap, &[long]);
    assert_eq!(empty, Err(AuditFault::Misshapen { cell }));
}

/// A freed cell's header is all zeros. One that still says anything is not a
/// freed cell, and not a live one either.
#[test]
fn a_freed_cell_that_says_anything_is_found() {
    for left in [2 << 40, 1 << 32, 7 << 48, 16 << 48] {
        let mut heap = Heap::default();
        let cell = tuple(&mut heap, &[Value::NIL]);
        heap.release(cell).expect("a live cell");
        assert_eq!(audit(&heap, &[]), Ok(()));
        heap.arena[1] = left;
        assert_eq!(audit(&heap, &[]), Err(AuditFault::NoCell { at: 1 }));
    }
}

/// A reference that carries another variant than its cell's, held by the run,
/// by a cell in the arena, by one in a box, and by a constant.
#[test]
fn a_reference_with_the_wrong_variant_is_found() {
    let (mut heap, ctors) = heap();
    let pair = heap.ctor(ctors[0], &[Value::NIL; 2]).expect("room");
    let wide = heap.ctor(ctors[1], &[Value::NIL; 300]).expect("room");
    let text = heap.string(b"of no variant").expect("room");
    for (cell, wrongs) in [
        (pair, [0, 4, 6, u16::MAX]),
        (wide, [0, 5, 7, 32_768]),
        (text, [1, 5, 6, u16::MAX]),
    ] {
        for variant in wrongs {
            let others = [pair, wide, text].into_iter().filter(|c| *c != cell);
            let others: Vec<Cell> = others.collect();
            let wrong = Cell::new(cell.index(), variant);
            let to = cell.index();
            let fault = |holder| {
                Err(AuditFault::WrongVariant {
                    holder,
                    to,
                    variant,
                })
            };
            let roots = [others.as_slice(), &[wrong]].concat();
            assert_eq!(audit(&heap, &roots), fault(None), "{variant}");
            for n in [1, 253, 300] {
                let mut values = vec![Value::NIL; n];
                values[n - 1] = Value::cell(cell);
                heap.share(cell).expect("a live cell");
                let holder = tuple(&mut heap, &values);
                let roots = [&[pair, wide, text], [holder].as_slice()].concat();
                assert_eq!(audit(&heap, &roots), Ok(()));
                heap.data_mut(holder)[n - 1] = Value::cell(wrong).bits();
                let held = audit(&heap, &roots);
                assert_eq!(held, fault(Some(holder.index())), "{variant} {n}");
                heap.release(holder).expect("a live cell");
            }
        }
    }
    let roots = [pair, wide, text];
    assert_eq!(audit(&heap, &roots), Ok(()));
    heap.keep_constant(scarlet_ir::core_ir::ConstId(0), text)
        .expect("room");
    heap.constants[0] = Some(Cell::new(text.index(), 3));
    assert_eq!(
        audit(&heap, &roots),
        Err(AuditFault::WrongVariant {
            holder: None,
            to: text.index(),
            variant: 3
        })
    );
}

#[test]
fn a_closure_with_no_function_is_found() {
    for n in [1, 254] {
        let mut heap = Heap::default();
        let captures = vec![Value::NIL; n];
        let cell = heap.closure(FuncIdx(3), &captures).expect("room");
        assert_eq!(audit(&heap, &[cell]), Ok(()));
        heap.data_mut(cell)[0] = Value::NIL.bits();
        let no_function = AuditFault::NoFunction { cell: cell.index() };
        assert_eq!(audit(&heap, &[cell]), Err(no_function));
    }
}

#[test]
fn a_count_with_the_wrong_number_of_holders_is_found() {
    let mut heap = Heap::default();
    let cell = heap.string(b"held").expect("room");
    let holder = tuple(&mut heap, &[Value::cell(cell)]);
    let wrong = |count, holders| {
        Err(AuditFault::Miscounted {
            cell: cell.index(),
            count,
            holders,
        })
    };
    assert_eq!(audit(&heap, &[holder, cell]), wrong(1, 2));
    heap.share(cell).expect("a live cell");
    assert_eq!(audit(&heap, &[holder, cell]), Ok(()));
    assert_eq!(audit(&heap, &[holder]), wrong(2, 1));
    // A cell a stopped release left waiting to be freed is no better.
    heap.recount(cell, -5);
    assert_eq!(audit(&heap, &[holder, cell]), wrong(-5, 2));
}

/// A root, or a cell, that holds something which is not a live cell's header.
#[test]
fn a_reference_to_no_live_cell_is_found() {
    let mut heap = Heap::default();
    let cell = tuple(&mut heap, &[Value::NIL, Value::NIL]);
    let gone = tuple(&mut heap, &[Value::NIL]);
    heap.release(gone).expect("a live cell");
    let past = u32::try_from(heap.arena.len()).expect("small");
    let inside = cell.index().0 + 1;
    for at in [gone.index().0, 0, past, u32::MAX, inside] {
        let to = Index(at);
        let fault = |holder| Err(AuditFault::Dangling { holder, to });
        let name = Cell::new(to, 0);
        assert_eq!(audit(&heap, &[cell, name]), fault(None), "{to:?}");
        heap.data_mut(cell)[1] = Value::cell(name).bits();
        let held = audit(&heap, &[cell]);
        assert_eq!(held, fault(Some(cell.index())), "{to:?}");
        heap.data_mut(cell)[1] = Value::NIL.bits();
    }
    assert_eq!(audit(&heap, &[cell]), Ok(()));
}

/// Freeing twice is refused, so it takes a write over the arena to put a
/// cell on its list twice, or on the wrong one.
#[test]
fn a_broken_free_list_is_found() {
    let freed = || {
        let mut heap = Heap::default();
        let cells = [0, 1, 2].map(|_| tuple(&mut heap, &[Value::NIL, Value::NIL]));
        for cell in cells {
            heap.release(cell).expect("a live cell");
        }
        assert_eq!(audit(&heap, &[]), Ok(()));
        (heap, cells)
    };
    let at = |cell: Cell| cell.index().0;
    let bad = |space, to| Err(AuditFault::BadLink { space, to });

    // The last links back to the first, which would never end.
    let (mut heap, cells) = freed();
    heap.arena[cells[0].index().at() + 1] |= u64::from(at(cells[2]));
    assert_eq!(audit(&heap, &[]), bad(3, at(cells[2])));

    // A list of another size starts at one of them.
    let (mut heap, cells) = freed();
    heap.free.resize(6, 0);
    heap.free[5] = at(cells[1]);
    assert_eq!(audit(&heap, &[]), bad(5, at(cells[1])));

    // A list names a live cell, and an index past the arena.
    let (mut heap, cells) = freed();
    let live = tuple(&mut heap, &[Value::NIL, Value::NIL]);
    assert_eq!(live, cells[2]);
    heap.free[3] = at(live);
    assert_eq!(audit(&heap, &[live]), bad(3, at(live)));
    heap.free[3] = u32::MAX;
    assert_eq!(audit(&heap, &[live]), bad(3, u32::MAX));

    // A list is cut short.
    let (mut heap, cells) = freed();
    heap.free[3] = at(cells[0]);
    let cell = cells[1].index();
    assert_eq!(audit(&heap, &[]), Err(AuditFault::Unlisted { cell }));
}

/// Every word of the arena past word 0 belongs to one cell.
#[test]
fn an_arena_that_is_not_made_of_cells_is_found() {
    // The cells of such an arena cannot be counted either.
    let at = |heap: &Heap, root| match audit(heap, &[root]) {
        Err(AuditFault::NoCell { at }) => {
            assert_eq!(heap.live_cells(), Err(AuditFault::NoCell { at }));
            Some(at)
        }
        _ => None,
    };
    let two = || {
        let mut heap = Heap::default();
        let cell = tuple(&mut heap, &[Value::NIL]);
        tuple(&mut heap, &[Value::NIL, Value::NIL]);
        (heap, cell)
    };
    // A header no cell has: no count, or more words than an arena cell.
    for h in [2 << 40, 7 << 48, 1 | 255 << 40] {
        let (mut heap, cell) = two();
        heap.arena[3] = h;
        assert_eq!(at(&heap, cell), Some(3));
    }
    // A cell that runs past the arena's end.
    let (mut heap, cell) = two();
    heap.arena.pop();
    assert_eq!(at(&heap, cell), Some(3));
    // A free cell that does not say how long it is.
    let (mut heap, cell) = two();
    heap.arena[3] = 0;
    heap.arena[4] = 0;
    assert_eq!(at(&heap, cell), Some(3));
    // A word left over at the end.
    let (mut heap, cell) = two();
    heap.arena.push(0);
    assert_eq!(at(&heap, cell), Some(6));
}

#[test]
fn boxes_and_stubs_that_do_not_match_are_found() {
    let long = |heap: &mut Heap| heap.make(Kind::Binary, &[9; 300]).expect("room");
    let two = || {
        let mut heap = Heap::default();
        let (a, b) = (long(&mut heap), long(&mut heap));
        let gone = long(&mut heap);
        heap.release(gone).expect("a live cell");
        assert_eq!(audit(&heap, &[a, b]), Ok(()));
        (heap, a, b)
    };
    // A stub with no box, and two with one box.
    for number in [2, 77, 0] {
        let (mut heap, a, b) = two();
        heap.arena[b.index().at() + STUB_WORDS - 1] = number;
        let cell = b.index();
        assert_eq!(audit(&heap, &[a, b]), Err(AuditFault::NoBox { cell }));
    }
    // A box with no stub, a place nothing would take again, one that would
    // be taken twice, and a wrong total.
    let broken: [fn(&mut Heap); 4] = [
        |heap| heap.outside.push(Some(Box::new([]))),
        |heap| heap.spare.clear(),
        |heap| heap.spare.push(2),
        |heap| heap.boxed += 1,
    ];
    for breaks in broken {
        let (mut heap, a, b) = two();
        breaks(&mut heap);
        assert_eq!(audit(&heap, &[a, b]), Err(AuditFault::Boxes));
    }
}

/// A constant is held by the heap itself and by any number of others, and
/// its count says nothing about them.
#[test]
fn a_constants_holders_are_not_counted() {
    let mut heap = Heap::default();
    let cell = heap.string(b"a constant").expect("room");
    heap.keep_constant(scarlet_ir::core_ir::ConstId(1), cell)
        .expect("room");
    let holder = tuple(&mut heap, &[Value::cell(cell), Value::cell(cell)]);
    assert_eq!(audit(&heap, &[holder]), Ok(()));
    assert_eq!(audit(&heap, &[holder, cell, cell]), Ok(()));
    heap.recount(cell, 4);
    let counted = AuditFault::CountedConstant { cell: cell.index() };
    assert_eq!(audit(&heap, &[holder]), Err(counted));
    let to = Index(holder.index().0 + 1);
    heap.constants[1] = Some(Cell::new(to, 0));
    assert_eq!(
        audit(&heap, &[holder]),
        Err(AuditFault::Dangling { holder: None, to })
    );
}

/// A cell that is never freed and is not a constant's would never be missed:
/// its count says nothing, and every reference to it is good.
#[test]
fn a_cell_never_counted_that_is_no_constant_is_found() {
    let mut heap = Heap::default();
    let constant = heap.string(b"a constant").expect("room");
    heap.keep_constant(scarlet_ir::core_ir::ConstId(0), constant)
        .expect("room");
    let cell = heap.string(b"not one").expect("room");
    assert_eq!(audit(&heap, &[cell, constant]), Ok(()));
    heap.recount(cell, UNCOUNTED);
    let fault = Err(AuditFault::NeverCounted { cell: cell.index() });
    assert_eq!(audit(&heap, &[cell, constant]), fault);
    assert_eq!(audit(&heap, &[]), fault, "held by nothing at all");
    // A constant given a second cell leaves the first one behind.
    let second = heap.string(b"a constant").expect("room");
    heap.recount(cell, 1);
    heap.keep_constant(scarlet_ir::core_ir::ConstId(0), second)
        .expect("room");
    assert_eq!(
        audit(&heap, &[cell]),
        Err(AuditFault::NeverCounted {
            cell: constant.index()
        })
    );
}

/// The system refuses a table it has no room for, and that is not a fault of
/// the heap's.
#[test]
fn a_table_there_is_no_memory_for_is_refused() {
    assert_eq!(table(3, 7), Ok(vec![7, 7, 7]));
    assert_eq!(table(usize::MAX, 7_u32), Err(AuditFailure::NoRoom));
    assert_eq!(table(usize::MAX, false), Err(AuditFailure::NoRoom));
}

/// What a person is shown for each.
#[test]
fn each_fault_names_the_cell() {
    let cell = Index(12);
    for (fault, text) in [
        (
            AuditFault::NoCell { at: 12 },
            "the heap has no whole cell at word 12",
        ),
        (
            AuditFault::Dangling {
                holder: Some(Index(3)),
                to: cell,
            },
            "cell 3 holds cell 12, which is not a live cell",
        ),
        (
            AuditFault::WrongVariant {
                holder: Some(Index(3)),
                to: cell,
                variant: 4,
            },
            "cell 3 holds cell 12 as variant 4, which it is not",
        ),
        (
            AuditFault::WrongVariant {
                holder: None,
                to: cell,
                variant: 0,
            },
            "the run holds cell 12 as variant 0, which it is not",
        ),
        (
            AuditFault::Misshapen { cell },
            "the header of cell 12 is none a cell of its class has",
        ),
        (
            AuditFault::NoFunction { cell },
            "the closure cell 12 has no function",
        ),
        (
            AuditFault::Dangling {
                holder: None,
                to: cell,
            },
            "the run holds cell 12, which is not a live cell",
        ),
        (
            AuditFault::Miscounted {
                cell,
                count: 1,
                holders: 2,
            },
            "cell 12 has a count of 1 and 2 holders",
        ),
        (
            AuditFault::BadLink { space: 3, to: 12 },
            "the free list for 3 words names cell 12, which is not a free cell of that size, or names it twice",
        ),
        (
            AuditFault::Unlisted { cell },
            "the free cell 12 is on no free list",
        ),
        (
            AuditFault::NoBox { cell },
            "the long cell 12 has no box of its own",
        ),
        (AuditFault::Boxes, "the boxes of long cells do not add up"),
        (
            AuditFault::CountedConstant { cell },
            "cell 12 is a constant's and can be freed",
        ),
        (
            AuditFault::NeverCounted { cell },
            "cell 12 can never be freed and is not a constant's",
        ),
    ] {
        assert_eq!(fault.to_string(), text);
    }
}

/// Make, share, copy, splice, discard and give up cells of every size in an
/// order that looks random and is the same every time, auditing the heap
/// after each step. At the end everything is given up and nothing is left.
#[test]
fn a_heap_stays_whole_through_a_long_run_of_everything() {
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut below = move |n: usize| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        usize::try_from(state >> 33).expect("small") % n
    };
    let (mut heap, ctors) = heap();
    let mut held: Vec<Cell> = Vec::new();
    for step in 0..2000 {
        match below(10) {
            0 => held.push(heap.string(&vec![b's'; below(60)]).expect("room")),
            1 => held.push(heap.make(Kind::Binary, &vec![1; below(600)]).expect("room")),
            // A cell that takes over some of what is held.
            2 | 3 => {
                let n = [0, 1, 2, 3, 8, 32, 260][below(7)];
                let mut values = vec![Value::NIL; n];
                for slot in values.iter_mut().take(below(4)) {
                    if !held.is_empty() {
                        *slot = Value::cell(held.swap_remove(below(held.len())));
                    }
                }
                held.push(tuple(&mut heap, &values));
            }
            4 if !held.is_empty() => {
                let cell = held[below(held.len())];
                heap.share(cell).expect("a live cell");
                held.push(cell);
            }
            5 if !held.is_empty() => {
                let cell = held[below(held.len())];
                held.push(heap.copy(cell).expect("room"));
            }
            // A constructor, narrow or wide, that takes over some.
            8 => {
                let wide = below(8) == 0;
                let mut values = vec![Value::NIL; if wide { 300 } else { 2 }];
                for slot in values.iter_mut().take(below(3)) {
                    if !held.is_empty() {
                        *slot = Value::cell(held.swap_remove(below(held.len())));
                    }
                }
                held.push(heap.ctor(ctors[usize::from(wide)], &values).expect("room"));
            }
            // One written over another, when nothing else holds it.
            9 if !held.is_empty() => {
                let cell = held.swap_remove(below(held.len()));
                let fields = heap.values(cell).count();
                let ctor = ctors[usize::from(fields > 2)];
                match heap.hollow(cell) {
                    Ok(true) if heap.fits_ctor(cell, fields) => {
                        held.push(heap.ctor_in(cell, ctor, vec![Value::NIL; fields].into_iter()));
                    }
                    Ok(_) => held.push(cell),
                    Err(DeadCell) => panic!("a live cell"),
                }
            }
            // A node that grows or shrinks by moving to another cell.
            6 => {
                let old = heap
                    .make(Kind::MapNode, &vec![Value::NIL.bits(); 1 + below(300)])
                    .expect("room");
                let n = heap.data(old).len();
                let cut = below(n);
                let edit = Edit {
                    cut: cut..n.min(cut + below(5)),
                    at: 0,
                    put: &[Value::NIL.bits(); 2],
                };
                let new = heap
                    .splice(Kind::MapNode, old, n, edit, below(5))
                    .expect("room");
                assert_eq!(audit(&heap, &[held.as_slice(), &[new]].concat()), Ok(()));
                heap.discard(new).expect("a live cell");
            }
            _ => {
                for _ in 0..=below(3) {
                    if !held.is_empty() {
                        let cell = held.swap_remove(below(held.len()));
                        heap.release(cell).expect("a live cell");
                    }
                }
            }
        }
        assert_eq!(audit(&heap, &held), Ok(()), "step {step}");
    }
    assert!(heap.made() > 800, "{}", heap.made());
    assert!(heap.reused() > 10, "{}", heap.reused());
    for cell in held {
        heap.release(cell).expect("a live cell");
    }
    assert_eq!(audit(&heap, &[]), Ok(()));
    assert_eq!((heap.live(), heap.boxed), (0, 0));
}
