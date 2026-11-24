use super::*;

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

/// The owned forms give up the caller's reference, so on an array that
/// was already freed they stop instead of freeing it again.
#[test]
fn an_owned_push_or_skip_of_a_freed_array_is_a_dead_cell() {
    let mut heap = Heap::default();
    let a = array_of(&mut heap, 0..40);
    heap.release(a).expect("a live cell");
    assert_eq!(heap.live(), 0);
    let x = Value::int(7).expect("small");
    assert_eq!(push_owned(&mut heap, a, x, End::Back), Err(Fault::DeadCell));
    assert_eq!(skip_owned(&mut heap, a, 1), Err(Fault::DeadCell));
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
            match next(10) {
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
                5 if next(20) == 0 => {
                    heap.share(a).expect("a live cell");
                    kept.push((a, want.clone()));
                }
                6 if next(20) == 0 && !kept.is_empty() => {
                    let (k, items) = kept.swap_remove(next(kept.len()));
                    assert_eq!(texts(&heap, k), items, "seed {seed} step {step}");
                    heap.release(k).expect("a live cell");
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
