//! Float arithmetic, following `docs/semantics.md`'s rule that no Float is
//! NaN or infinite. `x / 0.0` is `0.0` and `x % 0.0` is `x`, as for Int, so
//! `a == b * (a / b) + a % b` still holds. An answer too large stops at the
//! largest Float with its sign, and one with no answer is `0.0`.
//! [`Value::float`] does those last two.

use crate::value::Value;

/// An operation on two numbers. Here they are Floats. Elsewhere they can be
/// either kind, for operators whose operand type is only known when they run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NumOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Lt,
    Le,
    Gt,
    Ge,
}

pub(crate) fn op(op: NumOp, a: f64, b: f64) -> Value {
    match op {
        NumOp::Add => Value::float(a + b),
        NumOp::Sub => Value::float(a - b),
        NumOp::Mul => Value::float(a * b),
        NumOp::Div if b == 0.0 => Value::float(0.0),
        NumOp::Div => Value::float(a / b),
        NumOp::Rem if b == 0.0 => Value::float(a),
        NumOp::Rem => Value::float(a % b),
        NumOp::Lt => Value::bool(a < b),
        NumOp::Le => Value::bool(a <= b),
        NumOp::Gt => Value::bool(a > b),
        NumOp::Ge => Value::bool(a >= b),
    }
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
