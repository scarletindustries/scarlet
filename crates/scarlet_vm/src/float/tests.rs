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
    assert_eq!(float(op(Arith::Div, 1.5, 0.0)), 0.0);
    assert_eq!(float(op(Arith::Div, 0.0, 0.0)), 0.0);
    assert_eq!(float(op(Arith::Rem, 7.5, 0.0)), 7.5);
    assert_eq!(float(op(Arith::Rem, -7.5, 2.0)), -1.5);
    assert_eq!(float(op(Arith::Mul, f64::MAX, 2.0)), f64::MAX);
    assert_eq!(float(op(Arith::Mul, f64::MAX, -2.0)), f64::MIN);
    assert_eq!(float(op(Arith::Sub, f64::MIN, f64::MAX)), f64::MIN);
    // Order is kept, so an overflowed product is still bigger than 1.0.
    let big = float(op(Arith::Mul, 1e300, 1e300));
    assert!(big > 1.0);
}

/// Floats at every edge: both zeros, the smallest and the largest, ones that
/// overflow or vanish together, and ones with no exact form.
fn edges() -> Vec<f64> {
    let each = [
        0.0,
        f64::from_bits(1),
        f64::MIN_POSITIVE,
        f64::EPSILON,
        1.0 / 3.0,
        0.1,
        1.0,
        1.5,
        2.0,
        7.5,
        1e-300,
        1e300,
        9_007_199_254_740_993.0,
        f64::MAX,
    ];
    each.into_iter().flat_map(|x| [x, -x]).collect()
}

const ARITH: [Arith; 5] = [Arith::Add, Arith::Sub, Arith::Mul, Arith::Div, Arith::Rem];
const ORDER: [Order; 4] = [Order::Lt, Order::Le, Order::Gt, Order::Ge];

/// The fast path gives what the rules give, bit for bit, or nothing. It gives
/// nothing exactly when the plain answer is a NaN or an infinity.
#[test]
fn the_fast_path_answers_as_the_rules_do_or_not_at_all() {
    for x in edges() {
        for y in edges() {
            let (a, b) = (Value::float(x), Value::float(y));
            for op in ARITH {
                let plain = match op {
                    Arith::Add => x + y,
                    Arith::Sub => x - y,
                    Arith::Mul => x * y,
                    Arith::Div => x / y,
                    Arith::Rem => x % y,
                };
                match fast(op, a, b) {
                    Some(v) => {
                        assert_eq!(v.bits(), super::op(op, x, y).bits(), "{x:e} {op:?} {y:e}");
                        assert!(plain.is_finite(), "{x:e} {op:?} {y:e}");
                    }
                    None => assert!(!plain.is_finite(), "{x:e} {op:?} {y:e}"),
                }
            }
            for op in ORDER {
                let holds = match op {
                    Order::Lt => x < y,
                    Order::Le => x <= y,
                    Order::Gt => x > y,
                    Order::Ge => x >= y,
                };
                assert_eq!(fast_compare(op, a, b), Some(holds), "{x:e} {op:?} {y:e}");
                assert_eq!(compare(op, x, y), holds, "{x:e} {op:?} {y:e}");
            }
        }
        let negated = fast_neg(Value::float(x)).expect("minus a Float is one");
        assert_eq!(negated.bits(), (-x).to_bits(), "{x:e}");
    }
}

/// A word that is no Float gets through no operator, on either side.
#[test]
fn the_fast_path_answers_nothing_for_a_word_that_is_no_float() {
    let variant = scarlet_ir::core_ir::VariantRef {
        type_id: scarlet_ir::TypeId(263),
        variant_idx: 1,
    };
    let tagged = [
        Value::NIL,
        Value::bool(false),
        Value::bool(true),
        Value::int(0).expect("small"),
        Value::int(-1).expect("small"),
        Value::int((1 << 50) - 1).expect("small"),
        Value::int(-(1 << 50)).expect("small"),
        Value::func(scarlet_ir::core_ir::FuncIdx(3)),
        Value::nullary(variant),
        Value::cell(crate::heap::Cell::from_bits(1)),
    ];
    for t in tagged {
        assert!(fast_neg(t).is_none(), "{t:?}");
        let floats = edges().into_iter().map(Value::float);
        for v in floats.chain(tagged) {
            for op in ARITH {
                assert!(fast(op, t, v).is_none(), "{t:?} {op:?} {v:?}");
                assert!(fast(op, v, t).is_none(), "{v:?} {op:?} {t:?}");
            }
            for op in ORDER {
                assert!(fast_compare(op, t, v).is_none(), "{t:?} {op:?} {v:?}");
                assert!(fast_compare(op, v, t).is_none(), "{v:?} {op:?} {t:?}");
            }
        }
    }
}

/// The loader swaps a constant to the side the instruction holds, which is
/// right because an operator that commutes gives the same bits either way
/// round.
#[test]
fn an_operator_that_commutes_gives_the_same_float_either_way_round() {
    for x in edges() {
        for y in edges() {
            for op in ARITH.into_iter().filter(|op| op.commutes()) {
                let (one, other) = (super::op(op, x, y), super::op(op, y, x));
                assert_eq!(one.bits(), other.bits(), "{x:e} {op:?} {y:e}");
            }
        }
    }
}

#[test]
fn a_float_always_shows_a_point() {
    assert_eq!(text(1.0), "1.0");
    assert_eq!(text(-0.0), "-0.0");
    assert_eq!(text(1.5), "1.5");
    assert_eq!(text(0.1 + 0.2), "0.30000000000000004");
    assert_eq!(text(1e21), "1000000000000000000000.0");
}
