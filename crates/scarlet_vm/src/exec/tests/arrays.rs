//! The instructions that take an array apart and that push onto one.

use super::probes::{
    ARRAYS, Of, a_cell, all_given_up, array_of_kind, asks, counted, elements, found, in_register,
    is_array, is_element, miss, registers, rest_is, small,
};
use super::*;
use crate::code::{Hits, MaybeHit, Near};

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

/// An array as it may have come to be: one cell or a tree, with elements left behind in
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
    let (Of::Both(n), Some(_)) = (of, whole.as_cell()) else {
        return (whole, items);
    };
    match how {
        Grown::AsItIs => (whole, items),
        Grown::Cut if n > 2 => {
            let whole = stored(heap, whole);
            let left = array::skip(heap, whole, 2).expect("room");
            items.drain(..2);
            (left, items)
        }
        Grown::Pushed if n > 0 => {
            let mut a = whole;
            for i in 0..3 {
                let x = a_cell(heap, 900 + i);
                let array = stored(heap, a);
                a = array::push(heap, array, x, End::Front).expect("room");
                items.insert(0, Ok(x));
            }
            (a, items)
        }
        Grown::Cut | Grown::Pushed => (whole, items),
    }
}

/// `array` as what stores its elements.
fn stored(heap: &Heap, array: Value) -> Stored {
    match array::seq(heap, array).expect("an array") {
        Seq::Empty => Stored::Empty,
        Seq::Flat(flat) => Stored::Flat(flat),
        Seq::Tree(cell) => Stored::Tree(cell),
        Seq::Range { .. } => panic!("a range stores none"),
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
        let left = m.stack[i].bits();
        // The empty array has no reference to take, so it may be left where it is.
        if gone && v.bits() == Value::EMPTY_ARRAY.bits() {
            assert!([Value::NIL, *v].map(Value::bits).contains(&left), "{what}");
            continue;
        }
        let want = if gone { Value::NIL } else { *v };
        assert_eq!(left, want.bits(), "{what}: r{i}");
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
                    let in_front = matches!(case.how, Grown::Pushed) && items.len() > 33;
                    let short = matches!(case.of, Of::Both(n) if n <= 32);
                    if alone && (in_front || (short && items.len() <= 32)) {
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
    let items: Vec<_> = items.iter().copied().map(Arg::shared).collect();
    extend_taking(src, &items, end)
}

fn extend_taking(src: Arg<Reg>, items: &[Arg<Reg>], end: End) -> (Ins<Reg>, Lists<Reg>) {
    let ins = Ins::ArrayExtend {
        dst: Dst(TAIL),
        src,
        items: crate::code::ArgList(0),
        end,
    };
    let lists = Lists {
        args: vec![items.into()],
        ..Lists::default()
    };
    (ins, lists)
}

/// A literal of more elements than one cell has.
fn literal_of(items: &[Arg<Reg>]) -> (Ins<Reg>, Lists<Reg>) {
    let ins = Ins::ArrayWide {
        dst: Dst(TAIL),
        elements: crate::code::ArgList(0),
    };
    let lists = Lists {
        args: vec![items.into()],
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
    let (mut hits, mut in_place) = (0, 0);
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
                    // Only the place in its head moves along in an array that nothing
                    // else holds. A short one is all head.
                    let pushed = matches!(case.how, Grown::Pushed) && n > 0;
                    let in_head = match has {
                        ..=32 if n <= 32 => has,
                        // The first that did not fit was pushed alone.
                        33 | 34 if pushed => has - 32,
                        _ if pushed => 3,
                        _ => 0,
                    };
                    let alone = case.moved && !case.held && !matches!(case.of, Of::Range(..));
                    if alone && (1..in_head).contains(&asked.len) {
                        assert_eq!(left.bits(), before[usize::from(FROM.0)].bits(), "{what}");
                        in_place += 1;
                    }
                }
                all_given_up(m, &what);
            });
        }
    }
    assert!(hits > 5000 && in_place > 50, "{hits} {in_place}");
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

/// A pattern that asks an array of no elements for some, which none that the loader makes
/// does, stops. It writes nothing.
#[test]
fn the_empty_array_has_no_elements_to_take() {
    let asked = |wanted| Asked {
        len: 0,
        more: true,
        from: 0,
        wanted,
        tail: true,
    };
    for wanted in [0, 1, 0b111_1111] {
        let (ins, lists) = split_of(asked(wanted), Arg::moved(FROM), None);
        let code = asks_with(ins, lists);
        on_a_machine(|m| {
            let before = registers(&[(FROM, Value::EMPTY_ARRAY)]);
            let stop = run_with(m, code, &before);
            if wanted == 0 {
                found(&stop);
                let tail = in_register(m, TAIL);
                assert_eq!(tail.bits(), Value::EMPTY_ARRAY.bits());
            } else {
                let none = Broken::NoArrayElement { index: 0 };
                assert_eq!(stop, Stop::broken(none));
                rest_is(m, &before, None, &[], "elements");
            }
        });
    }
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
                        items: crate::code::ArgList(0),
                        end,
                    },
                };
                // Of those that name one register only the last may take it.
                let items_of = [
                    Arg::shared(WITH),
                    Arg::shared(WITH),
                    item,
                    Arg {
                        reg: AWAY,
                        moved: item_moved,
                    },
                ];
                let lists = Lists {
                    args: vec![items_of.into()],
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
                    if item_moved {
                        written.push(WITH);
                        assert_eq!(in_register(m, WITH).bits(), Value::NIL.bits(), "{what}");
                    }
                    if item_moved && many {
                        written.push(AWAY);
                        assert_eq!(in_register(m, AWAY).bits(), Value::NIL.bits(), "{what}");
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

/// Two arrays of any kind are joined in their order, whoever holds them and whichever of
/// them the instruction takes, and what goes on holding one holds it as it was. Two short
/// ones that fit in one cell are one, which is either's when that is taken, has slots for
/// the other and nothing else holds it.
#[test]
fn arrays_are_joined_in_their_order() {
    let mut in_place = 0;
    let kinds = || {
        ARRAYS
            .into_iter()
            .chain([Of::Both(4), Of::Both(5), Of::Both(17)])
    };
    for left in kinds() {
        for right in kinds() {
            if len_of_kind(left) + len_of_kind(right) > 1200 {
                continue;
            }
            for bits in 0..16u8 {
                let holds = |bit: u8| bits >> bit & 1 == 1;
                let (takes, held) = ([holds(0), holds(1)], [holds(2), holds(3)]);
                let arg = |reg, moved| Arg { reg, moved };
                let join = Ins::ArrayConcat {
                    dst: Dst([TAIL, FROM, WITH][usize::from(bits) % 3]),
                    a: arg(FROM, takes[0]),
                    b: arg(WITH, takes[1]),
                };
                let Ins::ArrayConcat { dst, .. } = join else {
                    panic!("a join")
                };
                // A register that is read after it is not the result's.
                let keeps = [(FROM, takes[0]), (WITH, takes[1])];
                if keeps.contains(&(dst.0, false)) {
                    continue;
                }
                let code = asks(join);
                on_a_machine(|m| {
                    let what = format!("{left:?} {right:?} {join:?} {held:?}");
                    m.rest.heap = Heap::new(code.ctors.clone());
                    m.rest.globals = Vec::new();
                    let heap = &mut m.rest.heap;
                    let (a, a_items) = array_of_kind(heap, left);
                    let (b, b_items) = array_of_kind(heap, right);
                    for (v, held) in [(a, held[0]), (b, held[1])] {
                        if held {
                            m.rest.globals.push(heap.own(v).expect("live"));
                        }
                    }
                    let made = heap.made();
                    run_with(m, code, &registers(&[(FROM, a), (WITH, b)]));
                    counted(m, &what);
                    let made = m.rest.heap.made() - made;
                    let items = [a_items.clone(), b_items.clone()].concat();
                    let joined = in_register(m, dst.0);
                    is_array(m, joined, &items, &what);
                    let short = |of: Of| matches!(of, Of::Both(1..=32));
                    let s = m.rest.seq(joined).expect("an array");
                    if short(left) && short(right) {
                        let flat = matches!(s, Seq::Flat(_));
                        assert_eq!(flat, items.len() <= 32, "{what}");
                        if made == 0 {
                            in_place += 1;
                            let whose = [a, b].map(Value::bits);
                            assert!(whose.contains(&joined.bits()), "{what}");
                        }
                        let room = |n: usize| n.next_power_of_two().max(4) - n;
                        let (n, k) = (a_items.len(), b_items.len());
                        let has = |i: usize| takes[i] && !held[i];
                        let fits = (has(0) && room(n) >= k) || (has(1) && room(k) >= n);
                        if flat {
                            assert_eq!(made == 0, fits, "{what}");
                        }
                    }
                    for (v, items, i) in [(a, &a_items, 0), (b, &b_items, 1)] {
                        if held[i] || !takes[i] {
                            is_array(m, v, items, &what);
                        }
                    }
                    counted(m, &what);
                    all_given_up(m, &what);
                });
            }
        }
    }
    assert!(in_place > 50, "{in_place}");
    // One that is joined to itself, which is kept or not.
    for of in ARRAYS {
        for moved in [false, true] {
            let code = asks(Ins::ArrayConcat {
                dst: Dst(TAIL),
                a: Arg::shared(FROM),
                b: Arg { reg: FROM, moved },
            });
            on_a_machine(|m| {
                let what = format!("{of:?} {moved}");
                m.rest.heap = Heap::new(code.ctors.clone());
                m.rest.globals = Vec::new();
                let (a, items) = array_of_kind(&mut m.rest.heap, of);
                run_with(m, code, &registers(&[(FROM, a)]));
                counted(m, &what);
                let twice = in_register(m, TAIL);
                is_array(m, twice, &[items.clone(), items].concat(), &what);
                all_given_up(m, &what);
            });
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
            for (how, n) in [(Grown::AsItIs, 9), (Grown::Pushed, 9), (Grown::AsItIs, 40)] {
                on_a_machine(|m| {
                    m.rest.heap = Heap::new(code.ctors.clone());
                    m.rest.globals = Vec::new();
                    let heap = &mut m.rest.heap;
                    let freed = Value::cell(a_freed_cell(heap, 2));
                    let mut whole = array::from_values(heap, &vec![freed; n]).expect("room");
                    if matches!(how, Grown::Pushed) {
                        for _ in 0..9 {
                            let array = stored(heap, whole);
                            whole = array::push(heap, array, freed, End::Front).expect("room");
                        }
                    }
                    let before = registers(&[(FROM, whole)]);
                    let stop = run_with(m, code, &before);
                    // One that only moves out is not looked at.
                    let moves = moved && n < 32 && matches!(ins, Ins::ArrayUncons { .. });
                    let want = if moves {
                        Broken::NoArm
                    } else {
                        Broken::DeadCell
                    };
                    assert_eq!(stop, Stop::broken(want), "{ins:?} {how:?}");
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
        (33usize, 0b11u8, 32usize, 33usize),
        (33, 0b1, 35, 35),
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
            let cell = whole.as_cell().expect("a tree");
            m.rest.heap.data_mut(cell)[0] = 50;
            let before = registers(&[(FROM, whole)]);
            let stop = run_with(m, code, &before);
            let want = Broken::NoArrayElement { index: missing };
            assert_eq!(stop, Stop::broken(want), "{ins:?}");
            rest_is(m, &before, None, &[], "a short array");
        });
    }
}

/// With no room for what it makes, each of these stops, and every reference is counted. The
/// array stays in its register, or the register has given it up, and no register names a
/// cell that is gone. What is kept elsewhere is as it was.
#[test]
fn an_instruction_with_no_room_leaves_every_register_right() {
    let arrays = [
        Of::Both(1),
        Of::Both(4),
        Of::Both(16),
        Of::Both(32),
        Of::Both(33),
        Of::Both(70),
        Of::Range(-3, 9),
        Of::Range((1 << 50) - 4, (1 << 50) + 9),
        Of::Range(i64::MAX - 9, i64::MAX),
    ];
    let asked = |len, wanted, from, tail| Asked {
        len,
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
                let set = Ins::ArraySet {
                    dst: Hit(TAIL),
                    src,
                    index: Src(AT),
                    value: item,
                    miss: miss(),
                };
                let mut all = vec![push_of(src, item, End::Front)];
                all.push(push_of(src, item, End::Back));
                all.push((set, Lists::default()));
                // Of the items in one register the last takes it.
                for (n, end) in [
                    (3, End::Front),
                    (3, End::Back),
                    (40, End::Front),
                    (40, End::Back),
                ] {
                    let mut items: Vec<_> = many[..n].iter().copied().map(Arg::shared).collect();
                    let last = items.iter_mut().rfind(|a| a.reg == WITH);
                    last.expect("there").moved = item_moved;
                    all.push(extend_taking(src, &items, end));
                }
                for n in [33, 70] {
                    let of = many.iter().copied().cycle().take(n).map(Arg::shared);
                    let mut items: Vec<_> = of.chain([src, item]).collect();
                    items.rotate_right(3);
                    let last = items.iter().rposition(|a| a.reg == WITH).expect("there");
                    items.iter_mut().for_each(|a| a.moved &= a.reg == FROM);
                    items[last].moved = item_moved;
                    all.push(literal_of(&items));
                }
                // The item is in the others' registers too.
                if !item_moved {
                    all.extend([
                        uncons_of(None, src),
                        uncons_of(Some(TO[0]), src),
                        split_of(asked(1, 0b1, 0, true), src, None),
                        split_of(asked(8, 0b11, 0, true), src, None),
                        split_of(asked(8, 0b1011, 4, true), src, None),
                        split_of(asked(8, 0b101_0101, 1, true), src, None),
                        split_of(asked(8, 0b111_1111, 0, false), src, None),
                    ]);
                    let slice = |dst| Ins::ArraySlice {
                        dst: MaybeHit::new(dst),
                        src,
                        start: Src(AT),
                        end: Src(UNTIL),
                        miss: miss(),
                    };
                    all.push((slice(Some(TAIL)), Lists::default()));
                    for (a, b) in [(FROM, SHORT), (SHORT, FROM), (FROM, FROM)] {
                        // Of two operands in one register only the last may take it.
                        let takes = |reg, last| Arg {
                            reg,
                            moved: moved && reg == FROM && (last || a != b),
                        };
                        let join = Ins::ArrayConcat {
                            dst: Dst(TAIL),
                            a: takes(a, false),
                            b: takes(b, true),
                        };
                        all.push((join, Lists::default()));
                    }
                }
                for (ins, lists) in all {
                    full += with_no_room(case, ins, lists, item_moved);
                }
            }
        }
    }
    assert!(full > 4000, "{full}");
}

/// A literal of more elements than one cell has is a tree of them. Each register that gives
/// its value up is empty then, and holds it otherwise.
#[test]
fn a_long_literal_takes_what_its_registers_give_up() {
    let regs = [&[FROM, WITH, AWAY, TAIL], &TO[..]].concat();
    for n in [33usize, 64, 65, 70, 1100] {
        let used: Vec<Reg> = regs.iter().copied().cycle().take(n).collect();
        for takes in 0..5usize {
            let last = |i: usize| used.iter().rposition(|r| *r == used[i]) == Some(i);
            let takes = |i: usize| match takes {
                0 => false,
                1 => true,
                _ => (i + takes).is_multiple_of(takes),
            };
            let items: Vec<_> = (0..n)
                .map(|i| Arg {
                    reg: used[i],
                    moved: last(i) && takes(i),
                })
                .collect();
            let (ins, lists) = literal_of(&items);
            let code = asks_with(ins, lists);
            on_a_machine(|m| {
                m.rest.heap = Heap::new(code.ctors.clone());
                m.rest.globals = Vec::new();
                let heap = &mut m.rest.heap;
                let cells = (0..).map(|i| a_cell(heap, i));
                let given: Vec<_> = regs.iter().copied().zip(cells).collect();
                let before = registers(&given);
                found(&run_with(m, code, &before));
                let what = format!("{n} {items:?}");
                let want: Vec<_> = used.iter().map(|r| Ok(before[usize::from(r.0)])).collect();
                let made = in_register(m, TAIL);
                assert!(matches!(array::seq(&m.rest.heap, made), Some(Seq::Tree(_))));
                is_array(m, made, &want, &what);
                let gone: Vec<Reg> = items.iter().filter(|a| a.moved).map(|a| a.reg).collect();
                rest_is(m, &before, Some(TAIL), &gone, &what);
                counted(m, &what);
                all_given_up(m, &what);
            });
        }
    }
}

/// Where an index is, the end of a slice, and a short array.
const AT: Reg = Reg(FIRST.0 + 11);
const UNTIL: Reg = Reg(FIRST.0 + 12);
const SHORT: Reg = Reg(FIRST.0 + 13);

/// Run `ins` on heaps with a little more room each time, until one has enough. Gives how
/// many had not.
fn with_no_room(case: Case, ins: Ins<Reg>, lists: Lists<Reg>, item_moved: bool) -> usize {
    let code = asks_with(ins, lists);
    let twice = matches!(ins, Ins::ArrayConcat { a, b, .. } if a.reg == FROM && b.reg == FROM);
    let mut full = 0;
    // Each word for a short cell, and fewer of them further on.
    for more in (0..).map(|i: usize| i + i / 24 * i / 2) {
        let mut done = false;
        on_a_machine(|m| {
            let what = format!("{case:?} {ins:?} {more}");
            let (mut before, items) = set_up(m, code, case);
            let heap = &mut m.rest.heap;
            // What two trees are joined to has no bound to ask for before there is some of it.
            let of = array::seq(heap, before[usize::from(FROM.0)]);
            let joins_trees = twice && matches!(of, Some(Seq::Tree(_)));
            let short: Vec<Value> = (0..3).map(|i| a_cell(heap, 60 + i)).collect();
            let short_items: Vec<_> = short.iter().copied().map(Ok).collect();
            let given = [
                (WITH, a_cell(heap, 5)),
                (AT, small(1)),
                (UNTIL, small(items.len() as i64 - 1)),
                (SHORT, array::from_values(heap, &short).expect("room")),
            ];
            for (reg, v) in given {
                before[usize::from(reg.0)] = v;
            }
            let limit = m.rest.heap.words_used() + more;
            m.rest.heap.limit_to(limit);
            let stop = run_with(m, code, &before);
            if stop != Stop::HeapFull {
                found(&stop);
                counted(m, &what);
                done = true;
                return;
            }
            full += 1;
            if !joins_trees {
                counted(m, &what);
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
            let left = in_register(m, SHORT);
            is_array(m, left, &short_items, &what);
            if !joins_trees {
                all_given_up(m, &what);
            }
        });
        if done {
            return full;
        }
        assert!(more < 9000, "{case:?} {ins:?}");
    }
    full
}

/// A range may have more elements than an `i64` counts, and then its length is a big Int.
#[test]
fn the_length_of_the_longest_range_is_a_big_int() {
    for (start, end, want) in [
        (i64::MIN, i64::MAX, BigInt::from(u64::MAX)),
        (i64::MIN, 0, BigInt::from(1u64 << 63)),
        (0, i64::MAX, BigInt::from(i64::MAX)),
        (0, (1 << 50) - 1, BigInt::from((1i64 << 50) - 1)),
        (0, 1 << 50, BigInt::from(1i64 << 50)),
        (i64::MAX, i64::MIN, BigInt::from(0)),
    ] {
        let code = asks(Ins::ArrayLen {
            dst: Dst(TAIL),
            src: Src(FROM),
        });
        on_a_machine(|m| {
            m.rest.heap = Heap::new(code.ctors.clone());
            m.rest.globals = Vec::new();
            let range = array::range(&mut m.rest.heap, start, end).expect("room");
            run_with(m, code, &registers(&[(FROM, range)]));
            let len = in_register(m, TAIL);
            let got = match len.view() {
                View::Int(n) => BigInt::from(n),
                View::Cell(c) => m.rest.heap.read_big_int(c),
                other => panic!("{other:?}"),
            };
            assert_eq!(got, want);
            assert_eq!(len.small_int().is_some(), want < BigInt::from(1i64 << 50));
            counted(m, "a length");
            all_given_up(m, "a length");
        });
    }
}

/// A cell of no elements, which nothing makes, is not taken for an array that has one or
/// for one that has none: what asks for its first element stops, and leaves everything
/// where it was. What only asks how long it is finds that it has none.
#[test]
fn a_cell_with_no_elements_stops_what_takes_it_apart() {
    let nil = Value::NIL.bits();
    let cells: [(Kind, &[u64]); 3] = [
        (Kind::ArrayRoot, &[0, 0, 0, nil, nil, nil]),
        (Kind::Range, &[4, 4]),
        (Kind::Range, &[9, 2]),
    ];
    for (kind, words) in cells {
        for moved in [false, true] {
            for head in [None, Some(TO[0])] {
                let src = Arg { reg: FROM, moved };
                let (ins, lists) = uncons_of(head, src);
                let code = asks_with(ins, lists);
                on_a_machine(|m| {
                    m.rest.heap = Heap::new(code.ctors.clone());
                    m.rest.globals = Vec::new();
                    let whole = Value::cell(m.rest.heap.make(kind, words).expect("room"));
                    let before = registers(&[(FROM, whole)]);
                    let what = format!("{kind:?} {words:?} {moved} {head:?}");
                    let stop = run_with(m, code, &before);
                    assert_eq!(stop, Stop::broken(Broken::EmptyCell), "{what}");
                    rest_is(m, &before, None, &[], &what);
                    let cell = whole.as_cell().expect("a cell");
                    assert_eq!(m.rest.heap.unique(cell), Ok(true), "{what}");
                    let s = m.rest.seq(whole).expect("an array");
                    assert_eq!(s.len(&m.rest.heap), 0, "{what}");
                    let items = elements(m, whole);
                    assert!(items.is_empty(), "{what}");
                    all_given_up(m, &what);
                });
            }
        }
    }
}

/// The empty array grows and is joined as any other is, takes no reference, and is left
/// in its register. Nothing is made where the other side stores its elements or is empty,
/// which is then the join. A range is built as an array that does.
#[test]
fn the_empty_array_is_grown_and_joined() {
    let empty = Value::EMPTY_ARRAY;
    for moved in [false, true] {
        let src = Arg { reg: FROM, moved };
        for end in [End::Front, End::Back] {
            let pushes = [
                (push_of(src, Arg::shared(WITH), end), 1),
                (push_of(src, Arg { reg: WITH, moved }, end), 1),
                (extend_of(src, &[WITH, AWAY], end), 2),
                (extend_of(src, &[], end), 0),
            ];
            for ((ins, lists), n) in pushes {
                let code = asks_with(ins, lists);
                on_a_machine(|m| {
                    m.rest.heap = Heap::new(code.ctors.clone());
                    m.rest.globals = Vec::new();
                    let heap = &mut m.rest.heap;
                    let (x, y) = (a_cell(heap, 1), a_cell(heap, 2));
                    let made = heap.made();
                    let before = registers(&[(FROM, empty), (WITH, x), (AWAY, y)]);
                    let what = format!("{ins:?}");
                    run_with(m, code, &before);
                    counted(m, &what);
                    assert_eq!(m.rest.heap.made() - made, n.min(1), "{what}");
                    let items = [Ok(x), Ok(y)];
                    let grown = in_register(m, TAIL);
                    is_array(m, grown, &items[..n], &what);
                    all_given_up(m, &what);
                });
            }
        }
    }
    let join = Ins::ArrayConcat {
        dst: Dst(TAIL),
        a: Arg::shared(FROM),
        b: Arg::shared(WITH),
    };
    let code = asks_with(join, Lists::default());
    for of in ARRAYS {
        for first in [false, true] {
            on_a_machine(|m| {
                m.rest.heap = Heap::new(code.ctors.clone());
                m.rest.globals = Vec::new();
                let (other, items) = array_of_kind(&mut m.rest.heap, of);
                let made = m.rest.heap.made();
                let (a, b) = if first {
                    (empty, other)
                } else {
                    (other, empty)
                };
                run_with(m, code, &registers(&[(FROM, a), (WITH, b)]));
                let what = format!("{of:?} {first}");
                counted(m, &what);
                let joined = in_register(m, TAIL);
                let s = m.rest.seq(other).expect("an array");
                if let Seq::Range { .. } = s {
                    let s = m.rest.seq(joined).expect("an array");
                    assert!(matches!(s, Seq::Flat(_)), "{what}");
                } else {
                    assert_eq!(m.rest.heap.made(), made, "{what}");
                    assert_eq!(joined.bits(), other.bits(), "{what}");
                }
                is_array(m, joined, &items, &what);
                all_given_up(m, &what);
            });
        }
    }
}

/// The loader gives no array instruction without elements. One that has none makes no cell.
#[test]
fn an_array_of_no_elements_is_no_cell() {
    let elements = crate::code::ArgList(1);
    let wide = Ins::ArrayWide {
        dst: Dst(r(0)),
        elements,
    };
    for ins in [array_of(0, &[]), wide] {
        after(1, vec![ins, ret(0)], |m, got| {
            let got = got.expect("the function returns");
            assert_eq!(got.bits(), Value::EMPTY_ARRAY.bits());
            assert_eq!(m.rest.heap.made(), 0);
        });
    }
}

/// A socket is given nothing to write where the parts are the empty array.
#[test]
fn the_empty_array_is_parts_to_write() {
    on_a_machine(|m| {
        let (ctors, by_width) = as_wide_as_you_like();
        m.rest.heap = Heap::new(ctors);
        m.rest.globals = Vec::new();
        let heap = &mut m.rest.heap;
        let handle = Value::cell(heap.make(Kind::Handle, &[41]).expect("room"));
        let socket = heap.ctor(by_width[2], &[handle, Value::NIL]).expect("room");
        let frame = registers(&[(FROM, Value::cell(socket)), (WITH, Value::EMPTY_ARRAY)]);
        let frame: [Value; WINDOW] = frame.try_into().expect("a window");
        let args = [FROM, WITH];
        // It gets as far as the host, which has no such socket, and the program has no
        // error to say so with.
        let got = m.rest.builtin(Intrinsic::SocketWriteParts, &frame, &args);
        let got = got.map(|v| v.bits());
        assert_eq!(got, Err(Stop::broken(Broken::NoNetTypes)));
    });
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
        let range = array::range(heap, 1, 4).expect("room");
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
