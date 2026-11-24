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
    let ops = [Arith::Add, Arith::Sub, Arith::Mul, Arith::Div, Arith::Rem];
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

fn op_values(heap: &mut Heap, op: Arith, x: Value, y: Value) -> Value {
    let read = |heap: &Heap, v: Value| match v.small_int() {
        Some(n) => Int::Small(n),
        None => Int::Big(heap.read_big_int(v.as_cell().expect("a cell"))),
    };
    let (a, b) = (read(heap, x), read(heap, y));
    super::op(heap, op, a, b).expect("room")
}
