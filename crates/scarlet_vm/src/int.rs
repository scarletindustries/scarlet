//! What the Int instructions do: each operator on two small Ints, and the
//! same operators on Ints of any size.
//!
//! An instruction asks [`fast`] when both its operands are small Ints. When
//! one is not, or `fast` has no answer, it asks [`op`], out of line. [`op`]
//! alone says what every operator gives, and `fast` gives the same or nothing.

use num_traits::ToPrimitive;

use crate::bigint::{self, Int};
use crate::heap::{Heap, Kind};
use crate::ops::{Arith, Compare};
use crate::stop::{Missing, Stop, fault};
use crate::value::{SmallInt, Value, View};

/// `a op b`, or `None` when the answer is no small Int. An instruction names
/// its operator, so inlined there this is that operator alone.
#[inline(always)]
pub(crate) fn fast(op: Arith, a: SmallInt, b: SmallInt) -> Option<Value> {
    let raised = match op {
        Arith::Add => a.raised().checked_add(b.raised())?,
        Arith::Sub => a.raised().checked_sub(b.raised())?,
        // With one factor raised and the other not, the product is raised.
        Arith::Mul => a.raised().checked_mul(b.get())?,
        // `checked_div` is `None` only for a divisor of 0 here, because no
        // small Int is as low as an `i64` goes. The lowest one over -1 fits
        // an `i64` and is no small Int, so the answer is tested.
        Arith::Div => return Value::int(a.get().checked_div(b.get()).unwrap_or(0)),
        Arith::Rem => return Value::int(a.get().checked_rem(b.get()).unwrap_or(a.get())),
    };
    Some(SmallInt::lower(raised))
}

/// `-a`, or `None` for the lowest small Int.
#[inline(always)]
pub(crate) fn fast_neg(a: SmallInt) -> Option<Value> {
    a.raised().checked_neg().map(SmallInt::lower)
}

/// The Int `v` holds.
pub(crate) fn read(heap: &Heap, v: Value) -> Result<Int, Stop> {
    match v.view() {
        View::Int(n) => Ok(Int::Small(n)),
        View::Cell(cell) if heap.is(cell, Kind::BigInt) => Ok(Int::Big(heap.read_big_int(cell))),
        v @ (View::Float(_)
        | View::Nil
        | View::Bool(_)
        | View::EmptyArray
        | View::Func(_)
        | View::Cell(_)
        | View::Nullary(_)) => Err(Stop::missing(Missing::IntOperation(v))),
    }
}

/// `x op y` on Ints of any size: in `i128` when both fit one word, and
/// through `num-bigint` when not. The answer is never one of the operands'
/// own cells.
#[inline(always)]
pub(crate) fn op(heap: &mut Heap, op: Arith, x: Value, y: Value) -> Result<Value, Stop> {
    if let (Some(a), Some(b)) = (bigint::wide(heap, x), bigint::wide(heap, y))
        && let Some(v) = bigint::wide_op(heap, op, a, b)
    {
        return v.map_err(fault);
    }
    let (a, b) = (read(heap, x)?, read(heap, y)?);
    bigint::op(heap, op, a, b).map_err(fault)
}

/// `-x` on an Int of any size. The answer is never `x`'s own cell.
pub(crate) fn neg(heap: &mut Heap, x: Value) -> Result<Value, Stop> {
    if let Some(n) = bigint::wide(heap, x)
        && let Some(v) = bigint::wide_op(heap, Arith::Sub, 0, n)
    {
        return v.map_err(fault);
    }
    let n = read(heap, x)?;
    bigint::neg(heap, n).map_err(fault)
}

/// Whether `x op y` holds, on Ints of any size.
#[cold]
#[inline(never)]
pub(crate) fn compare(heap: &Heap, op: Compare, x: Value, y: Value) -> Result<bool, Stop> {
    let order = match (bigint::wide(heap, x), bigint::wide(heap, y)) {
        (Some(a), Some(b)) => a.cmp(&b),
        _ => read(heap, x)?.big().cmp(&read(heap, y)?.big()),
    };
    Ok(op.holds(order))
}

/// The Float nearest an Int of any size. One too large stops at the largest
/// Float.
pub(crate) fn to_float(heap: &Heap, x: Value) -> Result<Value, Stop> {
    Ok(Value::float(match read(heap, x)? {
        Int::Small(n) => n as f64,
        Int::Big(n) => n.to_f64().unwrap_or(0.0),
    }))
}

#[cfg(test)]
mod tests;
