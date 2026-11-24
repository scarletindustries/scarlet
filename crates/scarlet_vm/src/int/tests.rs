use super::*;
use crate::ops::Order;
use num_bigint::BigInt;

const MIN: i64 = -(1 << 50);
const MAX: i64 = (1 << 50) - 1;
const ARITH: [Arith; 5] = [Arith::Add, Arith::Sub, Arith::Mul, Arith::Div, Arith::Rem];
const COMPARE: [Compare; 6] = [
    Compare::Eq,
    Compare::Ne,
    Compare::LT,
    Compare::LE,
    Compare::GT,
    Compare::GE,
];

fn small(n: i64) -> SmallInt {
    SmallInt::new(n).expect("in range")
}

/// Small Ints at every edge of an operator: 0, both ends of the range, and
/// factors whose product is at an end or one past it.
fn smalls() -> Vec<i64> {
    let each = [
        0,
        1,
        2,
        3,
        7,
        1 << 13,
        69_069,
        1 << 25,
        (1 << 25) + 1,
        1 << 26,
        1 << 49,
        MAX - 1,
        MAX,
    ];
    let both = each.into_iter().flat_map(|n: i64| [n, -n]);
    both.chain([MIN, MIN + 1]).collect()
}

/// The fast path gives the exact answer when it is a small Int, and nothing
/// when it is not.
#[test]
fn the_fast_path_answers_exactly_the_small_answers() {
    for x in smalls() {
        for y in smalls() {
            let (a, b) = (i128::from(x), i128::from(y));
            for op in ARITH {
                let exact = match op {
                    Arith::Add => a + b,
                    Arith::Sub => a - b,
                    Arith::Mul => a * b,
                    Arith::Div if b == 0 => 0,
                    Arith::Div => a / b,
                    Arith::Rem if b == 0 => a,
                    Arith::Rem => a % b,
                };
                let want = i64::try_from(exact).ok().and_then(Value::int);
                let got = fast(op, small(x), small(y));
                assert_eq!(
                    got.map(Value::bits),
                    want.map(Value::bits),
                    "{x} {op:?} {y}"
                );
            }
            for op in COMPARE {
                let holds = match op {
                    Compare::Eq => x == y,
                    Compare::Ne => x != y,
                    Compare::Order(Order::Lt) => x < y,
                    Compare::Order(Order::Le) => x <= y,
                    Compare::Order(Order::Gt) => x > y,
                    Compare::Order(Order::Ge) => x >= y,
                };
                assert_eq!(op.holds(small(x).cmp(&small(y))), holds, "{x} {op:?} {y}");
                assert_eq!(op.swapped().holds(small(y).cmp(&small(x))), holds);
            }
        }
        let want = Value::int(-x);
        assert_eq!(
            fast_neg(small(x)).map(Value::bits),
            want.map(Value::bits),
            "{x}"
        );
    }
}

/// The small Int range is not the same on both sides of 0: it holds `-2^50`
/// and stops one short of `2^50`. So the lowest one over -1, or negated, has
/// an answer that fits an `i64` and is no small Int.
#[test]
fn what_leaves_the_small_range_at_either_end() {
    let (lowest, highest) = (small(MIN), small(MAX));
    let (one, minus_one) = (small(1), small(-1));
    assert!(fast(Arith::Div, lowest, minus_one).is_none());
    assert!(fast_neg(lowest).is_none());
    assert!(fast(Arith::Mul, lowest, minus_one).is_none());
    assert!(fast(Arith::Add, highest, one).is_none());
    assert!(fast(Arith::Sub, lowest, one).is_none());
    assert!(fast(Arith::Sub, small(0), lowest).is_none());
    assert!(fast(Arith::Mul, small(1 << 25), small(1 << 25)).is_none());
    let number = |v: Option<Value>| v.and_then(Value::small_int);
    assert_eq!(
        number(fast(Arith::Mul, small(1 << 25), small(-(1 << 25)))),
        Some(MIN)
    );
    assert_eq!(number(fast(Arith::Div, highest, minus_one)), Some(-MAX));
    assert_eq!(number(fast(Arith::Rem, lowest, minus_one)), Some(0));
    assert_eq!(number(fast(Arith::Rem, lowest, small(0))), Some(MIN));
    assert_eq!(number(fast(Arith::Div, lowest, small(0))), Some(0));
    assert_eq!(number(fast_neg(highest)), Some(-MAX));
}

/// Ints of every size, small, one word, two words and more.
fn sizes() -> Vec<BigInt> {
    let mut all: Vec<BigInt> = smalls().into_iter().map(BigInt::from).collect();
    let past = [
        BigInt::from(MAX) + 1,
        BigInt::from(MAX) + 2,
        BigInt::from(i64::MAX),
        BigInt::from(u64::MAX),
        BigInt::from(u64::MAX) + 1,
        BigInt::from(i128::MAX),
        BigInt::from(i128::MAX) + 1,
        BigInt::from(3) << 200,
    ];
    all.extend(past.into_iter().flat_map(|n| [n.clone(), -n]));
    all
}

/// The slow path is right for operands of every size, each side big or
/// small, and puts every answer in its one form.
#[test]
fn the_slow_path_agrees_with_num_bigint() {
    let mut heap = Heap::default();
    let all = sizes();
    let number = |heap: &Heap, v: Value| read(heap, v).expect("an Int").big();
    for a in &all {
        for b in &all {
            let (x, y) = (
                bigint::value(&mut heap, a.clone()).expect("room"),
                bigint::value(&mut heap, b.clone()).expect("room"),
            );
            for op in ARITH {
                let exact = match op {
                    Arith::Add => a + b,
                    Arith::Sub => a - b,
                    Arith::Mul => a * b,
                    Arith::Div => a.checked_div(b).unwrap_or_default(),
                    Arith::Rem if b.sign() == num_bigint::Sign::NoSign => a.clone(),
                    Arith::Rem => a % b,
                };
                let got = super::op(&mut heap, op, x, y).expect("room");
                assert_eq!(number(&heap, got), exact, "{a} {op:?} {b}");
                let fits = exact.to_i64().and_then(Value::int).is_some();
                assert_eq!(got.small_int().is_some(), fits, "{a} {op:?} {b}");
                // A big answer is a cell of its own, so giving it up leaves
                // both operands.
                if let Some(cell) = got.as_cell() {
                    assert!(x.as_cell() != Some(cell) && y.as_cell() != Some(cell));
                    assert_eq!(heap.release(cell), Ok(()));
                }
            }
            for op in COMPARE {
                let holds = op.holds(a.cmp(b));
                assert_eq!(compare(&heap, op, x, y), Ok(holds), "{a} {op:?} {b}");
            }
            for cell in [x, y].into_iter().filter_map(Value::as_cell) {
                assert_eq!(heap.release(cell), Ok(()));
            }
            assert_eq!(heap.live(), 0, "{a} {b}");
        }
    }
}

/// So is a negate, whose answer is a cell of its own or none.
#[test]
fn a_negate_agrees_with_num_bigint() {
    let mut heap = Heap::default();
    for n in sizes() {
        let x = bigint::value(&mut heap, n.clone()).expect("room");
        let got = neg(&mut heap, x).expect("room");
        assert_eq!(read(&heap, got).expect("an Int").big(), -&n, "-{n}");
        let fits = (-&n).to_i64().and_then(Value::int).is_some();
        assert_eq!(got.small_int().is_some(), fits, "-{n}");
        for cell in [x, got].into_iter().filter_map(Value::as_cell) {
            assert_eq!(heap.release(cell), Ok(()));
        }
        assert_eq!(heap.live(), 0, "-{n}");
    }
    let nil = neg(&mut heap, Value::NIL).map(Value::bits);
    assert_eq!(nil, Err(Stop::missing(Missing::IntOperation(View::Nil))));
}

#[test]
fn an_int_becomes_the_nearest_float() {
    let mut heap = Heap::default();
    let mut float = |n: BigInt| {
        let v = bigint::value(&mut heap, n).expect("room");
        to_float(&heap, v).map(Value::bits)
    };
    let bits = |x: f64| Ok(x.to_bits());
    assert_eq!(float(BigInt::from(0)), bits(0.0));
    assert_eq!(float(BigInt::from(MIN)), bits(-1_125_899_906_842_624.0));
    assert_eq!(float(BigInt::from(1_i64 << 60)), bits((2.0_f64).powi(60)));
    // Halfway between two Floats, and one past it.
    let half: BigInt = (BigInt::from(1) << 53) + 1;
    assert_eq!(float(half.clone()), bits(9_007_199_254_740_992.0));
    assert_eq!(float(half + 2), bits(9_007_199_254_740_996.0));
    let huge: BigInt = BigInt::from(1) << 2000;
    assert_eq!(float(huge.clone()), bits(f64::MAX));
    assert_eq!(float(-huge), bits(f64::MIN));
    let nil = to_float(&heap, Value::NIL).map(Value::bits);
    assert_eq!(nil, Err(Stop::missing(Missing::IntOperation(View::Nil))));
}
