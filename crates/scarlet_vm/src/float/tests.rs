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
    // Order is kept, so an overflowed product is still bigger than 1.0.
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
