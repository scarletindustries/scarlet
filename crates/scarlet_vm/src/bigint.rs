//! Int arithmetic, small and big.
//!
//! An Int is always exact (`docs/semantics.md`). One that fits in 51 bits
//! lives in the value word, and any other is a heap cell. Every result goes
//! back into that form, so a result that fits is always small. That gives
//! each number one representation, and a small Int never equals a big one.
//!
//! Two small operands take the fast path, which is a checked operation on
//! `i64`. As soon as an answer needs more room, `num-bigint` does the work.

use num_bigint::BigInt;
use num_traits::{ToPrimitive, Zero};

use crate::code::IntOp;
use crate::heap::{Fault, Heap};
use crate::value::Value;

/// The most bits one Int can have, because a cell is at most 2^28 words of
/// 64 bits. An Int wider than that makes the heap full instead.
pub(crate) const MAX_BITS: u64 = (1 << 28) * 64;

/// An Int operand, read out of its value.
pub(crate) enum Int {
    Small(i64),
    Big(BigInt),
}

/// The number in decimal, as `int.to_string` writes it.
impl std::fmt::Display for Int {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Int::Small(n) => n.fmt(f),
            Int::Big(n) => n.fmt(f),
        }
    }
}

impl Int {
    pub(crate) fn big(self) -> BigInt {
        match self {
            Int::Small(n) => BigInt::from(n),
            Int::Big(n) => n,
        }
    }
}

/// `a op b`, following `docs/semantics.md`: `/` truncates toward zero and
/// `x / 0` is 0; `%` takes the dividend's sign and `x % 0` is `x`.
pub(crate) fn op(heap: &mut Heap, op: IntOp, a: Int, b: Int) -> Result<Value, Fault> {
    if let (Int::Small(x), Int::Small(y)) = (&a, &b)
        && let Some(v) = small_op(op, *x, *y)
    {
        return Ok(v);
    }
    let (a, b) = (a.big(), b.big());
    #[expect(
        clippy::arithmetic_side_effects,
        reason = "an Int grows instead of overflowing"
    )]
    let n = match op {
        IntOp::Add => a + b,
        IntOp::Sub => a - b,
        IntOp::Mul => a * b,
        // `num-bigint` panics on a zero divisor, so the zero rules come
        // first. `checked_div` is `None` only for that divisor.
        IntOp::Div => a.checked_div(&b).unwrap_or_else(BigInt::zero),
        IntOp::Rem if b.is_zero() => a,
        IntOp::Rem => a % b,
        IntOp::Eq => return Ok(Value::bool(a == b)),
        IntOp::Ne => return Ok(Value::bool(a != b)),
        IntOp::Lt => return Ok(Value::bool(a < b)),
        IntOp::Le => return Ok(Value::bool(a <= b)),
        IntOp::Gt => return Ok(Value::bool(a > b)),
        IntOp::Ge => return Ok(Value::bool(a >= b)),
    };
    value(heap, n)
}

pub(crate) fn neg(heap: &mut Heap, a: Int) -> Result<Value, Fault> {
    if let Int::Small(x) = a
        && let Some(v) = x.checked_neg().and_then(Value::int)
    {
        return Ok(v);
    }
    #[expect(
        clippy::arithmetic_side_effects,
        reason = "an Int grows instead of overflowing"
    )]
    let n = -a.big();
    value(heap, n)
}

/// `v` as an `i128`, when it is a small Int or a big one of one word. Those
/// are most of the big Ints a program makes, such as a sum just past 51
/// bits, and `i128` arithmetic on them needs no `num-bigint`.
pub(crate) fn wide(heap: &Heap, v: Value) -> Option<i128> {
    match v.small_int() {
        Some(n) => Some(i128::from(n)),
        None => v
            .as_cell()
            .filter(|c| heap.kind(*c) == Some(crate::heap::Kind::BigInt))
            .and_then(|c| heap.big_int_word(c)),
    }
}

/// `a op b` on two operands [`wide`] read, or `None` when the answer does
/// not fit an `i128`. The rules are [`op`]'s.
pub(crate) fn wide_op(
    heap: &mut Heap,
    op: IntOp,
    a: i128,
    b: i128,
) -> Option<Result<Value, Fault>> {
    let n = match op {
        IntOp::Add => a.checked_add(b)?,
        IntOp::Sub => a.checked_sub(b)?,
        IntOp::Mul => a.checked_mul(b)?,
        IntOp::Div if b == 0 => 0,
        IntOp::Div => a.checked_div(b)?,
        IntOp::Rem if b == 0 => a,
        IntOp::Rem => a.checked_rem(b)?,
        IntOp::Eq => return Some(Ok(Value::bool(a == b))),
        IntOp::Ne => return Some(Ok(Value::bool(a != b))),
        IntOp::Lt => return Some(Ok(Value::bool(a < b))),
        IntOp::Le => return Some(Ok(Value::bool(a <= b))),
        IntOp::Gt => return Some(Ok(Value::bool(a > b))),
        IntOp::Ge => return Some(Ok(Value::bool(a >= b))),
    };
    if let Some(v) = i64::try_from(n).ok().and_then(Value::int) {
        return Some(Ok(v));
    }
    Some(heap.big_int_i128(n).map(Value::cell))
}

/// The fast path, or `None` when the answer does not fit a small Int.
#[inline]
pub(crate) fn small_op(op: IntOp, a: i64, b: i64) -> Option<Value> {
    let n = match op {
        IntOp::Add => a.checked_add(b)?,
        IntOp::Sub => a.checked_sub(b)?,
        IntOp::Mul => a.checked_mul(b)?,
        IntOp::Div if b == 0 => 0,
        IntOp::Div => a.checked_div(b)?,
        IntOp::Rem if b == 0 => a,
        IntOp::Rem => a.checked_rem(b)?,
        IntOp::Eq => return Some(Value::bool(a == b)),
        IntOp::Ne => return Some(Value::bool(a != b)),
        IntOp::Lt => return Some(Value::bool(a < b)),
        IntOp::Le => return Some(Value::bool(a <= b)),
        IntOp::Gt => return Some(Value::bool(a > b)),
        IntOp::Ge => return Some(Value::bool(a >= b)),
    };
    Value::int(n)
}

/// `n` in its one form: small when it fits, a heap cell when it does not.
/// A machine integer as an Int. One that fits in a small Int never touches
/// `num-bigint`, so it costs no allocation.
// The small case is a test and a tag, and goes inside the caller. Called out
// of line, its result comes back through memory, which costs more than the
// work does.
#[inline(always)]
pub(crate) fn int<N>(heap: &mut Heap, n: N) -> Result<Value, Fault>
where
    N: TryInto<i64> + Into<BigInt> + Copy,
{
    if let Some(v) = n.try_into().ok().and_then(Value::int) {
        return Ok(v);
    }
    big(heap, n.into())
}

#[inline(never)]
fn big(heap: &mut Heap, n: BigInt) -> Result<Value, Fault> {
    Ok(Value::cell(heap.big_int(&n)?))
}

pub(crate) fn value(heap: &mut Heap, n: BigInt) -> Result<Value, Fault> {
    if let Some(v) = n.to_i64().and_then(Value::int) {
        return Ok(v);
    }
    Ok(Value::cell(heap.big_int(&n)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `i128` path gives what `num-bigint` gives, for operands around
    /// every edge: 0, inside the small Int and at its bounds, one word, and
    /// past it.
    #[test]
    fn the_wide_path_agrees_with_num_bigint() {
        let mut heap = Heap::default();
        let edges: Vec<i128> = [
            0,
            1,
            2,
            7,
            1 << 47,
            (1 << 47) - 1,
            1 << 48,
            1 << 50,
            (1 << 50) - 1,
            (1 << 50) + 1,
            1 << 51,
            i128::from(i64::MAX),
            i128::from(u64::MAX),
            i128::from(u64::MAX) - 1,
            3_000_000_000_000_000_000,
        ]
        .into_iter()
        .flat_map(|n| [n, -n])
        .collect();
        let ops = [
            IntOp::Add,
            IntOp::Sub,
            IntOp::Mul,
            IntOp::Div,
            IntOp::Rem,
            IntOp::Eq,
            IntOp::Ne,
            IntOp::Lt,
            IntOp::Le,
            IntOp::Gt,
            IntOp::Ge,
        ];
        let show = |heap: &Heap, v: Value| match v.small_int() {
            Some(n) => n.to_string(),
            None => match v.as_cell() {
                Some(c) => heap.read_big_int(c).to_string(),
                None => format!("{:?}", v.view()),
            },
        };
        for &a in &edges {
            for &b in &edges {
                let (x, y) = (
                    value(&mut heap, BigInt::from(a)).expect("room"),
                    value(&mut heap, BigInt::from(b)).expect("room"),
                );
                let (wa, wb) = (wide(&heap, x), wide(&heap, y));
                for op in ops {
                    let want = op_values(&mut heap, op, x, y);
                    let got = match (wa, wb) {
                        (Some(wa), Some(wb)) => match wide_op(&mut heap, op, wa, wb) {
                            Some(v) => v.expect("room"),
                            None => continue,
                        },
                        _ => continue,
                    };
                    assert_eq!(show(&heap, got), show(&heap, want), "{a} {op:?} {b}");
                    // One number has one form, so the cell is the one
                    // `num-bigint`'s result would make, hash and all.
                    if let (Some(g), Some(w)) = (got.as_cell(), want.as_cell()) {
                        assert_eq!(heap.data(g), heap.data(w), "{a} {op:?} {b}");
                    }
                }
            }
        }
    }

    /// The small Int range is not the same on both sides of 0: it holds
    /// `-2^50` and stops one short of `2^50`. So a divide or a negate of the
    /// lowest small Int has an answer that fits an `i64` and is still not
    /// small. Each number here must come back in its one form.
    #[test]
    fn divide_and_negate_keep_one_form_at_both_ends_of_the_small_range() {
        const MIN: i64 = -(1 << 50);
        const MAX: i64 = (1 << 50) - 1;
        let mut heap = Heap::default();
        let small = |v: Value| v.small_int();
        let big = |heap: &Heap, v: Value| v.as_cell().map(|c| heap.read_big_int(c));
        let past = BigInt::from(1_i64 << 50);

        assert!(small_op(IntOp::Div, MIN, -1).is_none());
        let v = wide_op(&mut heap, IntOp::Div, MIN.into(), -1)
            .expect("fits an i128")
            .expect("room");
        assert_eq!(big(&heap, v), Some(past.clone()));
        let v = op(&mut heap, IntOp::Div, Int::Small(MIN), Int::Small(-1)).expect("room");
        assert_eq!(big(&heap, v), Some(past.clone()));
        let v = neg(&mut heap, Int::Small(MIN)).expect("room");
        assert_eq!(big(&heap, v), Some(past.clone()));

        // Back the other way, the answer is the lowest small Int again.
        let v = neg(&mut heap, Int::Big(past.clone())).expect("room");
        assert_eq!(small(v), Some(MIN));
        let v = op(&mut heap, IntOp::Div, Int::Big(past), Int::Small(-1)).expect("room");
        assert_eq!(small(v), Some(MIN));
        let v = wide_op(&mut heap, IntOp::Div, 1 << 50, -1)
            .expect("fits an i128")
            .expect("room");
        assert_eq!(small(v), Some(MIN));

        // Nothing at the top end leaves the range.
        assert_eq!(small_op(IntOp::Div, MAX, -1).and_then(small), Some(-MAX));
        assert_eq!(small_op(IntOp::Div, MAX, 1).and_then(small), Some(MAX));
        assert_eq!(small_op(IntOp::Div, MIN, 1).and_then(small), Some(MIN));
        let v = neg(&mut heap, Int::Small(MAX)).expect("room");
        assert_eq!(small(v), Some(-MAX));
        let v = neg(&mut heap, Int::Small(-MAX)).expect("room");
        assert_eq!(small(v), Some(MAX));

        // One step past either end is a heap cell, and one step back is not.
        assert!(small_op(IntOp::Add, MAX, 1).is_none());
        assert!(small_op(IntOp::Sub, MIN, 1).is_none());
        assert!(small_op(IntOp::Mul, 1 << 25, 1 << 25).is_none());
        assert_eq!(
            small_op(IntOp::Mul, 1 << 25, -(1 << 25)).and_then(small),
            Some(MIN)
        );
        for (n, step) in [(MAX, 1), (MIN, -1)] {
            let out = wide_op(&mut heap, IntOp::Add, n.into(), step)
                .expect("fits an i128")
                .expect("room");
            assert!(out.as_cell().is_some(), "{n} + {step}");
            let out = wide(&heap, out).expect("one word");
            let back = wide_op(&mut heap, IntOp::Sub, out, step)
                .expect("fits an i128")
                .expect("room");
            assert_eq!(small(back), Some(n));
        }
        // The remainder rules at the ends: `x % 0` is `x`, and `MIN % -1` is 0.
        assert_eq!(small_op(IntOp::Rem, MIN, 0).and_then(small), Some(MIN));
        assert_eq!(small_op(IntOp::Rem, MIN, -1).and_then(small), Some(0));
        assert_eq!(small_op(IntOp::Div, MIN, 0).and_then(small), Some(0));
    }

    fn op_values(heap: &mut Heap, op: IntOp, x: Value, y: Value) -> Value {
        let read = |heap: &Heap, v: Value| match v.small_int() {
            Some(n) => Int::Small(n),
            None => Int::Big(heap.read_big_int(v.as_cell().expect("a cell"))),
        };
        let (a, b) = (read(heap, x), read(heap, y));
        super::op(heap, op, a, b).expect("room")
    }
}
