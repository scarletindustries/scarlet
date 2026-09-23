//! Int arithmetic, small and big.
//!
//! An Int is always exact (`docs/semantics.md`). One that fits in 48 bits
//! lives in the value word; any other is a heap cell. Every result is put back
//! in that form: a result that fits is always small. So each number has one
//! representation, and a small Int never equals a big one.
//!
//! Two small operands take the fast path, a checked operation on `i64`. The
//! moment an answer needs more room, the arithmetic moves to `num-bigint`.

use num_bigint::BigInt;
use num_traits::{ToPrimitive, Zero};

use crate::code::IntOp;
use crate::heap::{Full, Heap};
use crate::value::Value;

/// The most bits one Int can have: a cell is at most 2^28 words of 64 bits.
/// An Int wider than that is a full heap, not an Int.
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
pub(crate) fn op(heap: &mut Heap, op: IntOp, a: Int, b: Int) -> Result<Value, Full> {
    if let (Int::Small(x), Int::Small(y)) = (&a, &b)
        && let Some(v) = small_op(op, *x, *y)
    {
        return Ok(v);
    }
    let (a, b) = (a.big(), b.big());
    let n = match op {
        IntOp::Add => a + b,
        IntOp::Sub => a - b,
        IntOp::Mul => a * b,
        // `num-bigint` panics on a zero divisor, so the zero rules come first.
        IntOp::Div if b.is_zero() => BigInt::zero(),
        IntOp::Div => a / b,
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

pub(crate) fn neg(heap: &mut Heap, a: Int) -> Result<Value, Full> {
    if let Int::Small(x) = a
        && let Some(v) = x.checked_neg().and_then(Value::int)
    {
        return Ok(v);
    }
    value(heap, -a.big())
}

/// The fast path, or `None` when the answer does not fit a small Int.
fn small_op(op: IntOp, a: i64, b: i64) -> Option<Value> {
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
pub(crate) fn value(heap: &mut Heap, n: BigInt) -> Result<Value, Full> {
    if let Some(v) = n.to_i64().and_then(Value::int) {
        return Ok(v);
    }
    Ok(Value::cell(heap.big_int(&n)?))
}
