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
