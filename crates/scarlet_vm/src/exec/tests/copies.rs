//! The built-in that `array::repeat` is a loop over, and the longest ranges.

use super::arrays::{asks_with, extend_taking, push_of};
use super::probes::{
    ARRAYS, Of, a_cell, all_given_up, array_of_kind, asks, counted, elements, found, in_register,
    is_array, miss, number, registers, rest_is, small,
};
use super::*;
use crate::code::MaybeHit;

const FROM: Reg = Reg(FIRST.0);
const WITH: Reg = Reg(FIRST.0 + 1);
const COUNT: Reg = Reg(FIRST.0 + 2);
const TO: Reg = Reg(FIRST.0 + 3);
const UNTIL: Reg = Reg(FIRST.0 + 4);
const AGAIN: Reg = Reg(FIRST.0 + 5);

/// What is put on.
#[derive(Debug, Clone, Copy)]
enum Copied {
    Number,
    Cell,
    /// The array that it is put on.
    Itself,
    Empty,
}

const COPIED: [Copied; 4] = [Copied::Number, Copied::Cell, Copied::Itself, Copied::Empty];

fn copied(heap: &mut Heap, what: Copied, xs: Value) -> Value {
    match what {
        Copied::Number => small(6),
        Copied::Cell => a_cell(heap, 8),
        Copied::Itself => heap.own(xs).expect("live"),
        Copied::Empty => Value::EMPTY_ARRAY,
    }
}

/// How many a call that is asked for `n` puts on.
fn at_most_1024(n: &BigInt) -> usize {
    let n = n.clone().clamp(BigInt::from(0), BigInt::from(1024));
    n.to_usize().expect("small")
}

/// The built-in on an array, what is put on it and how many, which the globals hold.
fn push_copies(m: &mut Machine<'_, '_>) -> Result<Value, Stop> {
    let [xs, x, n] = [0, 1, 2].map(|i| m.rest.globals[i]);
    let frame = registers(&[(FROM, xs), (WITH, x), (COUNT, n)]);
    let frame: [Value; WINDOW] = frame.try_into().expect("a window");
    let args = [FROM, WITH, COUNT];
    m.rest.builtin(Intrinsic::ArrayPushCopies, &frame, &args)
}

/// A machine that holds nothing.
fn on_an_empty_machine(check: impl FnOnce(&mut Machine<'_, '_>)) {
    on_a_machine(|m| {
        m.rest.heap = Heap::new(as_wide_as_you_like().0);
        m.rest.globals = Vec::new();
        check(m);
    });
}

/// An array of any kind gets as many of any value as it is asked for and no more than 1024.
/// A count below 0 is none, and one that is a big Int is as many as there may be. The array is
/// only read, so it stays as it was, and every reference there is is counted.
#[test]
fn an_array_gets_as_many_copies_as_it_is_asked_for() {
    let far: BigInt = BigInt::from(1) << 70;
    let mut counts: Vec<BigInt> = [-1, 0, 1, 3, 4, 31, 32, 33, 64, 1000, 1023, 1024, 1025]
        .map(BigInt::from)
        .to_vec();
    counts.extend([-far.clone(), far]);
    counts.extend([1i64 << 49, i64::MAX, i64::MIN].map(BigInt::from));
    for of in ARRAYS {
        for n in &counts {
            for with in COPIED {
                for held in [false, true] {
                    copies_of(of, n, with, held);
                }
            }
        }
    }
}

fn copies_of(of: Of, n: &BigInt, with: Copied, held: bool) {
    let what = format!("{of:?} and {n} of {with:?}, held {held}");
    on_an_empty_machine(|m| {
        let heap = &mut m.rest.heap;
        let (xs, mut items) = array_of_kind(heap, of);
        let x = copied(heap, with, xs);
        let n_of = number(heap, n);
        m.rest.globals = vec![xs, x, n_of];
        if held {
            let held = m.rest.heap.own(xs).expect("live");
            m.rest.globals.push(held);
        }
        let got = push_copies(m).expect("room");
        m.rest.globals.push(got);
        counted(m, &what);
        is_array(m, xs, &items, &what);
        items.extend(std::iter::repeat_n(Ok(x), at_most_1024(n)));
        is_array(m, got, &items, &what);
        counted(m, &what);
        all_given_up(m, &what);
    });
}

/// With no room for them the run stops, and what was asked for has left nothing behind.
#[test]
fn copies_that_have_no_room_are_given_up() {
    let mut full = 0;
    for of in ARRAYS {
        for n in [1, 20, 33, 1024] {
            for with in COPIED {
                let further = std::iter::successors(Some(40usize), |n| Some(n + n / 4));
                for more in (0..40).chain(further) {
                    if copies_in(of, n, with, more) {
                        break;
                    }
                    full += 1;
                    assert!(more < 90_000, "{of:?} {n}");
                }
            }
        }
    }
    assert!(full > 5000, "{full}");
}

/// Whether there was room.
fn copies_in(of: Of, n: i64, with: Copied, more: usize) -> bool {
    let what = format!("{of:?} and {n} of {with:?} in {more}");
    let mut room = false;
    on_an_empty_machine(|m| {
        let heap = &mut m.rest.heap;
        let (xs, items) = array_of_kind(heap, of);
        let x = copied(heap, with, xs);
        m.rest.globals = vec![xs, x, small(n)];
        let limit = m.rest.heap.words_used() + more;
        m.rest.heap.limit_to(limit);
        let got = push_copies(m);
        match got {
            Ok(got) => {
                room = true;
                m.rest.globals.push(got);
            }
            Err(stop) => assert_eq!(stop, Stop::HeapFull, "{what}"),
        }
        counted(m, &what);
        // There may be no room to look at what a range has.
        if !matches!(of, Of::Range(..)) {
            is_array(m, xs, &items, &what);
        }
        all_given_up(m, &what);
    });
    room
}

/// What is no array, what is no count and a value that is freed each stop the run, and nothing
/// is made that stays.
#[test]
fn copies_of_what_is_wrong_stop_the_run() {
    on_an_empty_machine(|m| {
        let heap = &mut m.rest.heap;
        let (xs, _) = array_of_kind(heap, Of::Both(40));
        let short = array::from_values(heap, &[small(1)]).expect("room");
        let wrong = a_cell(heap, 4);
        let freed = a_cell(heap, 5);
        heap.release(freed.as_cell().expect("a cell"))
            .expect("live");
        m.rest.globals = vec![xs, short, wrong];
        let cases = [
            (wrong, small(1), small(2), Broken::NotAnArray(wrong.view())),
            (
                Value::NIL,
                small(1),
                small(2),
                Broken::NotAnArray(View::Nil),
            ),
            (xs, freed, small(2), Broken::DeadCell),
            (short, freed, small(2), Broken::DeadCell),
            (freed, small(1), small(2), Broken::NotAnArray(freed.view())),
        ];
        for (xs, x, n, want) in cases {
            let frame = registers(&[(FROM, xs), (WITH, x), (COUNT, n)]);
            let frame: [Value; WINDOW] = frame.try_into().expect("a window");
            let got = m
                .rest
                .builtin(Intrinsic::ArrayPushCopies, &frame, &[FROM, WITH, COUNT]);
            assert_eq!(got.map(Value::bits), Err(Stop::broken(want)));
            counted(m, "wrong");
        }
        let frame = registers(&[(FROM, xs), (WITH, small(1)), (COUNT, Value::NIL)]);
        let frame: [Value; WINDOW] = frame.try_into().expect("a window");
        let args = [FROM, WITH, COUNT];
        let got = m.rest.builtin(Intrinsic::ArrayPushCopies, &frame, &args);
        assert!(matches!(got, Err(Stop::NotBuiltYet(_))), "{got:?}");
        counted(m, "no count");
        all_given_up(m, "wrong");
    });
}

const LONG: [(i64, i64); 5] = [
    (0, i64::MAX),
    (i64::MIN, i64::MAX),
    (i64::MIN, 0),
    (-5, 1 << 53),
    ((1 << 50) - 3, (1 << 51) + 9),
];

/// Indices at the ends of a range, at those of a small Int and past both.
fn far_indices(start: i64, end: i64) -> Vec<BigInt> {
    let len = BigInt::from(end) - start;
    let mut all = Vec::new();
    for at in [
        BigInt::from(0),
        len,
        BigInt::from(1) << 50,
        BigInt::from(-1) << 50,
        BigInt::from(1) << 63,
        BigInt::from(1) << 64,
        BigInt::from(1) << 70,
        BigInt::from(-1) << 64,
    ] {
        all.extend([-2, -1, 0, 1].map(|by| &at + by));
    }
    all
}

/// A range has an element at each index below its length, however big the index is, and none
/// anywhere else.
#[test]
fn a_range_is_read_at_an_index_that_is_a_big_int() {
    for (start, end) in LONG {
        for at in far_indices(start, end) {
            for dst in [None, Some(TO)] {
                let what = format!("{start}..{end} at {at} to {dst:?}");
                let code = asks(Ins::ArrayGet {
                    dst: MaybeHit::new(dst),
                    src: Src(FROM),
                    index: Src(WITH),
                    miss: miss(),
                });
                on_an_empty_machine(|m| {
                    let range = array::range(&mut m.rest.heap, start, end).expect("room");
                    let i = number(&mut m.rest.heap, &at);
                    let regs = registers(&[(FROM, range), (WITH, i)]);
                    let stop = run_with(m, code, &regs);
                    let want = Some(&at + start).filter(|n| at >= 0.into() && *n < end.into());
                    assert_eq!(found(&stop), want.is_some(), "{what}");
                    rest_is(m, &regs, dst.filter(|_| want.is_some()), &[], &what);
                    if let (Some(dst), Some(want)) = (dst, want) {
                        let got = match in_register(m, dst).view() {
                            View::Int(n) => BigInt::from(n),
                            View::Cell(c) => m.rest.heap.read_big_int(c),
                            other => panic!("{other:?}"),
                        };
                        assert_eq!(got, want, "{what}");
                    }
                    counted(m, &what);
                    all_given_up(m, &what);
                });
            }
        }
    }
}

/// A slice of a range may start and end at big Ints.
#[test]
fn a_range_is_cut_at_indices_that_are_big_ints() {
    for (start, end) in LONG {
        let len = BigInt::from(end) - start;
        let indices = far_indices(start, end);
        for from in &indices {
            for to in &indices {
                let what = format!("{start}..{end} from {from} to {to}");
                let code = asks(Ins::ArraySlice {
                    dst: MaybeHit::new(Some(TO)),
                    src: Arg::shared(FROM),
                    start: Src(WITH),
                    end: Src(UNTIL),
                    miss: miss(),
                });
                on_an_empty_machine(|m| {
                    let heap = &mut m.rest.heap;
                    let range = array::range(heap, start, end).expect("room");
                    let (a, b) = (number(heap, from), number(heap, to));
                    let regs = registers(&[(FROM, range), (WITH, a), (UNTIL, b)]);
                    let stop = run_with(m, code, &regs);
                    let want = *from >= 0.into() && from <= to && *to <= len;
                    assert_eq!(found(&stop), want, "{what}");
                    rest_is(m, &regs, want.then_some(TO), &[], &what);
                    if want {
                        let got = in_register(m, TO);
                        let seq = array::seq(&m.rest.heap, got);
                        match seq {
                            Some(Seq::Empty) => assert_eq!(from, to, "{what}"),
                            Some(Seq::Range { start: s, end: e }) => {
                                assert_eq!(BigInt::from(s), from + start, "{what}");
                                assert_eq!(BigInt::from(e), to + start, "{what}");
                            }
                            _ => panic!("{what}"),
                        }
                    }
                    counted(m, &what);
                    all_given_up(m, &what);
                });
            }
        }
    }
}

/// A write to a range that the heap has no room for the elements of stops the run before it
/// has made any, and one at an index that the range has not is a miss, as it is in any array.
#[test]
fn a_write_to_a_range_with_no_room_for_it_makes_nothing() {
    for (start, end) in LONG {
        for at in far_indices(start, end) {
            for moved in [false, true] {
                let what = format!("{start}..{end} at {at}, takes {moved}");
                let code = asks(Ins::ArraySet {
                    dst: Hit(TO),
                    src: Arg { reg: FROM, moved },
                    index: Src(WITH),
                    value: Arg::shared(COUNT),
                    miss: miss(),
                });
                on_an_empty_machine(|m| {
                    let range = array::range(&mut m.rest.heap, start, end).expect("room");
                    let i = number(&mut m.rest.heap, &at);
                    let x = a_cell(&mut m.rest.heap, 3);
                    let made = m.rest.heap.made();
                    let regs = registers(&[(FROM, range), (WITH, i), (COUNT, x)]);
                    let stop = run_with(m, code, &regs);
                    let there = at >= 0.into() && &at + start < end.into();
                    match there {
                        true => assert_eq!(stop, Stop::HeapFull, "{what}"),
                        false => assert!(!found(&stop), "{what}"),
                    }
                    assert_eq!(m.rest.heap.made(), made, "{what}");
                    rest_is(m, &regs, None, &[], &what);
                    counted(m, &what);
                    all_given_up(m, &what);
                });
            }
        }
    }
}

/// What builds the elements of a range.
#[derive(Debug, Clone, Copy)]
enum Builds {
    Write,
    Push(End),
    Extend(End),
    /// A join with the range on the left.
    JoinLeft,
    JoinRight,
    JoinBoth,
    Copies,
}

const BUILDS: [Builds; 9] = [
    Builds::Write,
    Builds::Push(End::Front),
    Builds::Push(End::Back),
    Builds::Extend(End::Front),
    Builds::Extend(End::Back),
    Builds::JoinLeft,
    Builds::JoinRight,
    Builds::JoinBoth,
    Builds::Copies,
];

/// Do `builds` to the range in `FROM`, with a cell in `WITH` and a short array in `COUNT`.
fn build(m: &mut Machine<'_, '_>, builds: Builds, regs: &[Value], moved: bool) -> Stop {
    let src = Arg { reg: FROM, moved };
    let x = Arg { reg: WITH, moved };
    let join = |a, b| {
        let dst = Dst(TO);
        let join = Ins::ArrayJoin { dst, a, b };
        (join, Lists::default())
    };
    let (ins, lists) = match builds {
        Builds::Write => {
            let write = Ins::ArraySet {
                dst: Hit(TO),
                src,
                index: Src(UNTIL),
                value: x,
                miss: miss(),
            };
            (write, Lists::default())
        }
        Builds::Push(end) => push_of(src, x, end),
        Builds::Extend(end) => extend_taking(src, &[x, Arg::shared(COUNT)], end),
        Builds::JoinLeft => join(Taken(FROM), Taken(COUNT)),
        Builds::JoinRight => join(Taken(COUNT), Taken(FROM)),
        Builds::JoinBoth => join(Taken(FROM), Taken(AGAIN)),
        Builds::Copies => {
            m.rest.globals = [FROM, WITH, UNTIL].map(|r| regs[usize::from(r.0)]).into();
            return push_copies(m).expect_err("no room");
        }
    };
    run_with(m, asks_with(ins, lists), regs)
}

/// Whatever builds the elements of a range that the heap has no room for, the run stops with
/// nothing made, and whatever it was handed is given up once.
#[test]
fn a_range_with_no_room_for_it_makes_nothing() {
    let short = [(0, 2000), ((1 << 50) - 1000, (1 << 50) + 1000)];
    for (start, end) in LONG.into_iter().chain(short) {
        for builds in BUILDS {
            for moved in [false, true] {
                for held in [false, true] {
                    let what = format!("{start}..{end} {builds:?} {moved} {held}");
                    on_an_empty_machine(|m| {
                        let heap = &mut m.rest.heap;
                        let range = array::range(heap, start, end).expect("room");
                        heap.own(range).expect("live");
                        let x = a_cell(heap, 3);
                        let few = array::from_values(heap, &[small(1)]).expect("room");
                        let mut regs = registers(&[(WITH, x), (COUNT, few), (UNTIL, small(5))]);
                        let copies = matches!(builds, Builds::Copies);
                        for reg in [FROM, AGAIN] {
                            regs[usize::from(reg.0)] = range;
                        }
                        // The last one is run on the globals, which are all that holds these.
                        if copies {
                            for v in [few, range] {
                                heap.release(v.as_cell().expect("a cell")).expect("live");
                            }
                        }
                        if held && !copies {
                            let more = heap.own(range).expect("live");
                            m.rest.globals.push(more);
                        }
                        let limit = m.rest.heap.words_used() + 2000;
                        m.rest.heap.limit_to(limit);
                        let made = m.rest.heap.made();
                        let room = m.rest.heap.arena_room();
                        assert_eq!(build(m, builds, &regs, moved), Stop::HeapFull, "{what}");
                        assert_eq!(m.rest.heap.made(), made, "{what}");
                        assert_eq!(m.rest.heap.arena_room(), room, "{what}");
                        counted(m, &what);
                        all_given_up(m, &what);
                    });
                }
            }
        }
    }
}

/// The room for a range is asked for once, before any of its elements are made, and it is
/// enough for all of them. It is not much more than they take.
#[test]
fn a_range_is_refused_at_once_or_built() {
    const N: i64 = 2000;
    let big = 1 << 50;
    for (start, bigs) in [
        (0, 0),
        (big - N, 0),
        (big - N + 1, 1),
        (big - 700, 1300),
        (big, N),
        (i64::MAX - N, N),
        (-big, 0),
        (-big - 1, 1),
        (-big - 700, 700),
        (-big - N, N),
        (i64::MIN, N),
    ] {
        let takes = N + 3 * bigs;
        let mut full = 0;
        let mut built = 0;
        for more in (takes - 40..takes + 5000).step_by(40) {
            let what = format!("{start} in {more}");
            on_an_empty_machine(|m| {
                let heap = &mut m.rest.heap;
                heap.limit_to(heap.words_used() + more as usize);
                let (made, room) = (heap.made(), heap.arena_room());
                match m.rest.built(start, start + N) {
                    Ok(array) => {
                        built += 1;
                        let room = m.rest.heap.words_used() + 100_000;
                        m.rest.heap.limit_to(room);
                        m.rest.globals.push(array.value());
                        let items: Vec<_> = (start..start + N).map(Err).collect();
                        is_array(m, array.value(), &items, &what);
                    }
                    Err(stop) => {
                        full += 1;
                        assert_eq!(built, 0, "{what}");
                        assert_eq!(stop, Stop::HeapFull, "{what}");
                        assert_eq!(m.rest.heap.made(), made, "{what}");
                        assert_eq!(m.rest.heap.arena_room(), room, "{what}");
                    }
                }
                counted(m, &what);
                all_given_up(m, &what);
            });
        }
        assert!(full > 0 && built > 0, "{start}: {full} {built}");
    }
}

/// An array that is all one value, as `array::repeat` makes it, is the array that as many
/// pushes make, and holds its value once for each element.
#[test]
fn calls_one_after_the_other_make_a_long_array() {
    for n in [1025usize, 2048, 5000, 33_000] {
        on_an_empty_machine(|m| {
            let x = a_cell(&mut m.rest.heap, 8);
            m.rest.globals = vec![Value::EMPTY_ARRAY, x, Value::NIL];
            let mut left = n as i64;
            while left > 0 {
                m.rest.globals[2] = small(left);
                let got = push_copies(m).expect("room");
                let before = std::mem::replace(&mut m.rest.globals[0], got);
                m.rest.release(before).expect("live");
                left -= 1024;
            }
            let got = elements(m, m.rest.globals[0]);
            assert_eq!(got.len(), n);
            for v in got {
                assert_eq!(v.bits(), x.bits());
                m.rest.release(v).expect("live");
            }
            counted(m, "a long one");
            all_given_up(m, "a long one");
        });
    }
}

/// A count that no heap has room for is stopped by the heap's limit, a bounded step at a time.
#[test]
fn calls_without_end_stop_at_the_limit_of_the_heap() {
    for with in COPIED {
        on_an_empty_machine(|m| {
            let x = copied(&mut m.rest.heap, with, Value::EMPTY_ARRAY);
            let n = number(&mut m.rest.heap, &(BigInt::from(1) << 70));
            m.rest.globals = vec![Value::EMPTY_ARRAY, x, n];
            m.rest.heap.limit_to(40_000);
            let mut calls = 0;
            let stop = loop {
                match push_copies(m) {
                    Ok(got) => {
                        let before = std::mem::replace(&mut m.rest.globals[0], got);
                        m.rest.release(before).expect("live");
                    }
                    Err(stop) => break stop,
                }
                calls += 1;
                assert!(calls < 40, "{with:?}");
            };
            assert_eq!(stop, Stop::HeapFull);
            assert!(calls > 20, "{with:?}: {calls}");
            let seq = array::seq(&m.rest.heap, m.rest.globals[0]);
            let len = seq.map(|s| s.len(&m.rest.heap));
            assert_eq!(len, Some(calls * 1024));
            counted(m, "full");
            all_given_up(m, "full");
        });
    }
}
