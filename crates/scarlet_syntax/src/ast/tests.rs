use super::*;
use crate::{parser, scanner};

fn span(sl: i32, sc: i32, el: i32, ec: i32) -> Span {
    Span {
        start_line: sl,
        start_column: sc,
        end_line: el,
        end_column: ec,
    }
}

#[test]
fn statement_span_covers_all_kinds() {
    let src = "import scarlet/array\n\
                   x = 5\n\
                   fn f() { 0 }\n\
                   pub fn g() { 1 }\n\
                   (a, b) = (1, 2)\n\
                   Some(x) = Some(1)\n\
                   Nil = println('hi')\n";
    let mut sc = scanner::new_scanner(src.to_string());
    let p = parser::new_parser(&mut sc);
    let r = p.parse_program();
    assert!(
        r.diagnostics.is_empty(),
        "parse errors: {:?}",
        r.diagnostics
    );

    let mut saw_tuple = false;
    let mut saw_ctor = false;
    let mut count = 0;
    for node in &r.ast.body {
        if let Node::Statement(s) = node {
            let sp = s.span();
            assert!(
                sp.end_line > sp.start_line
                    || (sp.end_line == sp.start_line && sp.end_column >= sp.start_column),
                "degenerate span for {s:?}: {sp:?}"
            );
            count += 1;
            match s.as_ref() {
                Statement::Binding(b) if matches!(b.pattern, Pattern::Tuple { .. }) => {
                    saw_tuple = true
                }
                Statement::Binding(b) if matches!(b.pattern, Pattern::Constructor { .. }) => {
                    saw_ctor = true
                }
                _ => {}
            }
        }
    }
    assert!(count >= 6, "expected every statement kind, saw {count}");
    assert!(saw_tuple, "no tuple binding parsed");
    assert!(saw_ctor, "no constructor binding parsed");
}

#[test]
fn call_arg_and_error_node_spans() {
    let err = |s| {
        Expression::ErrorNode(ErrorNode {
            error: ParseError::ScannerError,
            span: s,
        })
    };

    let enode = span(0, 0, 0, 3);
    assert_eq!(err(enode).span(), enode);

    assert_eq!(
        CallArg::Positional(err(span(1, 0, 1, 4))).span(),
        span(1, 0, 1, 4)
    );
    assert_eq!(
        CallArg::Spread(err(span(2, 1, 2, 7))).span(),
        span(2, 1, 2, 7)
    );

    // A labelled arg spans from the label's start to the value's end.
    let labeled = CallArg::Labeled {
        label: Identifier {
            name: "k".to_string(),
            span: span(3, 2, 3, 3),
        },
        value: err(span(3, 5, 3, 11)),
        punned: false,
    };
    assert_eq!(labeled.span(), span(3, 2, 3, 11));
}

fn num(value: &str) -> NumberLiteral {
    NumberLiteral {
        value: value.to_string(),
        span: span(0, 0, 0, value.len() as i32),
    }
}

#[test]
fn as_int_parses_hex_bin_decimal_and_rejects_overflow() {
    assert_eq!(num("0xFF").as_int(), Some(255));
    assert_eq!(num("0xff").as_int(), Some(255));
    assert_eq!(num("0X10").as_int(), Some(16));
    assert_eq!(num("0b1010").as_int(), Some(10));
    assert_eq!(num("0B11").as_int(), Some(3));
    assert_eq!(num("0xDE_AD_BE_EF").as_int(), Some(3735928559));
    assert_eq!(num("-0xFF").as_int(), Some(-255));
    assert_eq!(num("255").as_int(), Some(255));
    assert_eq!(num("0x7FFFFFFFFFFFFFFF").as_int(), Some(i64::MAX));
    assert_eq!(num("-0x8000000000000000").as_int(), Some(i64::MIN));
    assert_eq!(num("-9223372036854775808").as_int(), Some(i64::MIN));
    // Magnitudes past i64::MAX overflow in every radix; hex is not bits.
    assert_eq!(num("0x8000000000000000").as_int(), None);
    assert_eq!(num("0xFFFFFFFFFFFFFFFF").as_int(), None);
    assert_eq!(num("9223372036854775808").as_int(), None);
    assert_eq!(num("1.5").as_int(), None);
}
