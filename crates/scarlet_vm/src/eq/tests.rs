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

/// The empty array is equal to itself and to no other value, an array with elements
/// among them.
#[test]
fn the_empty_array_equals_only_itself() {
    let mut heap = Heap::default();
    let empty = Value::EMPTY_ARRAY;
    assert!(equal(&heap, empty, empty));
    let held = [(); 2].map(|()| Value::cell(heap.tuple(&[empty, Value::NIL]).expect("room")));
    assert!(equal(&heap, held[0], held[1]));
    let others = [
        Value::NIL,
        Value::bool(false),
        Value::bool(true),
        Value::int(0).expect("small"),
        Value::int(3).expect("small"),
        Value::float(0.0),
        Value::func(FuncIdx(0)),
        Value::nullary(name(0, 0)),
        Value::cell(heap.string(b"").expect("room")),
        Value::cell(heap.make(Kind::Binary, &[0]).expect("room")),
        Value::cell(heap.tuple(&[]).expect("room")),
        Value::cell(map::empty(&mut heap).expect("room")),
        array::from_values(&mut heap, &[Value::NIL]).expect("room"),
        array::from_values(&mut heap, &[empty]).expect("room"),
        array::range(&mut heap, 0, 1).expect("room"),
        held[0],
    ];
    for other in others {
        assert!(!equal(&heap, empty, other), "{other:?}");
        assert!(!equal(&heap, other, empty), "{other:?}");
        let pairs = [(empty, other), (other, empty)]
            .map(|(a, b)| [a, b].map(|v| Value::cell(heap.tuple(&[v]).expect("room"))));
        for [a, b] in pairs {
            assert!(!equal(&heap, a, b), "{other:?}");
        }
    }
}

/// Two arrays are equal when they have equal elements, in whichever form each is, and an
/// array equals nothing that is not one.
#[test]
fn arrays_compare_in_every_form() {
    let mut heap = Heap::default();
    let ints: Vec<Value> = (3..6).map(|n| Value::int(n).expect("small")).collect();
    let tree = array::from_values(&mut heap, &ints).expect("room");
    let same = array::from_values(&mut heap, &ints).expect("room");
    let range = array::range(&mut heap, 3, 6).expect("room");
    let range_too = array::range(&mut heap, 3, 6).expect("room");
    let equals = [tree, same, range, range_too];
    for a in equals {
        for b in equals {
            assert!(equal(&heap, a, b));
        }
    }
    let shorter = array::from_values(&mut heap, &ints[..2]).expect("room");
    let others = [
        shorter,
        array::range(&mut heap, 3, 5).expect("room"),
        array::range(&mut heap, 4, 7).expect("room"),
        array::from_values(&mut heap, &[ints[0], ints[2], ints[1]]).expect("room"),
        Value::cell(heap.tuple(&ints).expect("room")),
        Value::cell(heap.make(Kind::Binary, &[3, 4]).expect("room")),
        Value::NIL,
    ];
    for a in equals {
        for b in others {
            assert!(!equal(&heap, a, b) && !equal(&heap, b, a), "{a:?} {b:?}");
        }
    }
}
