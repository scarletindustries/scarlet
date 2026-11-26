use super::*;

fn root(heap: &Heap, array: Cell) -> Root {
    super::root(heap, array).expect("a live array")
}

fn ints(heap: &Heap, array: Cell) -> Vec<i64> {
    elements(heap, array)
        .into_iter()
        .map(|v| match v.view() {
            crate::value::View::Int(n) => n,
            other => panic!("not an Int: {other:?}"),
        })
        .collect()
}

fn int(v: Value) -> Option<i64> {
    match v.view() {
        crate::value::View::Int(n) => Some(n),
        _ => None,
    }
}

fn array_of(heap: &mut Heap, range: std::ops::Range<i64>) -> Cell {
    let items: Vec<Value> = range.map(|n| Value::int(n).expect("small")).collect();
    from_values(heap, &items).expect("room")
}

/// Check every invariant of the tree. That is each node's height, each
/// leaf's and branch's slot count, each running total, and the root's
/// length.
fn check(heap: &Heap, array: Cell) {
    let r = root(heap, array);
    for buf in [r.head, r.tail] {
        let n = slots(heap, buf).len();
        assert!(
            buf.as_cell().is_none() || (1..=B).contains(&n),
            "a buffer of {n}"
        );
    }
    assert!(
        r.offset < node_len(heap, r.head).max(1),
        "an offset past its leaf"
    );
    let tree_len = if r.tree.as_cell().is_some() {
        check_node(heap, r.tree, r.shift)
    } else {
        0
    };
    assert_eq!(
        r.len,
        node_len(heap, r.head) - r.offset + tree_len + node_len(heap, r.tail)
    );
    assert_eq!(elements(heap, array).len(), r.len);
}

fn check_node(heap: &Heap, n: Value, shift: usize) -> usize {
    match node(heap, n).expect("a node") {
        Node::Leaf(items) => {
            assert_eq!(shift, 0, "a leaf above the bottom");
            assert!((1..=B).contains(&items.len()));
            items.len()
        }
        Node::Branch {
            shift: s,
            sizes,
            children,
        } => {
            assert_eq!(s, shift, "a branch at the wrong height");
            assert!((1..=B).contains(&children.len()));
            let mut total = 0;
            for (i, c) in children.iter().enumerate() {
                total += check_node(heap, *c, shift - BITS);
                assert_eq!(sizes[i] as usize, total);
            }
            total
        }
    }
}

#[test]
fn an_array_reads_back_at_every_size() {
    let mut heap = Heap::default();
    for n in [0, 1, 31, 32, 33, 64, 65, 1024, 1025, 40_000] {
        let a = array_of(&mut heap, 0..n);
        check(&heap, a);
        assert_eq!(len(&heap, a), n as usize);
        assert_eq!(ints(&heap, a), (0..n).collect::<Vec<_>>());
        for i in [0, n / 2, n - 1] {
            if n > 0 {
                assert_eq!(
                    get(&heap, a, i as usize).map(Value::view),
                    Value::int(i).map(Value::view)
                );
            }
        }
        assert!(get(&heap, a, n as usize).is_none());
        heap.release(a).expect("a live cell");
        assert_eq!(heap.live(), 0, "size {n}");
    }
}

/// Pushes at both ends, 3,000 of each, keep every invariant and leave the
/// array they started from as it was.
#[test]
fn pushing_at_either_end_leaves_the_old_array_alone() {
    let mut heap = Heap::default();
    let first = array_of(&mut heap, 0..10);
    let mut a = first;
    heap.share(a).expect("a live cell");
    let mut want: std::collections::VecDeque<i64> = (0..10).collect();
    for i in 0..3000 {
        let (end, x) = if i % 3 == 0 {
            (End::Front, -i)
        } else {
            (End::Back, 100 + i)
        };
        let next = push(&mut heap, a, Value::int(x).expect("small"), end).expect("room");
        heap.release(a).expect("a live cell");
        a = next;
        match end {
            End::Front => want.push_front(x),
            End::Back => want.push_back(x),
        }
    }
    check(&heap, a);
    assert_eq!(ints(&heap, a), want.into_iter().collect::<Vec<_>>());
    assert_eq!(ints(&heap, first), (0..10).collect::<Vec<_>>());
    heap.release(a).expect("a live cell");
    heap.release(first).expect("a live cell");
    assert_eq!(heap.live(), 0);
}

/// Pushes that give their array up change it in place when nothing else
/// holds it, so they must leave alone every version something else does
/// hold, and keep every reference counted. The items are strings, which
/// are cells, so a lost or extra reference shows in the live count.
#[test]
fn an_owned_push_leaves_every_kept_version_alone() {
    let mut heap = Heap::default();
    let text = |heap: &Heap, v: Value| {
        let mut out = Vec::new();
        if let Some(c) = v.as_cell() {
            heap.read_string(c, &mut out);
        }
        String::from_utf8(out).expect("UTF-8")
    };
    let texts = |heap: &Heap, a: Cell| -> Vec<String> {
        elements(heap, a)
            .into_iter()
            .map(|v| text(heap, v))
            .collect()
    };
    let mut a = from_values(&mut heap, &[]).expect("room");
    let mut want: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    let mut kept: Vec<(Cell, Vec<String>)> = Vec::new();
    for i in 0..2500 {
        if i % 41 == 0 {
            heap.share(a).expect("a live cell");
            kept.push((a, want.iter().cloned().collect()));
        }
        if i % 17 == 0 && !want.is_empty() {
            a = skip_owned(&mut heap, a, 1).expect("room");
            want.pop_front();
            continue;
        }
        let end = if i % 3 == 0 { End::Front } else { End::Back };
        let name = format!("s{i}");
        let x = Value::cell(heap.string(name.as_bytes()).expect("room"));
        a = push_owned(&mut heap, a, x, end).expect("room");
        match end {
            End::Front => want.push_front(name),
            End::Back => want.push_back(name),
        }
    }
    check(&heap, a);
    assert_eq!(texts(&heap, a), want.iter().cloned().collect::<Vec<_>>());
    for (k, items) in &kept {
        check(&heap, *k);
        assert_eq!(&texts(&heap, *k), items);
    }
    heap.release(a).expect("a live cell");
    for (k, _) in kept {
        heap.release(k).expect("a live cell");
    }
    assert_eq!(heap.live(), 0, "an owned push lost track of a reference");
}

#[test]
fn take_and_skip_cut_anywhere() {
    let mut heap = Heap::default();
    let mut a = array_of(&mut heap, 0..10);
    for i in 0..40 {
        let next =
            push(&mut heap, a, Value::int(-1 - i).expect("small"), End::Front).expect("room");
        heap.release(a).expect("a live cell");
        a = next;
    }
    let tail = array_of(&mut heap, 10..1500);
    let joined = concat(&mut heap, a, tail).expect("room");
    let all = ints(&heap, joined);
    for n in [0, 1, 5, 39, 40, 41, 50, 51, 700, 1539, 1540, 1541, 5000] {
        let t = take(&mut heap, joined, n).expect("room");
        check(&heap, t);
        assert_eq!(ints(&heap, t), all[..n.min(all.len())], "take {n}");
        let s = skip(&mut heap, joined, n).expect("room");
        check(&heap, s);
        assert_eq!(ints(&heap, s), all[n.min(all.len())..], "skip {n}");
        heap.release(t).expect("a live cell");
        heap.release(s).expect("a live cell");
    }
    for c in [a, tail, joined] {
        heap.release(c).expect("a live cell");
    }
    assert_eq!(heap.live(), 0);
}

/// Walking an array one element at a time, the way `[h, ..t]` does. Each
/// step keeps the same leaf and moves the offset along, and the tree is
/// cut only when a leaf runs out, so the cells this makes are a small
/// multiple of the number of leaves rather than of the elements. Every
/// step still reads back as the plain list does, from either end and
/// through `get`.
#[test]
fn walking_an_array_from_the_front_copies_no_leaf() {
    let mut heap = Heap::default();
    let n = 1000;
    let a = array_of(&mut heap, 0..n);
    let mut cur = a;
    heap.share(cur).expect("a live cell");
    // Every leaf the walk reads from. A leaf it copied would be a cell of
    // its own, so counting them is counting the copies.
    let mut heads: Vec<Option<Cell>> = Vec::new();
    for i in 0..n {
        assert_eq!(get(&heap, cur, 0).and_then(int), Some(i), "head at {i}");
        assert_eq!(len(&heap, cur), (n - i) as usize);
        let head = root(&heap, cur).head.as_cell();
        if heads.last() != Some(&head) {
            heads.push(head);
        }
        let next = skip(&mut heap, cur, 1).expect("room");
        check(&heap, next);
        heap.release(cur).expect("a live cell");
        cur = next;
    }
    assert_eq!(len(&heap, cur), 0);
    heap.release(cur).expect("a live cell");
    let leaves = (n as usize).div_ceil(B);
    assert!(
        heads.len() <= leaves + 1,
        "{} leaves read for {leaves} in the array",
        heads.len()
    );
    heap.release(a).expect("a live cell");
    assert_eq!(heap.live(), 0);
}

/// An array partly walked is an ordinary array. What it left behind is
/// invisible to reading it, cutting it, joining it and pushing onto it,
/// and none of those reach past its own front.
#[test]
fn an_array_walked_part_way_behaves_like_the_rest_of_it() {
    let mut heap = Heap::default();
    let whole = array_of(&mut heap, 0..200);
    let other = array_of(&mut heap, 200..260);
    for k in [1, 5, 31, 32, 33, 40, 64, 100] {
        let s = skip(&mut heap, whole, k).expect("room");
        check(&heap, s);
        let rest: Vec<i64> = (k as i64..200).collect();
        assert_eq!(ints(&heap, s), rest, "skip {k}");
        assert_eq!(get(&heap, s, 0).and_then(int), Some(k as i64));
        assert_eq!(get(&heap, s, 3).and_then(int), Some(k as i64 + 3));

        let t = take(&mut heap, s, 7).expect("room");
        check(&heap, t);
        assert_eq!(ints(&heap, t), rest[..7], "take after skip {k}");

        let again = skip(&mut heap, s, 9).expect("room");
        check(&heap, again);
        assert_eq!(ints(&heap, again), rest[9..], "skip twice {k}");

        let joined = concat(&mut heap, s, other).expect("room");
        check(&heap, joined);
        let want: Vec<i64> = rest.iter().copied().chain(200..260).collect();
        assert_eq!(ints(&heap, joined), want, "concat after skip {k}");

        let front = push(&mut heap, s, Value::int(-1).expect("small"), End::Front).expect("room");
        check(&heap, front);
        let want: Vec<i64> = std::iter::once(-1).chain(rest.iter().copied()).collect();
        assert_eq!(ints(&heap, front), want, "push front after skip {k}");

        let back = push(&mut heap, s, Value::int(-1).expect("small"), End::Back).expect("room");
        check(&heap, back);
        let want: Vec<i64> = rest.iter().copied().chain(std::iter::once(-1)).collect();
        assert_eq!(ints(&heap, back), want, "push back after skip {k}");

        for c in [s, t, again, joined, front, back] {
            heap.release(c).expect("a live cell");
        }
    }
    heap.release(whole).expect("a live cell");
    heap.release(other).expect("a live cell");
    assert_eq!(heap.live(), 0);
}

/// Joins of every size against every size, and a long run of joins,
/// keep every invariant, and the tree stays shallow.
#[test]
fn concat_keeps_the_tree_balanced() {
    let mut heap = Heap::default();
    let sizes = [0, 1, 17, 32, 33, 100, 1024, 1057, 5000];
    for &x in &sizes {
        for &y in &sizes {
            let l = array_of(&mut heap, 0..x);
            let r = array_of(&mut heap, x..x + y);
            let j = concat(&mut heap, l, r).expect("room");
            check(&heap, j);
            assert_eq!(ints(&heap, j), (0..x + y).collect::<Vec<_>>(), "{x} ++ {y}");
            for c in [l, r, j] {
                heap.release(c).expect("a live cell");
            }
        }
    }
    let mut acc = empty(&mut heap).expect("room");
    let mut want = Vec::new();
    for i in 0..300 {
        let piece = array_of(&mut heap, i * 7..i * 7 + (i % 40));
        want.extend(i * 7..i * 7 + (i % 40));
        let next = concat(&mut heap, acc, piece).expect("room");
        heap.release(acc).expect("a live cell");
        heap.release(piece).expect("a live cell");
        acc = next;
    }
    check(&heap, acc);
    assert_eq!(ints(&heap, acc), want);
    assert!(
        root(&heap, acc).shift <= 3 * BITS,
        "height {}",
        root(&heap, acc).shift
    );
    heap.release(acc).expect("a live cell");
    assert_eq!(heap.live(), 0);
}

/// Where everything in `array` sits: its root's numbers, and each node's own
/// slots and totals.
fn shape(heap: &Heap, array: Cell) -> String {
    fn of(heap: &Heap, v: Value) -> String {
        match node(heap, v) {
            None => "-".to_string(),
            Some(Node::Leaf(items)) => format!("{}", items.len()),
            Some(Node::Branch {
                shift,
                sizes,
                children,
            }) => {
                let each: Vec<String> = children.iter().map(|c| of(heap, *c)).collect();
                format!("{shift}{sizes:?}({})", each.join(" "))
            }
        }
    }
    let r = root(heap, array);
    let [head, tree, tail] = [r.head, r.tree, r.tail].map(|v| of(heap, v));
    format!("{} {} {}: {head} {tree} {tail}", r.len, r.shift, r.offset)
}

/// Adding a run of items at either end makes the array that pushing them one
/// at a time does, with the same nodes, whatever the array was, how many holders
/// it has and how many items there are. The array something else holds is left
/// as it was.
#[test]
fn a_run_of_items_is_added_as_they_are_one_at_a_time() {
    for end in [End::Front, End::Back] {
        for n in [
            0, 1, 20, 31, 32, 33, 64, 65, 300, 1023, 1024, 1025, 1056, 1057,
        ] {
            for more in [0, 1, 2, 11, 12, 13, 31, 32, 33, 63, 64, 65, 200, 1100] {
                for shared in [false, true] {
                    let mut heap = Heap::default();
                    // Everything the array is of is a cell, so that a
                    // reference lost or kept too long shows.
                    let strings = |heap: &mut Heap, from: &str, n: usize| -> Vec<Value> {
                        (0..n)
                            .map(|i| string(heap, &format!("{from}{i}")))
                            .collect()
                    };
                    let items = strings(&mut heap, "a", n);
                    let a = from_values(&mut heap, &items).expect("room");
                    let items = strings(&mut heap, "b", n);
                    let b = from_values(&mut heap, &items).expect("room");
                    let before = (shape(&heap, a), texts(&heap, a));
                    if shared {
                        heap.share(a).expect("a live cell");
                        heap.share(b).expect("a live cell");
                    }
                    let items = strings(&mut heap, "x", more);
                    let run = extend_owned(&mut heap, a, &items, end).expect("room");
                    let mut each = b;
                    let items = strings(&mut heap, "x", more);
                    let in_turn: Vec<Value> = match end {
                        End::Front => items.into_iter().rev().collect(),
                        End::Back => items,
                    };
                    for x in in_turn {
                        each = push_owned(&mut heap, each, x, end).expect("room");
                    }
                    let what = format!("{n} and {more}");
                    check(&heap, run);
                    assert_eq!(shape(&heap, run), shape(&heap, each), "{what}");
                    let named = |v: Vec<String>| -> Vec<String> {
                        v.into_iter().map(|s| s.replace('b', "a")).collect()
                    };
                    assert_eq!(texts(&heap, run), named(texts(&heap, each)), "{what}");
                    if shared {
                        assert_eq!((shape(&heap, a), texts(&heap, a)), before, "{what}");
                        heap.release(a).expect("a live cell");
                        heap.release(b).expect("a live cell");
                    } else if more > 0 {
                        assert_eq!(run, a, "{what}: the root changes in place");
                    }
                    heap.release(run).expect("a live cell");
                    heap.release(each).expect("a live cell");
                    assert_eq!(heap.live(), 0, "{what}, shared {shared}");
                }
            }
        }
    }
}

/// A literal made in pieces adds 32 items at a time to an array that is full
/// to its last leaf. Each run makes one leaf and at most one branch a level,
/// where a push of each made a leaf for each.
#[test]
fn a_run_of_32_items_makes_one_leaf() {
    let mut heap = Heap::default();
    let items: Vec<Value> = (0..32).map(small).collect();
    let mut a = from_values(&mut heap, &items).expect("room");
    let made = heap.made();
    for _ in 1..40 {
        a = extend_owned(&mut heap, a, &items, End::Back).expect("room");
    }
    check(&heap, a);
    assert_eq!(len(&heap, a), 1280);
    let made = heap.made() - made;
    assert!(made <= 39 * 2 + 1, "{made} cells for 39 runs");
    // The one leaf that a run adds to is in a branch of its own by then, and
    // nothing above it is copied.
    let before = heap.made();
    a = extend_owned(&mut heap, a, &items, End::Front).expect("room");
    assert_eq!(heap.made() - before, 1);
    heap.release(a).expect("a live cell");
    assert_eq!(heap.live(), 0);
}

/// A run added in front of items left behind is added in front of the live
/// ones only, and one added at the back leaves them where they are.
#[test]
fn a_run_is_added_past_the_items_left_behind() {
    for end in [End::Front, End::Back] {
        for dropped in [1, 5, 31] {
            for more in [2, 40, 100] {
                let mut heap = Heap::default();
                let mut a = array_of(&mut heap, 0..100);
                a = skip_owned(&mut heap, a, dropped).expect("room");
                let items: Vec<Value> = (0..more).map(|i| small(1000 + i)).collect();
                a = extend_owned(&mut heap, a, &items, end).expect("room");
                check(&heap, a);
                let old = (dropped as i64)..100;
                let new = 1000..1000 + more;
                let want: Vec<i64> = match end {
                    End::Front => new.chain(old).collect(),
                    End::Back => old.chain(new).collect(),
                };
                assert_eq!(ints(&heap, a), want);
                heap.release(a).expect("a live cell");
                assert_eq!(heap.live(), 0);
            }
        }
    }
}

/// The owned forms give up the caller's reference, so on an array that
/// was already freed they stop instead of freeing it again.
#[test]
fn an_owned_push_or_skip_of_a_freed_array_is_a_dead_cell() {
    let mut heap = Heap::default();
    let live = array_of(&mut heap, 0..3);
    let cells = heap.live();
    let a = array_of(&mut heap, 0..40);
    heap.release(a).expect("a live cell");
    assert_eq!(heap.live(), cells);
    let x = Value::int(7).expect("small");
    assert_eq!(push_owned(&mut heap, a, x, End::Back), Err(Fault::DeadCell));
    assert_eq!(skip_owned(&mut heap, a, 1), Err(Fault::DeadCell));
    for (head, taken) in [(true, true), (false, true), (true, false), (false, false)] {
        let first = uncons(&mut heap, a, head, taken);
        assert!(matches!(first, Err(Fault::DeadCell)));
    }
    // So they do with more items than a leaf has room for, which are not tried for a leaf.
    for n in [0, 1, 2, B - 1, B, B + 8] {
        for end in [End::Front, End::Back] {
            let run = extend_owned(&mut heap, a, &vec![x; n], end);
            assert_eq!(run, Err(Fault::DeadCell), "{n}");
        }
    }
    assert_eq!(concat(&mut heap, a, live), Err(Fault::DeadCell));
    assert_eq!(concat(&mut heap, live, a), Err(Fault::DeadCell));
    assert_eq!(slice(&mut heap, a, 1, 2), Err(Fault::DeadCell));
    assert_eq!(skip(&mut heap, a, 1), Err(Fault::DeadCell));
    assert_eq!(skip(&mut heap, a, 0), Err(Fault::DeadCell));
    heap.release(live).expect("a live cell");
    assert_eq!(heap.live(), 0);
}

/// With no room for the tail, the first element is left with the references it had, and
/// the array is still there.
#[test]
fn a_first_element_with_no_room_for_the_others_is_left_as_it_was() {
    let three = |heap: &mut Heap| {
        let all = ["a", "b", "c"].map(|name| string(heap, name));
        (all, from_values(heap, &all).expect("room"))
    };
    let mut room = Heap::default();
    three(&mut room);
    for taken in [false, true] {
        let mut heap = Heap::limited(room.words_used());
        let ([first, ..], a) = three(&mut heap);
        let got = uncons(&mut heap, a, true, taken);
        assert!(matches!(got, Err(Fault::Full)));
        let first = first.as_cell().expect("a string");
        assert_eq!(heap.unique(first), Ok(true));
        assert_eq!(heap.unique(a), Ok(true));
        assert_eq!(texts(&heap, a), ["a", "b", "c"]);
    }
}

/// A reader of a cell that is no array finds no element, however the cell got there.
#[test]
fn what_is_no_tree_has_no_elements() {
    let mut heap = Heap::default();
    let freed = array_of(&mut heap, 0..40);
    heap.release(freed).expect("a live cell");
    let other = heap.make(Kind::Binary, &[9; 12]).expect("room");
    let range = super::range(&mut heap, 3, 9).expect("room");
    for cell in [freed, other, range] {
        assert!(super::elements(&heap, cell).is_empty());
        assert_eq!(len(&heap, cell), 0);
        assert!(get(&heap, cell, 0).is_none());
        assert_eq!(tree_len(&heap, Value::cell(cell)), None);
        let split = Split {
            length: Length { len: 0, more: true },
            from: 0,
        };
        let found = super::split(&heap, Value::cell(cell), split, &mut []);
        match cell == range {
            true => assert!(matches!(found, Found::Range(3))),
            false => assert!(matches!(found, Found::Other)),
        }
    }
}

/// A freed array cannot be shared back to life, so an owned push does not
/// take it for one it may change in place.
#[test]
fn an_owned_push_onto_a_freed_array_is_a_dead_cell_after_a_share_is_refused() {
    let mut heap = Heap::default();
    let a = array_of(&mut heap, 0..3);
    heap.release(a).expect("a live cell");
    assert_eq!(heap.share(a), Err(DeadCell));
    let x = Value::int(7).expect("small");
    assert_eq!(push_owned(&mut heap, a, x, End::Back), Err(Fault::DeadCell));
    assert_eq!(heap.live(), 0);
}

fn small(n: i64) -> Value {
    Value::int(n).expect("small")
}

fn text(heap: &Heap, v: Value) -> String {
    let mut out = Vec::new();
    if let Some(c) = v.as_cell() {
        heap.read_string(c, &mut out);
    }
    String::from_utf8(out).expect("UTF-8")
}

fn texts(heap: &Heap, a: Cell) -> Vec<String> {
    elements(heap, a)
        .into_iter()
        .map(|v| text(heap, v))
        .collect()
}

fn string(heap: &mut Heap, s: &str) -> Value {
    Value::cell(heap.string(s.as_bytes()).expect("room"))
}

/// A write to an array nothing else holds changes it where it is. The
/// same root comes back, no cell is made, and only that slot differs,
/// whether the slot is in the head, the tree or the tail.
#[test]
fn a_write_to_an_array_with_one_holder_makes_no_cell() {
    let mut heap = Heap::default();
    for n in [1, 2, 31, 32, 33, 64, 65, 1024, 1025, 5000, 40_000] {
        let mut a = array_of(&mut heap, 0..n);
        // A head too, so all three parts of the root are written.
        for k in 0..3 {
            a = push_owned(&mut heap, a, small(-100 - k), End::Front).expect("room");
        }
        let mut want = ints(&heap, a);
        let made = heap.made();
        let len = want.len();
        for i in [0, 1, 2, 3, 4, len / 2, len - 2, len - 1] {
            if i < len {
                let wrote = set(&mut heap, a, i, small(7000 + i as i64));
                assert_eq!(wrote, Ok(a), "size {n} at {i}");
                want[i] = 7000 + i as i64;
            }
        }
        assert_eq!(heap.made(), made, "size {n}");
        check(&heap, a);
        assert_eq!(ints(&heap, a), want, "size {n}");
        heap.release(a).expect("a live cell");
        assert_eq!(heap.live(), 0, "size {n}");
    }
}

/// A write to an array something else holds leaves that holder's array
/// as it was. It copies the root and the nodes on the way to the slot,
/// one a level, and shares every other node.
#[test]
fn a_write_to_a_held_array_copies_one_node_a_level() {
    let mut heap = Heap::default();
    // The size, an index, and the nodes from the root to its slot.
    for (n, i, path) in [
        (1, 0, 2),
        (32, 31, 2),
        (33, 0, 2),
        (33, 32, 2),
        (1025, 500, 3),
        (1025, 1024, 2),
        (40_000, 20_000, 5),
        (40_000, 39_999, 2),
    ] {
        let a = array_of(&mut heap, 0..n);
        heap.share(a).expect("a live cell");
        let made = heap.made();
        let b = set(&mut heap, a, i as usize, small(-1)).expect("room");
        assert_ne!(b, a, "size {n} at {i}");
        assert_eq!(heap.made() - made, path, "size {n} at {i}");
        check(&heap, a);
        check(&heap, b);
        assert_eq!(ints(&heap, a), (0..n).collect::<Vec<_>>(), "size {n}");
        let mut want: Vec<i64> = (0..n).collect();
        want[i as usize] = -1;
        assert_eq!(ints(&heap, b), want, "size {n} at {i}");
        // The copy is its holder's alone, so a second write is in place.
        let made = heap.made();
        assert_eq!(set(&mut heap, b, i as usize, small(-2)), Ok(b));
        assert_eq!(heap.made(), made);
        heap.release(a).expect("a live cell");
        heap.release(b).expect("a live cell");
        assert_eq!(heap.live(), 0, "size {n} at {i}");
    }
}

/// An array can have a root of its own over nodes another array shares,
/// as after a push. A write copies just the shared nodes on its way.
#[test]
fn a_write_under_a_root_of_its_own_copies_only_what_is_shared() {
    let mut heap = Heap::default();
    let a = array_of(&mut heap, 0..2000);
    let b = push(&mut heap, a, small(2000), End::Back).expect("room");
    let made = heap.made();
    let c = set(&mut heap, b, 100, small(-1)).expect("room");
    assert_eq!(c, b, "the root was this array's alone");
    // The two branches above the slot and its leaf.
    assert_eq!(heap.made() - made, 3);
    assert_eq!(ints(&heap, a), (0..2000).collect::<Vec<_>>());
    let mut want: Vec<i64> = (0..=2000).collect();
    want[100] = -1;
    assert_eq!(ints(&heap, c), want);
    check(&heap, a);
    check(&heap, c);
    heap.release(a).expect("a live cell");
    heap.release(c).expect("a live cell");
    assert_eq!(heap.live(), 0);
}

/// An index the array does not have changes nothing and copies nothing,
/// held or not, and the value that was to go in is given up.
#[test]
fn a_write_at_an_index_the_array_does_not_have_changes_nothing() {
    let mut heap = Heap::default();
    for n in [0, 1, 40] {
        let a = array_of(&mut heap, 0..n);
        for held in [false, true] {
            if held {
                heap.share(a).expect("a live cell");
            }
            let live = heap.live();
            let made = heap.made();
            for i in [n as usize, n as usize + 1, usize::MAX] {
                let x = string(&mut heap, "never stored");
                assert_eq!(set(&mut heap, a, i, x), Ok(a), "size {n} at {i}");
            }
            assert_eq!(heap.live(), live, "size {n}");
            assert_eq!(heap.made() - made, 3, "only the three strings");
            assert_eq!(ints(&heap, a), (0..n).collect::<Vec<_>>());
        }
        heap.release(a).expect("a live cell");
        heap.release(a).expect("a live cell");
        assert_eq!(heap.live(), 0);
    }
}

/// An array walked part way keeps what it left behind in its head leaf.
/// A write counts from the array's own front, in place when the leaf is
/// its alone and in a copy when the array it was cut from still holds
/// the leaf.
#[test]
fn a_write_to_an_array_walked_part_way_counts_from_its_own_front() {
    let mut heap = Heap::default();
    for k in [1, 5, 31, 32, 33, 70] {
        let whole = array_of(&mut heap, 0..200);
        let rest = skip(&mut heap, whole, k).expect("room");
        let wrote = set(&mut heap, rest, 0, small(-1)).expect("room");
        let wrote = set(&mut heap, wrote, 3, small(-3)).expect("room");
        check(&heap, wrote);
        let mut want: Vec<i64> = (k as i64..200).collect();
        want[0] = -1;
        want[3] = -3;
        assert_eq!(ints(&heap, wrote), want, "skip {k}");
        assert_eq!(ints(&heap, whole), (0..200).collect::<Vec<_>>());
        heap.release(whole).expect("a live cell");
        let made = heap.made();
        let alone = skip_owned(&mut heap, wrote, 2).expect("room");
        let alone = set(&mut heap, alone, 0, small(-9)).expect("room");
        want.drain(..2);
        want[0] = -9;
        assert_eq!(ints(&heap, alone), want, "skip {k}, then 2 more");
        check(&heap, alone);
        assert!(heap.made() - made <= 4, "a walk and a write copied");
        heap.release(alone).expect("a live cell");
        assert_eq!(heap.live(), 0, "skip {k}");
    }
}

/// The element a write puts in is held by the array, and the one it
/// takes out is given up. The elements are strings, which are cells, so
/// a lost or an extra reference shows in the live count.
#[test]
fn a_write_gives_up_the_element_it_replaces() {
    let mut heap = Heap::default();
    let items: Vec<Value> = (0..40)
        .map(|i| string(&mut heap, &format!("s{i}")))
        .collect();
    let a = from_values(&mut heap, &items).expect("room");
    let live = heap.live();
    let x = string(&mut heap, "new");
    let a = set(&mut heap, a, 7, x).expect("room");
    assert_eq!(heap.live(), live, "one string in, one out");
    assert_eq!(text(&heap, get(&heap, a, 7).expect("an element")), "new");
    // Held, the old array keeps the old string and both keep the rest.
    heap.share(a).expect("a live cell");
    let y = string(&mut heap, "newer");
    let b = set(&mut heap, a, 7, y).expect("room");
    assert_eq!(text(&heap, get(&heap, a, 7).expect("an element")), "new");
    assert_eq!(text(&heap, get(&heap, b, 7).expect("an element")), "newer");
    heap.release(a).expect("a live cell");
    assert_eq!(texts(&heap, b)[6..9], ["s6", "newer", "s8"]);
    heap.release(b).expect("a live cell");
    assert_eq!(heap.live(), 0);
}

/// Writes mixed with pushes, walks and joins, on arrays that are held
/// and arrays that are not, against a plain list of what each should
/// hold. Every version kept along the way must read back as it was when
/// it was kept, and every reference must be counted.
#[test]
fn writes_among_other_edits_leave_every_kept_version_alone() {
    for seed in 1..=4u64 {
        let (mut near, mut apart) = (0, 0);
        let mut heap = Heap::default();
        let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let mut next = |below: usize| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (state >> 33) as usize % below
        };
        let mut a = from_values(&mut heap, &[]).expect("room");
        let mut want: Vec<String> = Vec::new();
        let mut kept: Vec<(Cell, Vec<String>)> = Vec::new();
        for step in 0..6000 {
            match next(12) {
                0 | 1 => {
                    let name = format!("b{step}");
                    let x = string(&mut heap, &name);
                    a = push_owned(&mut heap, a, x, End::Back).expect("room");
                    want.push(name);
                }
                2 => {
                    let name = format!("f{step}");
                    let x = string(&mut heap, &name);
                    a = push_owned(&mut heap, a, x, End::Front).expect("room");
                    want.insert(0, name);
                }
                3 if !want.is_empty() => {
                    a = skip_owned(&mut heap, a, 1).expect("room");
                    want.remove(0);
                }
                4 if next(8) == 0 => {
                    let more: Vec<String> = (0..next(90)).map(|k| format!("j{step}.{k}")).collect();
                    let items: Vec<Value> = more.iter().map(|s| string(&mut heap, s)).collect();
                    let piece = from_values(&mut heap, &items).expect("room");
                    let joined = concat(&mut heap, a, piece).expect("room");
                    heap.release(a).expect("a live cell");
                    heap.release(piece).expect("a live cell");
                    a = joined;
                    want.extend(more);
                }
                4 | 7 if next(8) == 0 => {
                    let names: Vec<String> =
                        (0..next(90)).map(|k| format!("r{step}.{k}")).collect();
                    let run: Vec<Value> = names.iter().map(|s| string(&mut heap, s)).collect();
                    let end = if next(2) == 0 { End::Front } else { End::Back };
                    a = extend_owned(&mut heap, a, &run, end).expect("room");
                    let at = if end == End::Front { 0 } else { want.len() };
                    want.splice(at..at, names);
                }
                5 if next(20) == 0 => {
                    heap.share(a).expect("a live cell");
                    kept.push((a, want.clone()));
                }
                6 if next(20) == 0 && !kept.is_empty() => {
                    let (k, items) = kept.swap_remove(next(kept.len()));
                    assert_eq!(texts(&heap, k), items, "seed {seed} step {step}");
                    heap.release(k).expect("a live cell");
                }
                10 => {
                    let (head, taken) = (next(2) == 0, next(3) > 0);
                    let found = uncons(&mut heap, a, head, taken).expect("room");
                    assert_eq!(found.is_some(), !want.is_empty(), "seed {seed} step {step}");
                    if let Some((first, left)) = found {
                        let name = want.remove(0);
                        match head {
                            true => assert_eq!(text(&heap, first), name),
                            false => assert_eq!(first.bits(), Value::NIL.bits()),
                        }
                        release(&mut heap, first).expect("live");
                        if taken {
                            a = left;
                        } else {
                            assert_eq!(texts(&heap, left), want, "seed {seed} step {step}");
                            heap.release(left).expect("a live cell");
                            want.insert(0, name);
                        }
                    }
                }
                11 => {
                    let len = next(want.len() + 2).min(want.len() + 1).min(400);
                    let from = next(len + 1).min(len);
                    let n = next(5).min(len - from);
                    let length = Length {
                        len: u32::try_from(len).expect("small"),
                        more: next(2) == 0,
                    };
                    let split = Split {
                        length,
                        from: u32::try_from(from).expect("small"),
                    };
                    let mut out = [Value::NIL; 4];
                    let out = &mut out[..n.min(4)];
                    let fitting = fits(want.len() as u64, length);
                    match super::split(&heap, Value::cell(a), split, out) {
                        Found::Miss => assert!(!fitting),
                        Found::All => {
                            near += 1;
                            assert!(fitting);
                        }
                        Found::Apart(r) => {
                            apart += 1;
                            assert!(fitting);
                            assert_eq!(run(&heap, &r, from, out), out.len());
                        }
                        Found::Range(_) | Found::Other => panic!("seed {seed} step {step}"),
                    }
                    if fitting {
                        for (i, v) in out.iter().enumerate() {
                            assert_eq!(text(&heap, *v), want[from + i]);
                        }
                    }
                }
                _ => {
                    // One write in eight is at an index past the end.
                    let i = next(want.len() + want.len() / 8 + 1);
                    let name = format!("w{step}");
                    let x = string(&mut heap, &name);
                    a = set(&mut heap, a, i, x).expect("room");
                    if let Some(slot) = want.get_mut(i) {
                        *slot = name;
                    }
                }
            }
            if step % 97 == 0 {
                check(&heap, a);
                assert_eq!(texts(&heap, a), want, "seed {seed} step {step}");
            }
        }
        check(&heap, a);
        assert_eq!(texts(&heap, a), want, "seed {seed}");
        for (k, items) in &kept {
            check(&heap, *k);
            assert_eq!(&texts(&heap, *k), items, "seed {seed}");
        }
        heap.release(a).expect("a live cell");
        for (k, _) in kept {
            heap.release(k).expect("a live cell");
        }
        assert_eq!(heap.live(), 0, "seed {seed} lost track of a reference");
        assert!(near > 20 && apart > 20, "seed {seed}: {near} {apart}");
    }
}

/// Arrays of `n` numbers from 0 on, put together in every way.
fn grown(heap: &mut Heap, n: i64) -> Vec<Cell> {
    let whole = array_of(heap, 0..n);
    let (mut back, mut front) = (array_of(heap, 0..0), array_of(heap, 0..0));
    for i in 0..n {
        back = push_owned(heap, back, small(i), End::Back).expect("room");
        front = push_owned(heap, front, small(n - 1 - i), End::Front).expect("room");
    }
    // One element is left in what was its first leaf.
    let more = array_of(heap, -31..n);
    let cut = skip_owned(heap, more, 31).expect("room");
    let mut pushed = array_of(heap, -31..n);
    pushed = push_owned(heap, pushed, small(-32), End::Front).expect("room");
    pushed = skip_owned(heap, pushed, 32).expect("room");
    let (a, b) = (array_of(heap, 0..n / 3), array_of(heap, n / 3..n));
    let joined = concat(heap, a, b).expect("room");
    for part in [a, b] {
        heap.release(part).expect("a live cell");
    }
    vec![whole, back, front, cut, pushed, joined]
}

/// A run of elements is found wherever it starts and however many leaves it is over, and
/// as far as the array goes. One that is all in the first leaf, or in an array's only
/// leaf, is found where the length is asked.
#[test]
fn a_run_of_elements_is_found_over_any_leaves() {
    let mut heap = Heap::default();
    for n in [1, 2, 5, 31, 32, 33, 34, 65, 100, 1100] {
        for a in grown(&mut heap, n) {
            check(&heap, a);
            let n = usize::try_from(n).expect("small");
            let r = root(&heap, a);
            for from in (0..n).filter(|i| *i < 70 || *i > n - 40 || *i % 97 == 0) {
                for m in [1, 2, 4, 8, 16, 40] {
                    let mut out = vec![Value::NIL; m];
                    let found = run(&heap, &r, from, &mut out);
                    assert_eq!(found, m.min(n - from));
                    let want: Vec<_> = (from..from + found).map(|i| Some(i as i64)).collect();
                    let got: Vec<_> = out.iter().take(found).map(|v| int(*v)).collect();
                    assert_eq!(got, want, "{n} {from} {m}");
                    let split = Split {
                        length: Length { len: 0, more: true },
                        from: u32::try_from(from).expect("small"),
                    };
                    let mut again = vec![Value::NIL; m];
                    let near = super::split(&heap, Value::cell(a), split, &mut again);
                    let head = head_len(&heap, &r);
                    let alone = head == 0 && r.tree.as_cell().is_none();
                    let leaf = if alone { n } else { head };
                    match near {
                        Found::All => {
                            assert!(from + m <= leaf, "{n} {from} {m}");
                            let same = again.iter().zip(&out).all(|(a, b)| a.bits() == b.bits());
                            assert!(same, "{n} {from} {m}");
                        }
                        Found::Apart(_) => assert!(from + m > leaf, "{n} {from} {m}"),
                        Found::Miss | Found::Range(_) | Found::Other => {
                            panic!("{n} {from} {m}")
                        }
                    }
                }
            }
            heap.release(a).expect("a live cell");
        }
    }
    assert_eq!(heap.live(), 0);
}

/// A run of a tree that has fewer elements than its root says ends where they do.
#[test]
fn a_run_ends_where_the_elements_do() {
    let mut heap = Heap::default();
    for n in [3, 40] {
        let a = array_of(&mut heap, 0..n);
        let mut r = root(&heap, a);
        r.len += 5;
        let mut out = [Value::NIL; 6];
        let from = usize::try_from(n).expect("small") - 2;
        assert_eq!(run(&heap, &r, from, &mut out), 2);
        assert_eq!(run(&heap, &r, from + 2, &mut out), 0);
        assert_eq!(run(&heap, &r, usize::MAX, &mut out), 0);
    }
}

/// A write gives up the caller's reference, so on an array that was
/// already freed it stops instead of writing into the freed cell. A
/// node freed behind the array's back stops it the same way.
#[test]
fn a_write_to_a_freed_array_or_through_a_freed_node_is_a_dead_cell() {
    let mut heap = Heap::default();
    let a = array_of(&mut heap, 0..40);
    heap.release(a).expect("a live cell");
    assert_eq!(set(&mut heap, a, 0, small(7)), Err(Fault::DeadCell));
    assert_eq!(heap.live(), 0);

    let b = array_of(&mut heap, 0..3);
    let tail = root(&heap, b).tail.as_cell().expect("a tail leaf");
    heap.release(tail).expect("a live cell");
    assert_eq!(set(&mut heap, b, 1, small(7)), Err(Fault::DeadCell));

    // A share does not bring a freed leaf back as one to copy.
    let c = array_of(&mut heap, 0..3);
    let tail = root(&heap, c).tail.as_cell().expect("a tail leaf");
    heap.release(tail).expect("a live cell");
    assert_eq!(heap.share(tail), Err(DeadCell));
    assert_eq!(set(&mut heap, c, 1, small(7)), Err(Fault::DeadCell));
}

/// A push that cannot change the array in place builds a new one and
/// then gives the old one up. When something freed the tail's leaf behind
/// the array's back, the push stops there.
#[test]
fn an_owned_push_onto_an_array_with_a_freed_leaf_is_a_dead_cell() {
    let mut heap = Heap::default();
    let a = array_of(&mut heap, 0..3);
    let tail = root(&heap, a).tail.as_cell().expect("a tail leaf");
    heap.release(tail).expect("a live cell");
    let x = Value::int(7).expect("small");
    assert_eq!(push_owned(&mut heap, a, x, End::Back), Err(Fault::DeadCell));
}

/// An array that `grow` made is walked to its end by its first element and the others, by
/// a walk that gives each up and by one that keeps the whole. Gives how many cells the
/// walk made.
fn walked(n: i64, grow: impl Fn(&mut Heap, i64) -> Cell, kept: bool) -> usize {
    let mut heap = Heap::default();
    let whole = grow(&mut heap, n);
    if kept {
        heap.share(whole).expect("a live cell");
    }
    let before = heap.made();
    let mut a = whole;
    let mut want = 0;
    while let Some((first, left)) = uncons(&mut heap, a, true, true).expect("room") {
        assert_eq!(int(first), Some(want));
        want += 1;
        a = left;
    }
    assert_eq!(want, n);
    let made = heap.made() - before;
    heap.release(a).expect("a live cell");
    if kept {
        assert_eq!(ints(&heap, whole), (0..n).collect::<Vec<_>>());
        heap.release(whole).expect("a live cell");
    }
    assert_eq!(heap.live(), 0);
    made
}

/// A walk of an array twice as long makes no more than twice as many cells, however the
/// array was built and whether or not something else holds it. A step in a leaf that
/// is in front makes none.
#[test]
fn a_walk_does_not_grow_faster_than_its_array() {
    let whole = |heap: &mut Heap, n: i64| array_of(heap, 0..n);
    let pushed = |end: End| {
        move |heap: &mut Heap, n: i64| {
            let mut a = array_of(heap, 0..0);
            for i in 0..n {
                let x = match end {
                    End::Back => i,
                    End::Front => n - 1 - i,
                };
                a = push_owned(heap, a, small(x), end).expect("room");
            }
            a
        }
    };
    for kept in [false, true] {
        for n in [1000, 4000] {
            let all = |n| {
                [
                    walked(n, whole, kept),
                    walked(n, pushed(End::Back), kept),
                    walked(n, pushed(End::Front), kept),
                ]
            };
            for (few, many) in all(n).into_iter().zip(all(2 * n)) {
                // The last leaf is walked by itself, however full it is.
                assert!(many <= few * 2 + 70, "{n} {kept}: {few} {many}");
            }
        }
        // One in each 32 steps cuts the tree.
        assert!(walked(3200, whole, kept) < 1100);
    }
}
