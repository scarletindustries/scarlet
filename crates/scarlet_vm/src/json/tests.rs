use super::*;

/// A parsed document on a heap of its own.
fn doc(src: &str) -> (Heap, Bits, Bits) {
    let (tape, arena) = parse(src.as_bytes()).unwrap_or_else(|e| panic!("{}", e.reason));
    let mut heap = Heap::default();
    let t = binary::make(&mut heap, &tape, tape.len() as u64 * 8).expect("room");
    let a = binary::make(&mut heap, &arena, arena.len() as u64 * 8).expect("room");
    let (t, a) = (
        binary::bits(&heap, t).expect("a binary"),
        binary::bits(&heap, a).expect("a binary"),
    );
    (heap, t, a)
}

#[test]
fn a_field_is_found_past_a_nested_object() {
    let (heap, tape, arena) = doc(r#"{"a": {"x": [1, 2, {"y": 3}]}, "b": "found"}"#);
    let t = Tape {
        heap: &heap,
        tape,
        arena,
    };
    let b = t.field(0, b"b").expect("a b");
    assert_eq!(t.string(b), Some(b"found".to_vec()));
    assert_eq!(t.field(0, b"c"), None);
    assert_eq!(t.len(0), Some(2));
}

#[test]
fn duplicate_keys_both_survive_and_the_first_wins() {
    let (heap, tape, arena) = doc(r#"{"k": 1, "k": 2}"#);
    let t = Tape {
        heap: &heap,
        tape,
        arena,
    };
    let first = t.field(0, b"k").expect("a k");
    assert_eq!(t.int(first), Some(1.into()));
    let members = t.members(0).expect("an object");
    assert_eq!(members.len(), 2);
    assert_eq!(t.int(members[1].1), Some(2.into()));
}

/// An `Int` has no bounds, so an integer past `i64::MAX` is one too.
#[test]
fn an_integer_past_i64_max_is_an_int() {
    let (heap, tape, arena) =
        doc("[9223372036854775807, 18446744073709551615, -9223372036854775808]");
    let t = Tape {
        heap: &heap,
        tape,
        arena,
    };
    let at = t.elements(0).expect("an array");
    assert_eq!(t.int(at[0]), Some(i64::MAX.into()));
    assert_eq!(t.int(at[1]), Some(u64::MAX.into()));
    assert_eq!(t.int(at[2]), Some(i64::MIN.into()));
    assert_eq!(t.kind(at[1]), Some(KIND_INT));
}

/// Past 64 bits the parser underneath has no integer to put it in.
#[test]
fn an_integer_past_64_bits_does_not_parse() {
    assert!(parse(b"[18446744073709551616]").is_err());
}

#[test]
fn ten_thousand_deep_nesting_parses_and_walks() {
    let src = format!("{}{}", "[".repeat(10_000), "]".repeat(10_000));
    let (heap, tape, arena) = doc(&src);
    let t = Tape {
        heap: &heap,
        tape,
        arena,
    };
    let mut at = 0;
    for _ in 0..9_999 {
        at = t.element(at, 0).expect("one deeper");
    }
    assert_eq!(t.len(at), Some(0));
}

#[test]
fn strings_are_unescaped() {
    let (heap, tape, arena) = doc(r#"["a\"b\\c\n\u00e9\ud83d\ude00"]"#);
    let t = Tape {
        heap: &heap,
        tape,
        arena,
    };
    let s = t.string(t.element(0, 0).expect("one")).expect("a string");
    assert_eq!(String::from_utf8(s).expect("UTF-8"), "a\"b\\c\né😀");
}

#[test]
fn a_lone_surrogate_is_refused_and_a_pair_is_not() {
    assert!(parse(br#"["\ud800"]"#).is_err());
    assert!(parse(br#"["\udc00"]"#).is_err());
    assert!(parse(br#"["\ud800x"]"#).is_err());
    assert!(parse(br#"["\ud83d\ude00"]"#).is_ok());
    assert!(parse(br#"["\\ud800"]"#).is_ok());
    assert_eq!(
        first_lone_surrogate(br#"{"k": "ok", "v": "\udfff"}"#),
        Some(18)
    );
}

#[test]
fn floats_keep_their_point_and_strings_their_escapes() {
    let mut out = Vec::new();
    for f in [1.0, -0.5, 1e300, 0.1] {
        write_float(&mut out, f);
        out.push(b' ');
    }
    write_string(&mut out, "q\"b\\n\n\u{1}é");
    assert_eq!(
        String::from_utf8(out).expect("UTF-8"),
        "1.0 -0.5 1e300 0.1 \"q\\\"b\\\\n\\n\\u0001é\""
    );
}

#[test]
fn only_a_json_number_is_a_number() {
    for s in ["0", "-0", "12", "1.5", "1e10", "1E+2", "-3.25e-7"] {
        assert!(is_number(s.as_bytes()), "{s}");
    }
    for s in ["", "-", "01", "1.", ".5", "1e", "+1", "NaN", "1 ", "0x1"] {
        assert!(!is_number(s.as_bytes()), "{s}");
    }
}

/// A node the walk reaches that is not on the tape is `None`, and so is
/// a node whose skip is 0, which would stop a walk moving.
#[test]
fn a_node_off_the_tape_is_none() {
    let mut heap = Heap::default();
    let mut bytes = Vec::new();
    let mut node = |kind: u8, skip: u64, payload: u64| {
        bytes.extend_from_slice(&(u64::from(kind) | skip << 32).to_le_bytes());
        bytes.extend_from_slice(&payload.to_le_bytes());
    };
    // An array that says it has 3 elements, holding one, then a zero skip.
    node(K_ARRAY, 2, 3);
    node(K_NULL, 1, 0);
    node(K_NULL, 0, 0);
    let t = binary::make(&mut heap, &bytes, bytes.len() as u64 * 8).expect("room");
    let a = binary::make(&mut heap, &[], 0).expect("room");
    let tape = Tape {
        heap: &heap,
        tape: binary::bits(&heap, t).expect("a binary"),
        arena: binary::bits(&heap, a).expect("a binary"),
    };
    assert!(tape.element(0, 0).is_some());
    assert_eq!(tape.element(0, 1), None);
    assert_eq!(tape.element(0, 2), None);
    assert_eq!(tape.elements(0), None);
    assert_eq!(tape.kind(99), None);
}
