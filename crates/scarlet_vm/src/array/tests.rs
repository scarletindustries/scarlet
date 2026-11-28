use super::*;

mod trees;

// What follows takes an array as the value that holds it.

impl Heap {
    /// `three` values as an array of each form there is: a cell whose elements start in its
    /// first slot, twice, and one whose do not, what is left of a longer array, a tree whose
    /// first is in its root and one that has lost some of its head, and last the range of 3, 4
    /// and 5, which is one of them when those are the values.
    pub(crate) fn three_in_every_form(&mut self, three: &[Value]) -> [Value; 7] {
        // One reference for each array there is of each of them, and for those dropped again.
        for v in three {
            for _ in 0..50 {
                self.own(*v).expect("live");
            }
        }
        let flat = from_values(self, three).expect("room");
        let same = from_values(self, three).expect("room");
        let mut turned = from_values(self, &three[2..]).expect("room");
        for x in [three[1], three[0]] {
            turned = push(self, turned, x, End::Front).expect("room");
        }
        // One in the head and two in the tail.
        let mut many = vec![three[0]; 32];
        many.extend(&three[1..]);
        let mut left = from_values(self, &many[..32]).expect("room");
        left = extend(self, left, &many[32..], End::Back).expect("room");
        left = skip(self, left, 31).expect("room");
        assert!(is_flat(self, turned) && !is_flat(self, left));
        // One in the root, one in the head and one in the tail.
        let mut head = from_values(self, &three[1..2]).expect("room");
        let tail = from_values(self, &three[2..]).expect("room");
        let front = Root {
            start: FRONT,
            front: three[0],
            ..tree_of(self, head, tail)
        };
        let front = close(self, None, front).expect("room");
        // Two in the head, behind two numbers, and one in the tail.
        let in_head = [&[Value::NIL, small(3)], &three[..2]].concat();
        head = from_values(self, &in_head).expect("room");
        let tail = from_values(self, &three[2..]).expect("room");
        let behind = Root {
            start: 2,
            ..tree_of(self, head, tail)
        };
        let behind = close(self, None, behind).expect("room");
        for v in [front, behind] {
            check(self, v);
        }
        let range = range(self, 3, 6).expect("room");
        [flat, same, turned, left, front, behind, range]
    }
}

/// A root of a head and a tail.
fn tree_of(heap: &Heap, head: Value, tail: Value) -> Root {
    let [head, tail] = [head, tail].map(|v| Edge::of(short(heap, v).expect("a short array")));
    Root {
        head,
        tail,
        ..Root::EMPTY
    }
}

/// `array` as what stores its elements.
pub(super) fn stored(heap: &Heap, array: Value) -> Stored {
    match seq(heap, array).expect("an array") {
        Seq::Empty => Stored::Empty,
        Seq::Flat(flat) => Stored::Flat(flat),
        Seq::Tree(cell) => Stored::Tree(cell),
        Seq::Range { .. } => panic!("a range stores none"),
    }
}

pub(super) fn is_flat(heap: &Heap, array: Value) -> bool {
    matches!(stored(heap, array), Stored::Flat(_))
}

/// The parts of a tree.
pub(super) fn root(heap: &Heap, array: Value) -> Root {
    tree(heap, array.as_cell().expect("a cell")).expect("a tree")
}

pub(super) fn elements(heap: &Heap, array: Value) -> Vec<Value> {
    super::elements(heap, seq(heap, array).expect("an array")).expect("no range")
}

fn len(heap: &Heap, array: Value) -> usize {
    let len = seq(heap, array).expect("an array").len(heap);
    usize::try_from(len).expect("small")
}

fn get(heap: &Heap, array: Value, i: usize) -> Option<Value> {
    match stored(heap, array) {
        Stored::Empty => None,
        Stored::Flat(flat) => flat.get(heap, i),
        Stored::Tree(cell) => super::get(heap, cell, i),
    }
}

pub(super) fn set(heap: &mut Heap, array: Value, i: usize, x: Value) -> Result<Value, Fault> {
    let array = stored(heap, array);
    super::set(heap, array, i, x)
}

pub(super) fn push(heap: &mut Heap, array: Value, x: Value, end: End) -> Result<Value, Fault> {
    let array = stored(heap, array);
    super::push(heap, array, x, end)
}

pub(super) fn extend(
    heap: &mut Heap,
    array: Value,
    items: &[Value],
    end: End,
) -> Result<Value, Fault> {
    let array = stored(heap, array);
    super::extend(heap, array, items, end)
}

pub(super) fn skip(heap: &mut Heap, array: Value, n: usize) -> Result<Value, Fault> {
    let array = stored(heap, array);
    super::skip(heap, array, n)
}

pub(super) fn slice(heap: &mut Heap, array: Value, from: usize, to: usize) -> Result<Value, Fault> {
    let array = stored(heap, array);
    super::slice(heap, array, from, to)
}

/// Both arrays joined, however many steps that takes.
pub(super) fn concat(heap: &mut Heap, l: Value, r: Value) -> Result<Value, Fault> {
    let (mut l, mut r) = (stored(heap, l), stored(heap, r));
    loop {
        match join(heap, l, r)? {
            Joined::All(all) => return Ok(all),
            Joined::Some(a, b) => (l, r) = (stored(heap, a), stored(heap, b)),
        }
    }
}

// The same, of an array that the caller keeps its reference to.

fn pushed_on(heap: &mut Heap, array: Value, x: Value, end: End) -> Result<Value, Fault> {
    heap.own(array)?;
    push(heap, array, x, end)
}

fn without(heap: &mut Heap, array: Value, n: usize) -> Result<Value, Fault> {
    heap.own(array)?;
    skip(heap, array, n)
}

fn front(heap: &mut Heap, array: Value, n: usize) -> Result<Value, Fault> {
    cut(heap, array, 0, n)
}

pub(super) fn cut(heap: &mut Heap, array: Value, from: usize, to: usize) -> Result<Value, Fault> {
    let to = to.min(len(heap, array));
    heap.own(array)?;
    slice(heap, array, from, to)
}

fn joined(heap: &mut Heap, l: Value, r: Value) -> Result<Value, Fault> {
    heap.own(l)?;
    heap.own(r)?;
    concat(heap, l, r)
}

/// The first element and the others, as the instruction does it. When `taken`, the array's
/// reference passes in.
pub(super) fn uncons(
    heap: &mut Heap,
    array: Value,
    head: bool,
    taken: bool,
) -> Result<Option<(Value, Value)>, Fault> {
    let Some(cell) = array.as_cell() else {
        return Ok(None);
    };
    if taken && let Some(first) = flat::take_first(heap, cell) {
        let left = if first.only {
            heap.discard(cell)?;
            Value::EMPTY_ARRAY
        } else {
            array
        };
        if !head {
            release(heap, first.value)?;
        }
        return Ok(Some((if head { first.value } else { Value::NIL }, left)));
    }
    if taken && let Some(first) = super::take_first(heap, cell) {
        if !head {
            release(heap, first)?;
        }
        return Ok(Some((if head { first } else { Value::NIL }, array)));
    }
    let found = match stored(heap, array) {
        Stored::Empty => None,
        Stored::Flat(flat) => flat::uncons(heap, flat, head)?,
        Stored::Tree(cell) if !taken || !heap.unique(cell)? => super::uncons(heap, cell, head)?,
        Stored::Tree(cell) => {
            let first = super::get(heap, cell, 0).expect("an element");
            let first = heap.own(if head { first } else { Value::NIL })?;
            return match super::skip(heap, Stored::Tree(cell), 1) {
                Ok(left) => Ok(Some((first, left))),
                Err(why) => {
                    release(heap, first)?;
                    Err(why)
                }
            };
        }
    };
    if taken {
        heap.release(cell)?;
    }
    Ok(Some(found.expect("an array that is not empty")))
}

/// What an operation gave, as something to compare.
pub(super) fn word(made: Result<Value, Fault>) -> Result<u64, Fault> {
    made.map(Value::bits)
}

fn ints(heap: &Heap, array: Value) -> Vec<i64> {
    elements(heap, array)
        .into_iter()
        .map(|v| match v.view() {
            crate::value::View::Int(n) => n,
            other => panic!("not an Int: {other:?}"),
        })
        .collect()
}

pub(super) fn int(v: Value) -> Option<i64> {
    match v.view() {
        crate::value::View::Int(n) => Some(n),
        _ => None,
    }
}

fn array_of(heap: &mut Heap, range: std::ops::Range<i64>) -> Value {
    let items: Vec<Value> = range.map(|n| Value::int(n).expect("small")).collect();
    from_values(heap, &items).expect("room")
}

/// Check every invariant of an array. A short one's cell is laid out as it says. Of a tree
/// that is how many each edge has, where each leaf is, that nothing else is there, and the
/// tree's height. The audit says the same.
pub(super) fn check(heap: &Heap, array: Value) {
    let Some(cell) = array.as_cell() else {
        assert_eq!(array.bits(), Value::EMPTY_ARRAY.bits());
        return;
    };
    if is_flat(heap, array) {
        assert!(flat::laid_out(heap.data(cell)).is_ok());
        assert_eq!(elements(heap, array).len(), len(heap, array));
        return;
    }
    let r = root(heap, array);
    for edge in [r.head, r.tail] {
        let has = short(heap, edge.items).map_or(0, Flat::len);
        assert_eq!(edge.len, has);
        assert_eq!(edge.len == 0, edge.items.as_cell().is_none());
    }
    match r.start {
        0 => {}
        FRONT => assert!(r.head.len > 0),
        start => {
            // What it has lost and still has is plain.
            assert!(start < r.head.len);
            assert!(all(heap, r.head.items, (0, start), plain_inside));
        }
    }
    assert!(r.start == FRONT || heap.data(cell)[0] == Value::NIL.bits());
    let mut places = Vec::new();
    places_of(heap, r.tree, r.shift, 0, &mut places);
    let all = B << r.shift;
    let mut want: Vec<usize> = (0..r.tree_len)
        .step_by(B)
        .map(|i| (r.first + i) % all)
        .collect();
    want.sort_unstable();
    assert_eq!(places, want);
    assert!(r.first < all && r.first.is_multiple_of(B));
    // No level less would do.
    let leaves = r.tree_len / B;
    let levels = (0..).find(|levels| leaves <= B.pow(*levels));
    assert_eq!(levels.map(|n| n * BITS), Some(r.shift));
    let both = r.head.len > 0 && r.tail.len > 0;
    assert!(r.tree_len > 0 || both, "a short array");
    assert_eq!(elements(heap, array).len(), r.len());
    assert!(audit(heap, cell).is_ok());
}

/// The places of the leaves under `node`, the first of which is `at`.
fn places_of(heap: &Heap, node: Value, shift: u32, at: usize, out: &mut Vec<usize>) {
    let Some(cell) = node.as_cell() else {
        return;
    };
    if shift == 0 {
        assert_eq!(short(heap, node).expect("a leaf").len(), B);
        out.push(at);
        return;
    }
    assert!(heap.is(cell, Kind::ArrayBranch));
    assert_eq!(heap.data(cell).len(), B);
    let before = out.len();
    for (i, child) in heap.values(cell).enumerate() {
        places_of(heap, child, shift - BITS, at + (i << shift), out);
    }
    assert!(out.len() > before, "a branch with no leaf");
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
        release(&mut heap, a).expect("a live cell");
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
    heap.own(a).expect("a live cell");
    let mut want: std::collections::VecDeque<i64> = (0..10).collect();
    for i in 0..3000 {
        let (end, x) = if i % 3 == 0 {
            (End::Front, -i)
        } else {
            (End::Back, 100 + i)
        };
        let next = pushed_on(&mut heap, a, Value::int(x).expect("small"), end).expect("room");
        release(&mut heap, a).expect("a live cell");
        a = next;
        match end {
            End::Front => want.push_front(x),
            End::Back => want.push_back(x),
        }
    }
    check(&heap, a);
    assert_eq!(ints(&heap, a), want.into_iter().collect::<Vec<_>>());
    assert_eq!(ints(&heap, first), (0..10).collect::<Vec<_>>());
    release(&mut heap, a).expect("a live cell");
    release(&mut heap, first).expect("a live cell");
    assert_eq!(heap.live(), 0);
}

/// Pushes that give their array up change it in place when nothing else
/// holds it, so they must leave alone every version something else does
/// hold, and keep every reference counted. The items are strings, which
/// are cells, so a lost or extra reference shows in the live count.
#[test]
fn a_push_leaves_every_kept_version_alone() {
    let mut heap = Heap::default();
    let text = |heap: &Heap, v: Value| {
        let mut out = Vec::new();
        if let Some(c) = v.as_cell() {
            heap.read_string(c, &mut out);
        }
        String::from_utf8(out).expect("UTF-8")
    };
    let texts = |heap: &Heap, a: Value| -> Vec<String> {
        elements(heap, a)
            .into_iter()
            .map(|v| text(heap, v))
            .collect()
    };
    let mut a = from_values(&mut heap, &[]).expect("room");
    let mut want: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    let mut kept: Vec<(Value, Vec<String>)> = Vec::new();
    for i in 0..2500 {
        if i % 41 == 0 {
            heap.own(a).expect("a live cell");
            kept.push((a, want.iter().cloned().collect()));
        }
        if i % 17 == 0 && !want.is_empty() {
            a = skip(&mut heap, a, 1).expect("room");
            want.pop_front();
            continue;
        }
        let end = if i % 3 == 0 { End::Front } else { End::Back };
        let name = format!("s{i}");
        let x = Value::cell(heap.string(name.as_bytes()).expect("room"));
        a = push(&mut heap, a, x, end).expect("room");
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
    release(&mut heap, a).expect("a live cell");
    for (k, _) in kept {
        release(&mut heap, k).expect("a live cell");
    }
    assert_eq!(heap.live(), 0, "a push lost track of a reference");
}

#[test]
fn take_and_skip_cut_anywhere() {
    let mut heap = Heap::default();
    let mut a = array_of(&mut heap, 0..10);
    for i in 0..40 {
        let next =
            pushed_on(&mut heap, a, Value::int(-1 - i).expect("small"), End::Front).expect("room");
        release(&mut heap, a).expect("a live cell");
        a = next;
    }
    let tail = array_of(&mut heap, 10..1500);
    let joined = joined(&mut heap, a, tail).expect("room");
    let all = ints(&heap, joined);
    for n in [0, 1, 5, 39, 40, 41, 50, 51, 700, 1539, 1540, 1541, 5000] {
        let t = front(&mut heap, joined, n).expect("room");
        check(&heap, t);
        assert_eq!(ints(&heap, t), all[..n.min(all.len())], "take {n}");
        let s = without(&mut heap, joined, n).expect("room");
        check(&heap, s);
        assert_eq!(ints(&heap, s), all[n.min(all.len())..], "skip {n}");
        release(&mut heap, t).expect("a live cell");
        release(&mut heap, s).expect("a live cell");
    }
    for c in [a, tail, joined] {
        release(&mut heap, c).expect("a live cell");
    }
    assert_eq!(heap.live(), 0);
}

/// Walking an array one element at a time, the way `[h, ..t]` does, while something else holds
/// it. Of numbers it copies no leaf: what is left of one is the leaf itself, whether the walk
/// holds each step or takes it. Of cells it copies each leaf once when it takes each step. One
/// that nothing else holds is walked with no cell made. Every step reads back as the plain list
/// does, from the front and through `get`, and the array that is kept is as it was.
#[test]
fn walking_an_array_from_the_front_copies_no_leaf() {
    let n = 1000;
    let leaves = n / B;
    for cells in [false, true] {
        for kept in [false, true] {
            for taken in [false, true] {
                let mut heap = Heap::default();
                let items: Vec<Value> = (0..n)
                    .map(|i| match cells {
                        true => string(&mut heap, &format!("{i}")),
                        false => small(i as i64),
                    })
                    .collect();
                let a = from_values(&mut heap, &items).expect("room");
                let parts = parts_of(&heap, a);
                let kept = kept.then(|| heap.own(a).expect("live"));
                let made = heap.made();
                let mut cur = a;
                let mut copied = 0;
                for (i, item) in items.iter().enumerate() {
                    assert_eq!(get(&heap, cur, 0).map(Value::bits), Some(item.bits()));
                    assert_eq!(len(&heap, cur), n - i);
                    if !is_flat(&heap, cur)
                        && let Some(head) = root(&heap, cur).head.items.as_cell()
                        && !parts.contains(&head)
                    {
                        copied += 1;
                    }
                    let found = uncons(&mut heap, cur, false, taken).expect("room");
                    let (_, next) = found.expect("an element");
                    check(&heap, next);
                    if !taken {
                        release(&mut heap, cur).expect("live");
                    }
                    cur = next;
                }
                assert_eq!(cur.bits(), Value::EMPTY_ARRAY.bits());
                let made = heap.made() - made;
                let what = format!("{cells} {} {taken}: {made}", kept.is_some());
                match (kept, taken) {
                    // A head is all that a step may make then, at the start of a leaf. What is
                    // left at the end is one cell, and is copied as it is.
                    (None, true) => assert!(made <= 2, "{what}"),
                    (Some(_), true) if cells => assert!(made <= leaves + 6, "{what}"),
                    (Some(_), true) => assert!(made <= 6, "{what}"),
                    // Each makes what is left.
                    (_, false) => assert!(made <= n + 3 * leaves, "{what}"),
                }
                if !cells {
                    assert_eq!(copied, 0, "{what}");
                }
                if let Some(kept) = kept {
                    check(&heap, kept);
                    let same = elements(&heap, kept);
                    assert!(same.iter().zip(&items).all(|(a, b)| a.bits() == b.bits()));
                    release(&mut heap, kept).expect("live");
                }
                assert_eq!(heap.live(), 0, "{what}");
            }
        }
    }
}

/// The short arrays that the tree `array` is made of.
fn parts_of(heap: &Heap, array: Value) -> Vec<Cell> {
    let r = root(heap, array);
    let leaves = (0..r.tree_len).step_by(B).map(|i| leaf(heap, &r, i));
    let all = leaves.map(|v| v.expect("a leaf"));
    all.chain([r.head.items, r.tail.items])
        .filter_map(Value::as_cell)
        .collect()
}

/// An array partly walked is an ordinary array. What it has lost and still
/// has in its head is invisible to reading it, cutting it, joining it and
/// pushing onto it, and none of those reach past its own front.
#[test]
fn an_array_walked_part_way_behaves_like_the_rest_of_it() {
    let mut heap = Heap::default();
    let whole = array_of(&mut heap, 0..200);
    let other = array_of(&mut heap, 200..260);
    for k in [1, 5, 31, 32, 33, 40, 64, 100] {
        let s = without(&mut heap, whole, k).expect("room");
        check(&heap, s);
        let rest: Vec<i64> = (k as i64..200).collect();
        assert_eq!(ints(&heap, s), rest, "skip {k}");
        assert_eq!(get(&heap, s, 0).and_then(int), Some(k as i64));
        assert_eq!(get(&heap, s, 3).and_then(int), Some(k as i64 + 3));

        let t = front(&mut heap, s, 7).expect("room");
        check(&heap, t);
        assert_eq!(ints(&heap, t), rest[..7], "take after skip {k}");

        let again = without(&mut heap, s, 9).expect("room");
        check(&heap, again);
        assert_eq!(ints(&heap, again), rest[9..], "skip twice {k}");

        let joined = joined(&mut heap, s, other).expect("room");
        check(&heap, joined);
        let want: Vec<i64> = rest.iter().copied().chain(200..260).collect();
        assert_eq!(ints(&heap, joined), want, "concat after skip {k}");

        let front =
            pushed_on(&mut heap, s, Value::int(-1).expect("small"), End::Front).expect("room");
        check(&heap, front);
        let want: Vec<i64> = std::iter::once(-1).chain(rest.iter().copied()).collect();
        assert_eq!(ints(&heap, front), want, "push front after skip {k}");

        let back =
            pushed_on(&mut heap, s, Value::int(-1).expect("small"), End::Back).expect("room");
        check(&heap, back);
        let want: Vec<i64> = rest.iter().copied().chain(std::iter::once(-1)).collect();
        assert_eq!(ints(&heap, back), want, "push back after skip {k}");

        for c in [s, t, again, joined, front, back] {
            release(&mut heap, c).expect("a live cell");
        }
    }
    release(&mut heap, whole).expect("a live cell");
    release(&mut heap, other).expect("a live cell");
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
            let j = joined(&mut heap, l, r).expect("room");
            check(&heap, j);
            assert_eq!(ints(&heap, j), (0..x + y).collect::<Vec<_>>(), "{x} ++ {y}");
            for c in [l, r, j] {
                release(&mut heap, c).expect("a live cell");
            }
        }
    }
    let mut acc = Value::EMPTY_ARRAY;
    let mut want = Vec::new();
    for i in 0..300 {
        let piece = array_of(&mut heap, i * 7..i * 7 + (i % 40));
        want.extend(i * 7..i * 7 + (i % 40));
        let next = joined(&mut heap, acc, piece).expect("room");
        release(&mut heap, acc).expect("a live cell");
        release(&mut heap, piece).expect("a live cell");
        acc = next;
    }
    check(&heap, acc);
    assert_eq!(ints(&heap, acc), want);
    assert!(
        root(&heap, acc).shift <= 3 * BITS,
        "height {}",
        root(&heap, acc).shift
    );
    release(&mut heap, acc).expect("a live cell");
    assert_eq!(heap.live(), 0);
}

/// Where everything in `array` sits: how many a short one has and in how many slots, or
/// that of a root's edges, and where the leaves are.
fn shape(heap: &Heap, array: Value) -> String {
    let edge = |v: Value| match short(heap, v) {
        Ok(flat) => format!("{} of {}", flat.len(), heap.data(flat.cell()).len() - 1),
        Err(_) => "none".to_string(),
    };
    if is_flat(heap, array) || array.as_cell().is_none() {
        return edge(array);
    }
    let r = root(heap, array);
    let mut places = Vec::new();
    places_of(heap, r.tree, r.shift, 0, &mut places);
    let [head, tail] = [r.head, r.tail].map(|e| format!("{} {}", e.len, edge(e.items)));
    let start = r.start.cast_signed();
    format!("{start} {head}, {} {} {places:?}, {tail}", r.first, r.shift)
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
                        heap.own(a).expect("a live cell");
                        heap.own(b).expect("a live cell");
                    }
                    let items = strings(&mut heap, "x", more);
                    let run = extend(&mut heap, a, &items, end).expect("room");
                    let mut each = b;
                    let items = strings(&mut heap, "x", more);
                    let in_turn: Vec<Value> = match end {
                        End::Front => items.into_iter().rev().collect(),
                        End::Back => items,
                    };
                    for x in in_turn {
                        each = push(&mut heap, each, x, end).expect("room");
                    }
                    let what = format!("{n} and {more}");
                    check(&heap, run);
                    // One that a cell is not filled by first, and one that is all there is, are
                    // packed otherwise.
                    if n >= 32 || n + more <= 32 {
                        assert_eq!(shape(&heap, run), shape(&heap, each), "{what}");
                    }
                    let named = |v: Vec<String>| -> Vec<String> {
                        v.into_iter().map(|s| s.replace('b', "a")).collect()
                    };
                    assert_eq!(texts(&heap, run), named(texts(&heap, each)), "{what}");
                    if shared {
                        assert_eq!((shape(&heap, a), texts(&heap, a)), before, "{what}");
                        release(&mut heap, a).expect("a live cell");
                        release(&mut heap, b).expect("a live cell");
                    } else if more > 0 && n > 32 {
                        assert_eq!(run.bits(), a.bits(), "{what}: the root changes in place");
                    }
                    release(&mut heap, run).expect("a live cell");
                    release(&mut heap, each).expect("a live cell");
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
        a = extend(&mut heap, a, &items, End::Back).expect("room");
    }
    check(&heap, a);
    assert_eq!(len(&heap, a), 1280);
    let made = heap.made() - made;
    assert!(made <= 39 * 2 + 1, "{made} cells for 39 runs");
    // The head is the array it was made of, which goes to a branch of its own.
    let before = heap.made();
    a = extend(&mut heap, a, &items, End::Front).expect("room");
    assert_eq!(heap.made() - before, 2);
    release(&mut heap, a).expect("a live cell");
    assert_eq!(heap.live(), 0);
}

/// A run that is added in front of an array that has lost some of its head comes before
/// what it still has of its own, and one added at the back leaves those where they are.
#[test]
fn a_run_is_added_to_what_is_left_of_an_array() {
    let sizes = [1usize, 5, 31].map(|dropped| [2i64, 40, 100].map(|more| (dropped, more)));
    for (dropped, more) in sizes.into_iter().flatten() {
        for end in [End::Front, End::Back] {
            for held in [false, true] {
                let mut heap = Heap::default();
                let whole = array_of(&mut heap, 0..100);
                let mut a = match held {
                    true => without(&mut heap, whole, dropped).expect("room"),
                    false => skip(&mut heap, whole, dropped).expect("room"),
                };
                assert_eq!(root(&heap, a).start, if held { dropped } else { 0 });
                let items: Vec<Value> = (0..more).map(|i| small(1000 + i)).collect();
                a = extend(&mut heap, a, &items, end).expect("room");
                check(&heap, a);
                let old = (dropped as i64)..100;
                let new = 1000..1000 + more;
                let want: Vec<i64> = match end {
                    End::Front => new.chain(old).collect(),
                    End::Back => old.chain(new).collect(),
                };
                assert_eq!(ints(&heap, a), want);
                release(&mut heap, a).expect("a live cell");
                if held {
                    assert!(ints(&heap, whole).into_iter().eq(0..100));
                    release(&mut heap, whole).expect("a live cell");
                }
                assert_eq!(heap.live(), 0);
            }
        }
    }
}

/// What an array loses is given up at once when nothing else holds the array, and when the
/// last that has it is given up otherwise, whatever is done to the rest of it first.
#[test]
fn what_an_array_loses_is_given_up_with_the_last_that_has_it() {
    for dropped in 1..=33usize {
        for run in [0, 1, 2, 3, 30, 31, 40] {
            for held in [false, true] {
                let mut heap = Heap::default();
                let handle = |heap: &mut Heap, i: usize| {
                    Value::cell(heap.make(Kind::Handle, &[i as u64]).expect("room"))
                };
                let released = |heap: &mut Heap| {
                    let mut all = heap.take_released();
                    all.sort_unstable();
                    all
                };
                let items: Vec<Value> = (0..70).map(|i| handle(&mut heap, i)).collect();
                let whole = from_values(&mut heap, &items).expect("room");
                if held {
                    heap.own(whole).expect("live");
                }
                let a = skip(&mut heap, whole, dropped).expect("room");
                let what = format!("{dropped} {run} {held}");
                let lost: Vec<u64> = (0..dropped as u64).collect();
                if held {
                    assert_eq!(released(&mut heap), [], "{what}");
                    check(&heap, whole);
                    assert!(
                        elements(&heap, whole)
                            .iter()
                            .map(|v| v.bits())
                            .eq(items.iter().map(|v| v.bits()))
                    );
                    release(&mut heap, whole).expect("live");
                }
                assert_eq!(released(&mut heap), lost, "{what}");
                let more: Vec<Value> = (0..run).map(|i| handle(&mut heap, 100 + i)).collect();
                let now = extend(&mut heap, a, &more, End::Front).expect("room");
                check(&heap, now);
                let want = more.iter().chain(&items[dropped..]).map(|v| v.bits());
                assert!(elements(&heap, now).iter().map(|v| v.bits()).eq(want));
                assert_eq!(released(&mut heap), [], "{what}");
                assert_eq!(heap.audit([now].into_iter()), Ok(()), "{what}");
                release(&mut heap, now).expect("live");
                assert_eq!(heap.live(), 0, "{what}");
            }
        }
    }
}

/// `[h, ..t]` of a long array that something else holds puts its first element in the root, and
/// copies none of the head, when no other array has that head. The array is the same to
/// everything, and can be changed as it could.
#[test]
fn an_array_may_have_its_first_element_in_its_root() {
    for n in [33usize, 40, 70, 1100] {
        for way in 0..11 {
            for held in [false, true] {
                let mut heap = Heap::default();
                let items: Vec<Value> =
                    (0..n).map(|i| holder(&mut heap, &format!("{i}"))).collect();
                let a = from_values(&mut heap, &items).expect("room");
                let made = heap.made();
                let t = without(&mut heap, a, 1).expect("room");
                assert_eq!(heap.made() - made, 1, "{n}");
                assert!(root(&heap, a).front().is_some());
                assert!(root(&heap, t).front().is_none());
                assert_eq!(
                    root(&heap, a).head.items.bits(),
                    root(&heap, t).head.items.bits()
                );
                let again = without(&mut heap, a, 1).expect("room");
                assert_eq!(heap.made() - made, 2, "{n}");
                let mut want: Vec<String> = (0..n).map(|i| i.to_string()).collect();
                for (v, from) in [(a, 0), (t, 1), (again, 1)] {
                    check(&heap, v);
                    assert_eq!(texts(&heap, v), want[from..]);
                    for i in [0, 1, 2, 31, n - 2] {
                        let there = get(&heap, v, i).map(|v| text(&heap, v));
                        assert_eq!(there.as_ref(), want[from..].get(i));
                    }
                }
                let r = root(&heap, a);
                for from in 0..3 {
                    for m in [1, 2, 33] {
                        let mut out = vec![Value::NIL; m];
                        let found = run(&heap, &r, from, &mut out);
                        assert_eq!(found, m.min(n - from));
                        let got: Vec<String> =
                            out[..found].iter().map(|v| text(&heap, *v)).collect();
                        assert_eq!(got, want[from..from + found]);
                    }
                }
                for other in [0, 1, n - 1] {
                    for differs in [false, true] {
                        let texts: Vec<Value> = (0..n)
                            .map(|i| match differs && i == other {
                                true => holder(&mut heap, "other"),
                                false => holder(&mut heap, &format!("{i}")),
                            })
                            .collect();
                        let fresh = from_values(&mut heap, &texts).expect("room");
                        let hash = crate::hash::hash;
                        assert_eq!(crate::eq::equal(&heap, a, fresh), !differs);
                        assert_eq!(crate::eq::equal(&heap, fresh, a), !differs);
                        assert_eq!(hash(&heap, a) == hash(&heap, fresh), !differs);
                        release(&mut heap, fresh).expect("live");
                    }
                }
                let kept = held.then(|| heap.own(a).expect("live"));
                let x = string(&mut heap, "x");
                let changed = match way {
                    0 => set(&mut heap, a, 0, x),
                    1 => set(&mut heap, a, n - 1, x),
                    2 => push(&mut heap, a, x, End::Front),
                    3 => push(&mut heap, a, x, End::Back),
                    4 => skip(&mut heap, a, 1),
                    5 => skip(&mut heap, a, 2),
                    6 => slice(&mut heap, a, 0, n - 1),
                    7 => {
                        heap.own(t).expect("live");
                        concat(&mut heap, a, t)
                    }
                    9 => {
                        heap.own(again).expect("live");
                        concat(&mut heap, again, a)
                    }
                    10 => {
                        let more: Vec<Value> = (0..n + 9).map(|_| x).collect();
                        for _ in 1..n + 9 {
                            heap.own(x).expect("live");
                        }
                        let longer = from_values(&mut heap, &more).expect("room");
                        concat(&mut heap, longer, a)
                    }
                    _ => uncons(&mut heap, a, false, true).map(|found| found.expect("one").1),
                }
                .expect("room");
                let before = want.clone();
                match way {
                    0 => want[0] = "x".to_string(),
                    1 => want[n - 1] = "x".to_string(),
                    2 => want.insert(0, "x".to_string()),
                    3 => want.push("x".to_string()),
                    4 | 8 => drop(want.remove(0)),
                    5 => drop(want.drain(..2)),
                    6 => drop(want.pop()),
                    7 => want.extend(before[1..].to_vec()),
                    9 => drop(want.splice(0..0, before[1..].to_vec())),
                    _ => drop(want.splice(0..0, vec!["x".to_string(); n + 9])),
                }
                if way > 3 && way != 10 {
                    release(&mut heap, x).expect("live");
                }
                check(&heap, changed);
                assert_eq!(texts(&heap, changed), want, "{n} {way} {held}");
                for v in [t, again] {
                    check(&heap, v);
                    assert_eq!(texts(&heap, v), before[1..], "{n} {way} {held}");
                    release(&mut heap, v).expect("live");
                }
                if let Some(kept) = kept {
                    check(&heap, kept);
                    assert_eq!(texts(&heap, kept), before, "{n} {way}");
                    assert_eq!(heap.audit([kept, changed].into_iter()), Ok(()));
                    release(&mut heap, kept).expect("live");
                }
                release(&mut heap, changed).expect("live");
                assert_eq!(heap.live(), 0, "{n} {way} {held}");
            }
        }
    }
}

/// A slice to the end of a tree that nothing else holds is the tree, with its start moved
/// up, unless that leaves 32 or fewer and gives up as many. What is then left is copied to
/// one cell, and so is what is left of a tree that something else holds.
#[test]
fn a_slice_off_the_front_of_a_tree_stays_the_tree() {
    for n in [33usize, 34, 40, 63, 64, 65, 66, 97, 100] {
        for from in 0..n {
            for held in [false, true] {
                let mut heap = Heap::default();
                let items: Vec<Value> =
                    (0..n).map(|i| string(&mut heap, &format!("{i}"))).collect();
                let a = from_values(&mut heap, &items).expect("room");
                if held {
                    heap.own(a).expect("live");
                }
                let r = root(&heap, a);
                let made = heap.made();
                let cut = slice(&mut heap, a, from, n).expect("room");
                let what = format!("{n} {from} {held}");
                check(&heap, cut);
                let left: Vec<String> = (from..n).map(|i| i.to_string()).collect();
                assert_eq!(texts(&heap, cut), left, "{what}");
                let kept = n - from;
                let in_tail = from >= n - r.tail.len;
                let tree = kept > B || (kept > from && !held && !in_tail);
                assert_eq!(!is_flat(&heap, cut), tree || from == 0, "{what}");
                if tree && !held {
                    assert_eq!(cut.bits(), a.bits(), "{what}");
                    assert_eq!(heap.made(), made, "{what}");
                }
                release(&mut heap, cut).expect("live");
                if held {
                    assert_eq!(texts(&heap, a).len(), n, "{what}");
                    check(&heap, a);
                    release(&mut heap, a).expect("live");
                }
                assert_eq!(heap.live(), 0, "{what}");
            }
        }
    }
}

/// An array that is pushed to and cut again by turns, at any ends of it, makes few cells for
/// each turn, whatever its length, and at 32 and 33 too.
#[test]
fn an_array_that_hovers_is_not_copied_each_time() {
    for n in 28..=70usize {
        for by in 1..=4usize {
            for built in [End::Front, End::Back] {
                for (put, cuts) in [
                    (End::Back, "slice"),
                    (End::Back, "walk"),
                    (End::Front, "walk"),
                ] {
                    let mut heap = Heap::default();
                    let mut a = Value::EMPTY_ARRAY;
                    let mut want = std::collections::VecDeque::new();
                    for i in 0..n as i64 {
                        a = push(&mut heap, a, small(i), built).expect("room");
                        match built {
                            End::Front => want.push_front(i),
                            End::Back => want.push_back(i),
                        }
                    }
                    let made = heap.made();
                    let turns = 400;
                    for t in 0..turns {
                        let items: Vec<i64> = (0..by as i64).map(|i| 1000 + t * 10 + i).collect();
                        let more: Vec<Value> = items.iter().map(|i| small(*i)).collect();
                        a = extend(&mut heap, a, &more, put).expect("room");
                        match put {
                            End::Back => want.extend(items),
                            End::Front => items.into_iter().rev().for_each(|i| want.push_front(i)),
                        }
                        a = match cuts {
                            "slice" => slice(&mut heap, a, by, n + by),
                            _ => skip(&mut heap, a, by),
                        }
                        .expect("room");
                        want.drain(..by);
                        check(&heap, a);
                        assert!(ints(&heap, a).iter().eq(&want));
                    }
                    let each = (heap.made() - made) as f64 / turns as f64;
                    let what = format!("{n} {by} {built:?} {put:?} {cuts}: {each}");
                    assert!(each <= 3.2, "{what}");
                    release(&mut heap, a).expect("live");
                    assert_eq!(heap.live(), 0);
                }
            }
        }
    }
}

/// A walk of a tree that nothing else holds moves its leaves up to be its head one by one, and
/// then what is in its tail, in the root that it has, with no cell made.
#[test]
fn a_walk_past_the_head_changes_the_root_in_place() {
    for head in [false, true] {
        for n in [65usize, 100, 1100, 1200] {
            let mut heap = Heap::default();
            let items: Vec<Value> = (0..n).map(|i| string(&mut heap, &format!("{i}"))).collect();
            let a = from_values(&mut heap, &items).expect("room");
            let leaves = parts_of(&heap, a);
            let made = heap.made();
            let mut cur = a;
            // The tail is the array in the end, when no leaf is left.
            let in_tail = root(&heap, a).tail.len;
            for i in 0..n - in_tail - 1 {
                let found = uncons(&mut heap, cur, head, true);
                let (first, left) = found.expect("room").expect("one");
                assert_eq!(left.bits(), a.bits(), "{n} {i}");
                assert_eq!(head, first.as_cell().is_some());
                release(&mut heap, first).expect("live");
                check(&heap, left);
                let at = root(&heap, left).head.items.as_cell();
                assert!(at.is_none_or(|at| leaves.contains(&at)), "{n} {i}");
                assert_eq!(texts(&heap, left).len(), n - i - 1);
                cur = left;
            }
            assert_eq!(heap.made(), made, "{n}");
            release(&mut heap, cur).expect("live");
            assert_eq!(heap.live(), 0);
        }
    }
}

/// An array whose first element is in its root, and that nothing else holds any more, gives it
/// up there, whoever has its head. Those behind it go from its head when nothing else has that.
#[test]
fn a_first_element_in_the_root_is_taken_with_no_cell() {
    for n in [33usize, 40, 70, 1100] {
        for shared in [false, true] {
            for way in 0..6 {
                let mut heap = Heap::default();
                let items: Vec<Value> =
                    (0..n).map(|i| holder(&mut heap, &format!("{i}"))).collect();
                let a = from_values(&mut heap, &items).expect("room");
                let t = without(&mut heap, a, 1).expect("room");
                assert!(root(&heap, a).front().is_some());
                if !shared {
                    release(&mut heap, t).expect("live");
                }
                let made = heap.made();
                let gone = match way {
                    0 | 1 => 1,
                    n => n - 1,
                };
                let left = match way {
                    0 | 1 => {
                        let found = uncons(&mut heap, a, way == 0, true).expect("room");
                        let (first, left) = found.expect("an element");
                        assert_eq!(way == 0, first.as_cell().is_some());
                        release(&mut heap, first).expect("live");
                        left
                    }
                    _ => skip(&mut heap, a, gone).expect("room"),
                };
                let what = format!("{n} {shared} {way}");
                // The rest of a head that another array has is copied.
                let copied = usize::from(shared && gone > 1);
                assert_eq!(heap.made() - made, copied, "{what}");
                assert_eq!(left.bits(), a.bits(), "{what}");
                check(&heap, left);
                let want: Vec<String> = (0..n).map(|i| i.to_string()).collect();
                assert_eq!(texts(&heap, left), want[gone..], "{what}");
                if shared {
                    check(&heap, t);
                    assert_eq!(texts(&heap, t), want[1..], "{what}");
                    release(&mut heap, t).expect("live");
                }
                assert_eq!(heap.audit([left].into_iter()), Ok(()), "{what}");
                release(&mut heap, left).expect("live");
                assert_eq!(heap.live(), 0, "{what}");
            }
        }
    }
}

/// A push at the back of an array that nothing else holds is in place wherever the array starts
/// and whoever has its head.
#[test]
fn a_push_at_the_back_does_not_look_at_where_the_array_starts() {
    for cells in [false, true] {
        for end in [End::Front, End::Back] {
            let mut heap = Heap::default();
            let items: Vec<Value> = (0..40)
                .map(|i| match cells {
                    true => holder(&mut heap, &format!("{i}")),
                    false => small(i),
                })
                .collect();
            let mut a = from_values(&mut heap, &items[..32]).expect("room");
            for x in &items[32..] {
                a = push(&mut heap, a, *x, End::Back).expect("room");
            }
            let left = without(&mut heap, a, 1).expect("room");
            // The one with something in front, or the one that has lost something.
            let (a, other) = if cells { (a, left) } else { (left, a) };
            // That gets a tail of its own.
            let other = push(&mut heap, other, small(0), End::Back).expect("room");
            let start = if cells { FRONT } else { 1 };
            assert_eq!(root(&heap, a).start, start);
            let made = heap.made();
            let mut now = a;
            for i in 0..4 {
                now = extend(&mut heap, now, &[small(i)], end).expect("room");
                check(&heap, now);
            }
            assert_eq!(now.bits(), a.bits());
            assert_eq!(heap.made() == made, end == End::Back, "{cells}");
            if end == End::Back {
                assert_eq!(root(&heap, now).start, start);
            }
            for v in [now, other] {
                release(&mut heap, v).expect("live");
            }
            assert_eq!(heap.live(), 0);
        }
    }
}

/// An array that is held, and whose elements are no cells, or short ones that hold nothing or no
/// more than such, loses the first of its head by having its root say so, whatever else has
/// that. A long string, what holds one, a tuple of tuples, a handle and what holds one are none
/// of these.
#[test]
fn what_is_plain_is_lost_where_it_is() {
    let long = "x".repeat(4000);
    for kind in 0..15 {
        let mut heap = Heap::default();
        let tuple = |heap: &mut Heap, held| Value::cell(heap.tuple(&[held]).expect("room"));
        let value = |heap: &mut Heap, i: usize| match kind {
            0 => small(i as i64),
            1 => Value::NIL,
            2 => string(heap, &format!("{i}")),
            3 => {
                let big = num_bigint::BigInt::from(1) << 90;
                Value::cell(heap.big_int(&big).expect("room"))
            }
            4 => Value::cell(heap.make(Kind::Binary, &[7; 3]).expect("room")),
            5 => {
                let held = string(heap, "held");
                tuple(heap, held)
            }
            6 => {
                let held = string(heap, "held");
                from_values(heap, &[small(1), held, Value::NIL]).expect("room")
            }
            7 => string(heap, &long),
            8 => holder(heap, "held"),
            9 => Value::cell(heap.make(Kind::Handle, &[i as u64]).expect("room")),
            10 => {
                let held = heap.make(Kind::Handle, &[i as u64]).expect("room");
                tuple(heap, Value::cell(held))
            }
            11 => {
                let held = Value::cell(heap.make(Kind::Handle, &[i as u64]).expect("room"));
                from_values(heap, &[small(1), held]).expect("room")
            }
            12 => {
                let held = string(heap, &long);
                tuple(heap, held)
            }
            13 => range(heap, 1, 5).expect("room"),
            _ => Value::cell(crate::map::empty(heap).expect("room")),
        };
        let items: Vec<Value> = (0..40).map(|i| value(&mut heap, i)).collect();
        let each = heap.live() / 40;
        let a = from_values(&mut heap, &items).expect("room");
        let (made, live) = (heap.made(), heap.live());
        let left = without(&mut heap, a, 3).expect("room");
        check(&heap, left);
        let stays = kind < 7 || kind == 13;
        let (start, cells) = if stays { (3, 1) } else { (0, 2) };
        assert_eq!(root(&heap, left).start, start, "{kind}");
        assert_eq!(heap.made() - made, cells, "{kind}");
        let same = |got: Vec<Value>, want: &[Value]| {
            got.len() == want.len() && got.iter().zip(want).all(|(a, b)| a.bits() == b.bits())
        };
        assert!(same(elements(&heap, left), &items[3..]), "{kind}");
        assert_eq!(heap.audit([a, left].into_iter()), Ok(()), "{kind}");
        // When the other has gone, a step of a walk gives up all that is lost.
        release(&mut heap, a).expect("live");
        let left = skip(&mut heap, left, 3).expect("room");
        check(&heap, left);
        assert_eq!(root(&heap, left).start, 0, "{kind}");
        assert!(same(elements(&heap, left), &items[6..]), "{kind}");
        assert_eq!(heap.live(), live - 6 * each, "{kind}");
        release(&mut heap, left).expect("live");
        assert_eq!(heap.live(), 0, "{kind}");
    }
}

/// Where one of those that go is not plain the rest of the head is copied, and nothing that is
/// given up is kept.
#[test]
fn what_is_lost_stays_in_a_head_only_when_it_is_plain() {
    for n in [33usize, 70, 1100] {
        // Which of the first ten are cells.
        for cells in [0u32, 1, 0b10, 0b1000, 0b11_0000, 0b10_0000_0000] {
            for gone in 1..10 {
                for (array_held, head_held) in
                    [(false, false), (true, false), (false, true), (true, true)]
                {
                    let mut heap = Heap::default();
                    let items: Vec<Value> = (0..n)
                        .map(|i| match i < 10 && cells >> i & 1 == 1 {
                            true => {
                                Value::cell(heap.make(Kind::Handle, &[i as u64]).expect("room"))
                            }
                            false => small(i as i64),
                        })
                        .collect();
                    let a = from_values(&mut heap, &items).expect("room");
                    // One with another tail has its head too.
                    let twin = head_held
                        .then(|| pushed_on(&mut heap, a, small(0), End::Back).expect("room"));
                    let kept = array_held.then(|| heap.own(a).expect("live"));
                    let made = heap.made();
                    let left = skip(&mut heap, a, gone).expect("room");
                    let what = format!("{n} {cells:b} {gone} {array_held} {head_held}");
                    check(&heap, left);
                    let r = root(&heap, left);
                    let lost = cells & ((1 << gone) - 1) != 0;
                    let held = array_held || head_held;
                    let first = lost && gone == 1 && !head_held;
                    let copied = held && lost && !first;
                    let start = if held && !lost { gone } else { 0 };
                    assert_eq!(r.start, start, "{what}");
                    let cells_made = usize::from(array_held) + usize::from(copied);
                    assert_eq!(heap.made() - made, cells_made, "{what}");
                    let got = elements(&heap, left);
                    let want = &items[gone..];
                    assert!(got.iter().zip(want).all(|(a, b)| a.bits() == b.bits()));
                    assert_eq!(got.len(), want.len(), "{what}");
                    for v in [kept, twin].into_iter().flatten() {
                        check(&heap, v);
                        assert_eq!(get(&heap, v, 0).map(Value::bits), Some(items[0].bits()));
                        release(&mut heap, v).expect("live");
                    }
                    // Every cell that it has lost is given up now.
                    let mut released = heap.take_released();
                    released.sort_unstable();
                    let want: Vec<u64> = (0..gone as u64).filter(|i| cells >> i & 1 == 1).collect();
                    assert_eq!(released, want, "{what}");
                    assert_eq!(heap.audit([left].into_iter()), Ok(()), "{what}");
                    release(&mut heap, left).expect("live");
                    assert_eq!(heap.live(), 0, "{what}");
                }
            }
        }
    }
}

/// A tree that loses none of its elements is the array it was, and needs no room.
#[test]
fn a_cut_of_nothing_makes_no_cell() {
    let mut heap = Heap::default();
    let items: Vec<Value> = (0..100).map(small).collect();
    let a = from_values(&mut heap, &items).expect("room");
    heap.limit_to(heap.words_used());
    let made = heap.made();
    let parts = root(&heap, a).parts().map(Value::bits);
    for held in [false, true] {
        if held {
            heap.own(a).expect("live");
        }
        for whole in [skip(&mut heap, a, 0), slice(&mut heap, a, 0, 100)] {
            assert_eq!(word(whole), Ok(a.bits()), "{held}");
            assert_eq!(heap.made(), made, "{held}");
        }
        assert_eq!(root(&heap, a).parts().map(Value::bits), parts);
        assert_eq!(heap.unique(a.as_cell().expect("a cell")), Ok(!held));
    }
}

/// Everything that is handed an array gives up the caller's reference, so on one that
/// was already freed it stops instead of freeing it again.
#[test]
fn whatever_is_done_to_a_freed_array_is_a_dead_cell() {
    let mut heap = Heap::default();
    let live = array_of(&mut heap, 0..3);
    let cells = heap.live();
    let x = Value::int(7).expect("small");
    for n in [1, 5, 32, 40] {
        let a = array_of(&mut heap, 0..n);
        let freed = stored(&heap, a);
        release(&mut heap, a).expect("a live cell");
        assert_eq!(heap.live(), cells);
        let dead = Err(Fault::DeadCell);
        for end in [End::Front, End::Back] {
            assert_eq!(word(super::push(&mut heap, freed, x, end)), dead, "{n}");
            // So they do with more items than a leaf has room for, which are not
            // tried for a leaf.
            for k in [0, 1, 2, B - 1, B, B + 8] {
                let run = super::extend(&mut heap, freed, &vec![x; k], end);
                assert_eq!(word(run), dead, "{n} {k}");
            }
        }
        assert_eq!(word(super::set(&mut heap, freed, 0, x)), dead, "{n}");
        for from in [0, 1] {
            assert_eq!(word(super::skip(&mut heap, freed, from + 1)), dead);
            assert_eq!(word(super::slice(&mut heap, freed, from, 1)), dead);
        }
        let kept = stored(&heap, live);
        for (l, r) in [(freed, kept), (kept, freed), (freed, freed)] {
            heap.own(live).expect("live");
            let joined = join(&mut heap, l, r).map(|_| 0);
            assert_eq!(joined, dead);
        }
        let first = match freed {
            Stored::Flat(flat) => flat::uncons(&mut heap, flat, true),
            Stored::Tree(cell) => {
                assert!(super::get(&heap, cell, 0).is_none());
                Ok(None)
            }
            Stored::Empty => Ok(None),
        };
        assert!(matches!(first, Ok(None)), "{n}");
        if n > 32 {
            let cell = a.as_cell().expect("a tree");
            assert!(super::take_first(&mut heap, cell).is_none());
        }
        let cell = a.as_cell().expect("a cell");
        assert_eq!(heap.live(), cells, "{n}");
        assert!(flat::take_first(&mut heap, cell).is_none());
        let wrote = flat::set_alone(&mut heap, cell, 0, x);
        assert!(matches!(wrote, Near::Other));
        assert!(!flat::push_alone(&mut heap, cell, x, End::Back));
    }
}

/// With no room for the tail, the first element is left with the references it had, and
/// the array is still there, unless it was taken. One that is taken and nothing else holds
/// needs room only where its head has run out, and is given up where there is none.
#[test]
fn a_first_element_with_no_room_for_the_others_is_left_as_it_was() {
    let names: Vec<String> = (0..100).map(|i| format!("s{i}")).collect();
    for n in [3, 32, 40, 100] {
        let whole = |heap: &mut Heap| {
            let all: Vec<Value> = names[..n].iter().map(|s| string(heap, s)).collect();
            (all[0], from_values(heap, &all).expect("room"))
        };
        if n > 32 {
            let mut heap = Heap::default();
            let (_, mut a) = whole(&mut heap);
            a = skip(&mut heap, a, 32).expect("room");
            heap.limit_to(heap.words_used());
            let got = uncons(&mut heap, a, true, true);
            assert_eq!(matches!(got, Err(Fault::Full)), n == 100, "{n}");
            assert_eq!(heap.live() == 0, n == 100, "{n}");
        }
        for taken in [false, true] {
            let mut heap = Heap::default();
            let (first, a) = whole(&mut heap);
            heap.limit_to(heap.words_used());
            let got = uncons(&mut heap, a, true, taken);
            if taken {
                assert!(got.is_ok(), "{n}");
                continue;
            }
            assert!(matches!(got, Err(Fault::Full)));
            let first = first.as_cell().expect("a string");
            assert_eq!(heap.unique(first), Ok(true));
            assert_eq!(heap.unique(a.as_cell().expect("a cell")), Ok(true));
            assert_eq!(texts(&heap, a), names[..n]);
        }
    }
}

/// A reader of a cell that is no array finds no element, however the cell got there.
#[test]
fn what_is_no_tree_has_no_elements() {
    let mut heap = Heap::default();
    let freed = array_of(&mut heap, 0..40);
    release(&mut heap, freed).expect("a live cell");
    let other = heap.make(Kind::Binary, &[9; 12]).expect("room");
    let range = range(&mut heap, 3, 9).expect("room");
    let branch = Value::cell(branch(&mut heap).expect("room"));
    for v in [freed, Value::cell(other), range, branch] {
        let cell = v.as_cell().expect("a cell");
        assert!(elements_of_tree(&heap, cell).is_empty());
        assert_eq!(Seq::Tree(cell).len(&heap), 0);
        assert!(super::get(&heap, cell, 0).is_none());
        let found = seq(&heap, v);
        match v.bits() == range.bits() {
            true => assert!(matches!(found, Some(Seq::Range { start: 3, end: 9 }))),
            false => assert!(found.is_none()),
        }
    }
    for v in [Value::NIL, Value::UNSET, small(3), Value::bool(true)] {
        assert!(seq(&heap, v).is_none(), "{v:?}");
    }
}

/// A freed array cannot be shared back to life, so a push does not take it for one it may
/// change in place.
#[test]
fn a_push_onto_a_freed_array_is_a_dead_cell_after_a_share_is_refused() {
    for n in [3, 40] {
        let mut heap = Heap::default();
        let a = array_of(&mut heap, 0..n);
        let freed = stored(&heap, a);
        release(&mut heap, a).expect("a live cell");
        assert_eq!(heap.own(a).map(Value::bits), Err(DeadCell));
        let x = Value::int(7).expect("small");
        let pushed = super::push(&mut heap, freed, x, End::Back);
        assert_eq!(word(pushed), Err(Fault::DeadCell));
        assert_eq!(heap.live(), 0);
    }
}

pub(super) fn small(n: i64) -> Value {
    Value::int(n).expect("small")
}

/// The text of a string, or of the one that a [`holder`] has.
pub(super) fn text(heap: &Heap, v: Value) -> String {
    let mut out = Vec::new();
    match v.as_cell() {
        Some(c) if heap.is(c, Kind::Tuple) => {
            return heap.values(c).map(|v| text(heap, v)).collect();
        }
        Some(c) => {
            heap.read_string(c, &mut out);
        }
        None => {}
    }
    String::from_utf8(out).expect("UTF-8")
}

pub(super) fn texts(heap: &Heap, a: Value) -> Vec<String> {
    elements(heap, a)
        .into_iter()
        .map(|v| text(heap, v))
        .collect()
}

/// A tuple of a tuple of a string, which is a cell that may hold a handle for all that is asked
/// of it.
fn holder(heap: &mut Heap, s: &str) -> Value {
    let mut v = string(heap, s);
    for _ in 0..2 {
        v = Value::cell(heap.tuple(&[v]).expect("room"));
    }
    v
}

pub(super) fn string(heap: &mut Heap, s: &str) -> Value {
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
            a = push(&mut heap, a, small(-100 - k), End::Front).expect("room");
        }
        let mut want = ints(&heap, a);
        let made = heap.made();
        let len = want.len();
        for i in [0, 1, 2, 3, 4, len / 2, len - 2, len - 1] {
            if i < len {
                let wrote = set(&mut heap, a, i, small(7000 + i as i64));
                assert_eq!(word(wrote), Ok(a.bits()), "size {n} at {i}");
                want[i] = 7000 + i as i64;
            }
        }
        assert_eq!(heap.made(), made, "size {n}");
        check(&heap, a);
        assert_eq!(ints(&heap, a), want, "size {n}");
        release(&mut heap, a).expect("a live cell");
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
        (1, 0, 1),
        (32, 31, 1),
        (33, 0, 2),
        (33, 32, 2),
        (1025, 500, 3),
        (1025, 1024, 2),
        (40_000, 20_000, 5),
        (40_000, 39_999, 2),
    ] {
        let a = array_of(&mut heap, 0..n);
        heap.own(a).expect("a live cell");
        let made = heap.made();
        let b = set(&mut heap, a, i as usize, small(-1)).expect("room");
        assert_ne!(b.bits(), a.bits(), "size {n} at {i}");
        assert_eq!(heap.made() - made, path, "size {n} at {i}");
        check(&heap, a);
        check(&heap, b);
        assert_eq!(ints(&heap, a), (0..n).collect::<Vec<_>>(), "size {n}");
        let mut want: Vec<i64> = (0..n).collect();
        want[i as usize] = -1;
        assert_eq!(ints(&heap, b), want, "size {n} at {i}");
        // The copy is its holder's alone, so a second write is in place.
        let made = heap.made();
        let again = set(&mut heap, b, i as usize, small(-2));
        assert_eq!(word(again), Ok(b.bits()));
        assert_eq!(heap.made(), made);
        release(&mut heap, a).expect("a live cell");
        release(&mut heap, b).expect("a live cell");
        assert_eq!(heap.live(), 0, "size {n} at {i}");
    }
}

/// An array can have a root of its own over nodes another array shares,
/// as after a push. A write copies just the shared nodes on its way.
#[test]
fn a_write_under_a_root_of_its_own_copies_only_what_is_shared() {
    let mut heap = Heap::default();
    let a = array_of(&mut heap, 0..2000);
    let b = pushed_on(&mut heap, a, small(2000), End::Back).expect("room");
    let made = heap.made();
    let c = set(&mut heap, b, 100, small(-1)).expect("room");
    assert_eq!(c.bits(), b.bits(), "the root was this array's alone");
    // The two branches above the slot and its leaf.
    assert_eq!(heap.made() - made, 3);
    assert_eq!(ints(&heap, a), (0..2000).collect::<Vec<_>>());
    let mut want: Vec<i64> = (0..=2000).collect();
    want[100] = -1;
    assert_eq!(ints(&heap, c), want);
    check(&heap, a);
    check(&heap, c);
    release(&mut heap, a).expect("a live cell");
    release(&mut heap, c).expect("a live cell");
    assert_eq!(heap.live(), 0);
}

/// The short way to write does so only when nothing else holds the root, a branch or the leaf
/// on the way to the slot, and changes nothing otherwise.
#[test]
fn a_write_in_place_asks_every_node_on_the_way() {
    for n in [40i64, 2000, 40_000] {
        let all: Vec<i64> = (0..n).collect();
        for i in [0usize, 5, 35, n as usize - 1] {
            // Nothing is held, the array is, and each part of it is but what is above.
            for held in 0..6 {
                let mut heap = Heap::default();
                let a = array_of(&mut heap, 0..n);
                let cell = a.as_cell().expect("a tree");
                let r = root(&heap, a);
                let way = match r.place(i).expect("an element") {
                    Place::Front(_) => panic!("a front"),
                    Place::Edge(items, _) => vec![items],
                    Place::Leaf(j) => {
                        let (mut way, mut shift) = (vec![r.tree], r.shift);
                        while shift > 0 {
                            let node = way[way.len() - 1].as_cell().expect("a branch");
                            let child = heap.field(node, (r.first + j) >> shift & (B - 1));
                            way.push(child.expect("a child"));
                            shift -= BITS;
                        }
                        way
                    }
                };
                let keeps = match held {
                    0 => None,
                    1 => Some(a),
                    _ => way.get(held - 2).copied(),
                };
                if let Some(keeps) = keeps {
                    heap.own(keeps).expect("live");
                }
                let wrote = set_alone(&mut heap, cell, i as i64, small(-1)).expect("live");
                let mut want = all.clone();
                match keeps {
                    Some(_) => assert!(matches!(wrote, Wrote::Other), "{n} {i} {held}"),
                    None => {
                        assert!(matches!(wrote, Wrote::Done), "{n} {i} {held}");
                        want[i] = -1;
                    }
                }
                assert_eq!(ints(&heap, a), want, "{n} {i} {held}");
                release(&mut heap, keeps.unwrap_or(Value::NIL)).expect("live");
                release(&mut heap, a).expect("live");
                assert_eq!(heap.live(), 0);
            }
        }
    }
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
                heap.own(a).expect("a live cell");
            }
            let live = heap.live();
            let made = heap.made();
            for i in [n as usize, n as usize + 1, usize::MAX] {
                let x = string(&mut heap, "never stored");
                let same = set(&mut heap, a, i, x);
                assert_eq!(word(same), Ok(a.bits()), "size {n} at {i}");
            }
            assert_eq!(heap.live(), live, "size {n}");
            assert_eq!(heap.made() - made, 3, "only the three strings");
            assert_eq!(ints(&heap, a), (0..n).collect::<Vec<_>>());
        }
        release(&mut heap, a).expect("a live cell");
        release(&mut heap, a).expect("a live cell");
        assert_eq!(heap.live(), 0);
    }
}

/// An array walked part way may still have what it has lost in its head.
/// A write counts from the array's own front, in place when the head is
/// its alone and in a copy when the array it was cut from still has
/// it.
#[test]
fn a_write_to_an_array_walked_part_way_counts_from_its_own_front() {
    let mut heap = Heap::default();
    for k in [1, 5, 31, 32, 33, 70] {
        let whole = array_of(&mut heap, 0..200);
        let rest = without(&mut heap, whole, k).expect("room");
        let wrote = set(&mut heap, rest, 0, small(-1)).expect("room");
        let wrote = set(&mut heap, wrote, 3, small(-3)).expect("room");
        check(&heap, wrote);
        let mut want: Vec<i64> = (k as i64..200).collect();
        want[0] = -1;
        want[3] = -3;
        assert_eq!(ints(&heap, wrote), want, "skip {k}");
        assert_eq!(ints(&heap, whole), (0..200).collect::<Vec<_>>());
        release(&mut heap, whole).expect("a live cell");
        let made = heap.made();
        let alone = skip(&mut heap, wrote, 2).expect("room");
        let alone = set(&mut heap, alone, 0, small(-9)).expect("room");
        want.drain(..2);
        want[0] = -9;
        assert_eq!(ints(&heap, alone), want, "skip {k}, then 2 more");
        check(&heap, alone);
        assert!(heap.made() - made <= 4, "a walk and a write copied");
        release(&mut heap, alone).expect("a live cell");
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
    heap.own(a).expect("a live cell");
    let y = string(&mut heap, "newer");
    let b = set(&mut heap, a, 7, y).expect("room");
    assert_eq!(text(&heap, get(&heap, a, 7).expect("an element")), "new");
    assert_eq!(text(&heap, get(&heap, b, 7).expect("an element")), "newer");
    release(&mut heap, a).expect("a live cell");
    assert_eq!(texts(&heap, b)[6..9], ["s6", "newer", "s8"]);
    release(&mut heap, b).expect("a live cell");
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
        let mut kept: Vec<(Value, Vec<String>)> = Vec::new();
        for step in 0..6000 {
            match next(12) {
                0 | 1 => {
                    let name = format!("b{step}");
                    let x = string(&mut heap, &name);
                    a = push(&mut heap, a, x, End::Back).expect("room");
                    want.push(name);
                }
                2 => {
                    let name = format!("f{step}");
                    let x = string(&mut heap, &name);
                    a = push(&mut heap, a, x, End::Front).expect("room");
                    want.insert(0, name);
                }
                3 if !want.is_empty() => {
                    a = skip(&mut heap, a, 1).expect("room");
                    want.remove(0);
                }
                4 if next(8) == 0 => {
                    let more: Vec<String> = (0..next(90)).map(|k| format!("j{step}.{k}")).collect();
                    let items: Vec<Value> = more.iter().map(|s| string(&mut heap, s)).collect();
                    let piece = from_values(&mut heap, &items).expect("room");
                    let joined = joined(&mut heap, a, piece).expect("room");
                    release(&mut heap, a).expect("a live cell");
                    release(&mut heap, piece).expect("a live cell");
                    a = joined;
                    want.extend(more);
                }
                4 | 7 if next(8) == 0 => {
                    let names: Vec<String> =
                        (0..next(90)).map(|k| format!("r{step}.{k}")).collect();
                    let run: Vec<Value> = names.iter().map(|s| string(&mut heap, s)).collect();
                    let end = if next(2) == 0 { End::Front } else { End::Back };
                    a = extend(&mut heap, a, &run, end).expect("room");
                    let at = if end == End::Front { 0 } else { want.len() };
                    want.splice(at..at, names);
                }
                5 if next(20) == 0 => {
                    heap.own(a).expect("a live cell");
                    kept.push((a, want.clone()));
                }
                6 if next(20) == 0 && !kept.is_empty() => {
                    let (k, items) = kept.swap_remove(next(kept.len()));
                    assert_eq!(texts(&heap, k), items, "seed {seed} step {step}");
                    release(&mut heap, k).expect("a live cell");
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
                            release(&mut heap, left).expect("a live cell");
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
                    let mut out = [Value::NIL; 4];
                    let out = &mut out[..n.min(4)];
                    let fitting = fits(want.len() as u64, length);
                    if let Stored::Flat(flat) = stored(&heap, a) {
                        if fitting {
                            flat.read_run(&heap, from, out);
                        }
                    } else {
                        let r = root(&heap, a);
                        assert_eq!(fits(r.len() as u64, length), fitting);
                        if fitting {
                            let first = part(&heap, &r, from);
                            let first = first.map_or(0, |p| from + p.left());
                            match from + out.len() <= first {
                                true => near += 1,
                                false => apart += 1,
                            }
                            assert_eq!(run(&heap, &r, from, out), out.len());
                        }
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
        release(&mut heap, a).expect("a live cell");
        for (k, _) in kept {
            release(&mut heap, k).expect("a live cell");
        }
        assert_eq!(heap.live(), 0, "seed {seed} lost track of a reference");
        assert!(near > 20 && apart > 5, "seed {seed}: {near} {apart}");
    }
}

/// Arrays of `n` numbers from 0 on, put together in every way.
fn grown(heap: &mut Heap, n: i64) -> Vec<Value> {
    let whole = array_of(heap, 0..n);
    let (mut back, mut front) = (array_of(heap, 0..0), array_of(heap, 0..0));
    for i in 0..n {
        back = push(heap, back, small(i), End::Back).expect("room");
        front = push(heap, front, small(n - 1 - i), End::Front).expect("room");
    }
    // One element is left in what was its first leaf.
    let more = array_of(heap, -31..n);
    let cut = skip(heap, more, 31).expect("room");
    let mut pushed = array_of(heap, -31..n);
    pushed = push(heap, pushed, small(-32), End::Front).expect("room");
    pushed = skip(heap, pushed, 32).expect("room");
    let (a, b) = (array_of(heap, 0..n / 3), array_of(heap, n / 3..n));
    let joined = joined(heap, a, b).expect("room");
    for part in [a, b] {
        release(heap, part).expect("a live cell");
    }
    vec![whole, back, front, cut, pushed, joined]
}

/// A run of elements is found wherever it starts and however many leaves it is over, and
/// as far as the array goes.
#[test]
fn a_run_of_elements_is_found_over_any_leaves() {
    let mut heap = Heap::default();
    for n in [1, 2, 5, 31, 32, 33, 34, 65, 100, 1100] {
        for a in grown(&mut heap, n) {
            check(&heap, a);
            let n = usize::try_from(n).expect("small");
            if is_flat(&heap, a) {
                for from in 0..n {
                    let mut out = vec![Value::NIL; (n - from).min(4)];
                    let Stored::Flat(flat) = stored(&heap, a) else {
                        panic!("no short array");
                    };
                    flat.read_run(&heap, from, &mut out);
                    let got: Vec<_> = out.iter().map(|v| int(*v)).collect();
                    let want: Vec<_> = (from..n).take(4).map(|i| Some(i as i64)).collect();
                    assert_eq!(got, want, "{n} {from}");
                }
                release(&mut heap, a).expect("a live cell");
                continue;
            }
            let r = root(&heap, a);
            for from in (0..n).filter(|i| *i < 70 || *i > n - 40 || *i % 97 == 0) {
                for m in [1, 2, 4, 8, 16, 40] {
                    let mut out = vec![Value::NIL; m];
                    let found = run(&heap, &r, from, &mut out);
                    assert_eq!(found, m.min(n - from));
                    let want: Vec<_> = (from..from + found).map(|i| Some(i as i64)).collect();
                    let got: Vec<_> = out.iter().take(found).map(|v| int(*v)).collect();
                    assert_eq!(got, want, "{n} {from} {m}");
                }
            }
            release(&mut heap, a).expect("a live cell");
        }
    }
    assert_eq!(heap.live(), 0);
}

/// A run of a tree that has fewer elements than its root says ends where they do.
#[test]
fn a_run_ends_where_the_elements_do() {
    let mut heap = Heap::default();
    for n in [33, 40] {
        let a = array_of(&mut heap, 0..n);
        let mut r = root(&heap, a);
        r.tail.len += 5;
        let mut out = [Value::NIL; 6];
        let from = usize::try_from(n).expect("small") - 2;
        assert_eq!(run(&heap, &r, from, &mut out), 2);
        assert_eq!(run(&heap, &r, from + 2, &mut out), 0);
        assert_eq!(run(&heap, &r, usize::MAX, &mut out), 0);
    }
}

/// A node freed behind the array's back stops a write to it.
#[test]
fn a_write_through_a_freed_node_is_a_dead_cell() {
    let mut heap = Heap::default();
    let b = array_of(&mut heap, 0..35);
    let tail = root(&heap, b).tail.items.as_cell().expect("a tail leaf");
    heap.release(tail).expect("a live cell");
    assert_eq!(word(set(&mut heap, b, 33, small(7))), Err(Fault::DeadCell));

    // A share does not bring a freed leaf back as one to copy.
    let c = array_of(&mut heap, 0..35);
    let tail = root(&heap, c).tail.items.as_cell().expect("a tail leaf");
    heap.release(tail).expect("a live cell");
    assert_eq!(heap.share(tail), Err(DeadCell));
    assert_eq!(word(set(&mut heap, c, 33, small(7))), Err(Fault::DeadCell));
}

/// A push that cannot change the array in place builds a new one and
/// then gives the old one up. When something freed the tail's leaf behind
/// the array's back, the push stops there.
#[test]
fn a_push_onto_an_array_with_a_freed_leaf_is_a_dead_cell() {
    let mut heap = Heap::default();
    let a = array_of(&mut heap, 0..35);
    let tail = root(&heap, a).tail.items.as_cell().expect("a tail leaf");
    heap.release(tail).expect("a live cell");
    let x = Value::int(7).expect("small");
    assert_eq!(word(push(&mut heap, a, x, End::Back)), Err(Fault::DeadCell));
}

/// An array that `grow` made is walked to its end by its first element and the others, by
/// a walk that gives each up and by one that keeps the whole. Gives how many cells the
/// walk made.
fn walked(n: i64, grow: impl Fn(&mut Heap, i64) -> Value, kept: bool) -> usize {
    let mut heap = Heap::default();
    let whole = grow(&mut heap, n);
    if kept {
        heap.own(whole).expect("a live cell");
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
    release(&mut heap, a).expect("a live cell");
    if kept {
        assert_eq!(ints(&heap, whole), (0..n).collect::<Vec<_>>());
        release(&mut heap, whole).expect("a live cell");
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
                a = push(heap, a, small(x), end).expect("room");
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

/// A root's cell that says it has no elements, which nothing makes.
fn a_root_of_none(heap: &mut Heap) -> Cell {
    let nil = Value::NIL.bits();
    let words = [nil, nil, nil, nil, 0, 0, 0];
    heap.make(Kind::ArrayRoot, &words).expect("room")
}

/// Whatever gives an array of no elements gives the one empty array and makes no cell,
/// and gives up what it was handed.
#[test]
fn an_array_of_no_elements_is_never_a_cell() {
    let empty = Ok(Value::EMPTY_ARRAY.bits());
    let mut heap = Heap::default();
    assert_eq!(word(from_values(&mut heap, &[])), empty);
    for (start, end) in [(0, 0), (5, 5), (9, 2), (i64::MAX, i64::MIN)] {
        assert_eq!(word(range(&mut heap, start, end)), empty);
    }
    for (start, end) in [(3, 6), (i64::MAX - 3, i64::MAX), (i64::MIN, i64::MIN + 3)] {
        assert_eq!(range_len(start, end), 3);
        for at in 0..=3 {
            assert_eq!(word(slice_of_range(&mut heap, start, at, at)), empty);
        }
    }
    for x in 0..3 {
        assert_eq!(word(super::slice(&mut heap, Stored::Empty, x, x)), empty);
        assert_eq!(word(super::skip(&mut heap, Stored::Empty, x)), empty);
    }
    let joined = concat(&mut heap, Value::EMPTY_ARRAY, Value::EMPTY_ARRAY);
    assert_eq!(word(joined), empty);
    let none = flat::new_flat(&mut heap, &[]).map(Stored::value);
    assert_eq!(word(none), empty);
    assert_eq!(word(close(&mut heap, None, Root::EMPTY)), empty);
    for end in [End::Front, End::Back] {
        assert_eq!(
            word(super::extend(&mut heap, Stored::Empty, &[], end)),
            empty
        );
    }
    assert_eq!(heap.made(), 0);
    for n in [1, 2, 31, 32, 33, 34, 65, 100, 1100] {
        let mut heap = Heap::default();
        for a in grown(&mut heap, n) {
            let len = usize::try_from(n).expect("small");
            let cell = a.as_cell().expect("a cell");
            let made = heap.made();
            // All of it is it, and is shared.
            let whole = cut(&mut heap, a, 0, len).expect("room");
            assert_eq!(whole.bits(), a.bits());
            release(&mut heap, whole).expect("live");
            assert_eq!(word(front(&mut heap, a, 0)), empty);
            for past in [0, 1, 1000, usize::MAX - len] {
                assert_eq!(word(without(&mut heap, a, len + past)), empty);
            }
            for at in [0, 1, len / 2, len] {
                assert_eq!(word(cut(&mut heap, a, at, at)), empty);
            }
            assert_eq!(word(cut(&mut heap, a, 1, 0)), empty);
            assert_eq!(heap.made(), made);
            assert_eq!(heap.unique(cell), Ok(true));
            let gone = skip(&mut heap, a, len);
            assert_eq!(word(gone), empty);
            assert_eq!(heap.made(), made);
        }
        assert_eq!(heap.live(), 0, "{n}");
    }
}

/// The one element of an array is taken out of it, however the array came to have one,
/// and what is left is the empty array.
#[test]
fn what_is_left_of_one_element_is_the_empty_array() {
    for (head, taken) in [(true, true), (false, true), (true, false), (false, false)] {
        for held in [false, true] {
            let mut heap = Heap::default();
            let ways = 5;
            for way in 0..ways {
                let x = string(&mut heap, "x");
                let none = Value::EMPTY_ARRAY;
                let mut a = match way {
                    0 => from_values(&mut heap, &[x]),
                    1 => push(&mut heap, none, x, End::Back),
                    2 => push(&mut heap, none, x, End::Front),
                    3 => extend(&mut heap, none, &[x], End::Front),
                    _ => {
                        let mut all = vec![small(0); 40];
                        all.push(x);
                        from_values(&mut heap, &all)
                    }
                }
                .expect("room");
                let spare = len(&heap, a) - 1;
                a = skip(&mut heap, a, spare).expect("room");
                if held {
                    heap.own(a).expect("live");
                }
                let made = heap.made();
                let (first, left) = uncons(&mut heap, a, head, taken)
                    .expect("room")
                    .expect("an element");
                assert_eq!(left.bits(), Value::EMPTY_ARRAY.bits(), "{way}");
                assert_eq!(heap.made(), made, "{way}");
                if head {
                    assert_eq!(first.bits(), x.bits());
                    release(&mut heap, first).expect("live");
                }
                for _ in 0..usize::from(!taken) + usize::from(held) {
                    assert_eq!(texts(&heap, a), ["x"]);
                    release(&mut heap, a).expect("live");
                }
            }
            assert_eq!(heap.live(), 0, "{head} {taken} {held}");
        }
    }
}

/// The empty array is grown into the same array by a run of items and one at a time.
#[test]
fn the_empty_array_grows_into_the_array_it_did() {
    for n in [1, 2, 31, 32, 33, 64, 65, 1100] {
        for end in [End::Front, End::Back] {
            let mut heap = Heap::default();
            let items: Vec<Value> = (0..n).map(small).collect();
            let run = extend(&mut heap, Value::EMPTY_ARRAY, &items, end).expect("room");
            let mut each = Value::EMPTY_ARRAY;
            for i in 0..n {
                let x = match end {
                    End::Back => i,
                    End::Front => n - 1 - i,
                };
                each = push(&mut heap, each, small(x), end).expect("room");
                check(&heap, each);
            }
            // A longer run that is all there is has only full leaves in front of its tail.
            if n <= 32 {
                assert_eq!(shape(&heap, run), shape(&heap, each));
            }
            assert_eq!(ints(&heap, run), (0..n).collect::<Vec<_>>());
            assert_eq!(ints(&heap, each), (0..n).collect::<Vec<_>>());
            let one = pushed_on(&mut heap, Value::EMPTY_ARRAY, small(7), end).expect("room");
            assert!(is_flat(&heap, one));
            for a in [one, run, each] {
                release(&mut heap, a).expect("live");
            }
            assert_eq!(heap.live(), 0);
        }
    }
}

/// A cell of no elements, which nothing makes, has no first element, and it says so
/// rather than give a tail that counts below none.
#[test]
fn a_cell_of_no_elements_has_no_first_one() {
    let mut heap = Heap::default();
    let cell = a_root_of_none(&mut heap);
    let none = [Value::NIL.bits(); 4].into_iter().chain([0]);
    let words: Vec<u64> = none.collect();
    let flat = heap.make(Kind::ArrayFlat, &words).expect("room");
    assert!(super::get(&heap, cell, 0).is_none());
    for head in [true, false] {
        let of = Flat::of(&heap, flat).expect("a short array's cell");
        assert_eq!(of.len(), 0);
        assert!(matches!(flat::uncons(&mut heap, of, head), Ok(None)));
        assert!(flat::take_first(&mut heap, flat).is_none());
        assert!(super::take_first(&mut heap, cell).is_none());
    }
    assert_eq!(heap.unique(cell), Ok(true));
    assert_eq!(heap.unique(flat), Ok(true));
}

/// Nothing happens to the empty array when it is written, and what was to go in is given
/// up.
#[test]
fn a_write_to_the_empty_array_gives_up_the_value() {
    let mut heap = Heap::default();
    for i in [0, 1, usize::MAX] {
        let x = string(&mut heap, "never stored");
        let same = super::set(&mut heap, Stored::Empty, i, x);
        assert_eq!(word(same), Ok(Value::EMPTY_ARRAY.bits()));
        assert_eq!(heap.live(), 0);
    }
}

/// How many cells it takes to build an array of `n` at `end` from `[]` and to walk it
/// down to `[]` again, with the array held while it grows or not and while it is walked or
/// not. It is the same for each part.
fn there_and_back(n: i64, end: End, held: bool) -> (usize, usize) {
    let mut heap = Heap::default();
    let mut a = Value::EMPTY_ARRAY;
    for i in 0..n {
        let before = a;
        if held {
            heap.own(before).expect("live");
        }
        a = push(&mut heap, a, small(i), end).expect("room");
        if held {
            release(&mut heap, before).expect("live");
        }
    }
    let built = heap.made();
    let whole = a;
    if held {
        heap.own(whole).expect("live");
    }
    for i in 0..n {
        let want = match end {
            End::Back => i,
            End::Front => n - 1 - i,
        };
        let (first, left) = uncons(&mut heap, a, true, true)
            .expect("room")
            .expect("an element");
        assert_eq!(int(first), Some(want));
        a = left;
    }
    assert_eq!(a.bits(), Value::EMPTY_ARRAY.bits());
    let walked = heap.made() - built;
    if held {
        release(&mut heap, whole).expect("live");
    }
    assert_eq!(heap.live(), 0);
    (built, walked)
}

/// A build from `[]` and a walk back to it make twice as many cells for an array twice as
/// long, at both ends, held or not.
#[test]
fn a_build_from_the_empty_array_does_not_grow_faster_than_it() {
    for end in [End::Front, End::Back] {
        for held in [false, true] {
            for n in [250, 1000, 4000] {
                let (built, walked) = there_and_back(n, end, held);
                let (built_2, walked_2) = there_and_back(2 * n, end, held);
                // What is short is one cell each time, and not two.
                assert!(built_2 <= built * 2 + 100, "{n} {built} {built_2}");
                assert!(walked_2 <= walked * 2 + 40, "{n} {walked} {walked_2}");
            }
        }
    }
    // A short array is in one cell of 4, 8, 16 and 32 slots in turn. The 33rd push makes a root
    // and another of 32, and no other does until that is full.
    for end in [End::Front, End::Back] {
        for (from, cells) in [(1, 1), (5, 2), (9, 3), (17, 4), (33, 6), (64, 6)] {
            let to = if from < 33 { 2 * from - 2 } else { from };
            for n in from..=to.max(from) {
                let (built, walked) = there_and_back(n, end, false);
                assert_eq!(built, cells, "{end:?} {n}");
                assert!(n > 32 || walked == 0, "{end:?} {n}");
            }
        }
    }
}
