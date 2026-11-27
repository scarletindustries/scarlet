//! Float arithmetic, following `docs/semantics.md`'s rule that no Float is
//! NaN or infinite. `x / 0.0` is `0.0` and `x % 0.0` is `x`, as for Int, so
//! `a == b * (a / b) + a % b` still holds. An answer too large stops at the
//! largest Float with its sign, and one with no answer is `0.0`.
//! [`Value::float`] does those last two.
//!
//! A Float instruction does its operator on the two words as they are and
//! tests only the answer. A word that is no Float is a NaN, so it gives a
//! NaN, and so does an answer that is none, and one too large is an
//! infinity. [`Value::finite`] refuses all three, and the instruction then
//! reads the two words again, out of line, and does what [`op`] says or stops.
//! An operator may work this way only if a NaN operand always gives it a NaN.
//! `min`, `max` and `pow` do not, so they are built-ins that check what they
//! are given. A comparison has no answer to test, so it asks `partial_cmp`,
//! which gives `None` when an operand is a NaN.

use crate::ops::{Arith, Order};
use crate::stop::{Broken, Stop};
use crate::value::{Value, View};

/// `a op b` for two Floats, by the rules above.
pub(crate) fn op(op: Arith, a: f64, b: f64) -> Value {
    match op {
        Arith::Add => Value::float(a + b),
        Arith::Sub => Value::float(a - b),
        Arith::Mul => Value::float(a * b),
        Arith::Div if b == 0.0 => Value::float(0.0),
        Arith::Div => Value::float(a / b),
        Arith::Rem if b == 0.0 => Value::float(a),
        Arith::Rem => Value::float(a % b),
    }
}

/// Whether `a op b` holds for two Floats. Two Floats always stand in some
/// order, because neither is a NaN.
pub(crate) fn compare(op: Order, a: f64, b: f64) -> bool {
    a.partial_cmp(&b).is_some_and(|order| op.holds(order))
}

/// `x op y` on two words that should be Floats, or `None` when one is not or
/// the answer is none or too large. An instruction names its operator, so
/// inlined there this is that operator alone.
#[inline(always)]
pub(crate) fn fast(op: Arith, x: Value, y: Value) -> Option<Value> {
    let (x, y) = (x.unchecked_float(), y.unchecked_float());
    Value::finite(match op {
        Arith::Add => x + y,
        Arith::Sub => x - y,
        Arith::Mul => x * y,
        // By zero this is an infinity or a NaN, and the remainder a NaN.
        Arith::Div => x / y,
        Arith::Rem => x % y,
    })
}

/// `-x`, or `None` when `x` is no Float. Minus a small Int's word would be a
/// word with a cell's tag, so a negate needs its test more than any other
/// operator.
#[inline(always)]
pub(crate) fn fast_neg(x: Value) -> Option<Value> {
    Value::finite(-x.unchecked_float())
}

/// Whether `x op y` holds of two words that should be Floats, or `None` when
/// one is not.
#[inline(always)]
pub(crate) fn fast_compare(op: Order, x: Value, y: Value) -> Option<bool> {
    let order = x.unchecked_float().partial_cmp(&y.unchecked_float())?;
    Some(op.holds(order))
}

/// The Float `v` is.
pub(crate) fn read(v: Value) -> Result<f64, Stop> {
    match v.view() {
        View::Float(f) => Ok(f),
        v @ (View::Int(_)
        | View::Nil
        | View::Bool(_)
        | View::EmptyArray
        | View::Func(_)
        | View::Cell(_)
        | View::Nullary(_)) => Err(Stop::broken(Broken::FloatOperation(v))),
    }
}

/// Whether `x op y` holds, for a comparison with an operand that is no Float.
#[cold]
#[inline(never)]
pub(crate) fn slow_compare(op: Order, x: Value, y: Value) -> Result<bool, Stop> {
    Ok(compare(op, read(x)?, read(y)?))
}

/// A Float as `println` shows it: always with a decimal point, so `1.0` does
/// not read as the Int `1`.
pub(crate) fn text(f: f64) -> String {
    let mut s = f.to_string();
    if !s.bytes().any(|b| matches!(b, b'.' | b'e' | b'E')) {
        s.push_str(".0");
    }
    s
}

#[cfg(test)]
mod tests;
