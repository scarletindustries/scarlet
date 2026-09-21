//! Float arithmetic, by `docs/semantics.md`'s rule that no Float is NaN or
//! infinite: `x / 0.0` is `0.0`, `x % 0.0` is `x` (both as for Int, so
//! `a == b * (a / b) + a % b` still holds), an answer too large stops at the
//! largest Float with its sign, and one with no answer is `0.0`. The last two
//! are [`Value::float`]'s.

use crate::value::Value;

/// A two-operand operation on numbers: of Floats here, or of either kind for
/// the operators whose operand type is only known when they run.
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
mod tests {
    use super::*;
    use crate::value::View;

    fn float(v: Value) -> f64 {
        match v.view() {
            View::Float(f) => f,
            other => panic!("not a Float: {other:?}"),
        }
    }

    #[test]
    fn no_answer_is_zero_and_too_large_stops_at_the_largest() {
        assert_eq!(float(op(NumOp::Div, 1.5, 0.0)), 0.0);
        assert_eq!(float(op(NumOp::Div, 0.0, 0.0)), 0.0);
        assert_eq!(float(op(NumOp::Rem, 7.5, 0.0)), 7.5);
        assert_eq!(float(op(NumOp::Rem, -7.5, 2.0)), -1.5);
        assert_eq!(float(op(NumOp::Mul, f64::MAX, 2.0)), f64::MAX);
        assert_eq!(float(op(NumOp::Mul, f64::MAX, -2.0)), f64::MIN);
        assert_eq!(float(op(NumOp::Sub, f64::MIN, f64::MAX)), f64::MIN);
        // Order is kept: an overflowed product still beats 1.0.
        let big = float(op(NumOp::Mul, 1e300, 1e300));
        assert!(big > 1.0);
    }

    #[test]
    fn a_float_always_shows_a_point() {
        assert_eq!(text(1.0), "1.0");
        assert_eq!(text(-0.0), "-0.0");
        assert_eq!(text(1.5), "1.5");
        assert_eq!(text(0.1 + 0.2), "0.30000000000000004");
        assert_eq!(text(1e21), "1000000000000000000000.0");
    }
}
