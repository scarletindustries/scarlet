//! The instructions that ask for what may not be there.

use super::*;
use crate::code::MaybeHit;

/// Where the tests put the operands, and a result that has a register of its own.
const FROM: Reg = Reg(FIRST.0);
const AT: Reg = Reg(FIRST.0 + 1);
const TO: Reg = Reg(FIRST.0 + 2);
const WITH: Reg = Reg(FIRST.0 + 3);
const AWAY: Reg = Reg(FIRST.0 + 4);

/// A function that does `ins` and then stops, where it says whether that jumped.
fn asks(ins: Ins<Reg>) -> &'static Code {
    let hit = Ins::Bad { why: WhyId(0) };
    let miss = Ins::Bad { why: WhyId(9) };
    function_that(ReadsClosure::Nothing, 0, 20, vec![ins, hit, miss])
}

const MISS: u32 = 2;

fn miss() -> Pc {
    pc(MISS)
}

/// Whether a run of [`asks`] found something.
fn found(stop: &Stop) -> bool {
    if *stop == Stop::broken(Broken::NoArm) {
        return true;
    }
    assert_eq!(*stop, Stop::broken(Broken::NoPlace));
    false
}

fn small(n: i64) -> Value {
    Value::int(n).expect("small")
}

fn a_cell(heap: &mut Heap, n: u64) -> Value {
    Value::cell(heap.make(Kind::Binary, &[n; 12]).expect("room"))
}

fn big(heap: &mut Heap, n: &BigInt) -> Value {
    Value::cell(heap.big_int(n).expect("room"))
}

/// The registers, by what each is to hold. Any other holds Nil.
fn registers(held: &[(Reg, Value)]) -> Vec<Value> {
    let mut all = vec![Value::NIL; WINDOW];
    for (reg, v) in held {
        all[usize::from(reg.0)] = *v;
    }
    all
}

/// Every reference there is, from the registers and the globals, is counted.
fn counted(m: &Machine<'_, '_>, what: &str) {
    assert_eq!(check(&m.stack, &m.rest, Value::NIL), Ok(()), "{what}");
}

/// The registers of the frame but `dst` hold `want`, with nothing in `gone`.
fn rest_is(m: &Machine<'_, '_>, want: &[Value], dst: Option<Reg>, gone: &[Reg], what: &str) {
    for (i, v) in want.iter().enumerate().skip(usize::from(FIRST.0)) {
        if dst.is_some_and(|dst| usize::from(dst.0) == i) {
            continue;
        }
        let taken = gone.iter().any(|g| usize::from(g.0) == i);
        let want = if taken { Value::NIL } else { *v };
        assert_eq!(m.stack[i].bits(), want.bits(), "{what}: r{i}");
    }
}

fn in_register(m: &Machine<'_, '_>, r: Reg) -> Value {
    m.stack[usize::from(r.0)]
}

/// Give up everything the test holds, after which the heap is empty.
fn all_given_up(m: &mut Machine<'_, '_>, what: &str) {
    for v in std::mem::take(&mut m.stack) {
        m.rest.release(v).expect("live");
    }
    for v in std::mem::take(&mut m.rest.globals) {
        m.rest.release(v).expect("live");
    }
    assert_eq!(m.rest.heap.live(), 0, "{what}");
}

/// What an array is made of.
#[derive(Debug, Clone, Copy)]
enum Of {
    /// Numbers and cells by turns.
    Both(usize),
    Range(i64, i64),
}

const ARRAYS: [Of; 13] = [
    Of::Both(0),
    Of::Both(1),
    Of::Both(2),
    Of::Both(31),
    Of::Both(32),
    Of::Both(33),
    Of::Both(65),
    Of::Both(1100),
    Of::Range(-3, 5),
    Of::Range(4, 4),
    Of::Range(9, 2),
    Of::Range((1 << 50) - 2, (1 << 50) + 2),
    Of::Range(i64::MAX - 3, i64::MAX),
];

/// The array, and each of its elements, as a word it holds or the number it is.
fn array_of_kind(heap: &mut Heap, of: Of) -> (Value, Vec<Result<Value, i64>>) {
    match of {
        Of::Both(n) => {
            let items: Vec<Value> = (0..n as u64)
                .map(|i| match i % 2 {
                    0 => a_cell(heap, i),
                    _ => small(i as i64),
                })
                .collect();
            let cell = array::from_values(heap, &items).expect("room");
            (Value::cell(cell), items.into_iter().map(Ok).collect())
        }
        Of::Range(start, end) => {
            let cell = array::range(heap, start, end).expect("room");
            (
                Value::cell(cell),
                (start..end.max(start)).map(Err).collect(),
            )
        }
    }
}

/// Whether `v` is the element.
fn is_element(m: &Machine<'_, '_>, v: Value, want: &Result<Value, i64>) -> bool {
    match want {
        Ok(word) => word.bits() == v.bits(),
        Err(n) => match v.view() {
            View::Int(got) => got == *n,
            View::Cell(c) => m.rest.heap.read_big_int(c) == BigInt::from(*n),
            _ => false,
        },
    }
}

/// Every element of the array `v`.
fn elements(m: &mut Machine<'_, '_>, v: Value) -> Vec<Value> {
    let s = m.rest.seq(v).expect("an array");
    let n = usize::try_from(s.len(&m.rest.heap)).expect("short");
    (0..n)
        .map(|i| m.rest.element(s, i).expect("live").expect("there"))
        .collect()
}

fn is_array(m: &mut Machine<'_, '_>, v: Value, want: &[Result<Value, i64>], what: &str) {
    let got = elements(m, v);
    assert_eq!(got.len(), want.len(), "{what}");
    for (got, want) in got.iter().zip(want) {
        assert!(is_element(m, *got, want), "{what}");
    }
    for v in got {
        m.rest.release(v).expect("live");
    }
}

/// Indices at and around the ends of an array of `n`, and far from both.
fn indices(n: usize) -> Vec<BigInt> {
    let n = n as i64;
    let mut all: Vec<BigInt> = [-1, 0, 1, n / 2, n - 1, n, n + 1, 1 << 40, -(1 << 50)]
        .map(BigInt::from)
        .to_vec();
    all.push((BigInt::from(1) << 50) - 1);
    let far: BigInt = BigInt::from(1) << 70;
    all.extend([far.clone(), -far]);
    all.sort();
    all.dedup();
    all
}

fn number(heap: &mut Heap, n: &BigInt) -> Value {
    match n.to_i64().and_then(Value::int) {
        Some(v) => v,
        None => big(heap, n),
    }
}

fn index(n: &BigInt, len: usize) -> Option<usize> {
    n.to_usize().filter(|i| *i < len)
}

/// A read finds the element and no other, puts it in its register with a reference of
/// its own, and changes nothing else. When it is put nowhere, and where there is none,
/// nothing at all changes. So it is of every array, with any index, with the result in
/// a register of its own, the array's or the index's, held elsewhere or not.
#[test]
fn a_read_finds_the_element_or_goes_on_at_its_miss() {
    for of in ARRAYS {
        let len = match of {
            Of::Both(n) => n,
            Of::Range(a, b) => usize::try_from(b.saturating_sub(a).max(0)).expect("few"),
        };
        for at in indices(len) {
            for dst in [None, Some(AWAY), Some(FROM), Some(AT)] {
                for held in [false, true] {
                    let what = format!("{of:?}[{at}] to {dst:?}, held {held}");
                    let code = asks(Ins::ArrayGet {
                        dst: MaybeHit::new(dst),
                        src: Src(FROM),
                        index: Src(AT),
                        miss: miss(),
                    });
                    on_a_machine(|m| {
                        let heap = &mut m.rest.heap;
                        let (xs, items) = array_of_kind(heap, of);
                        let i = number(heap, &at);
                        let old = a_cell(heap, 77);
                        if held {
                            m.rest.globals.push(heap.own(xs).expect("live"));
                        }
                        let regs = registers(&[(FROM, xs), (AT, i), (AWAY, old)]);
                        let made = heap.made();
                        let stop = run_with(m, code, &regs);
                        let want = index(&at, len);
                        assert_eq!(found(&stop), want.is_some(), "{what}");
                        let wrote = dst.filter(|_| want.is_some());
                        rest_is(m, &regs, wrote, &[], &what);
                        let mut new = 0;
                        if let (Some(dst), Some(i)) = (wrote, want) {
                            let got = in_register(m, dst);
                            assert!(is_element(m, got, &items[i]), "{what}");
                            new = usize::from(items[i].is_err() && got.as_cell().is_some());
                        }
                        assert_eq!(m.rest.heap.made() - made, new, "{what}");
                        counted(m, &what);
                        all_given_up(m, &what);
                    });
                }
            }
        }
    }
}

/// A slice finds the elements from the one index up to the other, when both are in the
/// array and in order, and makes nothing otherwise, or when it is put nowhere.
#[test]
fn a_slice_finds_the_elements_or_goes_on_at_its_miss() {
    for of in ARRAYS {
        let len = match of {
            Of::Both(n) => n,
            Of::Range(a, b) => usize::try_from(b.saturating_sub(a).max(0)).expect("few"),
        };
        for from in indices(len) {
            for to in indices(len) {
                for dst in [None, Some(AWAY), Some(FROM), Some(AT), Some(TO)] {
                    let what = format!("{of:?}[{from}..{to}] to {dst:?}");
                    let code = asks(Ins::ArraySlice {
                        dst: MaybeHit::new(dst),
                        src: Src(FROM),
                        start: Src(AT),
                        end: Src(TO),
                        miss: miss(),
                    });
                    on_a_machine(|m| {
                        let heap = &mut m.rest.heap;
                        let (xs, items) = array_of_kind(heap, of);
                        let (a, b) = (number(heap, &from), number(heap, &to));
                        let old = a_cell(heap, 77);
                        let regs = registers(&[(FROM, xs), (AT, a), (TO, b), (AWAY, old)]);
                        let made = heap.made();
                        let stop = run_with(m, code, &regs);
                        let ends = index(&from, len + 1).zip(index(&to, len + 1));
                        let want = ends.filter(|(a, b)| a <= b);
                        assert_eq!(found(&stop), want.is_some(), "{what}");
                        let wrote = dst.filter(|_| want.is_some());
                        rest_is(m, &regs, wrote, &[], &what);
                        match (wrote, want) {
                            (Some(dst), Some((a, b))) => {
                                let got = in_register(m, dst);
                                counted(m, &what);
                                is_array(m, got, &items[a..b], &what);
                            }
                            _ => assert_eq!(m.rest.heap.made(), made, "{what}"),
                        }
                        counted(m, &what);
                        all_given_up(m, &what);
                    });
                }
            }
        }
    }
}

/// A write finds the array with the value in its place, and nothing else changes: not
/// the array, when a register or anything else goes on holding it, and no register but
/// those it takes from. A write that takes an array that nothing else holds makes no
/// cell. Where there is no such index nothing is written and nothing is taken. The
/// value may be the array itself, and the result in any register that is then free.
#[test]
fn a_write_finds_the_array_or_goes_on_at_its_miss() {
    for of in ARRAYS {
        let len = match of {
            Of::Both(n) => n,
            Of::Range(a, b) => usize::try_from(b.saturating_sub(a).max(0)).expect("few"),
        };
        for at in indices(len) {
            for bits in 0..8u8 {
                let [src, value, held] = [0, 1, 2].map(|i| bits >> i & 1 == 1);
                for itself in [false, true] {
                    for dst in [AWAY, FROM, AT, WITH] {
                        let what = format!(
                            "{of:?}[{at}] to {dst:?}, takes {src} {value}, held {held}, itself {itself}"
                        );
                        a_write_of(of, len, &at, [src, value, held, itself], dst, &what);
                    }
                }
            }
        }
    }
}

fn a_write_of(
    of: Of,
    len: usize,
    at: &BigInt,
    [src, value, held, itself]: [bool; 4],
    dst: Reg,
    what: &str,
) {
    // Of two operands in one register only the last may take it.
    if itself && src && value {
        return;
    }
    let with = if itself { FROM } else { WITH };
    let code = asks(Ins::ArraySet {
        dst: Hit(dst),
        src: Arg {
            reg: FROM,
            moved: src,
        },
        index: Src(AT),
        value: Arg {
            reg: with,
            moved: value,
        },
        miss: miss(),
    });
    on_a_machine(|m| {
        let heap = &mut m.rest.heap;
        let (xs, mut items) = array_of_kind(heap, of);
        let i = number(heap, at);
        let (old, new) = (a_cell(heap, 77), a_cell(heap, 78));
        if held {
            m.rest.globals.push(heap.own(xs).expect("live"));
        }
        let regs = registers(&[(FROM, xs), (AT, i), (AWAY, old), (WITH, new)]);
        let made = heap.made();
        let stop = run_with(m, code, &regs);
        let want = index(at, len);
        assert_eq!(found(&stop), want.is_some(), "{what}");
        let Some(i) = want else {
            rest_is(m, &regs, None, &[], what);
            assert_eq!(m.rest.heap.made(), made, "{what}");
            counted(m, what);
            all_given_up(m, what);
            return;
        };
        let mut gone = Vec::new();
        gone.extend(src.then_some(FROM));
        gone.extend(value.then_some(with));
        rest_is(m, &regs, Some(dst), &gone, what);
        counted(m, what);
        let tree = matches!(of, Of::Both(_));
        let alone = src && !held && !itself && tree;
        assert_eq!(alone, m.rest.heap.made() == made, "{what}");
        let before = items.clone();
        items[i] = Ok(if itself { xs } else { new });
        let got = in_register(m, dst);
        is_array(m, got, &items, what);
        // What went on holding the array holds it as it was.
        if held || (!src && dst != FROM) {
            is_array(m, xs, &before, what);
        }
        counted(m, what);
        all_given_up(m, what);
    });
}

/// A lookup finds what the map has for a key that is equal, whatever cell it is, and
/// goes on at its miss for any other.
#[test]
fn a_lookup_finds_the_value_or_goes_on_at_its_miss() {
    // Two keys of each kind are equal, and no two kinds have one in common.
    let key = |heap: &mut Heap, n: u64| -> Value {
        let text = format!("key {n}, which is a long one");
        match n % 4 {
            0 => small(n as i64),
            1 => big(heap, &((BigInt::from(1) << 70) + n)),
            2 => Value::cell(heap.string(text.as_bytes()).expect("room")),
            _ => {
                let part = Value::cell(heap.string(text.as_bytes()).expect("room"));
                Value::cell(heap.tuple(&[small(n as i64), part]).expect("room"))
            }
        }
    };
    for size in [0u64, 1, 2, 4, 8, 40, 400] {
        for asked in [0, 1, 2, 3, 4, 5, 6, 7, size, size + 1, size + 2, size + 3] {
            for dst in [None, Some(AWAY), Some(FROM), Some(AT)] {
                let what = format!("{asked} of {size} to {dst:?}");
                let code = asks(Ins::MapGet {
                    dst: MaybeHit::new(dst),
                    map: Src(FROM),
                    key: Src(AT),
                    miss: miss(),
                });
                on_a_machine(|m| {
                    let heap = &mut m.rest.heap;
                    let mut map = map::empty(heap).expect("room");
                    let mut values = Vec::new();
                    for n in 0..size {
                        let k = key(heap, n);
                        let v = match n % 3 {
                            0 => small(n as i64),
                            _ => a_cell(heap, n),
                        };
                        map = map::set_owned(heap, map, k, v).expect("room");
                        values.push(v);
                        for v in [k, v] {
                            if let Some(c) = v.as_cell() {
                                heap.release(c).expect("live");
                            }
                        }
                    }
                    let k = key(heap, asked);
                    let old = a_cell(heap, 77);
                    let regs = registers(&[(FROM, Value::cell(map)), (AT, k), (AWAY, old)]);
                    let made = heap.made();
                    let stop = run_with(m, code, &regs);
                    let want = usize::try_from(asked).ok().and_then(|n| values.get(n));
                    assert_eq!(found(&stop), want.is_some(), "{what}");
                    let wrote = dst.filter(|_| want.is_some());
                    rest_is(m, &regs, wrote, &[], &what);
                    if let (Some(dst), Some(want)) = (wrote, want) {
                        assert_eq!(in_register(m, dst).bits(), want.bits(), "{what}");
                    }
                    assert_eq!(m.rest.heap.made(), made, "{what}");
                    counted(m, &what);
                    all_given_up(m, &what);
                });
            }
        }
    }
}

/// A probe of what is no array or no map, or at what is no Int, stops the run and says
/// what it found. It has changed nothing.
#[test]
fn a_probe_of_the_wrong_kind_stops() {
    let (src, index) = (Src(FROM), Src(AT));
    let some = |dst: bool| MaybeHit::new(dst.then_some(AWAY));
    for dst in [false, true] {
        let array = [
            Ins::ArrayGet {
                dst: some(dst),
                src,
                index,
                miss: miss(),
            },
            Ins::ArraySlice {
                dst: some(dst),
                src,
                start: index,
                end: Src(TO),
                miss: miss(),
            },
            Ins::ArraySlice {
                dst: some(dst),
                src,
                start: Src(TO),
                end: index,
                miss: miss(),
            },
            Ins::ArraySet {
                dst: Hit(AWAY),
                src: Arg {
                    reg: FROM,
                    moved: dst,
                },
                index,
                value: Arg {
                    reg: WITH,
                    moved: dst,
                },
                miss: miss(),
            },
        ];
        let lookup = Ins::MapGet {
            dst: some(dst),
            map: src,
            key: index,
            miss: miss(),
        };
        let kinds = |m: &mut Machine<'_, '_>| one_of_each_kind(&mut m.rest.heap).len();
        for (ins, is_map) in array
            .map(|i| (i, false))
            .into_iter()
            .chain([(lookup, true)])
        {
            let code = asks(ins);
            on_a_machine(|m| {
                let n = kinds(m);
                m.rest.heap = Heap::default();
                for kind in 0..n {
                    for wrong in [FROM, AT] {
                        let what = format!("{ins:?} of kind {kind} in {wrong:?}");
                        let heap = &mut m.rest.heap;
                        let all = one_of_each_kind(heap);
                        let v = all[kind];
                        for other in all.iter().filter(|o| o.bits() != v.bits()) {
                            if let Some(c) = other.as_cell() {
                                heap.release(c).expect("live");
                            }
                        }
                        let of = match is_map {
                            true => Value::cell(map::empty(heap).expect("room")),
                            false => array_of_kind(heap, Of::Both(2)).0,
                        };
                        let mut held = [(FROM, of), (AT, small(0)), (TO, small(1)), (WITH, of)];
                        held[3].1 = a_cell(heap, 5);
                        let at = usize::from(wrong == AT);
                        let was = std::mem::replace(&mut held[at].1, v);
                        if let Some(c) = was.as_cell() {
                            heap.release(c).expect("live");
                        }
                        let regs = registers(&held);
                        let is_int = match v.view() {
                            View::Int(_) => true,
                            View::Cell(c) => m.rest.heap.is(c, Kind::BigInt),
                            _ => false,
                        };
                        let stop = run_with(m, code, &regs);
                        // Any value is a key, and any Int a place.
                        let fits = match wrong == FROM {
                            true => false,
                            false => is_map || is_int,
                        };
                        match fits {
                            true => {
                                found(&stop);
                            }
                            false => {
                                let want = match (wrong == FROM, is_map) {
                                    (true, true) => Broken::NotAMap(v.view()),
                                    (true, false) => Broken::NotAnArray(v.view()),
                                    (false, _) => Broken::Position(v.view()),
                                };
                                assert_eq!(stop, Stop::broken(want), "{what}");
                                rest_is(m, &regs, None, &[], &what);
                            }
                        }
                        counted(m, &what);
                        for v in &mut m.stack {
                            let v = std::mem::replace(v, Value::NIL);
                            if let Some(c) = v.as_cell() {
                                m.rest.heap.release(c).expect("live");
                            }
                        }
                        assert_eq!(m.rest.heap.live(), 0, "{what}");
                    }
                }
            });
        }
    }
}

/// A probe that finds a freed cell stops the run as a wrong count, where it would add
/// a reference to it.
#[test]
fn a_probe_that_finds_a_freed_cell_stops() {
    let code = asks(Ins::ArrayGet {
        dst: MaybeHit::new(Some(AWAY)),
        src: Src(FROM),
        index: Src(AT),
        miss: miss(),
    });
    let lookup = asks(Ins::MapGet {
        dst: MaybeHit::new(Some(AWAY)),
        map: Src(FROM),
        key: Src(AT),
        miss: miss(),
    });
    let cut = asks(Ins::ArraySlice {
        dst: MaybeHit::new(Some(AWAY)),
        src: Src(FROM),
        start: Src(AT),
        end: Src(TO),
        miss: miss(),
    });
    for (code, is_map) in [(code, false), (cut, false), (lookup, true)] {
        on_a_machine(|m| {
            let heap = &mut m.rest.heap;
            let lost = a_cell(heap, 1);
            let of = match is_map {
                true => {
                    let map = map::empty(heap).expect("room");
                    map::set_owned(heap, map, small(0), lost).expect("room")
                }
                false => array::from_values(heap, &[lost, Value::NIL]).expect("room"),
            };
            let cell = lost.as_cell().expect("a cell");
            while heap.unique(cell) != Err(DeadCell) {
                heap.release(cell).expect("live");
            }
            let regs = registers(&[(FROM, Value::cell(of)), (AT, small(0)), (TO, small(1))]);
            let stop = run_with(m, code, &regs);
            assert_eq!(stop, Stop::broken(Broken::DeadCell));
            assert_eq!(in_register(m, AWAY).bits(), Value::NIL.bits());
        });
    }
}
