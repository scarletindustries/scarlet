use super::*;
use scarlet_ir::TypeId;
use scarlet_ir::core_ir::{FuncIdx, VariantRef};

fn name(type_id: i32, variant_idx: u16) -> VariantRef {
    VariantRef {
        type_id: TypeId(type_id),
        variant_idx,
    }
}

/// Two cells are equal when they are the same constructor with equal fields.
/// The variant in their words is not enough to say so, in the arena or in a
/// box, and neither is what they hold.
#[test]
fn only_the_same_constructor_is_equal() {
    let names = [
        (name(1, 0), 2),
        (name(1, 4), 2),
        (name(2, 4), 2),
        (name(1, 5), 300),
        (name(2, 5), 300),
    ];
    let (mut heap, ctors) = Heap::knowing(&names);
    let mut cells = Vec::new();
    for ((_, fields), ctor) in names.into_iter().zip(ctors) {
        let text = Value::cell(heap.string(b"held").expect("room"));
        let mut values = vec![Value::NIL; fields];
        values[fields - 1] = text;
        let same = [(); 2].map(|()| Value::cell(heap.ctor(ctor, &values).expect("room")));
        values[fields - 1] = Value::bool(true);
        let other = heap.ctor(ctor, &values).expect("room");
        cells.extend(same.into_iter().chain([Value::cell(other)]));
    }
    let text = Value::cell(heap.string(b"held").expect("room"));
    let pair = heap.tuple(&[Value::NIL, text]).expect("room");
    cells.push(Value::cell(pair));
    for (i, a) in cells.iter().enumerate() {
        for (j, b) in cells.iter().enumerate() {
            let same = i / 3 == j / 3 && (i % 3 == 2) == (j % 3 == 2);
            assert_eq!(equal(&heap, *a, *b), same, "{i} {j}");
        }
    }
}

/// Closures are equal when they run the same function over equal captures.
#[test]
fn closures_compare_by_their_function_and_captures() {
    let mut heap = Heap::default();
    let one = Value::bool(true);
    let mut made =
        |f, captures: &[Value]| Value::cell(heap.closure(FuncIdx(f), captures).expect("room"));
    let first = made(3, &[one, Value::NIL]);
    let same = made(3, &[one, Value::NIL]);
    let others = [
        made(4, &[one, Value::NIL]),
        made(3, &[Value::NIL, one]),
        made(3, &[one]),
        made(3, &[one, Value::NIL, Value::NIL]),
    ];
    let like = heap
        .tuple(&[Value::func(FuncIdx(3)), one, Value::NIL])
        .expect("room");
    assert!(equal(&heap, first, same));
    for other in others.into_iter().chain([Value::cell(like)]) {
        assert!(!equal(&heap, first, other));
    }
}
