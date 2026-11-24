use super::*;
use crate::heap::{Edit, Kind, STUB_WORDS};

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

/// A cell's words for each kind there is, with `n` in each that is a plain
/// number and `h` in each that holds a value.
fn each_kind(n: u64, h: u64) -> [(Kind, Vec<u64>); 15] {
    [
        (Kind::String, vec![n, n]),
        (Kind::BigInt, vec![n, n]),
        (Kind::Ctor, vec![n, h, h]),
        (Kind::Closure, vec![n, h]),
        (Kind::Tuple, vec![h, h]),
        // Its length, height and offset, then the head, tree and tail.
        (Kind::ArrayRoot, vec![n, n, n, h, h, h]),
        (Kind::ArrayLeaf, vec![h, h, h]),
        // Its height, two sizes, then two children.
        (Kind::ArrayBranch, vec![n, n, n, h, h]),
        (Kind::Range, vec![n, n]),
        (Kind::Binary, vec![n, n]),
        // The binary it is a slice of, then where it starts and its length.
        (Kind::BinarySlice, vec![h, n, n]),
        (Kind::Map, vec![n, h]),
        (Kind::MapNode, vec![n, h, h, h]),
        (Kind::MapCollision, vec![n, h, h]),
        (Kind::Handle, vec![n]),
    ]
}

#[test]
fn each_kind_is_every_kind() {
    let kinds = each_kind(0, 0).map(|(kind, _)| Some(kind));
    let all: Vec<Option<Kind>> = (1..16).map(|bits| kind_of(bits << 32)).collect();
    assert_eq!(kinds.as_slice(), all.as_slice());
}

#[test]
fn a_heap_with_nothing_in_it_passes() {
    assert_eq!(audit(&Heap::default(), &[]), Ok(()));
}

/// A heap with every kind of cell in it, long and short, live and free, held
/// by roots and by each other, passes as it is.
#[test]
fn a_heap_whose_counts_are_right_passes() {
    let mut heap = Heap::default();
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
    for (kind, data) in each_kind(7, t.bits()) {
        for w in &data {
            if *w == t.bits() {
                heap.share(text).expect("a live cell");
            }
        }
        roots.push(heap.make(kind, &data).expect("room"));
    }
    assert_eq!(audit(&heap, &roots), Ok(()));
    for root in roots {
        heap.release(root).expect("a live cell");
    }
    assert_eq!(audit(&heap, &[]), Ok(()));
    assert_eq!(heap.live(), 0);
}

/// A constructor's tag, a closure's function, a branch's size table, a
/// slice's offset and the like are plain numbers, so nothing there counts as
/// a holder, even when it has the bits of a cell. Every other word does.
#[test]
fn only_the_words_that_hold_values_are_holders() {
    let mut heap = Heap::default();
    let number = heap.string(b"only a number's bits").expect("room");
    let held = heap.string(b"held").expect("room");
    let (n, h) = (Value::cell(number).bits(), Value::cell(held).bits());
    for (kind, data) in each_kind(n, h) {
        let cell = heap.make(kind, &data).expect("room");
        let holds = data.iter().filter(|w| **w == h).count();
        for _ in 0..holds {
            heap.share(held).expect("a live cell");
        }
        assert_eq!(audit(&heap, &[number, held, cell]), Ok(()), "{kind:?}");
        // Giving the cell up gives up what the audit says it holds.
        heap.release(cell).expect("a live cell");
        assert_eq!(audit(&heap, &[number, held]), Ok(()), "{kind:?}");
    }
}

#[test]
fn a_count_with_the_wrong_number_of_holders_is_found() {
    let mut heap = Heap::default();
    let cell = heap.string(b"held").expect("room");
    let holder = tuple(&mut heap, &[Value::cell(cell)]);
    let wrong = |count, holders| {
        Err(AuditFault::Miscounted {
            cell,
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
    let past = Cell(u32::try_from(heap.arena.len()).expect("small"));
    for to in [gone, Cell(0), past, Cell(u32::MAX), Cell(cell.0 + 1)] {
        let fault = |holder| Err(AuditFault::Dangling { holder, to });
        assert_eq!(audit(&heap, &[cell, to]), fault(None), "{to:?}");
        heap.data_mut(cell)[1] = Value::cell(to).bits();
        assert_eq!(audit(&heap, &[cell]), fault(Some(cell)), "{to:?}");
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
    let bad = |space, to: Cell| Err(AuditFault::BadLink { space, to: to.0 });

    // The last links back to the first, which would never end.
    let (mut heap, cells) = freed();
    heap.arena[cells[0].index() + 1] |= u64::from(cells[2].0);
    assert_eq!(audit(&heap, &[]), bad(3, cells[2]));

    // A list of another size starts at one of them.
    let (mut heap, cells) = freed();
    heap.free.resize(6, 0);
    heap.free[5] = cells[1].0;
    assert_eq!(audit(&heap, &[]), bad(5, cells[1]));

    // A list names a live cell, and an index past the arena.
    let (mut heap, cells) = freed();
    let live = tuple(&mut heap, &[Value::NIL, Value::NIL]);
    assert_eq!(live, cells[2]);
    heap.free[3] = live.0;
    assert_eq!(audit(&heap, &[live]), bad(3, live));
    heap.free[3] = u32::MAX;
    assert_eq!(audit(&heap, &[live]), bad(3, Cell(u32::MAX)));

    // A list is cut short.
    let (mut heap, cells) = freed();
    heap.free[3] = cells[0].0;
    assert_eq!(
        audit(&heap, &[]),
        Err(AuditFault::Unlisted { cell: cells[1] })
    );
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
    // A header no cell has: no kind, or no count.
    for h in [7, 5 << 32] {
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
        heap.arena[b.index() + STUB_WORDS - 1] = number;
        assert_eq!(audit(&heap, &[a, b]), Err(AuditFault::NoBox { cell: b }));
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
    assert_eq!(
        audit(&heap, &[holder]),
        Err(AuditFault::CountedConstant { cell })
    );
    heap.constants[1] = holder.0 + 1;
    let to = Cell(holder.0 + 1);
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
    let fault = Err(AuditFault::NeverCounted { cell });
    assert_eq!(audit(&heap, &[cell, constant]), fault);
    assert_eq!(audit(&heap, &[]), fault, "held by nothing at all");
    // A constant given a second cell leaves the first one behind.
    let second = heap.string(b"a constant").expect("room");
    heap.recount(cell, 1);
    heap.keep_constant(scarlet_ir::core_ir::ConstId(0), second)
        .expect("room");
    assert_eq!(
        audit(&heap, &[cell]),
        Err(AuditFault::NeverCounted { cell: constant })
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
    let cell = Cell(12);
    for (fault, text) in [
        (
            AuditFault::NoCell { at: 12 },
            "the heap has no whole cell at word 12",
        ),
        (
            AuditFault::Dangling {
                holder: Some(Cell(3)),
                to: cell,
            },
            "cell 3 holds cell 12, which is not a live cell",
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
    let mut heap = Heap::default();
    let mut held: Vec<Cell> = Vec::new();
    for step in 0..1500 {
        match below(8) {
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
    assert!(heap.made() > 700, "{}", heap.made());
    for cell in held {
        heap.release(cell).expect("a live cell");
    }
    assert_eq!(audit(&heap, &[]), Ok(()));
    assert_eq!((heap.live(), heap.boxed), (0, 0));
}
