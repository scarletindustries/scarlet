use super::*;

/// The rounds are right: SipHash-2-4 gives the paper's answers
/// (Appendix A), with the key `00 01 .. 0f` and the message `00 01 ..`.
#[test]
fn siphash_2_4_matches_the_papers_test_vectors() {
    let k0 = u64::from_le_bytes([0, 1, 2, 3, 4, 5, 6, 7]);
    let k1 = u64::from_le_bytes([8, 9, 10, 11, 12, 13, 14, 15]);
    let run = |n: u8| {
        let mut s = Sip::<2, 4>::new(k0, k1);
        s.bytes(&(0..n).collect::<Vec<u8>>());
        s.finish()
    };
    assert_eq!(run(0), 0x726f_db47_dd0e_0e31);
    assert_eq!(run(1), 0x74f8_39c5_93dc_67fd);
    assert_eq!(run(15), 0xa129_ca61_49be_45e5);
}

/// A map's order comes from these hashes, and a program can see it. So
/// each is pinned, and no change to how a value is stored may move one.
#[test]
fn the_hash_of_a_plain_value_does_not_depend_on_its_word() {
    let mut heap = Heap::default();
    let nullary = |t, v| {
        Value::nullary(VariantRef {
            type_id: scarlet_ir::TypeId(t),
            variant_idx: v,
        })
    };
    let func = |f| Value::func(scarlet_ir::core_ir::FuncIdx(f));
    let int = |n| Value::int(n).expect("small");
    let pinned = [
        (Value::NIL, 0xd85b_9296_886d_def0),
        (Value::bool(false), 0x6994_423c_a56f_4da0),
        (Value::bool(true), 0x40d9_1fce_6a74_595d),
        (func(0), 0xb1a9_9d9e_d09c_0185),
        (func(7), 0x86bf_fefa_4fbe_9b58),
        (func(u32::MAX), 0x72a7_391d_e862_ae42),
        (nullary(0, 0), 0x5b0a_b266_62e7_7842),
        (nullary(263, 1), 0xa235_c7dd_2d07_de50),
        (nullary(-1, 7), 0x0cdd_652c_2674_8da6),
        (nullary(i32::MAX, u16::MAX), 0x78b0_7bfb_c87a_b9b6),
        (int(0), 0x6c85_978d_e7cc_c2e3),
        (int(-1), 0xd551_7ece_616f_fee6),
        (int(42), 0x45c9_0af3_9aaa_7d08),
        (Value::float(1.5), 0xe92d_7e62_b635_5bcb),
    ];
    for (v, want) in pinned {
        assert_eq!(hash(&heap, v), want, "{v:?}");
    }
    let pair = heap
        .tuple(&[Value::bool(true), nullary(263, 1)])
        .expect("room");
    assert_eq!(hash(&heap, Value::cell(pair)), 0x80b5_8fd6_9cae_65c5);
}

/// A constructor hashes by its type and variant and a closure by its function,
/// whatever class its cell has and wherever it comes in the program's table.
#[test]
fn the_hash_of_a_constructor_does_not_depend_on_its_class() {
    let name = |t, variant_idx| VariantRef {
        type_id: scarlet_ir::TypeId(t),
        variant_idx,
    };
    let int = |n| Value::int(n).expect("small");
    let pinned: [(i32, u16, u64, u64); 5] = [
        (263, 0, 0x734a_bf89_c28e_6c48, 0xd766_eb53_495b_a84f),
        (263, 1, 0x2524_36fa_db40_f0fb, 0x3888_b980_e343_6820),
        (264, 1, 0x8a82_d212_7222_adff, 0xb0b9_48b5_6ef1_c1c5),
        (-1, 7, 0xf7cc_4904_b0ba_2132, 0x2466_61cd_022d_9075),
        (
            i32::MAX,
            u16::MAX,
            0x7d24_9483_a509_66a8,
            0x7ef0_1472_87e7_9bf8,
        ),
    ];
    let mut names: Vec<(VariantRef, usize)> = pinned
        .iter()
        .flat_map(|(t, v, ..)| [(name(*t, *v), 1), (name(*t, *v), 3)])
        .collect();
    for _ in 0..2 {
        let (mut heap, ctors) = Heap::knowing(&names);
        for (t, v, one, three) in pinned {
            let ctor = |fields: usize| {
                let at = names.iter().position(|n| *n == (name(t, v), fields));
                ctors[at.expect("in the table")]
            };
            let inner = heap.ctor(ctor(1), &[int(7)]).expect("room");
            assert_eq!(hash(&heap, Value::cell(inner)), one, "{t}.{v}");
            let fields = [int(1), Value::cell(inner), Value::NIL];
            let outer = heap.ctor(ctor(3), &fields).expect("room");
            assert_eq!(hash(&heap, Value::cell(outer)), three, "{t}.{v}");
        }
        // The same names in another order have other classes.
        names.reverse();
    }
    let mut heap = Heap::default();
    let func = scarlet_ir::core_ir::FuncIdx(9);
    let closure = heap.closure(func, &[int(1), Value::NIL]).expect("room");
    let closure = Value::cell(closure);
    assert_eq!(hash(&heap, closure), 0xc475_4c72_fc79_d1d8);
    let pair = heap.tuple(&[closure; 2]).expect("room");
    assert_eq!(hash(&heap, Value::cell(pair)), 0x3155_43af_dd95_f3c1);
}

/// An Int hashes by its number and not by its form, so the width of the
/// small Int moves no Int key. The numbers are well inside the small
/// range, at both of its ends, a step past each, and far past it.
#[test]
fn an_int_hashes_the_same_small_or_big() {
    let mut heap = Heap::default();
    let pinned: [(i64, u64); 7] = [
        (1 << 47, 0x3628_f273_d695_c247),
        ((1 << 47) - 1, 0xf1a5_6322_1915_85dc),
        (1 << 50, 0xb8e5_c728_bdba_d507),
        ((1 << 50) - 1, 0x8385_3df3_d9c9_7b44),
        (-(1 << 50), 0x4b27_7203_7923_0f26),
        (-(1 << 50) - 1, 0x69a9_7e22_4284_8c91),
        (1 << 62, 0xedde_a07f_b473_9d65),
    ];
    for (n, want) in pinned {
        let v = crate::bigint::int(&mut heap, n).expect("room");
        assert_eq!(hash(&heap, v), want, "{n}");
        // Inside another value an Int is hashed by `int` when it is small
        // and by `big` when it is a cell. This cell holds a number that
        // may be small, which no Int operation makes.
        let as_cell = heap.big_int(&num_bigint::BigInt::from(n)).expect("room");
        assert_eq!(hash(&heap, Value::cell(as_cell)), want, "{n}");
        let held = heap.tuple(&[v]).expect("room");
        let held_as_cell = heap.tuple(&[Value::cell(as_cell)]).expect("room");
        assert_eq!(
            hash(&heap, Value::cell(held)),
            hash(&heap, Value::cell(held_as_cell)),
            "{n}"
        );
    }
}

/// A word fed whole hashes as its eight bytes fed one at a time.
#[test]
fn a_word_is_its_bytes() {
    let mut a = Sip::<1, 3>::new(K0, K1);
    a.bytes(&[9]);
    a.word(0x0102_0304_0506_0708);
    let mut b = Sip::<1, 3>::new(K0, K1);
    b.bytes(&[9]);
    b.bytes(&0x0102_0304_0506_0708u64.to_le_bytes());
    assert_eq!(a.finish(), b.finish());
}

/// An array hashes as its elements do, whatever form it has, so a map finds one by another.
#[test]
fn an_array_hashes_the_same_in_every_form() {
    let mut heap = Heap::default();
    let ints: Vec<Value> = (3..6).map(|n| Value::int(n).expect("small")).collect();
    let forms = heap.three_in_every_form(&ints);
    let hashes = forms.map(|v| hash(&heap, v));
    assert_eq!(hashes, [hashes[6]; 7]);
    // The same of elements that are no Ints.
    let names: Vec<Value> = ["a", "b", "c"]
        .map(|s| Value::cell(heap.string(s.as_bytes()).expect("room")))
        .to_vec();
    let [forms @ .., _] = heap.three_in_every_form(&names);
    let hashes = forms.map(|v: Value| hash(&heap, v));
    assert_eq!(hashes, [hashes[0]; 6]);
    let other = [names[0], names[2], names[1]];
    let other = array::from_values(&mut heap, &other).expect("room");
    assert_ne!(hash(&heap, other), hashes[0]);
}

/// The empty array hashes as the array of no elements that a cell once was, so no map
/// that is keyed by one, or by something that holds one, has changed its order.
#[test]
fn the_empty_array_hashes_as_an_array_of_length_0() {
    let mut heap = Heap::default();
    let nil = Value::NIL.bits();
    let words = [nil, nil, nil, nil, 0, 0, 0];
    let tree = Value::cell(heap.make(Kind::ArrayRoot, &words).expect("room"));
    let range = Value::cell(heap.make(Kind::Range, &[5, 5]).expect("room"));
    let words = [nil, nil, nil, nil, 0];
    let flat = Value::cell(heap.make(Kind::ArrayFlat, &words).expect("room"));
    let want = 0xb955_a07b_b007_5496;
    for v in [Value::EMPTY_ARRAY, tree, flat, range] {
        assert_eq!(hash(&heap, v), want);
        let held = heap.tuple(&[v, Value::bool(true)]).expect("room");
        assert_eq!(hash(&heap, Value::cell(held)), 0x81aa_2ae2_bc07_0faf);
    }
    let one = heap.make(Kind::Range, &[0, 1]).expect("room");
    assert_ne!(hash(&heap, Value::cell(one)), want);
    for v in [Value::NIL, Value::bool(false), Value::bool(true)] {
        assert_ne!(hash(&heap, v), want);
    }
}
