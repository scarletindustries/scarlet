//! The instructions that take an array apart and that push onto one.

use super::probes::{
    ARRAYS, Of, a_cell, all_given_up, array_of_kind, asks, counted, elements, found, in_register,
    is_array, is_element, miss, registers, rest_is, small,
};
use super::*;
use crate::code::{Hits, MaybeHit, Near};
use crate::heap::AuditFailure;

const FROM: Reg = Reg(FIRST.0);
const WITH: Reg = Reg(FIRST.0 + 1);
const AWAY: Reg = Reg(FIRST.0 + 2);
/// Where what is found goes, the tail last.
const TO: [Reg; 7] = [
    Reg(FIRST.0 + 3),
    Reg(FIRST.0 + 4),
    Reg(FIRST.0 + 5),
    Reg(FIRST.0 + 6),
    Reg(FIRST.0 + 7),
    Reg(FIRST.0 + 8),
    Reg(FIRST.0 + 9),
];
const TAIL: Reg = Reg(FIRST.0 + 10);

fn len_of_kind(of: Of) -> usize {
    match of {
        Of::Both(n) => n,
        Of::Range(start, end) => usize::try_from(end - start).unwrap_or(0),
    }
}

/// An array as it may have come to be: one leaf or a tree, with elements left behind in
/// front, or in both of its buffers.
#[derive(Debug, Clone, Copy)]
enum Grown {
    AsItIs,
    /// Two were in front of it, and were cut off.
    Cut,
    /// Its first three were pushed there one by one.
    Pushed,
}

fn grown(heap: &mut Heap, of: Of, how: Grown) -> (Value, Vec<Result<Value, i64>>) {
    let (whole, mut items) = array_of_kind(heap, of);
    let (Of::Both(n), Some(cell)) = (of, whole.as_cell()) else {
        return (whole, items);
    };
    match how {
        Grown::AsItIs => (whole, items),
        Grown::Cut if n > 2 => {
            let left = array::skip_owned(heap, cell, 2).expect("room");
            items.drain(..2);
            (Value::cell(left), items)
        }
        Grown::Pushed if n > 0 => {
            let mut a = cell;
            for i in 0..3 {
                let x = a_cell(heap, 900 + i);
                a = array::push_owned(heap, a, x, End::Front).expect("room");
                items.insert(0, Ok(x));
            }
            (Value::cell(a), items)
        }
        Grown::Cut | Grown::Pushed => (whole, items),
    }
}

/// Where each value that a test asks for goes, and who holds the array.
#[derive(Debug, Clone, Copy)]
struct Case {
    of: Of,
    how: Grown,
    /// Whether the instruction takes the array.
    moved: bool,
    /// Whether something else holds it too.
    held: bool,
}

fn cases() -> Vec<Case> {
    let mut all = Vec::new();
    for of in ARRAYS {
        for how in [Grown::AsItIs, Grown::Cut, Grown::Pushed] {
            for bits in 0..4 {
                all.push(Case {
                    of,
                    how,
                    moved: bits & 1 != 0,
                    held: bits & 2 != 0,
                });
            }
        }
    }
    all
}

/// A machine that is about to run an instruction on the array of a case. The array is in
/// [`FROM`], and each register that may be written holds a cell of its own.
fn set_up(
    m: &mut Machine<'_, '_>,
    code: &'static Code,
    case: Case,
) -> (Vec<Value>, Vec<Result<Value, i64>>) {
    m.rest.heap = Heap::new(code.ctors.clone());
    m.rest.globals = Vec::new();
    set_up_in(m, case)
}

/// The same, on the heap that the machine has.
fn set_up_in(m: &mut Machine<'_, '_>, case: Case) -> (Vec<Value>, Vec<Result<Value, i64>>) {
    let heap = &mut m.rest.heap;
    let (whole, items) = grown(heap, case.of, case.how);
    if case.held {
        heap.own(whole).expect("live");
        m.rest.globals.push(whole);
    }
    let heap = &mut m.rest.heap;
    let mut given = vec![(FROM, whole), (AWAY, a_cell(heap, 77))];
    for to in TO.iter().chain([&TAIL]) {
        given.push((*to, a_cell(heap, 88)));
    }
    (registers(&given), items)
}

/// What a taken array and what is found leave: every register as it was but those
/// written, each of which lost what it held, and the kept array as it was.
fn checked(
    m: &mut Machine<'_, '_>,
    before: &[Value],
    case: Case,
    written: &[Reg],
    items: &[Result<Value, i64>],
    what: &str,
) {
    counted(m, what);
    for (i, v) in before.iter().enumerate().skip(usize::from(FIRST.0)) {
        if written.iter().any(|w| usize::from(w.0) == i) {
            continue;
        }
        let gone = case.moved && i == usize::from(FROM.0);
        let want = if gone { Value::NIL } else { *v };
        assert_eq!(m.stack[i].bits(), want.bits(), "{what}: r{i}");
    }
    if case.held {
        let kept = m.rest.globals[0];
        is_array(m, kept, items, what);
    }
}

/// The first element and the others go where they are to, each with a reference of its
/// own. An array that is taken and that nothing else holds is the tail itself. An array
/// with no element is as it was.
#[test]
fn an_array_is_taken_apart_into_its_first_element_and_the_others() {
    for case in cases() {
        for head in [None, Some(TO[0]), Some(FROM)] {
            for tail in [TAIL, FROM] {
                // A register that is read after it is none of the results'.
                if (head == Some(tail)) || (!case.moved && [head, Some(tail)].contains(&Some(FROM)))
                {
                    continue;
                }
                let code = asks(Ins::ArrayUncons {
                    head: MaybeHit::new(head),
                    tail: Hit(tail),
                    src: Arg {
                        reg: FROM,
                        moved: case.moved,
                    },
                    miss: miss(),
                });
                on_a_machine(|m| {
                    let what = format!("{case:?} {head:?} {tail:?}");
                    let (before, items) = set_up(m, code, case);
                    let made = m.rest.heap.made();
                    let stop = run_with(m, code, &before);
                    if !found(&stop) {
                        assert!(items.is_empty(), "{what}");
                        rest_is(m, &before, None, &[], &what);
                        counted(m, &what);
                        all_given_up(m, &what);
                        return;
                    }
                    assert!(!items.is_empty(), "{what}");
                    let big = matches!(case.of, Of::Range(..));
                    let alone = case.moved && !case.held && !big;
                    let in_front = matches!(case.how, Grown::Pushed) && items.len() > 3;
                    if alone && in_front {
                        assert_eq!(m.rest.heap.made(), made, "{what}");
                    }
                    let mut written: Vec<Reg> = head.into_iter().collect();
                    written.push(tail);
                    checked(m, &before, case, &written, &items, &what);
                    let left = in_register(m, tail);
                    is_array(m, left, &items[1..], &what);
                    if let Some(head) = head {
                        let first = in_register(m, head);
                        assert!(is_element(m, first, &items[0]), "{what}");
                    }
                    all_given_up(m, &what);
                });
            }
        }
    }
}

/// A run over `len` elements, or that and more, that asks for those from `from` on.
#[derive(Debug, Clone, Copy)]
struct Asked {
    len: usize,
    more: bool,
    from: usize,
    /// Bit `i` says whether element `from + i` is asked for.
    wanted: u8,
    tail: bool,
}

/// The registers for the elements of [`Asked`].
fn places(asked: Asked, first: Option<Reg>) -> Vec<Option<Reg>> {
    let n = usize::try_from(8 - asked.wanted.leading_zeros()).expect("few");
    let mut regs = first.into_iter().chain(TO);
    let one = |i: usize| (asked.wanted >> i & 1 == 1).then(|| regs.next().expect("enough"));
    (0..n).map(one).collect()
}

fn split_of(asked: Asked, src: Arg<Reg>, first: Option<Reg>) -> (Ins<Reg>, Lists<Reg>) {
    let length = Length {
        len: u32::try_from(asked.len).expect("small"),
        more: asked.more,
    };
    let from = u32::try_from(asked.from).expect("small");
    let split = Split { length, from };
    let hits: Vec<_> = places(asked, first)
        .into_iter()
        .map(MaybeHit::new)
        .collect();
    let tail = MaybeHit::new(asked.tail.then_some(TAIL));
    if let (Some(near), Some(elements)) = (Near::new(split), Hits::new(&hits)) {
        let ins = Ins::ArraySplit {
            elements,
            tail,
            src,
            split: near,
            miss: miss(),
        };
        return (ins, Lists::default());
    }
    let lists = Lists {
        splits: vec![SplitWide {
            split,
            elements: hits.into(),
        }],
        ..Lists::default()
    };
    let elements = HitList(0);
    let ins = Ins::ArraySplitWide {
        elements,
        tail,
        src,
        miss: miss(),
    };
    (ins, lists)
}

fn extend_of(src: Arg<Reg>, items: &[Reg], end: End) -> (Ins<Reg>, Lists<Reg>) {
    let ins = Ins::ArrayExtend {
        dst: Dst(TAIL),
        src,
        items: crate::code::ShareList(0),
        end,
    };
    let lists = Lists {
        regs: vec![items.into()],
        ..Lists::default()
    };
    (ins, lists)
}

fn push_of(src: Arg<Reg>, item: Arg<Reg>, end: End) -> (Ins<Reg>, Lists<Reg>) {
    let ins = Ins::ArrayPush {
        dst: Dst(TAIL),
        src,
        item,
        end,
    };
    (ins, Lists::default())
}

fn uncons_of(head: Option<Reg>, src: Arg<Reg>) -> (Ins<Reg>, Lists<Reg>) {
    let ins = Ins::ArrayUncons {
        head: MaybeHit::new(head),
        tail: Hit(TAIL),
        src,
        miss: miss(),
    };
    (ins, Lists::default())
}

/// A function that does `ins` and then stops, as [`asks`].
fn asks_with(ins: Ins<Reg>, lists: Lists<Reg>) -> &'static Code {
    let body = vec![ins, Ins::Bad { why: WhyId(0) }, Ins::Bad { why: WhyId(9) }];
    function_of(ReadsClosure::Nothing, 0, 60, body, lists, ctors().0)
}

/// Everything a split of an array of `n` elements may ask that is worth asking.
fn asked_of(n: usize) -> Vec<Asked> {
    let mut all = Vec::new();
    let lens = [0, 1, 2, 3, 5, 31, 32, 33, n.saturating_sub(1), n, n + 1];
    for (i, len) in lens.into_iter().enumerate() {
        for (more, tail) in [(false, false), (true, false), (true, true)] {
            for wanted in [0u8, 0b1, 0b10, 0b101, 0b1111, 0b1_1011, 0b101_0101] {
                for from in [0, 1, 4, 30, len.saturating_sub(2)] {
                    let n = usize::try_from(8 - wanted.leading_zeros()).expect("few");
                    let asked = Asked {
                        len,
                        more,
                        from,
                        wanted,
                        tail,
                    };
                    if from + n <= len && (i + from + usize::from(wanted)) % 3 != 0 {
                        all.push(asked);
                    }
                }
            }
        }
    }
    all
}

/// A split finds the elements it asks for, and what is behind those that it counts, when
/// the array has the length and not when it has another. It takes the array or leaves it,
/// and a register it writes may be the one it takes.
#[test]
fn an_array_is_split_when_it_has_the_length() {
    let mut hits = 0;
    for case in cases() {
        let n = len_of_kind(case.of);
        for (k, asked) in asked_of(n).into_iter().enumerate() {
            let first = (case.moved && k % 2 == 0).then_some(FROM);
            let src = Arg {
                reg: FROM,
                moved: case.moved,
            };
            let (ins, lists) = split_of(asked, src, first);
            let hit = Ins::Bad { why: WhyId(0) };
            let other = Ins::Bad { why: WhyId(9) };
            let code = function_of(
                ReadsClosure::Nothing,
                0,
                20,
                vec![ins, hit, other],
                lists,
                ctors().0,
            );
            on_a_machine(|m| {
                let what = format!("{case:?} {asked:?}");
                let (before, items) = set_up(m, code, case);
                let has = items.len();
                let fits = has == asked.len || (asked.more && has > asked.len);
                let stop = run_with(m, code, &before);
                assert_eq!(found(&stop), fits, "{what}");
                if !fits {
                    rest_is(m, &before, None, &[], &what);
                    counted(m, &what);
                    all_given_up(m, &what);
                    return;
                }
                hits += 1;
                let to = places(asked, first);
                let mut written: Vec<Reg> = to.iter().copied().flatten().collect();
                if asked.tail {
                    written.push(TAIL);
                }
                checked(m, &before, case, &written, &items, &what);
                for (i, to) in to.iter().enumerate() {
                    if let Some(to) = to {
                        let v = in_register(m, *to);
                        assert!(is_element(m, v, &items[asked.from + i]), "{what}: {i}");
                    }
                }
                if asked.tail {
                    let left = in_register(m, TAIL);
                    is_array(m, left, &items[asked.len..], &what);
                }
                all_given_up(m, &what);
            });
        }
    }
    assert!(hits > 5000, "{hits}");
}

/// A test of a length goes on where it holds and jumps where it does not, and leaves
/// everything as it was. So does the length itself.
#[test]
fn a_length_is_asked_for_and_nothing_changes() {
    for case in cases().into_iter().filter(|c| !c.moved) {
        let n = len_of_kind(case.of);
        for len in [0, 1, 2, n.saturating_sub(1), n, n + 1] {
            for more in [false, true] {
                let length = Length {
                    len: u32::try_from(len).expect("small"),
                    more,
                };
                let code = asks(Ins::JumpUnlessLength {
                    src: Src(FROM),
                    length,
                    to: miss(),
                });
                on_a_machine(|m| {
                    let what = format!("{case:?} {length:?}");
                    let (before, items) = set_up(m, code, case);
                    let stop = run_with(m, code, &before);
                    let has = items.len();
                    assert_eq!(found(&stop), has == len || (more && has > len), "{what}");
                    rest_is(m, &before, None, &[], &what);
                    checked(m, &before, case, &[], &items, &what);
                    all_given_up(m, &what);
                });
            }
        }
        let code = asks(len_of(1, 0));
        on_a_machine(|m| {
            let what = format!("{case:?}");
            let (mut before, items) = set_up(m, code, case);
            let (from, to) = (usize::from(r(0).0), usize::from(r(1).0));
            before.swap(from, usize::from(FROM.0));
            before[to] = Value::NIL;
            run_with(m, code, &before);
            assert_eq!(
                m.stack[to].bits(),
                small(items.len() as i64).bits(),
                "{what}"
            );
        });
    }
}

/// Each of these stops when what it is given is no array, and nothing has changed.
#[test]
fn what_is_no_array_is_not_taken_apart() {
    let src = Arg::moved(FROM);
    let asked = |len, wanted, tail| Asked {
        len,
        more: true,
        from: 0,
        wanted,
        tail,
    };
    let mut all = vec![
        (split_of(asked(3, 0b101, false), src, None)),
        (split_of(asked(3, 0b101, true), src, None)),
        (split_of(asked(7, 0b101_0101, true), src, None)),
    ];
    let others = [
        Ins::ArrayUncons {
            head: MaybeHit::new(Some(TO[0])),
            tail: Hit(TAIL),
            src,
            miss: miss(),
        },
        Ins::JumpUnlessLength {
            src: Src(FROM),
            length: Length { len: 0, more: true },
            to: miss(),
        },
        Ins::ArrayLen {
            dst: Dst(TO[0]),
            src: Src(FROM),
        },
        Ins::ArrayPush {
            dst: Dst(TO[0]),
            src,
            item: Arg::moved(AWAY),
            end: End::Back,
        },
    ];
    all.extend(others.map(|ins| (ins, Lists::default())));
    all.push(extend_of(src, &[AWAY, AWAY], End::Front));
    for (ins, lists) in all {
        let body = vec![ins, Ins::Bad { why: WhyId(0) }, Ins::Bad { why: WhyId(9) }];
        let code = function_of(ReadsClosure::Nothing, 0, 20, body, lists, ctors().0);
        on_a_machine(|m| {
            m.rest.heap = Heap::new(code.ctors.clone());
            m.rest.globals = Vec::new();
            let (other, item) = (a_cell(&mut m.rest.heap, 1), a_cell(&mut m.rest.heap, 2));
            for wrong in [other, small(7), Value::NIL] {
                let before = registers(&[(FROM, wrong), (AWAY, item)]);
                let stop = run_with(m, code, &before);
                assert_eq!(
                    stop,
                    Stop::broken(Broken::NotAnArray(wrong.view())),
                    "{ins:?}"
                );
                rest_is(m, &before, None, &[], &format!("{ins:?}"));
            }
            for v in [other, item] {
                m.rest.release(v).expect("live");
            }
            assert_eq!(m.rest.heap.live(), 0);
        });
    }
    // One whose list is not there stops too.
    let code = asks(Ins::ArraySplitWide {
        elements: HitList(3),
        tail: MaybeHit::new(None),
        src,
        miss: miss(),
    });
    on_a_machine(|m| {
        let before = registers(&[(FROM, small(1))]);
        assert_eq!(run_with(m, code, &before), Stop::broken(Broken::NoPlace));
    });
}

/// A push puts its item at the end, whoever holds the array and the item, and a kept
/// array stays as it was. One onto an array that is taken and held by nothing else has
/// its root.
#[test]
fn an_item_is_pushed_at_either_end() {
    let holds = |bits: u8, bit: u8| bits >> bit & 1 == 1;
    for case in cases() {
        for end in [End::Front, End::Back] {
            for bits in 0..8u8 {
                let (item_moved, same_dst, many) = (holds(bits, 0), holds(bits, 1), holds(bits, 2));
                let dst = if same_dst && case.moved { FROM } else { TAIL };
                let item = Arg {
                    reg: WITH,
                    moved: item_moved,
                };
                let src = Arg {
                    reg: FROM,
                    moved: case.moved,
                };
                let dst = Dst(dst);
                let ins = match many {
                    false => Ins::ArrayPush {
                        dst,
                        src,
                        item,
                        end,
                    },
                    true => Ins::ArrayExtend {
                        dst,
                        src,
                        items: crate::code::ShareList(0),
                        end,
                    },
                };
                let items_of: Vec<Reg> = [WITH; 3].into_iter().chain([AWAY]).collect();
                let lists = Lists {
                    regs: vec![items_of.into()],
                    ..Lists::default()
                };
                let body = vec![ins, Ins::Bad { why: WhyId(0) }];
                let code = function_of(ReadsClosure::Nothing, 0, 20, body, lists, ctors().0);
                on_a_machine(|m| {
                    let what = format!("{case:?} {ins:?}");
                    let (mut before, mut items) = set_up(m, code, case);
                    let x = a_cell(&mut m.rest.heap, 5);
                    before[usize::from(WITH.0)] = x;
                    let away = before[usize::from(AWAY.0)];
                    let stop = run_with(m, code, &before);
                    assert_eq!(stop, Stop::broken(Broken::NoArm), "{what}");
                    let kept = items.clone();
                    let new: Vec<_> = match many {
                        false => vec![Ok(x)],
                        true => [Ok(x); 3].into_iter().chain([Ok(away)]).collect(),
                    };
                    match end {
                        End::Front => items.splice(0..0, new),
                        End::Back => items.splice(kept.len().., new),
                    };
                    let mut written = vec![dst.0];
                    if item_moved && !many {
                        written.push(WITH);
                        assert_eq!(in_register(m, WITH).bits(), Value::NIL.bits(), "{what}");
                    }
                    checked(m, &before, case, &written, &kept, &what);
                    let pushed = in_register(m, dst.0);
                    is_array(m, pushed, &items, &what);
                    all_given_up(m, &what);
                });
            }
        }
    }
}

/// An item that is the array it is pushed onto is pushed, however many of the two
/// operands the instruction takes.
#[test]
fn an_array_may_be_pushed_onto_itself() {
    for moved in [false, true] {
        let code = asks(Ins::ArrayPush {
            dst: Dst(TAIL),
            src: Arg::shared(FROM),
            item: Arg { reg: FROM, moved },
            end: End::Front,
        });
        on_a_machine(|m| {
            m.rest.heap = Heap::new(code.ctors.clone());
            m.rest.globals = Vec::new();
            let (whole, items) = array_of_kind(&mut m.rest.heap, Of::Both(5));
            let before = registers(&[(FROM, whole)]);
            run_with(m, code, &before);
            counted(m, "itself");
            let pushed = in_register(m, TAIL);
            let got = elements(m, pushed);
            assert_eq!(got.len(), 6);
            assert_eq!(got[0].bits(), whole.bits());
            for v in got {
                m.rest.release(v).expect("live");
            }
            is_array(m, whole, &items, "itself");
            all_given_up(m, "itself");
        });
    }
}

/// Where a reference cannot be added, because the cell is freed, the run stops.
#[test]
fn a_freed_element_stops_the_run() {
    let split = |wanted, tail| Asked {
        len: 8,
        more: true,
        from: 0,
        wanted,
        tail,
    };
    for moved in [false, true] {
        let src = Arg { reg: FROM, moved };
        let mut all = vec![uncons_of(Some(TO[0]), src)];
        for asked in [
            split(0b1, false),
            split(0b11, true),
            split(0b111_1111, false),
        ] {
            all.push(split_of(asked, src, None));
        }
        for (ins, lists) in all {
            let code = asks_with(ins, lists);
            for how in [Grown::AsItIs, Grown::Pushed] {
                on_a_machine(|m| {
                    m.rest.heap = Heap::new(code.ctors.clone());
                    m.rest.globals = Vec::new();
                    let heap = &mut m.rest.heap;
                    let freed = Value::cell(a_freed_cell(heap, 2));
                    let mut whole = array::from_values(heap, &[freed; 9]).expect("room");
                    if matches!(how, Grown::Pushed) {
                        for _ in 0..9 {
                            whole =
                                array::push_owned(heap, whole, freed, End::Front).expect("room");
                        }
                    }
                    let before = registers(&[(FROM, Value::cell(whole))]);
                    let stop = run_with(m, code, &before);
                    assert_eq!(stop, Stop::broken(Broken::DeadCell), "{ins:?} {how:?}");
                });
            }
        }
        // So does an item that is pushed.
        for end in [End::Front, End::Back] {
            for (ins, lists) in [
                push_of(src, Arg::shared(WITH), end),
                extend_of(src, &[AWAY, WITH], end),
            ] {
                let code = asks_with(ins, lists);
                on_a_machine(|m| {
                    m.rest.heap = Heap::new(code.ctors.clone());
                    m.rest.globals = Vec::new();
                    let heap = &mut m.rest.heap;
                    let (whole, _) = array_of_kind(heap, Of::Both(5));
                    let away = a_cell(heap, 3);
                    let freed = Value::cell(a_freed_cell(heap, 2));
                    let before = registers(&[(FROM, whole), (WITH, freed), (AWAY, away)]);
                    let stop = run_with(m, code, &before);
                    assert_eq!(stop, Stop::broken(Broken::DeadCell), "{ins:?}");
                });
            }
        }
    }
}

/// An array that does not have the elements that its root counts stops the run that asks
/// for them, and says which is not there.
#[test]
fn an_element_that_is_counted_and_not_there_stops_the_run() {
    for (n, wanted, from, missing) in [
        (9usize, 0b11u8, 8usize, 9usize),
        (9, 0b1, 11, 11),
        (40, 0b111_1111, 36, 40),
        (40, 0b1010, 37, 40),
    ] {
        let asked = Asked {
            len: 45,
            more: true,
            from,
            wanted,
            tail: false,
        };
        let (ins, lists) = split_of(asked, Arg::shared(FROM), None);
        let code = asks_with(ins, lists);
        on_a_machine(|m| {
            m.rest.heap = Heap::new(code.ctors.clone());
            m.rest.globals = Vec::new();
            let items: Vec<Value> = (0..n).map(|i| small(i as i64)).collect();
            let whole = array::from_values(&mut m.rest.heap, &items).expect("room");
            m.rest.heap.data_mut(whole)[0] = 50;
            let before = registers(&[(FROM, Value::cell(whole))]);
            let stop = run_with(m, code, &before);
            let want = Broken::NoArrayElement { index: missing };
            assert_eq!(stop, Stop::broken(want), "{ins:?}");
            rest_is(m, &before, None, &[], "a short array");
        });
    }
}

/// Nothing that the registers and the globals hold has fewer references than it should.
/// There may be some too many.
fn none_lost(m: &Machine<'_, '_>, what: &str) {
    let roots = m.stack.iter().chain(&m.rest.globals).copied();
    match m.rest.heap.audit(roots) {
        Ok(()) => {}
        Err(AuditFailure::Found(AuditFault::Miscounted { count, holders, .. })) => {
            assert!(i64::from(count) > i64::from(holders), "{what}");
        }
        Err(other) => panic!("{what}: {other:?}"),
    }
}

/// With no room for what it makes, each of these stops. The array stays in its register,
/// or the register has given it up, and no register names a cell that is gone. What is
/// kept elsewhere is as it was.
#[test]
fn an_instruction_with_no_room_leaves_every_register_right() {
    let arrays = [
        Of::Both(8),
        Of::Both(33),
        Of::Both(70),
        Of::Range(-3, 9),
        Of::Range((1 << 50) - 4, (1 << 50) + 9),
        Of::Range(i64::MAX - 9, i64::MAX),
    ];
    let asked = |wanted, from, tail| Asked {
        len: 8,
        more: true,
        from,
        wanted,
        tail,
    };
    let many: Vec<Reg> = [WITH, AWAY].into_iter().cycle().take(40).collect();
    let mut full = 0;
    for of in arrays {
        for how in [Grown::AsItIs, Grown::Cut, Grown::Pushed] {
            for bits in 0..8u8 {
                let [moved, held, item_moved] = [0, 1, 2].map(|i| bits >> i & 1 == 1);
                let case = Case {
                    of,
                    how,
                    moved,
                    held,
                };
                let src = Arg { reg: FROM, moved };
                let item = Arg {
                    reg: WITH,
                    moved: item_moved,
                };
                let mut all = vec![push_of(src, item, End::Front)];
                all.push(push_of(src, item, End::Back));
                // The item is in the others' registers too.
                if !item_moved {
                    all.extend([
                        uncons_of(None, src),
                        uncons_of(Some(TO[0]), src),
                        split_of(asked(0b11, 0, true), src, None),
                        split_of(asked(0b1011, 4, true), src, None),
                        split_of(asked(0b101_0101, 1, true), src, None),
                        split_of(asked(0b111_1111, 0, false), src, None),
                        extend_of(src, &many[..3], End::Front),
                        extend_of(src, &many[..3], End::Back),
                        extend_of(src, &many, End::Front),
                        extend_of(src, &many, End::Back),
                    ]);
                }
                for (ins, lists) in all {
                    full += with_no_room(case, ins, lists, item_moved);
                }
            }
        }
    }
    assert!(full > 2000, "{full}");
}

/// Run `ins` on heaps with a little more room each time, until one has enough. Gives how
/// many had not.
fn with_no_room(case: Case, ins: Ins<Reg>, lists: Lists<Reg>, item_moved: bool) -> usize {
    let code = asks_with(ins, lists);
    let mut full = 0;
    for more in 0.. {
        let mut done = false;
        on_a_machine(|m| {
            let what = format!("{case:?} {ins:?} {more}");
            set_up(m, code, case);
            a_cell(&mut m.rest.heap, 5);
            let limit = m.rest.heap.words_used() + more;
            m.rest.heap = Heap::limited_for(limit, code.ctors.clone());
            m.rest.globals = Vec::new();
            let (mut before, items) = set_up_in(m, case);
            before[usize::from(WITH.0)] = a_cell(&mut m.rest.heap, 5);
            let stop = run_with(m, code, &before);
            if stop != Stop::HeapFull {
                found(&stop);
                done = true;
                return;
            }
            full += 1;
            // One that adds many leaves stops between two of them, and what it has of the
            // array by then is lost. Nothing can come by that any more.
            if !matches!(ins, Ins::ArrayExtend { .. }) {
                none_lost(m, &what);
            }
            for (i, v) in before.iter().enumerate().skip(usize::from(FIRST.0)) {
                let from = case.moved && i == usize::from(FROM.0);
                let with = item_moved && i == usize::from(WITH.0);
                let got = m.stack[i].bits();
                let gone = (from || with) && got == Value::NIL.bits();
                assert!(gone || got == v.bits(), "{what}: r{i}");
            }
            // There may be no room to look at what a range has.
            if matches!(case.of, Of::Both(_)) {
                let left = in_register(m, FROM);
                if left.as_cell().is_some() {
                    is_array(m, left, &items, &what);
                }
                if case.held {
                    let kept = m.rest.globals[0];
                    is_array(m, kept, &items, &what);
                }
            }
            for v in std::mem::take(&mut m.stack) {
                m.rest.release(v).expect("live");
            }
            for v in std::mem::take(&mut m.rest.globals) {
                m.rest.release(v).expect("live");
            }
            let pushes = matches!(ins, Ins::ArrayPush { .. } | Ins::ArrayExtend { .. });
            if matches!(case.of, Of::Range(..)) && !pushes {
                assert_eq!(m.rest.heap.live(), 0, "{what}");
            }
        });
        if done {
            return full;
        }
        assert!(more < 2000, "{case:?} {ins:?}");
    }
    full
}

/// What is given to a socket to write in parts is an array, and anything else stops the
/// run, even one that was an array and is freed.
#[test]
fn what_is_no_array_is_not_written_in_parts() {
    on_a_machine(|m| {
        let (ctors, by_width) = as_wide_as_you_like();
        m.rest.heap = Heap::new(ctors);
        m.rest.globals = Vec::new();
        let heap = &mut m.rest.heap;
        let handle = Value::cell(heap.make(Kind::Handle, &[41]).expect("room"));
        let socket = heap.ctor(by_width[2], &[handle, Value::NIL]).expect("room");
        let other = a_cell(heap, 4);
        let range = Value::cell(array::range(heap, 1, 4).expect("room"));
        let (freed, _) = array_of_kind(heap, Of::Both(40));
        heap.release(freed.as_cell().expect("an array"))
            .expect("live");
        for wrong in [freed, other, range, small(7), Value::NIL] {
            let frame = registers(&[(FROM, Value::cell(socket)), (WITH, wrong)]);
            let frame: [Value; WINDOW] = frame.try_into().expect("a window");
            let args = [FROM, WITH];
            let got = m.rest.builtin(Intrinsic::SocketWriteParts, &frame, &args);
            let got = got.map(|v| v.bits());
            let want = Stop::broken(Broken::NotAnArray(wrong.view()));
            assert_eq!(got, Err(want));
        }
    });
}
