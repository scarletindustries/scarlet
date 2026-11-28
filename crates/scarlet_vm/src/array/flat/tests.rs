use super::super::tests::{
    check, concat, cut, elements, extend, int, is_flat, push, root, set, skip, slice, small,
    stored, string, text, texts, uncons, word,
};
use super::super::{Stored, from_values, release, seq};
use super::*;
use crate::heap::Kind;
use std::collections::VecDeque;

/// Numbers that look random.
struct Dice(u64);

impl Dice {
    fn below(&mut self, n: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) as usize % n
    }
}

fn flat_at(heap: &Heap, array: Value) -> Flat {
    match stored(heap, array) {
        Stored::Flat(flat) => flat,
        Stored::Empty | Stored::Tree(_) => panic!("no short array"),
    }
}

fn slots_of(heap: &Heap, array: Value) -> usize {
    heap.data(array.as_cell().expect("a cell")).len() - 1
}

/// Every reference there is, from `held`, is counted.
fn counted(heap: &Heap, held: &[Value], what: &str) {
    assert_eq!(heap.audit(held.iter().copied()), Ok(()), "{what}");
}

/// A short array of the numbers from 0 to `len`, in `slots` slots, the first of which its
/// first element is in is `start`.
fn turned(heap: &mut Heap, slots: usize, start: usize, len: usize) -> Value {
    let mut words = vec![Value::NIL.bits(); slots];
    for i in 0..len {
        words[(start + i) % slots] = small(i as i64).bits();
    }
    words.push(Window::from(start, len).0);
    let cell = heap.make(Kind::ArrayFlat, &words).expect("room");
    check(heap, Value::cell(cell));
    Value::cell(cell)
}

fn numbers(heap: &Heap, array: Value) -> Vec<i64> {
    let all = elements(heap, array);
    all.into_iter().map(|v| int(v).expect("an Int")).collect()
}

#[test]
fn a_new_cell_has_as_few_slots_as_there_may_be() {
    for len in 1..=MOST {
        let want = [4, 8, 16, 32].into_iter().find(|n| *n >= len);
        assert_eq!(Some(slots_for(len)), want);
    }
    let mut heap = Heap::default();
    for len in 1..=MOST {
        let items: Vec<Value> = (0..len).map(|i| small(i as i64)).collect();
        for a in [
            new_flat(&mut heap, &items).expect("room").value(),
            at_end(&mut heap, items.iter().copied()).expect("room"),
        ] {
            check(&heap, a);
            assert_eq!(slots_of(&heap, a), slots_for(len));
            release(&mut heap, a).expect("live");
        }
        let a = in_freed(&mut heap, items.iter().copied()).expect("one was freed");
        check(&heap, a);
        assert_eq!(elements(&heap, a).len(), len);
        release(&mut heap, a).expect("live");
    }
    assert_eq!(heap.live(), 0);
}

/// A window names the slots of its elements wherever it starts, on past the last slot, and
/// wherever the number it counts them by turns over.
#[test]
fn a_window_goes_round() {
    for slots in [4usize, 8, 16, 32] {
        for start in [
            0,
            1,
            5,
            31,
            1 << 31,
            u64::from(u32::MAX) - 2,
            u64::from(u32::MAX),
        ] {
            let at = |i: usize| (start as usize + i) % slots;
            let mut w = Window(start << 32 | 2);
            assert_eq!(
                (w.len(), w.place(0, slots), w.place(1, slots)),
                (2, at(0), at(1))
            );
            // One more at the back leaves the first where it is, and one in front is in the
            // slot before it.
            w = w.and_one(End::Back);
            assert_eq!(
                (w.len(), w.place(0, slots), w.place(2, slots)),
                (3, at(0), at(2))
            );
            let mut longer = w;
            for n in 1..=8 {
                longer = longer.and_one(End::Front);
                let first = (at(0) + 8 * slots - n) % slots;
                assert_eq!((longer.len(), longer.place(0, slots)), (3 + n, first));
                assert_eq!(longer.place(n, slots), at(0));
            }
            for from in 0..3 {
                let cut = w.cut(from, 3 - from);
                assert_eq!((cut.len(), cut.place(0, slots)), (3 - from, at(from)));
            }
            assert_eq!(longer.cut(8, 3).place(0, slots), at(0));
        }
    }
}

/// Whatever is done to a short array is the same wherever in its slots its elements start,
/// and whether or not something else holds it, which is then as it was.
#[test]
fn every_turn_of_a_short_array_is_the_same_array() {
    for slots in [4usize, 8, 16, 32] {
        let lens = [1, 2, slots / 2 + 1, slots - 1, slots];
        for start in 0..slots {
            for len in lens.into_iter().filter(|n| *n > slots / 2 || slots == 4) {
                for held in [false, true] {
                    each_of(slots, start, len, held);
                }
            }
        }
    }
}

/// What is done to an array, and what is then in it.
type Does<'a> = &'a mut dyn FnMut(&mut Heap, Value) -> (Value, Vec<i64>);

fn each_of(slots: usize, start: usize, len: usize, held: bool) {
    let mut heap = Heap::default();
    let want: Vec<i64> = (0..len as i64).collect();
    let what = format!("{slots} from {start}, {len}, held {held}");
    let mut on = |f: Does<'_>| {
        let a = turned(&mut heap, slots, start, len);
        if held {
            heap.own(a).expect("live");
        }
        let (got, now) = f(&mut heap, a);
        check(&heap, got);
        assert_eq!(numbers(&heap, got), now, "{what}");
        let mut all = vec![got];
        if held {
            assert_eq!(numbers(&heap, a), want, "{what}");
            all.push(a);
        }
        counted(&heap, &all, &what);
        for v in all {
            release(&mut heap, v).expect("live");
        }
        assert_eq!(heap.live(), 0, "{what}");
    };
    let with = |at: usize, new: &[i64]| {
        let mut all = want.clone();
        all.splice(at..at, new.iter().copied());
        all
    };
    for (end, at) in [(End::Front, 0), (End::Back, len)] {
        on(&mut |heap, a| {
            (
                push(heap, a, small(70), end).expect("room"),
                with(at, &[70]),
            )
        });
        for k in [0, 2, 3, 5, 33 - len.min(31)] {
            let new: Vec<i64> = (100..100 + k as i64).collect();
            let items: Vec<Value> = new.iter().copied().map(small).collect();
            on(&mut |heap, a| {
                let longer = extend(heap, a, &items, end).expect("room");
                (longer, with(at, &new))
            });
        }
    }
    for i in [0, len / 2, len - 1] {
        on(&mut |heap, a| {
            let mut now = want.clone();
            now[i] = 7;
            (set(heap, a, i, small(7)).expect("room"), now)
        });
        assert_eq!(want[i], i as i64);
    }
    for from in [0, 1, len / 2, len - 1, len] {
        for to in [from, (from + 1).min(len), len - 1, len] {
            if from <= to {
                on(&mut |heap, a| {
                    let part = slice(heap, a, from, to).expect("room");
                    (part, want[from..to].to_vec())
                });
            }
        }
        on(&mut |heap, a| (skip(heap, a, from).expect("room"), want[from..].to_vec()));
    }
    on(&mut |heap, a| {
        let found = uncons(heap, a, true, true).expect("room");
        let (first, left) = found.expect("an element");
        assert_eq!(int(first), Some(0));
        (left, want[1..].to_vec())
    });
    for other in [1, 3, 4, 20, 32] {
        for turn in [0, 3] {
            for first in [false, true] {
                on(&mut |heap, a| {
                    let wide = slots_for(other);
                    let b = turned(heap, wide, turn, other);
                    let more: Vec<i64> = (0..other as i64).collect();
                    let (l, r) = if first { (b, a) } else { (a, b) };
                    let joined = concat(heap, l, r).expect("room");
                    let at = if first { 0 } else { len };
                    (joined, with(at, &more))
                });
            }
        }
    }
}

/// An array joined to itself, held or not, is twice itself.
#[test]
fn an_array_is_joined_to_itself() {
    for len in [1usize, 2, 4, 5, 16, 17, 31, 32] {
        for held in [0, 1, 2] {
            let mut heap = Heap::default();
            let names: Vec<String> = (0..len).map(|i| format!("s{i}")).collect();
            let items: Vec<Value> = names.iter().map(|s| string(&mut heap, s)).collect();
            let a = from_values(&mut heap, &items).expect("room");
            // The two that are joined, and those that keep it.
            for _ in 0..1 + held {
                heap.own(a).expect("live");
            }
            let twice = concat(&mut heap, a, a).expect("room");
            check(&heap, twice);
            assert_eq!(texts(&heap, twice), [names.clone(), names.clone()].concat());
            let mut all = vec![twice];
            all.extend(std::iter::repeat_n(a, held));
            counted(&heap, &all, "twice");
            if held > 0 {
                assert_eq!(texts(&heap, a), names);
            }
            for v in all {
                release(&mut heap, v).expect("live");
            }
            assert_eq!(heap.live(), 0, "{len} {held}");
        }
    }
}

/// An array may hold itself as it was. What is pushed onto it or written into it is then
/// another holder, so the array is copied.
#[test]
fn an_array_is_put_in_itself() {
    for len in [1usize, 3, 4, 31, 32, 33] {
        for way in 0..4 {
            let mut heap = Heap::default();
            let items: Vec<Value> = (0..len).map(|i| small(i as i64)).collect();
            let a = from_values(&mut heap, &items).expect("room");
            heap.own(a).expect("live");
            let b = match way {
                0 => push(&mut heap, a, a, End::Back),
                1 => push(&mut heap, a, a, End::Front),
                2 => set(&mut heap, a, len - 1, a),
                _ => extend(&mut heap, a, &[a], End::Back),
            }
            .expect("room");
            assert_ne!(a.bits(), b.bits());
            check(&heap, b);
            let at = match way {
                1 => 0,
                2 => len - 1,
                _ => len,
            };
            assert_eq!(elements(&heap, b)[at].bits(), a.bits());
            assert_eq!(elements(&heap, a).len(), len);
            counted(&heap, &[b], "itself");
            release(&mut heap, b).expect("live");
            assert_eq!(heap.live(), 0);
        }
    }
}

/// What is done at random to arrays of a few elements, of which some are kept, does what it
/// does to a plain list, and after each of it every reference is counted.
#[test]
fn a_short_array_does_what_a_list_does() {
    let (mut flat, mut trees, mut crossed) = (0, 0, 0);
    for seed in 1..=12u64 {
        let mut dice = Dice(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let mut heap = Heap::default();
        let mut a = Value::EMPTY_ARRAY;
        let mut want: VecDeque<String> = VecDeque::new();
        let mut kept: Vec<(Value, Vec<String>)> = Vec::new();
        // The sizes it is drawn to: those at which a cell is full, and those past them.
        let mut aim = 4;
        for step in 0..1500 {
            let what = format!("seed {seed} step {step}");
            if dice.below(40) == 0 {
                aim = [1, 2, 4, 5, 8, 9, 16, 17, 31, 32, 33, 40][dice.below(12)];
            }
            let new = |heap: &mut Heap, n: usize| {
                let names: Vec<String> = (0..n).map(|k| format!("s{step}.{k}")).collect();
                let items: Vec<Value> = names.iter().map(|s| string(heap, s)).collect();
                (names, items)
            };
            let end = [End::Front, End::Back][dice.below(2)];
            let at = |len: usize| if end == End::Front { 0 } else { len };
            let grow = want.len() < aim || dice.below(4) == 0;
            let was_flat = is_flat(&heap, a);
            match dice.below(12) {
                0..=3 if grow => {
                    let (names, items) = new(&mut heap, 1);
                    a = push(&mut heap, a, items[0], end).expect("room");
                    let i = at(want.len());
                    want.insert(i, names[0].clone());
                }
                4 if grow => {
                    let n = dice.below(7);
                    let (names, items) = new(&mut heap, n);
                    a = extend(&mut heap, a, &items, end).expect("room");
                    let i = at(want.len());
                    for (k, name) in names.into_iter().enumerate() {
                        want.insert(i + k, name);
                    }
                }
                5 if grow => {
                    let n = 1 + dice.below(aim);
                    let (names, items) = new(&mut heap, n);
                    let b = from_values(&mut heap, &items).expect("room");
                    let (l, r) = if end == End::Front { (b, a) } else { (a, b) };
                    a = concat(&mut heap, l, r).expect("room");
                    let i = at(want.len());
                    for (k, name) in names.into_iter().enumerate() {
                        want.insert(i + k, name);
                    }
                }
                0..=3 | 6 if !want.is_empty() => {
                    let head = dice.below(2) == 0;
                    let found = uncons(&mut heap, a, head, true).expect("room");
                    let (first, left) = found.expect("an element");
                    let name = want.pop_front().expect("as many");
                    if head {
                        assert_eq!(text(&heap, first), name, "{what}");
                    }
                    release(&mut heap, first).expect("live");
                    a = left;
                }
                4 | 7 if !want.is_empty() => {
                    let from = dice.below(3).min(want.len());
                    let to = want.len() - dice.below(3).min(want.len() - from);
                    a = slice(&mut heap, a, from, to).expect("room");
                    want = want.drain(from..to).collect();
                }
                5 if !want.is_empty() => {
                    let n = dice.below(4).min(want.len());
                    a = skip(&mut heap, a, n).expect("room");
                    want.drain(..n);
                }
                8 | 9 if !want.is_empty() => {
                    let i = dice.below(want.len());
                    let (names, items) = new(&mut heap, 1);
                    a = set(&mut heap, a, i, items[0]).expect("room");
                    want[i] = names[0].clone();
                }
                10 if dice.below(3) == 0 => {
                    heap.own(a).expect("live");
                    kept.push((a, want.iter().cloned().collect()));
                }
                11 if !kept.is_empty() && dice.below(2) == 0 => {
                    let (k, items) = kept.swap_remove(dice.below(kept.len()));
                    assert_eq!(texts(&heap, k), items, "{what}");
                    release(&mut heap, k).expect("live");
                }
                _ => {}
            }
            check(&heap, a);
            assert_eq!(texts(&heap, a), Vec::from(want.clone()), "{what}");
            let short = is_flat(&heap, a);
            flat += usize::from(short);
            trees += usize::from(!short && !want.is_empty());
            crossed += usize::from(was_flat != short);
            let held: Vec<Value> = kept.iter().map(|(k, _)| *k).chain([a]).collect();
            counted(&heap, &held, &what);
        }
        for (k, items) in kept {
            assert_eq!(texts(&heap, k), items, "seed {seed}");
            release(&mut heap, k).expect("live");
        }
        release(&mut heap, a).expect("live");
        assert_eq!(heap.live(), 0, "seed {seed}");
    }
    assert!(
        flat > 5000 && trees > 1000 && crossed > 100,
        "{flat} {trees} {crossed}"
    );
}

/// Pushes and takes by turns, at whatever length, copy nothing once there is a cell that
/// the longest of it fits in. So it is where a cell is full, which is at 4, 8, 16 and 32,
/// and with the array a little longer than 32 and a little shorter by turns.
#[test]
fn a_queue_and_a_stack_make_no_cell_for_each_turn() {
    for len in (1..=40).chain([63, 64, 65]) {
        for (end, swing) in [End::Front, End::Back]
            .into_iter()
            .flat_map(|e| [(e, 1), (e, 2), (e, 3)])
        {
            let mut heap = Heap::default();
            let mut a = Value::EMPTY_ARRAY;
            for i in 0..len + swing {
                a = push(&mut heap, a, small(i), end).expect("room");
            }
            let turn = |heap: &mut Heap, a: &mut Value| {
                for _ in 0..swing {
                    let (_, left) = uncons(heap, *a, true, true).expect("room").expect("one");
                    *a = left;
                }
                for i in 0..swing {
                    *a = push(heap, *a, small(i), end).expect("room");
                }
            };
            let made = heap.made();
            let turns = 400;
            for _ in 0..turns {
                turn(&mut heap, &mut a);
            }
            let made = heap.made() - made;
            check(&heap, a);
            if len + swing <= 32 {
                assert_eq!(made, 0, "{end:?} {len} {swing}");
            } else {
                // A tree's leaf is made one longer at each push, and its root again where a
                // leaf of it is spent.
                let each = 4 * swing as usize;
                assert!(made <= turns * each, "{end:?} {len} {swing}: {made}");
            }
            release(&mut heap, a).expect("live");
            assert_eq!(heap.live(), 0);
        }
    }
}

/// A cell that has 32 elements, whichever slot the first is in, is the head of the tree that
/// the array outgrows it as, or a leaf of it, so a push past it copies no element, held or not.
#[test]
fn an_array_that_outgrows_its_cell_keeps_it() {
    for held in [false, true] {
        for start in [0, 1, 16, 31] {
            for end in [End::Front, End::Back] {
                for more in [1usize, 2, 40] {
                    let mut heap = Heap::default();
                    let a = turned(&mut heap, 32, start, 32);
                    if held {
                        heap.own(a).expect("live");
                    }
                    let items: Vec<Value> = (0..more).map(|i| small(100 + i as i64)).collect();
                    let made = heap.made();
                    let b = extend(&mut heap, a, &items, end).expect("room");
                    check(&heap, b);
                    // A root, what the items are in, and a branch over two leaves.
                    let leaves = usize::from(end == End::Front && more > 32);
                    let want = 1 + more.div_ceil(32) + leaves;
                    assert_eq!(heap.made() - made, want, "{start} {end:?} {more}");
                    let r = root(&heap, b);
                    let parts = r.parts().into_iter();
                    let leaves = r.tree.as_cell().into_iter().flat_map(|c| heap.values(c));
                    let there = parts.chain(leaves).any(|v| v.bits() == a.bits());
                    assert!(there, "{start} {end:?} {more}");
                    let mut want: Vec<i64> = (0..32).collect();
                    let at = if end == End::Front { 0 } else { 32 };
                    want.splice(at..at, 100..100 + more as i64);
                    assert_eq!(numbers(&heap, b), want);
                    if !held {
                        counted(&heap, &[b], "outgrown");
                        // It is the tree's alone, and is written in place.
                        let made = heap.made();
                        let c = set(&mut heap, b, at.abs_diff(more) % want.len(), small(-1));
                        assert_eq!(word(c), Ok(b.bits()));
                        assert_eq!(heap.made(), made);
                        release(&mut heap, b).expect("live");
                        assert_eq!(heap.live(), 0);
                        continue;
                    }
                    counted(&heap, &[a, b], "outgrown");
                    // A write to either leaves the other alone.
                    heap.own(b).expect("live");
                    let c = set(&mut heap, b, if at == 0 { more } else { 0 }, small(-1));
                    let c = c.expect("room");
                    assert_eq!(numbers(&heap, a), (0..32).collect::<Vec<_>>());
                    assert_eq!(numbers(&heap, b), want);
                    let a = set(&mut heap, a, 5, small(-2)).expect("room");
                    assert_eq!(numbers(&heap, b), want);
                    assert_eq!(elements(&heap, a)[5].bits(), small(-2).bits());
                    counted(&heap, &[a, b, c], "written");
                    for v in [a, b, c] {
                        check(&heap, v);
                        release(&mut heap, v).expect("live");
                    }
                    assert_eq!(heap.live(), 0);
                }
            }
        }
    }
}

/// An array of 32 or fewer that is cut from a longer one, or joined or made, is one cell. What
/// is left of a longer one as its front is taken stays a tree until only its tail is left,
/// whatever is written or pushed to it, and is the same array to everything.
#[test]
fn a_short_array_is_one_cell_unless_it_is_left_of_a_long_one() {
    let mut heap = Heap::default();
    let items: Vec<Value> = (0..40).map(small).collect();
    let long = from_values(&mut heap, &items).expect("room");
    assert!(!is_flat(&heap, long));
    for (from, to) in [(0, 1), (0, 32), (8, 40), (39, 40), (3, 20)] {
        let part = cut(&mut heap, long, from, to).expect("room");
        assert!(is_flat(&heap, part));
        assert_eq!(
            numbers(&heap, part),
            (from as i64..to as i64).collect::<Vec<_>>()
        );
        release(&mut heap, part).expect("live");
    }
    // 32 in the inner tree and 8 in the tail, or those and 5 in the head.
    for in_front in [0, 5] {
        for held in [false, true] {
            let mut left = long;
            heap.own(left).expect("live");
            for i in 0..in_front {
                left = push(&mut heap, left, small(i), End::Front).expect("room");
            }
            let all = 40 + in_front as usize;
            for n in (0..all).rev() {
                let before = left;
                if held {
                    heap.own(before).expect("live");
                }
                let found = uncons(&mut heap, left, false, true).expect("room");
                left = found.expect("one").1;
                if held {
                    release(&mut heap, before).expect("live");
                }
                check(&heap, left);
                match n {
                    0 => assert_eq!(left.bits(), Value::EMPTY_ARRAY.bits()),
                    _ => assert_eq!(is_flat(&heap, left), n <= 8, "{n}"),
                }
                // Writing to it and pushing on it leave it what it is.
                if n > 0 {
                    let was_flat = is_flat(&heap, left);
                    left = set(&mut heap, left, 0, small(-1)).expect("room");
                    assert_eq!(is_flat(&heap, left), was_flat, "{n}");
                }
                let same = elements(&heap, left);
                let same = from_values(&mut heap, &same).expect("room");
                assert_eq!(is_flat(&heap, same), (1..=32).contains(&n));
                assert!(crate::eq::equal(&heap, left, same));
                assert!(crate::eq::equal(&heap, same, left));
                let hashes = [left, same].map(|v| crate::hash::hash(&heap, v));
                assert_eq!(hashes[0], hashes[1]);
                release(&mut heap, same).expect("live");
            }
        }
    }
    // With as many more behind it as are taken from it, it is never cut to its tail.
    let mut a = long;
    for i in 0..200 {
        a = push(&mut heap, a, small(i), End::Back).expect("room");
        let found = uncons(&mut heap, a, false, true).expect("room");
        a = found.expect("one").1;
        check(&heap, a);
    }
    release(&mut heap, a).expect("live");
    assert_eq!(heap.live(), 0);
}

/// An element is given up as soon as the array that held it last loses it, and none is while
/// something else holds the array.
#[test]
fn an_element_is_given_up_when_the_array_loses_it() {
    let names = |from: u64, to: u64| (from..to).collect::<Vec<u64>>();
    for held in [false, true] {
        for len in [1u64, 2, 4, 5, 17, 32] {
            for way in 0..7 {
                let mut heap = Heap::default();
                let handles: Vec<Value> = (0..len)
                    .map(|i| Value::cell(heap.make(Kind::Handle, &[i]).expect("room")))
                    .collect();
                let a = from_values(&mut heap, &handles).expect("room");
                if held {
                    heap.own(a).expect("live");
                }
                let n = len as usize;
                let (left, gone) = match way {
                    0 => {
                        let found = uncons(&mut heap, a, false, true).expect("room");
                        (found.expect("one").1, names(0, 1))
                    }
                    1 => {
                        let found = uncons(&mut heap, a, true, true).expect("room");
                        let (first, left) = found.expect("one");
                        assert_eq!(heap.take_released(), []);
                        release(&mut heap, first).expect("live");
                        (left, names(0, 1))
                    }
                    2 => (skip(&mut heap, a, n / 2).expect("room"), names(0, len / 2)),
                    3 => (
                        slice(&mut heap, a, 0, n / 2).expect("room"),
                        names(len / 2, len),
                    ),
                    4 => {
                        let part = slice(&mut heap, a, n / 4, n - n / 4).expect("room");
                        let ends = [names(0, len / 4), names(len - len / 4, len)].concat();
                        (part, ends)
                    }
                    5 => (
                        set(&mut heap, a, n - 1, small(1)).expect("room"),
                        names(len - 1, len),
                    ),
                    // One that is longer loses none.
                    _ => (
                        push(&mut heap, a, small(1), End::Front).expect("room"),
                        Vec::new(),
                    ),
                };
                let mut now = heap.take_released();
                now.sort_unstable();
                let what = format!("{held} {len} {way}");
                assert_eq!(now, if held { Vec::new() } else { gone.clone() }, "{what}");
                release(&mut heap, left).expect("live");
                let mut then = heap.take_released();
                then.sort_unstable();
                if held {
                    assert_eq!(then, [], "{what}");
                    release(&mut heap, a).expect("live");
                    then = heap.take_released();
                    then.sort_unstable();
                    assert_eq!(then, names(0, len), "{what}");
                } else {
                    let rest: Vec<u64> = (0..len).filter(|i| !gone.contains(i)).collect();
                    assert_eq!(then, rest, "{what}");
                }
                assert_eq!(heap.live(), 0);
            }
        }
    }
}

/// With no room for what it makes, everything that gives an array gives up what it was
/// handed, and what something else holds is as it was.
#[test]
fn what_has_no_room_gives_up_what_it_was_handed() {
    let mut full = 0;
    for len in [1usize, 4, 5, 8, 16, 31, 32, 33, 40] {
        for held in [false, true] {
            for way in 0..9 {
                for more in (0..48).chain([100, 400, 1200, 5000, 20_000]) {
                    let mut heap = Heap::default();
                    let strings = |heap: &mut Heap, n: usize| -> Vec<Value> {
                        (0..n).map(|i| string(heap, &format!("s{i}"))).collect()
                    };
                    let items = strings(&mut heap, len);
                    let a = from_values(&mut heap, &items).expect("room");
                    if held {
                        heap.own(a).expect("live");
                    }
                    let given = strings(&mut heap, 35);
                    let live = heap.live();
                    heap.limit_to(heap.words_used() + more);
                    let (made, used) = match way {
                        0 => (push(&mut heap, a, given[0], End::Back), 1),
                        1 => (push(&mut heap, a, given[0], End::Front), 1),
                        2 => (extend(&mut heap, a, &given[..3], End::Back), 3),
                        3 => (extend(&mut heap, a, &given, End::Front), 35),
                        4 => (set(&mut heap, a, len / 2, given[0]), 1),
                        5 => (skip(&mut heap, a, 1), 0),
                        6 => (slice(&mut heap, a, 0, len - 1), 0),
                        7 => {
                            let b = match from_values(&mut heap, &given[..2]) {
                                Ok(b) => b,
                                Err(why) => {
                                    // The items went with it, and the array is here still.
                                    assert_eq!(why, Fault::Full);
                                    release(&mut heap, a).expect("live");
                                    full += 1;
                                    let left: Vec<Value> = given[2..].to_vec();
                                    counted(&heap, &kept(held, a, &left), "made");
                                    continue;
                                }
                            };
                            (concat(&mut heap, a, b), 2)
                        }
                        _ => {
                            heap.own(a).expect("live");
                            (concat(&mut heap, a, a), 0)
                        }
                    };
                    let what = format!("{len} {held} {way} {more}");
                    let mut all = kept(held, a, &given[used..]);
                    match made {
                        Ok(made) => all.push(made),
                        Err(why) => {
                            assert_eq!(why, Fault::Full, "{what}");
                            full += 1;
                            if held && way != 8 {
                                assert_eq!(elements(&heap, a).len(), len, "{what}");
                                assert_eq!(heap.live(), live - used, "{what}");
                            }
                        }
                    }
                    counted(&heap, &all, &what);
                }
            }
        }
    }
    assert!(full > 1000, "{full}");
}

fn kept(held: bool, a: Value, given: &[Value]) -> Vec<Value> {
    let mut all = given.to_vec();
    if held {
        all.push(a);
    }
    all
}

/// An array of any length that there is no room for has given up its elements.
#[test]
fn an_array_with_no_room_gives_up_its_elements() {
    for len in [1usize, 4, 5, 32, 33, 64, 65, 100, 1024, 1025, 1100] {
        let mut room = Heap::default();
        let items: Vec<Value> = (0..len)
            .map(|i| string(&mut room, &format!("{i}")))
            .collect();
        let strings = room.words_used();
        from_values(&mut room, &items).expect("room");
        let needs = room.words_used() - strings;
        // A tree asks for more than it comes to take.
        let all = if len > 32 { needs + 20_000 } else { needs };
        for more in (0..40.min(needs)).chain([needs / 2, needs - 1, all]) {
            let mut heap = Heap::default();
            let items: Vec<Value> = (0..len)
                .map(|i| string(&mut heap, &format!("{i}")))
                .collect();
            heap.limit_to(strings + more);
            match from_values(&mut heap, &items) {
                Ok(made) => {
                    assert!(more >= needs, "{len}");
                    check(&heap, made);
                    counted(&heap, &[made], "made");
                }
                Err(why) => {
                    assert_eq!(why, Fault::Full);
                    assert!(more < all, "{len} {more}");
                    assert_eq!(heap.live(), 0, "{len} {more}");
                    counted(&heap, &[], "refused");
                }
            }
        }
    }
}

/// An item pushed on the empty array is given up where there is no room for the array.
#[test]
fn a_push_on_nothing_with_no_room_gives_up_the_item() {
    for end in [End::Front, End::Back] {
        let mut heap = Heap::default();
        let x = string(&mut heap, "x");
        heap.limit_to(heap.words_used() + 5);
        let made = push(&mut heap, Value::EMPTY_ARRAY, x, end);
        assert_eq!(word(made), Err(Fault::Full));
        assert_eq!(heap.live(), 0);
        counted(&heap, &[], "refused");
    }
}

/// Two short arrays that do not fit in one cell are the head and the tail of a tree, whole,
/// or a leaf and one of those, the same one twice as well, so only the root is made. With no
/// room for that both are given up.
#[test]
fn a_join_that_outgrows_a_cell_makes_a_root() {
    let mut full = 0;
    for (a, b) in [
        (32, 2),
        (2, 32),
        (32, 32),
        (17, 16),
        (16, 17),
        (31, 2),
        (20, 30),
    ] {
        for (turn_a, turn_b) in [(0, 0), (0, 3), (5, 0), (5, 3)] {
            for itself in [false, true] {
                for held in [false, true] {
                    for more in [0, 8, 9, 10, 100, 10_000] {
                        if itself && 2 * a <= 32 {
                            continue;
                        }
                        let mut heap = Heap::default();
                        let x = turned(&mut heap, 32, turn_a, a);
                        let y = match itself {
                            true => heap.own(x).expect("live"),
                            false => turned(&mut heap, 32, turn_b, b),
                        };
                        let mut kept = Vec::new();
                        if held {
                            kept.extend([x, y].map(|v| heap.own(v).expect("live")));
                        }
                        let want = [numbers(&heap, x), numbers(&heap, y)].concat();
                        heap.limit_to(heap.words_used() + more);
                        let made = heap.made();
                        let what = format!("{a} {b} {turn_a} {turn_b} {itself} {held} {more}");
                        match concat(&mut heap, x, y) {
                            Ok(joined) => {
                                assert_eq!(heap.made() - made, 1, "{what}");
                                check(&heap, joined);
                                assert_eq!(numbers(&heap, joined), want, "{what}");
                                kept.push(joined);
                            }
                            Err(why) => {
                                assert_eq!(why, Fault::Full);
                                assert!(more < 10_000, "{what}");
                                full += 1;
                            }
                        }
                        counted(&heap, &kept, &what);
                        for v in kept {
                            release(&mut heap, v).expect("live");
                        }
                        assert_eq!(heap.live(), 0, "{what}");
                    }
                }
            }
        }
    }
    assert!(full > 100, "{full}");
}

/// A slice that leaves a quarter of the slots or less, in a cell that nothing else holds,
/// leaves them in one of fewer slots. A pattern leaves the cell as it is.
#[test]
fn a_slice_does_not_keep_a_cell_much_too_long() {
    for slots in [4usize, 8, 16, 32] {
        for start in 0..slots {
            for from in 0..slots {
                for to in from + 1..=slots {
                    let mut heap = Heap::default();
                    let a = turned(&mut heap, slots, start, slots);
                    let all = numbers(&heap, a);
                    let cut = slice(&mut heap, a, from, to).expect("room");
                    let what = format!("{slots} {start} {from} {to}");
                    check(&heap, cut);
                    assert_eq!(numbers(&heap, cut), all[from..to], "{what}");
                    let kept = to - from;
                    let want = match kept * 4 <= slots {
                        true => slots_for(kept),
                        false => slots,
                    };
                    assert_eq!(slots_of(&heap, cut), want, "{what}");
                    counted(&heap, &[cut], &what);
                    release(&mut heap, cut).expect("live");
                    let b = turned(&mut heap, slots, start, slots);
                    let left = skip(&mut heap, b, from).expect("room");
                    assert_eq!(slots_of(&heap, left), slots, "{what}");
                    release(&mut heap, left).expect("live");
                    assert_eq!(heap.live(), 0, "{what}");
                }
            }
        }
    }
}

#[test]
fn a_short_array_that_loses_nothing_is_the_one_it_was() {
    let mut heap = Heap::default();
    let items: Vec<Value> = (0..9).map(small).collect();
    let a = from_values(&mut heap, &items).expect("room");
    let made = heap.made();
    for held in [false, true] {
        if held {
            heap.own(a).expect("live");
        }
        assert_eq!(word(skip(&mut heap, a, 0)), Ok(a.bits()), "{held}");
        assert_eq!(word(slice(&mut heap, a, 0, 9)), Ok(a.bits()), "{held}");
    }
    assert_eq!(heap.made(), made);
}

/// What is taken out by a pattern leaves `Nil` where it was, and the rest is given up with
/// the array or cut from it.
#[test]
fn a_run_is_taken_out_of_a_short_array() {
    for (slots, start) in [(8usize, 0usize), (8, 6), (32, 30), (4, 3)] {
        for held in [false, true] {
            let mut heap = Heap::default();
            let a = turned(&mut heap, slots, start, 4);
            if held {
                heap.own(a).expect("live");
            }
            let cell = a.as_cell().expect("a cell");
            let mut out = [Value::NIL; 2];
            let wanted = [true, false].into_iter();
            let took = take_run(&mut heap, cell, 1, wanted, &mut out);
            assert_eq!(took, !held);
            if held {
                assert_eq!(numbers(&heap, a), [0, 1, 2, 3]);
                continue;
            }
            assert_eq!(int(out[0]), Some(1));
            assert_eq!(out[1].bits(), Value::NIL.bits());
            let left: Vec<Option<i64>> = elements(&heap, a).into_iter().map(int).collect();
            assert_eq!(left, [Some(0), None, Some(2), Some(3)]);
            let rest = skip(&mut heap, a, 3).expect("room");
            assert_eq!(numbers(&heap, rest), [3]);
            check(&heap, rest);
        }
    }
    // Of a cell that is no short array's, nothing.
    let mut heap = Heap::default();
    let other = heap.make(Kind::Binary, &[1, 2, 3, 4, 5]).expect("room");
    let taken = take_run(&mut heap, other, 0, [true].into_iter(), &mut [Value::NIL]);
    assert!(!taken && take_first(&mut heap, other).is_none());
    assert!(!push_alone(&mut heap, other, small(1), End::Back));
    let wrote = set_alone(&mut heap, other, 0, small(1));
    assert!(matches!(wrote, Near::Other));
    assert!(seq(&heap, Value::cell(other)).is_none());
    let flat = new_flat(&mut heap, &[small(1), small(2)]).expect("room");
    let flat = flat.value();
    let cell = flat.as_cell().expect("a cell");
    for past in [2, usize::MAX] {
        let wrote = set_alone(&mut heap, cell, past, small(1));
        assert!(matches!(wrote, Near::Past));
    }
    assert_eq!(numbers(&heap, flat), [1, 2]);
    assert_eq!(flat_at(&heap, flat).len(), 2);
}
