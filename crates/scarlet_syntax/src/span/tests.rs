use super::Span;

#[test]
fn dummy_contains_no_point() {
    assert!(!Span::DUMMY.contains(0, 0));
}

#[test]
fn union_is_order_independent() {
    let a = Span::single_line(1, 2, 5);
    let b = Span::single_line(3, 0, 4);
    assert_eq!(a.union(&b), b.union(&a));
    assert_eq!(
        a.union(&b),
        Span {
            start_line: 1,
            start_column: 2,
            end_line: 3,
            end_column: 4,
        }
    );
}
