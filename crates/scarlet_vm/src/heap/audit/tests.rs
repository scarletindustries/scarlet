use scarlet_ir::TypeId;
use scarlet_ir::core_ir::{FuncIdx, VariantRef};

use super::*;
use crate::heap::{Class, Ctor, Edit, Kind, Opened, STUB_WORDS};

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
        (Kind::Range, vec![n.wrapping_sub(1), n]),
        (Kind::Binary, vec![n, n]),
        (Kind::Handle, vec![n]),
        // The binary it is a slice of, then where it starts and its length.
        (Kind::BinarySlice, vec![h, n, n]),
        (Kind::Tuple, vec![h, h]),
        (Kind::Closure, vec![Value::func(FuncIdx(1)).bits(), h]),
        // Its front, head, tree and tail, then how many each has and where the leaves are.
        (Kind::ArrayRoot, vec![h, h, h, h, n, n, n]),
        (Kind::ArrayBranch, vec![h; 32]),
        // Its slots, then that all of them are its elements'.
        (Kind::ArrayFlat, vec![h, h, h, h, 4]),
        (Kind::Map, vec![n, h]),
        (Kind::MapNode, vec![n, h, h, h]),
        (Kind::MapCollision, vec![n, h, h]),
    ]
}

/// The classes below the first constructor's are these kinds.
#[test]
fn each_kind_is_every_kind() {
    let kinds = each_kind(0, 0).map(|(kind, _)| Some(kind));
    let all: Vec<Option<Kind>> = (1..15).map(|c| Class(c).kind()).collect();
    assert_eq!(kinds.as_slice(), all.as_slice());
    assert_eq!(Class(0).kind(), None);
    for c in [15, 16, 17, 255, 256, u16::MAX] {
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

/// A tree of a front, four in its head and as many in its tail, with a leaf between them. Each
/// element is `h`.
fn a_tree(heap: &mut Heap, h: u64) -> Vec<u64> {
    let head = flat_of(heap, &[h; 4], 4);
    let leaf = flat_of(heap, &[h; 32], 32);
    let tail = flat_of(heap, &[h; 4], 4);
    vec![h, head, leaf, tail, lens(0xFF, 4, 4, 0), 32, 0]
}

/// What a root says of where it starts, how many are in its head and its tail, and how tall its
/// tree is.
fn lens(start: u64, head: u64, tail: u64, shift: u64) -> u64 {
    start | head << 8 | tail << 16 | shift << 24
}

/// A cell of each kind, a long one of each that can be, and a constructor's
/// narrow and wide, with every word that holds a value holding `held`, whose
/// count goes up with each, but for an array's root, which holds arrays that do.
fn one_of_each(heap: &mut Heap, ctors: [Ctor; 2], held: Cell, n: u64) -> Vec<Cell> {
    let h = Value::cell(held).bits();
    let mut cells = Vec::new();
    for (kind, data) in each_kind(n, h).into_iter().chain(each_long_kind(n, h)) {
        let shares = match kind {
            Kind::ArrayRoot => 41,
            _ => data.iter().filter(|w| **w == h).count(),
        };
        for _ in 0..shares {
            heap.share(held).expect("a live cell");
        }
        let data = match kind {
            Kind::ArrayRoot => a_tree(heap, h),
            _ => data,
        };
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
    for root in roots {
        heap.release(root).expect("a live cell");
    }
    assert_eq!(audit(&heap, &[]), Ok(()));
    assert_eq!(heap.live(), 0);
}

/// A root's lengths, a slice's offset and the like are plain numbers, so
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
    let cells = one_of_each(&mut heap, ctors, held, 1);
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
        // A class no cell has, a freed cell's and that of a constructor the program does
        // not have, and a short array's on a cell that is none.
        for wrong in [0, 11, 17, u64::from(u16::MAX)] {
            if wrong == right >> 48 {
                continue;
            }
            let h = right & !(0xFFFF << 48) | wrong << 48;
            assert_eq!(found(h), misshapen, "{right:x}");
        }
        // A constructor's class on a cell with another number of words.
        for wrong in [15, 16] {
            let h = right & !(0xFFFF << 48) | wrong << 48;
            let fields = if wrong == 15 { 2 } else { 300 };
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

/// The array with no elements is no cell, so neither an array's cell nor a node of one
/// has none.
#[test]
fn an_array_with_no_elements_in_a_cell_is_found() {
    let (nil, h) = (Value::NIL.bits(), Value::bool(true).bits());
    let (low, high) = (i64::MIN.cast_unsigned(), i64::MAX.cast_unsigned());
    let mut one = [nil; 32];
    one[31] = h;
    let cases: [(Kind, &[u64], bool); 8] = [
        (Kind::ArrayRoot, &[nil, nil, nil, nil, 0, 0, 0], true),
        (Kind::ArrayBranch, &one, false),
        (Kind::ArrayBranch, &[nil; 32], true),
        (Kind::Range, &[3, 4], false),
        (Kind::Range, &[low, high], false),
        (Kind::Range, &[3, 3], true),
        (Kind::Range, &[4, 3], true),
        (Kind::Range, &[high, low], true),
    ];
    for (kind, words, empty) in cases {
        let mut heap = Heap::default();
        let cell = heap.make(kind, words).expect("room");
        let fault = AuditFault::EmptyArray { cell: cell.index() };
        let want = if empty { Err(fault) } else { Ok(()) };
        assert_eq!(audit(&heap, &[cell]), want, "{kind:?} {words:?}");
    }
}

/// A short array's cell is 4, 8, 16 or 32 slots and a window that is no longer than they
/// are, has an element, and has nothing in a slot that is none of its elements', whether
/// that is a cell or not and wherever the run of them starts.
#[test]
fn a_short_array_laid_out_wrong_is_found() {
    let (nil, one) = (Value::NIL.bits(), Value::bool(true).bits());
    for slots in [4usize, 8, 16, 32] {
        for start in [0u64, 1, 3, 31, u64::from(u32::MAX), 1 << 31] {
            for len in 0..=slots + 1 {
                let mut words = vec![nil; slots];
                for i in 0..len.min(slots) {
                    words[(start as usize + i) % slots] = one;
                }
                words.push(start << 32 | len as u64);
                let mut heap = Heap::default();
                let cell = heap.make(Kind::ArrayFlat, &words).expect("room");
                let at = cell.index();
                let want = match len {
                    0 => Err(AuditFault::EmptyArray { cell: at }),
                    _ if len > slots => Err(AuditFault::Misshapen { cell: at }),
                    _ => Ok(()),
                };
                assert_eq!(audit(&heap, &[cell]), want, "{slots} {start} {len}");
                for past in len..slots {
                    if len == 0 {
                        break;
                    }
                    let slot = (start as usize + past) % slots;
                    let held = heap.string(b"held").expect("room");
                    for stale in [one, Value::cell(held).bits()] {
                        heap.data_mut(cell)[slot] = stale;
                        let fault = AuditFault::StaleSlot { cell: at };
                        let found = audit(&heap, &[cell, held]);
                        assert_eq!(found, Err(fault), "{slots} {start} {len} {slot}");
                        assert!(fault.to_string().contains("not one of its elements"));
                    }
                    heap.data_mut(cell)[slot] = nil;
                    heap.release(held).expect("live");
                }
            }
        }
    }
    for slots in [0, 1, 2, 3, 5, 7, 12, 31, 33, 64, 253, 300] {
        let mut words = vec![one; slots];
        words.push(slots as u64);
        let mut heap = Heap::default();
        let cell = heap.make(Kind::ArrayFlat, &words).expect("room");
        let fault = AuditFault::Misshapen { cell: cell.index() };
        assert_eq!(audit(&heap, &[cell]), Err(fault), "{slots}");
    }
}

/// A short array's cell is a leaf of a tree only when it is full, whichever slot its elements
/// start in, and nothing else is one.
#[test]
fn a_leaf_that_is_not_full_is_found() {
    let nil = Value::NIL.bits();
    let one = Value::int(1).expect("small").bits();
    for slots in [4u64, 8, 16, 32] {
        for len in [1, slots - 1, slots] {
            for start in [0, 1, slots - 1, slots] {
                for (shift, tuple) in [(0, false), (5, false), (5, true)] {
                    let mut words = vec![nil; slots as usize];
                    for i in 0..len {
                        words[((start + i) % slots) as usize] = one;
                    }
                    words.push(start << 32 | len);
                    let mut heap = Heap::default();
                    let full = flat_of(&mut heap, &[one; 32], 32);
                    let leaf = match tuple {
                        true => heap.make(Kind::Tuple, &words).expect("room"),
                        false => heap.make(Kind::ArrayFlat, &words).expect("room"),
                    };
                    let held = Value::cell(leaf).bits();
                    let tail = flat_of(&mut heap, &[one, nil, nil, nil], 1);
                    let mut branch = [nil; 32];
                    (branch[0], branch[1]) = (full, held);
                    let tree = match shift {
                        0 => {
                            heap.release(Value::from_bits(full).as_cell().expect("a cell"))
                                .expect("live");
                            held
                        }
                        _ => {
                            let branch = heap.make(Kind::ArrayBranch, &branch).expect("room");
                            Value::cell(branch).bits()
                        }
                    };
                    let in_tree = if shift == 0 { 32 } else { 64 };
                    let root = [nil, nil, tree, tail, lens(0, 0, 1, shift), in_tree, 0];
                    let root = heap.make(Kind::ArrayRoot, &root).expect("room");
                    let fault = AuditFault::PartLeaf { root: root.index() };
                    let want = match len == 32 && !tuple {
                        true => Ok(()),
                        false => Err(fault),
                    };
                    let what = format!("{shift} {tuple} {slots} {start} {len}");
                    assert_eq!(audit(&heap, &[root]), want, "{what}");
                    assert!(fault.to_string().contains("no array of 32"));
                }
            }
        }
    }
}

/// A short array's cell with these slots, of which as many as `len` are elements.
fn flat_of(heap: &mut Heap, slots: &[u64], len: u64) -> u64 {
    let words: Vec<u64> = slots.iter().copied().chain([len]).collect();
    Value::cell(heap.make(Kind::ArrayFlat, &words).expect("room")).bits()
}

/// A root that says something else of what is under it is found, and so is one that a
/// short array would do for.
#[test]
fn a_root_that_does_not_say_what_is_under_it_is_found() {
    let nil = Value::NIL.bits();
    let one = Value::int(1).expect("small").bits();
    // What is said of a tree of 4 in the head, two leaves, and 2 in the tail: where it starts,
    // how many are in its head, its tail and its leaves, where those are, and its height. And
    // what is in front.
    let cases = [
        ([0u64, 4, 2, 64, 0, 5], nil, true),
        ([0, 4, 2, 64, 0, 5], one, false),
        ([0xFF, 4, 2, 64, 0, 5], one, true),
        ([1, 4, 2, 64, 0, 5], nil, true),
        ([1, 4, 2, 64, 0, 5], one, false),
        ([3, 4, 2, 64, 0, 5], nil, true),
        ([4, 4, 2, 64, 0, 5], nil, false),
        ([5, 4, 2, 64, 0, 5], nil, false),
        ([0x80, 4, 2, 64, 0, 5], nil, false),
        ([0xFE, 4, 2, 64, 0, 5], one, false),
        ([0, 3, 2, 64, 0, 5], nil, false),
        ([0, 5, 2, 64, 0, 5], nil, false),
        ([0, 0, 2, 64, 0, 5], nil, false),
        ([0, 4, 1, 64, 0, 5], nil, false),
        ([0, 4, 3, 64, 0, 5], nil, false),
        ([0, 4, 0, 64, 0, 5], nil, false),
        ([0, 4, 2, 32, 0, 5], nil, false),
        ([0, 4, 2, 96, 0, 5], nil, false),
        ([0, 4, 2, 0, 0, 0], nil, false),
        ([0, 4, 2, 64, 32, 5], nil, false),
        ([0, 4, 2, 64, 1, 5], nil, false),
        ([0, 4, 2, 64, 1024, 5], nil, false),
        ([0, 4, 2, 64, 0, 0], nil, false),
        ([0, 4, 2, 64, 0, 10], nil, false),
        ([0, 4, 2, 1024, 0, 5], nil, false),
        ([0, 4, 2, 64, 0, 5 + (1 << 35)], nil, false),
    ];
    for (says, front, right) in cases {
        let mut heap = Heap::default();
        let head = flat_of(&mut heap, &[one; 4], 4);
        let tail = flat_of(&mut heap, &[one, one, nil, nil], 2);
        let mut branch = [nil; 32];
        for leaf in &mut branch[..2] {
            *leaf = flat_of(&mut heap, &[one; 32], 32);
        }
        let tree = Value::cell(heap.make(Kind::ArrayBranch, &branch).expect("room")).bits();
        let [start, in_head, in_tail, in_tree, first, shift] = says;
        let lens = lens(start, in_head, in_tail, shift);
        let root = [front, head, tree, tail, lens, in_tree, first];
        let root = heap.make(Kind::ArrayRoot, &root).expect("room");
        let fault = AuditFault::Misshapen { cell: root.index() };
        let want = if right { Ok(()) } else { Err(fault) };
        assert_eq!(audit(&heap, &[root]), want, "{says:?} {front}");
    }
    // One that has lost some of its head, which hold something or do not.
    for (at, start, plain_passes, deep_passes) in [
        (0, 0, true, true),
        (0, 0xFF, true, true),
        (0, 1, true, false),
        (1, 1, true, true),
        (1, 2, true, false),
        (3, 3, true, true),
    ] {
        for (tuples, right) in [(0, plain_passes), (1, plain_passes), (2, deep_passes)] {
            let mut heap = Heap::default();
            let mut head = [one; 4];
            let mut cell = Value::cell(heap.string(b"a cell").expect("room"));
            for _ in 0..tuples {
                cell = Value::cell(heap.tuple(&[cell]).expect("room"));
            }
            head[at] = cell.bits();
            let head = flat_of(&mut heap, &head, 4);
            let tail = flat_of(&mut heap, &[one; 4], 4);
            let front = if start == 0xFF { one } else { nil };
            let root = [front, head, nil, tail, lens(start, 4, 4, 0), 0, 0];
            let root = heap.make(Kind::ArrayRoot, &root).expect("room");
            let fault = AuditFault::Misshapen { cell: root.index() };
            let want = if right { Ok(()) } else { Err(fault) };
            assert_eq!(audit(&heap, &[root]), want, "{at} {start} {tuples}");
        }
    }
    // A front that there is no head for, and a place for a head, a tail or a tree that there is
    // none of, with something in it.
    for (words, start, right) in [
        ([nil; 3], 0, true),
        ([nil; 3], 0xFF, false),
        ([nil; 3], 1, false),
        ([one, nil, nil], 0, false),
        ([nil, one, nil], 0, false),
        ([nil, nil, one], 0, false),
    ] {
        let mut heap = Heap::default();
        let leaf = flat_of(&mut heap, &[one; 32], 32);
        let [head, tail, in_branch] = words;
        let mut branch = [nil; 32];
        branch[0] = leaf;
        branch[1] = flat_of(&mut heap, &[one; 32], 32);
        branch[7] = in_branch;
        let tree = Value::cell(heap.make(Kind::ArrayBranch, &branch).expect("room")).bits();
        let root = [nil, head, tree, tail, lens(start, 0, 0, 5), 64, 0];
        let root = heap.make(Kind::ArrayRoot, &root).expect("room");
        let fault = AuditFault::Misshapen { cell: root.index() };
        let want = if right { Ok(()) } else { Err(fault) };
        assert_eq!(audit(&heap, &[root]), want, "{words:?} {start}");
    }
    for (tree, right) in [(nil, true), (one, false)] {
        let mut heap = Heap::default();
        let [head, tail] = [0; 2].map(|_| flat_of(&mut heap, &[one; 4], 4));
        let root = [nil, head, tree, tail, lens(0, 4, 4, 0), 0, 0];
        let root = heap.make(Kind::ArrayRoot, &root).expect("room");
        let fault = AuditFault::Misshapen { cell: root.index() };
        let want = if right { Ok(()) } else { Err(fault) };
        assert_eq!(audit(&heap, &[root]), want, "{tree}");
    }
    // A tree that is a level taller than it has to be.
    for leaves in [1usize, 2, 32, 33] {
        let mut heap = Heap::default();
        let (mut top, mut left) = ([nil; 32], leaves);
        for place in &mut top {
            let mut branch = [nil; 32];
            for leaf in branch.iter_mut().take(left) {
                *leaf = flat_of(&mut heap, &[one; 32], 32);
            }
            if left > 0 {
                let branch = heap.make(Kind::ArrayBranch, &branch).expect("room");
                *place = Value::cell(branch).bits();
            }
            left = left.saturating_sub(32);
        }
        let tree = Value::cell(heap.make(Kind::ArrayBranch, &top).expect("room")).bits();
        let root = [
            nil,
            nil,
            tree,
            nil,
            lens(0, 0, 0, 10),
            32 * leaves as u64,
            0,
        ];
        let root = heap.make(Kind::ArrayRoot, &root).expect("room");
        let fault = AuditFault::Misshapen { cell: root.index() };
        let want = if leaves > 32 { Ok(()) } else { Err(fault) };
        assert_eq!(audit(&heap, &[root]), want, "{leaves}");
    }
    // A tree of a head alone, of a tail alone, and of both.
    for (head, tail) in [(4, 0), (0, 4), (4, 4)] {
        let mut heap = Heap::default();
        let mut edge = |n| match n {
            0 => nil,
            _ => flat_of(&mut heap, &[one; 4], 4),
        };
        let root = [
            nil,
            edge(head),
            nil,
            edge(tail),
            lens(0, head, tail, 0),
            0,
            0,
        ];
        let root = heap.make(Kind::ArrayRoot, &root).expect("room");
        let fault = AuditFault::Misshapen { cell: root.index() };
        let want = if head == tail { Ok(()) } else { Err(fault) };
        assert_eq!(audit(&heap, &[root]), want, "{head} {tail}");
    }
}

#[test]
fn an_array_cell_with_the_wrong_number_of_words_is_found() {
    let nil = Value::NIL.bits();
    let cases: [(Kind, &[u64]); 8] = [
        (Kind::ArrayRoot, &[nil]),
        (Kind::ArrayRoot, &[nil, nil, nil, nil, 1, 0]),
        (Kind::ArrayRoot, &[nil, nil, nil, nil, 1, 0, 0, 0]),
        (Kind::ArrayBranch, &[nil]),
        (Kind::ArrayBranch, &[nil; 31]),
        (Kind::ArrayBranch, &[nil; 33]),
        (Kind::Range, &[3]),
        (Kind::Range, &[3, 4, 5]),
    ];
    for (kind, words) in cases {
        let mut heap = Heap::default();
        let cell = heap.make(kind, words).expect("room");
        let fault = AuditFault::Misshapen { cell: cell.index() };
        assert_eq!(audit(&heap, &[cell]), Err(fault), "{kind:?} {words:?}");
    }
    // A root that has all it needs and a word more.
    for more in [false, true] {
        let mut heap = Heap::default();
        let mut words = a_tree(&mut heap, nil);
        words.extend(more.then_some(0));
        let root = heap.make(Kind::ArrayRoot, &words).expect("room");
        let fault = AuditFault::Misshapen { cell: root.index() };
        let want = if more { Err(fault) } else { Ok(()) };
        assert_eq!(audit(&heap, &[root]), want, "{more}");
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
            AuditFault::EmptyArray { cell },
            "the array's cell 12 has no elements",
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
    let mut opened = 0;
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
                let values: Vec<Value> = heap.values(cell).collect();
                for v in &values {
                    heap.own(*v).expect("a live cell");
                }
                held.push(tuple(&mut heap, &values));
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
            // One taken apart, when it is the narrow one. It stays when
            // something else holds it.
            9 if !held.is_empty() => {
                let cell = held.swap_remove(below(held.len()));
                match heap.open::<2>(cell, ctors[0]) {
                    Some(Opened::Alone(fields)) => {
                        opened += 1;
                        held.extend(fields.iter().filter_map(|v| v.as_cell()));
                    }
                    // It has lost this holder, who now has none.
                    Some(Opened::Held(..)) => {}
                    None => held.push(cell),
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
    assert!(opened > 10, "{opened}");
    for cell in held {
        heap.release(cell).expect("a live cell");
    }
    assert_eq!(audit(&heap, &[]), Ok(()));
    assert_eq!((heap.live(), heap.boxed), (0, 0));
}
