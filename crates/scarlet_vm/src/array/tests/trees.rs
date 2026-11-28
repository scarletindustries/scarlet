//! Long arrays against a list, with the tree they are checked at each step.

use super::*;
use std::collections::VecDeque;

/// Numbers that look random.
struct Dice(u64);

impl Dice {
    fn below(&mut self, n: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) as usize % n.max(1)
    }
}

/// Which numbers are strings that say them, and which are cells that hold such a string.
#[derive(Debug, Clone, Copy)]
enum Cells {
    None,
    Some,
    All,
}

impl Cells {
    fn value(self, heap: &mut Heap, n: i64) -> Value {
        match self {
            Cells::Some if n.rem_euclid(5) > 2 => small(n),
            Cells::Some if n.rem_euclid(5) > 0 => string(heap, &n.to_string()),
            Cells::None => small(n),
            Cells::Some | Cells::All => holder(heap, &n.to_string()),
        }
    }

    fn values(self, heap: &mut Heap, all: &[i64]) -> Vec<Value> {
        all.iter().map(|n| self.value(heap, *n)).collect()
    }
}

/// The number that [`Cells::value`] was given.
fn number(heap: &Heap, v: Value) -> i64 {
    int(v).unwrap_or_else(|| text(heap, v).parse().expect("a number"))
}

/// The array holds what the list does, its tree is whole, and what it reads at some indexes
/// is there.
fn same(heap: &Heap, a: Value, want: &VecDeque<i64>, dice: &mut Dice, what: &str) {
    check(heap, a);
    let got = elements(heap, a).into_iter().map(|v| number(heap, v));
    assert!(got.eq(want.iter().copied()), "{what}");
    for _ in 0..3 {
        let i = dice.below(want.len() + 2);
        let got = get(heap, a, i).map(|v| number(heap, v));
        assert_eq!(got, want.get(i).copied(), "{what}");
    }
}

/// An array that is pushed to, added to, cut, joined and written at random, about some length
/// that changes, does what a list does, whether or not something else holds it at the time.
/// What held it is as it was.
#[test]
fn a_long_array_does_what_a_list_does() {
    for seed in 1..=9u64 {
        let cells = [Cells::None, Cells::Some, Cells::All][seed as usize % 3];
        let mut heap = Heap::default();
        let mut dice = Dice(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let mut a = Value::EMPTY_ARRAY;
        let mut want = VecDeque::new();
        let mut kept: Vec<(Value, VecDeque<i64>)> = Vec::new();
        let mut n = 0i64;
        for step in 0..1500 {
            let aim = [70, 1000, 1100, 2100, 40, 3000][step / 250 % 6];
            let grow = dice.below(aim * 2) > want.len();
            let end = [End::Front, End::Back][dice.below(2)];
            let size = [1, 2, 31, 32, 33, 64, 100, 300][dice.below(8)];
            let size = 1 + dice.below(size);
            let new: Vec<i64> = (n..).take(size).collect();
            n += size as i64;
            if dice.below(9) == 0 {
                heap.own(a).expect("live");
                kept.push((a, want.clone()));
            }
            let put = |want: &mut VecDeque<i64>| match end {
                End::Back => want.extend(&new),
                End::Front => new.iter().rev().for_each(|x| want.push_front(*x)),
            };
            match dice.below(5) {
                0 if grow => {
                    let items = cells.values(&mut heap, &new);
                    a = extend(&mut heap, a, &items, end).expect("room");
                    put(&mut want);
                }
                1 if grow => {
                    // With a new array, or with one that was kept.
                    let old = kept.get(dice.below(6)).filter(|(_, old)| old.len() < 1500);
                    let (b, more) = match old {
                        Some((b, old)) => (heap.own(*b).expect("live"), old.clone()),
                        None => {
                            let items = cells.values(&mut heap, &new);
                            let b = from_values(&mut heap, &items).expect("room");
                            (b, new.iter().copied().collect())
                        }
                    };
                    let (l, r) = if end == End::Front { (b, a) } else { (a, b) };
                    a = concat(&mut heap, l, r).expect("room");
                    match end {
                        End::Back => want.extend(more),
                        End::Front => more.into_iter().rev().for_each(|x| want.push_front(x)),
                    }
                }
                2 if grow => {
                    for x in &new {
                        let item = cells.value(&mut heap, *x);
                        a = push(&mut heap, a, item, end).expect("room");
                        match end {
                            End::Back => want.push_back(*x),
                            End::Front => want.push_front(*x),
                        }
                    }
                }
                0 | 1 if !want.is_empty() => {
                    let (from, to) = match dice.below(3) {
                        0 => (size.min(want.len()), want.len()),
                        1 => (0, want.len().saturating_sub(size)),
                        _ => {
                            let from = size.min(want.len());
                            (from, want.len().saturating_sub(size).max(from))
                        }
                    };
                    a = slice(&mut heap, a, from, to).expect("room");
                    want.truncate(to);
                    want.drain(..from);
                }
                2 if !want.is_empty() => {
                    let by = size.min(want.len());
                    a = skip(&mut heap, a, by).expect("room");
                    want.drain(..by);
                }
                3 => {
                    for _ in 0..size {
                        let taken = dice.below(4) > 0;
                        let found = uncons(&mut heap, a, true, taken).expect("room");
                        let Some((first, left)) = found else { break };
                        let got = Some(number(&heap, first));
                        assert_eq!(got, want.pop_front(), "{seed} {step}");
                        release(&mut heap, first).expect("live");
                        if !taken {
                            release(&mut heap, a).expect("live");
                        }
                        a = left;
                    }
                }
                _ => {
                    for x in &new {
                        let i = dice.below(want.len() + 1);
                        let item = cells.value(&mut heap, *x);
                        a = set(&mut heap, a, i, item).expect("room");
                        if let Some(slot) = want.get_mut(i) {
                            *slot = *x;
                        }
                    }
                }
            }
            same(&heap, a, &want, &mut dice, &format!("{seed} {step}"));
            if kept.len() > 3 {
                let (k, items) = kept.swap_remove(dice.below(4));
                same(&heap, k, &items, &mut dice, &format!("{seed} {step} kept"));
                release(&mut heap, k).expect("live");
            }
            if step % 50 == 0 {
                let held = kept.iter().map(|(v, _)| *v).chain([a]);
                assert_eq!(heap.audit(held), Ok(()), "{seed} {step}");
            }
        }
        for (k, items) in kept {
            same(&heap, k, &items, &mut dice, "kept to the end");
            release(&mut heap, k).expect("live");
        }
        release(&mut heap, a).expect("live");
        assert_eq!(heap.live(), 0, "{seed}");
    }
}

/// A queue's leaves go round in its tree, so however many have gone through it, it is as tall
/// as its length asks for and no more cells are live than one that was made at once has.
#[test]
fn a_queue_goes_round_in_its_tree() {
    for len in [40usize, 64, 100, 1000, 1023, 1024, 1090, 1100, 1200, 2100] {
        for (put, by) in [
            (End::Back, 1),
            (End::Back, 33),
            (End::Back, 100),
            (End::Front, 1),
        ] {
            for held in [false, true] {
                let mut heap = Heap::default();
                let mut want: VecDeque<i64> = (0..len as i64).collect();
                let items: Vec<Value> = want.iter().copied().map(small).collect();
                let mut a = from_values(&mut heap, &items).expect("room");
                let live = heap.live();
                let mut next = len as i64;
                let turns = 3 * 1024 / by;
                for turn in 0..turns {
                    let before = a;
                    if held {
                        heap.own(before).expect("live");
                    }
                    let new: Vec<i64> = (next..).take(by).collect();
                    next += by as i64;
                    let items: Vec<Value> = new.iter().copied().map(small).collect();
                    a = extend(&mut heap, a, &items, put).expect("room");
                    a = match put {
                        End::Back => {
                            want.extend(new);
                            want.drain(..by);
                            skip(&mut heap, a, by).expect("room")
                        }
                        End::Front => {
                            new.into_iter().rev().for_each(|x| want.push_front(x));
                            want.truncate(len);
                            slice(&mut heap, a, 0, len).expect("room")
                        }
                    };
                    if held {
                        release(&mut heap, before).expect("live");
                    }
                    if turn % 37 == 0 {
                        check(&heap, a);
                        assert!(ints(&heap, a).iter().eq(&want), "{len} {by} {turn}");
                    }
                }
                check(&heap, a);
                // The head and the tail are each part full, and the leaves' run may be over
                // one branch more.
                assert!(heap.live() <= live + 3, "{len} {by}: {}", heap.live());
                release(&mut heap, a).expect("live");
                assert_eq!(heap.live(), 0, "{len} {by} {held}");
            }
        }
    }
}

/// A piece of a very long array is as tall as its own length asks for, wherever it is from,
/// and what it is of is as it was.
#[test]
fn a_piece_of_a_long_array_is_no_taller_than_it_needs() {
    let mut heap = Heap::default();
    let n = 70_000usize;
    let items: Vec<Value> = (0..n as i64).map(small).collect();
    let mut whole = from_values(&mut heap, &items).expect("room");
    // The second time its first leaf is in the last place but two.
    for start in [0, -100i64] {
        for at in (start + 1..0).rev() {
            whole = push(&mut heap, whole, small(at), End::Front).expect("room");
        }
        let first = ints(&heap, whole)[0];
        for len in [
            33usize, 100, 1000, 1024, 1025, 1056, 1057, 1100, 2000, 33_000,
        ] {
            for from in [
                0, 1, 31, 32, 33, 1000, 1023, 1024, 32_700, 32_768, 32_769, 36_000,
            ] {
                let piece = cut(&mut heap, whole, from, from + len).expect("room");
                check(&heap, piece);
                let got = ints(&heap, piece);
                let from = first + from as i64;
                assert!(got.into_iter().eq(from..from + len as i64), "{len} {from}");
                // It can be changed at both ends.
                let more: Vec<Value> = (0..40).map(small).collect();
                let longer = extend(&mut heap, piece, &more, End::Front).expect("room");
                let longer = extend(&mut heap, longer, &more, End::Back).expect("room");
                check(&heap, longer);
                let want = (0..40).chain(from..from + len as i64).chain(0..40);
                assert!(ints(&heap, longer).into_iter().eq(want), "{len} {from}");
                release(&mut heap, longer).expect("live");
            }
        }
        check(&heap, whole);
        assert!(ints(&heap, whole).into_iter().eq(first..n as i64));
    }
    release(&mut heap, whole).expect("live");
    assert_eq!(heap.live(), 0);
}

/// An array of any number of leaves that start at any place becomes a level taller, and one
/// less tall again, with every leaf where it was.
#[test]
fn a_tree_that_goes_round_becomes_taller_and_less_tall() {
    let items: Vec<Value> = (0..1024).map(small).collect();
    for turned in (0..32).step_by(3) {
        for end in [End::Front, End::Back] {
            for held in [false, true] {
                let mut heap = Heap::default();
                // As many leaves as one branch has, the first of which is in place `turned`.
                let mut a = from_values(&mut heap, &items).expect("room");
                let round: Vec<Value> = (0..turned * 32).map(small).collect();
                a = extend(&mut heap, a, &round, End::Back).expect("room");
                a = skip(&mut heap, a, round.len()).expect("room");
                let mut want: VecDeque<i64> = (round.len() as i64..1024).collect();
                want.extend(0..round.len() as i64);
                let mut kept = Vec::new();
                for x in 0..200 {
                    if held {
                        heap.own(a).expect("live");
                        kept.push((a, want.clone()));
                    }
                    a = push(&mut heap, a, small(-x), end).expect("room");
                    match end {
                        End::Back => want.push_back(-x),
                        End::Front => want.push_front(-x),
                    }
                    check(&heap, a);
                }
                assert!(ints(&heap, a).iter().eq(&want), "{turned} {end:?} {held}");
                for (from, to) in [(100, 1224), (0, 1000), (30, 900), (1, 870), (800, 869)] {
                    a = slice(&mut heap, a, from, to).expect("room");
                    want.truncate(to);
                    want.drain(..from);
                    check(&heap, a);
                    assert!(ints(&heap, a).iter().eq(&want), "{turned} {end:?} {held}");
                }
                for (k, want) in kept {
                    check(&heap, k);
                    assert!(ints(&heap, k).iter().eq(&want), "{turned} {end:?}");
                    release(&mut heap, k).expect("live");
                }
                release(&mut heap, a).expect("live");
                assert_eq!(heap.live(), 0, "{turned} {end:?} {held}");
            }
        }
    }
}

/// A join moves the shorter array onto the longer one in steps, each of which moves a bounded
/// number of elements and leaves two arrays that are the join together. An array that
/// something else holds is as it was.
#[test]
fn a_join_is_made_in_steps() {
    let sizes = [1usize, 33, 100, 2047, 2048, 2049, 4096, 5000, 9001];
    for &x in &sizes {
        for &y in &sizes {
            for (front, held) in [(0, false), (0, true), (5, true)] {
                let mut heap = Heap::default();
                let a = long(&mut heap, 0, x, front);
                let b = long(&mut heap, x as i64, y, front);
                if held {
                    heap.own(a).expect("live");
                    heap.own(b).expect("live");
                }
                let (mut l, mut r) = (stored(&heap, a), stored(&heap, b));
                let mut steps = 1;
                let all = loop {
                    match join(&mut heap, l, r).expect("room") {
                        Joined::All(all) => break all,
                        Joined::Some(one, other) => {
                            check(&heap, one);
                            check(&heap, other);
                            assert_eq!(len(&heap, one) + len(&heap, other), x + y);
                            let held = [a, b].into_iter().filter(|_| held);
                            let now = held.chain([one, other]);
                            assert_eq!(heap.audit(now), Ok(()), "{x} {y}");
                            (l, r) = (stored(&heap, one), stored(&heap, other));
                            steps += 1;
                        }
                    }
                };
                // 64 leaves, and what else is in the last of them.
                let most = x.min(y).div_ceil(2048) + 1;
                let least = x.min(y).div_ceil(2048 + 32);
                assert!((least..=most).contains(&steps), "{x} {y} {steps}");
                check(&heap, all);
                let want: Vec<i64> = (0..(x + y) as i64).collect();
                assert_eq!(ints(&heap, all), want, "{x} {y}");
                release(&mut heap, all).expect("live");
                if held {
                    for (v, from, n) in [(a, 0, x), (b, x, y)] {
                        check(&heap, v);
                        assert_eq!(ints(&heap, v), want[from..from + n]);
                        release(&mut heap, v).expect("live");
                    }
                }
                assert_eq!(heap.live(), 0, "{x} {y} {held}");
            }
        }
    }
}

/// The `n` numbers from `first` on, with `front` of them pushed at the front.
fn long(heap: &mut Heap, first: i64, n: usize, front: usize) -> Value {
    let all: Vec<Value> = (first..first + n as i64).map(small).collect();
    let (before, rest) = all.split_at(front.min(n - 1));
    let mut a = from_values(heap, rest).expect("room");
    for x in before.iter().rev() {
        a = push(heap, a, *x, End::Front).expect("room");
    }
    a
}

/// A join of long arrays whose leaves line up shares them, and makes one node for each 32.
#[test]
fn a_join_shares_the_leaves_that_line_up() {
    for (x, y) in [(4096usize, 3200usize), (3200, 4096), (1024, 1024)] {
        let mut heap = Heap::default();
        let a = long(&mut heap, 0, x, 0);
        let b = long(&mut heap, x as i64, y, 0);
        let made = heap.made();
        let all = joined(&mut heap, a, b).expect("room");
        let made = heap.made() - made;
        check(&heap, all);
        assert!(made <= 2 * x.min(y) / 32 / 32 + 12, "{x} {y}: {made}");
        for v in [a, b, all] {
            release(&mut heap, v).expect("live");
        }
        assert_eq!(heap.live(), 0);
    }
}

/// An array joined to itself again and again is as long as that says, and its leaves that
/// line up are one leaf.
#[test]
fn an_array_is_joined_to_itself_over_and_over() {
    let mut heap = Heap::default();
    let mut a = long(&mut heap, 0, 32, 0);
    for _ in 0..12 {
        heap.own(a).expect("live");
        a = concat(&mut heap, a, a).expect("room");
        check(&heap, a);
    }
    assert_eq!(len(&heap, a), 32 << 12);
    let want = (0..32).cycle().take(32 << 12);
    assert!(ints(&heap, a).into_iter().eq(want));
    assert!(heap.live() < 4200, "{}", heap.live());
    release(&mut heap, a).expect("live");
    assert_eq!(heap.live(), 0);
}

/// How an array came to be, and whether another has its head too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Form {
    Made,
    /// Its first is in its root.
    Front {
        twin: bool,
    },
    /// Its head has a number that it has lost.
    Behind {
        twin: bool,
    },
}

const FORMS: [Form; 5] = [
    Form::Made,
    Form::Front { twin: false },
    Form::Front { twin: true },
    Form::Behind { twin: false },
    Form::Behind { twin: true },
];

/// The array of `items`, whose references pass to it and the first of which is not plain, and the
/// other that has its head.
fn formed(heap: &mut Heap, items: &[Value], form: Form) -> (Value, Option<Value>) {
    let number = [small(-1)];
    let before: &[Value] = match form {
        Form::Made | Form::Front { .. } => &[],
        Form::Behind { .. } => &number,
    };
    let whole = from_values(heap, &[before, items].concat()).expect("room");
    if form == Form::Made {
        return (whole, None);
    }
    let found = uncons(heap, whole, false, false).expect("room");
    let left = found.expect("an element").1;
    let (array, other, kept) = match form {
        Form::Made => (whole, left, true),
        Form::Front { twin } => (whole, left, twin),
        Form::Behind { twin } => (left, whole, twin),
    };
    if !kept {
        release(heap, other).expect("live");
    }
    let r = root(heap, array);
    assert_eq!(
        r.start,
        if form == (Form::Front { twin: kept }) {
            FRONT
        } else {
            1
        }
    );
    assert_eq!(
        heap.unique(r.head.items.as_cell().expect("a head")),
        Ok(!kept)
    );
    (array, kept.then_some(other))
}

/// With no room for what it makes, everything that changes a tree gives up what it was handed,
/// at whichever step that is, and what something else holds is as it was.
#[test]
fn a_tree_that_has_no_room_gives_up_what_it_was_handed() {
    let (mut full, mut done) = (0, 0);
    let sizes = [33usize, 64, 97, 1060, 5000].map(|n| FORMS.map(|form| (n, form)));
    for (n, form) in sizes.into_iter().flatten() {
        if n > 1060 && form != Form::Made {
            continue;
        }
        for held in [false, true] {
            for way in 0..13 {
                for more in (0..40).map(|i| i * i * 13) {
                    let mut heap = Heap::default();
                    let strings = |heap: &mut Heap, n: usize| -> Vec<Value> {
                        let one = |i| match i % 2 {
                            0 => holder(heap, &format!("s{i}")),
                            _ => string(heap, &format!("s{i}")),
                        };
                        (0..n).map(one).collect()
                    };
                    let items = strings(&mut heap, n);
                    let (a, other) = formed(&mut heap, &items, form);
                    let items = strings(&mut heap, n - 1);
                    let b = from_values(&mut heap, &items).expect("room");
                    let x = string(&mut heap, "x");
                    let given = strings(&mut heap, 70);
                    if held {
                        heap.own(a).expect("live");
                    }
                    heap.limit_to(heap.words_used() + more);
                    let made = match way {
                        0 => push(&mut heap, a, x, End::Back),
                        1 => push(&mut heap, a, x, End::Front),
                        2 => extend(&mut heap, a, &given, End::Back),
                        3 => extend(&mut heap, a, &given, End::Front),
                        4 => set(&mut heap, a, n / 2, x),
                        5 => skip(&mut heap, a, 1),
                        6 => slice(&mut heap, a, 1, n - 1),
                        7 => slice(&mut heap, a, n / 2, n / 2 + 20),
                        8 => concat(&mut heap, a, b),
                        9 => concat(&mut heap, b, a),
                        10 => skip(&mut heap, a, 2),
                        11 => skip(&mut heap, a, 40.min(n - 1)),
                        // What it finds keeps the array that it is asked of.
                        _ => {
                            let found = uncons(&mut heap, a, true, false);
                            let found = found.map(|found| found.expect("an element"));
                            if found.is_err() || !held {
                                release(&mut heap, a).expect("live");
                            }
                            found.map(|(first, left)| {
                                release(&mut heap, first).expect("live");
                                if held {
                                    release(&mut heap, a).expect("live");
                                }
                                left
                            })
                        }
                    };
                    let what = format!("{n} {form:?} {held} {way} {more}");
                    let mut left = Vec::new();
                    left.extend(other);
                    if ![0, 1, 4].contains(&way) {
                        left.push(x);
                    }
                    if ![2, 3].contains(&way) {
                        left.extend(&given);
                    }
                    if ![8, 9].contains(&way) {
                        left.push(b);
                    }
                    if held {
                        left.push(a);
                        assert_eq!(elements(&heap, a).len(), n, "{what}");
                    }
                    match made {
                        Ok(made) => {
                            check(&heap, made);
                            left.push(made);
                            done += 1;
                        }
                        Err(why) => {
                            assert_eq!(why, Fault::Full, "{what}");
                            full += 1;
                        }
                    }
                    assert_eq!(heap.audit(left.iter().copied()), Ok(()), "{what}");
                    for v in left {
                        release(&mut heap, v).expect("live");
                    }
                    assert_eq!(heap.live(), 0, "{what}");
                }
            }
        }
    }
    assert!(full > 1000 && done > 1000, "{full} {done}");
}

/// A join that has no room for a step, or for what a step leaves, gives up both of what it has,
/// wherever that is.
#[test]
fn a_join_with_no_room_for_what_a_step_leaves_gives_up_both() {
    let mut steps = 0;
    for held in [false, true] {
        for front in [0, 7] {
            for more in (0..12_000).step_by(90) {
                let mut heap = Heap::default();
                let mut a = long(&mut heap, 0, 6000, front);
                let mut b = long(&mut heap, 6000, 5000, 3);
                let kept: Vec<Value> = [a, b].into_iter().filter(|_| held).collect();
                for v in &kept {
                    heap.own(*v).expect("live");
                }
                heap.limit_to(heap.words_used() + more);
                let made = loop {
                    let (l, r) = (stored(&heap, a), stored(&heap, b));
                    match join(&mut heap, l, r) {
                        Ok(Joined::All(all)) => break Some(all),
                        Ok(Joined::Some(l, r)) => (a, b) = (l, r),
                        Err(why) => {
                            assert_eq!(why, Fault::Full);
                            break None;
                        }
                    }
                    steps += 1;
                };
                let left = kept.iter().copied().chain(made);
                assert_eq!(heap.audit(left.clone()), Ok(()), "{held} {front} {more}");
                for v in left {
                    release(&mut heap, v).expect("live");
                }
                assert_eq!(heap.live(), 0, "{held} {front} {more}");
            }
        }
    }
    assert!(steps > 50, "{steps}");
}

/// An array that something held when it lost some of its head keeps none of them, nor any that
/// it loses at its back, when it is cut to some of that head or to all of it.
#[test]
fn a_cut_at_the_back_leaves_the_head_its_own_elements() {
    for n in [33usize, 36, 40] {
        for gone in 1..8 {
            for cut in n - 32..14 {
                let mut heap = Heap::default();
                let items: Vec<Value> = (0..n)
                    .map(|i| Value::cell(heap.make(Kind::Handle, &[i as u64]).expect("room")))
                    .collect();
                let whole = from_values(&mut heap, &items).expect("room");
                let a = without(&mut heap, whole, gone).expect("room");
                release(&mut heap, whole).expect("live");
                let left = slice(&mut heap, a, 0, n - gone - cut).expect("room");
                check(&heap, left);
                let got = elements(&heap, left);
                let want = &items[gone..n - cut];
                assert!(
                    got.iter()
                        .map(|v| v.bits())
                        .eq(want.iter().map(|v| v.bits()))
                );
                let mut released = heap.take_released();
                released.sort_unstable();
                let want: Vec<u64> = (0..gone).chain(n - cut..n).map(|i| i as u64).collect();
                assert_eq!(released, want, "{n} {gone} {cut}");
                release(&mut heap, left).expect("live");
                assert_eq!(heap.live(), 0);
            }
        }
    }
}

/// What is asked for before a change is all that it comes to need: with that much room, and
/// no freed cell to use, nothing is refused half way.
#[test]
fn a_change_takes_no_more_room_than_it_asks_for() {
    let sizes = [33usize, 1024, 1060, 33_000].map(|n| FORMS.map(|form| (n, form)));
    for (n, form) in sizes.into_iter().flatten() {
        for round in [0, 500] {
            for way in 0..11 {
                if form != Form::Made && round > 0 {
                    continue;
                }
                let mut heap = Heap::default();
                let first = holder(&mut heap, "first");
                let rest = (1..n as i64).map(small);
                let items: Vec<Value> = std::iter::once(first).chain(rest).collect();
                let (mut a, _other) = formed(&mut heap, &items, form);
                for x in 0..round {
                    a = push(&mut heap, a, small(x), End::Back).expect("room");
                    a = skip(&mut heap, a, 1).expect("room");
                }
                let m = n.min(STEP);
                let b = long(&mut heap, 0, m, 3);
                let few: Vec<Value> = (0..40).map(small).collect();
                let full = from_values(&mut heap, &few[..32]).expect("room");
                let more: Vec<Value> = (0..20_000).map(small).collect();
                let whole = heap.own(a).expect("live");
                // Nothing that was freed is there to be used.
                heap.forget_freed();
                let before = heap.words_carved();
                let asked = match way {
                    0..=2 => CHANGE,
                    3 | 4 | 8 => words_for(20_000),
                    5 => words_for(1),
                    6 | 7 => words_for(m),
                    _ => words_for(40),
                };
                heap.limit_to(heap.words_used() + asked);
                let made = match way {
                    0 => slice(&mut heap, a, n / 3, n / 2 + 40),
                    1 => set(&mut heap, a, n / 2, small(1)),
                    2 => skip(&mut heap, a, n / 2),
                    3 => extend(&mut heap, a, &more, End::Back),
                    4 => extend(&mut heap, a, &more, End::Front),
                    5 => push(&mut heap, a, small(1), End::Front),
                    6 => concat(&mut heap, a, b),
                    7 => concat(&mut heap, b, a),
                    8 => from_values(&mut heap, &more),
                    9 => extend(&mut heap, full, &few, End::Back),
                    _ => extend(&mut heap, full, &few, End::Front),
                };
                let took = heap.words_carved() - before;
                let what = format!("{n} {form:?} {round} {way}: {took} of {asked}");
                assert!(took <= asked, "{what}");
                check(&heap, whole);
                // Nor was it refused when that was all the room there was.
                check(&heap, made.expect(&what));
            }
        }
    }
}
