use super::*;

/// A pattern keyed by strings, for tests. It's interned into a `Pat`
/// before use.
#[derive(Debug, Clone)]
enum SPat {
    Wildcard,
    Ctor { name: String, args: Vec<SPat> },
    Or { patterns: Vec<SPat> },
}

fn intern_spat(p: &SPat, interner: &mut Interner) -> Pat {
    match p {
        SPat::Wildcard => Pat::Wildcard,
        SPat::Ctor { name, args } => Pat::Ctor {
            id: interner.intern(name),
            args: args.iter().map(|a| intern_spat(a, interner)).collect(),
        },
        SPat::Or { patterns } => Pat::Or {
            patterns: patterns.iter().map(|a| intern_spat(a, interner)).collect(),
        },
    }
}

impl UsefulnessMatrix {
    fn intern(&mut self, p: &SPat) -> Pat {
        intern_spat(p, &mut self.interner)
    }
    fn push_s(&mut self, p: &SPat) {
        let ip = self.intern(p);
        self.push(&ip);
    }
    fn is_useful_s(&mut self, p: &SPat) -> bool {
        let ip = self.intern(p);
        self.is_useful(&ip)
    }
}

fn check_exhaustiveness(pats: &[SPat], t: &Space) -> Option<String> {
    let mut um = UsefulnessMatrix::new(t);
    let ipats: Vec<Pat> = pats.iter().map(|p| um.intern(p)).collect();
    um.find_missing(&ipats).unwrap()
}

#[track_caller]
fn assert_witness(pats: &[SPat], t: &Space, w: &str) {
    assert_eq!(check_exhaustiveness(pats, t).as_deref(), Some(w));
}

#[track_caller]
fn assert_exhaustive(pats: &[SPat], t: &Space) {
    assert_eq!(check_exhaustiveness(pats, t), None);
}

/// Build a `Named` enum type from a `(variant, [(field, ty)])` table.
fn t_enum(_id: i32, _name: &str, variants: Vec<(&str, Vec<(&str, Space)>)>) -> Space {
    Space::Variants(
        variants
            .into_iter()
            .map(|(v, fs)| (v.to_string(), fs.into_iter().map(|(_, ty)| ty).collect()))
            .collect(),
    )
}

fn t_int() -> Space {
    Space::Infinite
}

fn t_string() -> Space {
    Space::Infinite
}

fn t_array(element: Space) -> Space {
    Space::Array(Rc::new(element))
}

fn t_tuple(elements: Vec<Space>) -> Space {
    Space::Tuple(elements)
}

/// `Bool` isn't a primitive. It has the same `Named` shape the prelude
/// makes.
fn t_bool() -> Space {
    t_enum(99, "Bool", vec![("True", vec![]), ("False", vec![])])
}

fn ctor(name: &str, args: Vec<SPat>) -> SPat {
    SPat::Ctor {
        name: name.to_string(),
        args,
    }
}

fn t_option(inner: Space) -> Space {
    t_enum(
        1,
        "Option",
        vec![("Some", vec![("value", inner)]), ("None", vec![])],
    )
}

fn t_result(ok: Space, err: Space) -> Space {
    t_enum(
        2,
        "Result",
        vec![("Ok", vec![("value", ok)]), ("Err", vec![("error", err)])],
    )
}

fn enum3() -> Space {
    t_enum(
        10,
        "Letter",
        vec![("A", vec![]), ("B", vec![]), ("C", vec![])],
    )
}

/// What the last of `pats` stands for, given the arms before it.
fn catch_all_of(pats: &[SPat], t: &Space) -> Option<Vec<String>> {
    let mut um = UsefulnessMatrix::new(t);
    let ipats: Vec<Pat> = pats.iter().map(|p| um.intern(p)).collect();
    let (last, earlier) = ipats.split_last()?;
    um.catch_all(earlier.iter().map(|p| (p, false)), last)
        .unwrap()
}

#[test]
fn a_wildcard_after_a_named_variant_stands_for_the_rest() {
    let pats = [ctor("A", vec![]), SPat::Wildcard];
    assert_eq!(
        catch_all_of(&pats, &enum3()),
        Some(vec!["B".into(), "C".into()])
    );
}

#[test]
fn a_lone_wildcard_stands_for_nothing_named() {
    assert_eq!(catch_all_of(&[SPat::Wildcard], &enum3()), None);
}

#[test]
fn naming_every_variant_leaves_no_catch_all() {
    let rest = SPat::Or {
        patterns: vec![ctor("B", vec![]), ctor("C", vec![])],
    };
    assert_eq!(catch_all_of(&[ctor("A", vec![]), rest], &enum3()), None);
}

/// `Some(_)` ignores the payload. No arm split it, so it isn't a catch-all.
#[test]
fn a_wildcard_inside_an_unsplit_field_is_fine() {
    let t = t_option(enum3());
    let pats = [ctor("None", vec![]), ctor("Some", vec![SPat::Wildcard])];
    assert_eq!(catch_all_of(&pats, &t), None);
}

#[test]
fn a_wildcard_inside_a_split_field_stands_for_the_rest() {
    let t = t_option(enum3());
    let pats = [
        ctor("Some", vec![ctor("A", vec![])]),
        ctor("Some", vec![SPat::Wildcard]),
    ];
    assert_eq!(catch_all_of(&pats, &t), Some(vec!["B".into(), "C".into()]));
}

/// The rule follows each tuple column the arms split.
#[test]
fn a_tuple_wildcard_stands_for_the_columns_split_before_it() {
    let t = t_tuple(vec![t_bool(), t_int()]);
    let tn = tuple_ctor_name(2);
    let pats = [
        ctor(&tn, vec![ctor("True", vec![]), SPat::Wildcard]),
        SPat::Wildcard,
    ];
    assert_eq!(catch_all_of(&pats, &t), Some(vec!["False".into()]));
}

/// Values that cannot be listed keep their `_`.
#[test]
fn an_int_wildcard_after_literals_is_fine() {
    let pats = [ctor("1", vec![]), SPat::Wildcard];
    assert_eq!(catch_all_of(&pats, &t_int()), None);
}

/// An array's `[]` and `[_, ..]` are lengths, which cannot be listed.
#[test]
fn an_array_wildcard_after_the_empty_array_is_fine() {
    let t = t_array(t_bool());
    let pats = [ctor("[]", vec![]), SPat::Wildcard];
    assert_eq!(catch_all_of(&pats, &t), None);
}

#[test]
fn a_payload_variant_renders_with_its_dots() {
    let t = t_option(t_int());
    let pats = [ctor("None", vec![]), SPat::Wildcard];
    assert_eq!(catch_all_of(&pats, &t), Some(vec!["Some(..)".into()]));
}

#[test]
fn bool_missing_false() {
    assert_witness(&[ctor("True", vec![])], &t_bool(), "False");
}

#[test]
fn bool_exhaustive() {
    assert_exhaustive(&[ctor("True", vec![]), ctor("False", vec![])], &t_bool());
}

#[test]
fn bool_wildcard_exhaustive() {
    assert_exhaustive(&[SPat::Wildcard], &t_bool());
}

#[test]
fn enum_missing_third_variant() {
    assert_witness(&[ctor("A", vec![]), ctor("B", vec![])], &enum3(), "C");
}

#[test]
fn enum_all_variants_exhaustive() {
    let pats = [ctor("A", vec![]), ctor("B", vec![]), ctor("C", vec![])];
    assert_exhaustive(&pats, &enum3());
}

#[test]
fn tuple_bool_bool_missing_fourth() {
    let t = t_tuple(vec![t_bool(), t_bool()]);
    let tn = tuple_ctor_name(2);
    let pats = vec![
        ctor(&tn, vec![ctor("True", vec![]), ctor("True", vec![])]),
        ctor(&tn, vec![ctor("True", vec![]), ctor("False", vec![])]),
        ctor(&tn, vec![ctor("False", vec![]), ctor("True", vec![])]),
    ];
    assert_witness(&pats, &t, "(False, False)");
}

#[test]
fn tuple_bool_bool_exhaustive() {
    let t = t_tuple(vec![t_bool(), t_bool()]);
    let tn = tuple_ctor_name(2);
    let pats = vec![
        ctor(&tn, vec![ctor("True", vec![]), ctor("True", vec![])]),
        ctor(&tn, vec![ctor("True", vec![]), ctor("False", vec![])]),
        ctor(&tn, vec![ctor("False", vec![]), SPat::Wildcard]),
    ];
    assert_exhaustive(&pats, &t);
}

/// `(True, _)` and `(_, True)` miss exactly `(False, False)`, not the
/// overlapping `(False, _)`.
#[test]
fn tuple_witness_recurses_default_matrix() {
    let t = t_tuple(vec![t_bool(), t_bool()]);
    let tn = tuple_ctor_name(2);
    let pats = vec![
        ctor(&tn, vec![ctor("True", vec![]), SPat::Wildcard]),
        ctor(&tn, vec![SPat::Wildcard, ctor("True", vec![])]),
    ];
    assert_witness(&pats, &t, "(False, False)");
}

/// The witness's cons chain is walked so the head renders, not `[_, ..]`.
#[test]
fn array_witness_renders_cons_head() {
    let t = t_array(t_bool());
    let pats = vec![
        ctor("[]", vec![]),
        ctor("::", vec![ctor("True", vec![]), SPat::Wildcard]),
    ];
    assert_witness(&pats, &t, "[False]");
}

#[test]
fn nested_option_option_int_missing_some_none() {
    let t = t_option(t_option(t_int()));
    let pats = vec![
        ctor("Some", vec![ctor("Some", vec![SPat::Wildcard])]),
        ctor("None", vec![]),
    ];
    assert_witness(&pats, &t, "Some(None)");
}

#[test]
fn nested_option_option_int_exhaustive() {
    let t = t_option(t_option(t_int()));
    let pats = vec![
        ctor("Some", vec![ctor("Some", vec![SPat::Wildcard])]),
        ctor("Some", vec![ctor("None", vec![])]),
        ctor("None", vec![]),
    ];
    assert_exhaustive(&pats, &t);
}

#[test]
fn option_missing_none() {
    assert_witness(
        &[ctor("Some", vec![SPat::Wildcard])],
        &t_option(t_int()),
        "None",
    );
}

#[test]
fn result_missing_err() {
    let t = t_result(t_int(), t_string());
    assert_witness(&[ctor("Ok", vec![SPat::Wildcard])], &t, "Err(_)");
}

#[test]
fn array_missing_empty() {
    let t = t_array(t_int());
    assert_witness(
        &[ctor("::", vec![SPat::Wildcard, SPat::Wildcard])],
        &t,
        "[]",
    );
}

#[test]
fn array_missing_nonempty() {
    assert_witness(&[ctor("[]", vec![])], &t_array(t_int()), "[_, ..]");
}

#[test]
fn int_literal_never_exhaustive() {
    assert_witness(
        &[ctor("lit:1", vec![]), ctor("lit:2", vec![])],
        &t_int(),
        "_",
    );
}

#[test]
fn or_pattern_useful_if_any_alt_useful() {
    let mut m = UsefulnessMatrix::new(&t_bool());
    m.push_s(&ctor("True", vec![]));
    let new = SPat::Or {
        patterns: vec![ctor("True", vec![]), ctor("False", vec![])],
    };
    assert!(m.is_useful_s(&new));
}

#[test]
fn or_pattern_not_useful_if_all_covered() {
    let mut m = UsefulnessMatrix::new(&t_bool());
    m.push_s(&ctor("True", vec![]));
    m.push_s(&ctor("False", vec![]));
    let new = SPat::Or {
        patterns: vec![ctor("True", vec![]), ctor("False", vec![])],
    };
    assert!(!m.is_useful_s(&new));
}

#[test]
fn usefulness_redundant_after_wildcard() {
    let mut m = UsefulnessMatrix::new(&t_bool());
    m.push_s(&SPat::Wildcard);
    assert!(!m.is_useful_s(&ctor("True", vec![])));
}

#[test]
fn usefulness_distinct_ctor() {
    let mut m = UsefulnessMatrix::new(&t_bool());
    m.push_s(&ctor("True", vec![]));
    assert!(m.is_useful_s(&ctor("False", vec![])));
}

#[test]
fn enum_with_payload_witness() {
    let t = t_enum(
        11,
        "Tree",
        vec![
            ("Leaf", vec![]),
            ("Node", vec![("left", t_int()), ("right", t_int())]),
        ],
    );
    assert_witness(&[ctor("Leaf", vec![])], &t, "Node(_, _)");
}

/// The prelude's opaque `Binary`. It's `Named` with no variants, so
/// `get_type_ctors` reports it as infinite.
fn t_binary() -> Space {
    t_enum(98, "Binary", vec![])
}

/// A checked binary pattern, as the tests write one.
struct TBin {
    segs: Vec<(TPart, Option<i64>)>,
    open: bool,
}

enum TPart {
    Any,
    Int(i64),
    Str(&'static str),
}

fn num(v: &str) -> i64 {
    let v = v.replace('_', "");
    if let Some(h) = v.strip_prefix("0x") {
        return i64::from_str_radix(h, 16).unwrap_or_default();
    }
    v.parse().unwrap_or_default()
}

impl PatternShape for TBin {
    fn shape(&self) -> PatShape<'_, Self> {
        if self.segs.is_empty() && self.open {
            return PatShape::Wild;
        }
        let segs = self
            .segs
            .iter()
            .map(|(v, size)| BinSegShape {
                value: match v {
                    TPart::Any => BinPart::Any,
                    TPart::Int(n) => BinPart::Int(*n),
                    TPart::Str(s) => BinPart::Str(s),
                },
                unit: BinUnit::Bits,
                size: size.map(BinPart::Int),
            })
            .collect();
        PatShape::Binary {
            segs,
            open: self.open,
        }
    }
}

fn bin(segs: Vec<(TPart, Option<&str>)>, open: bool) -> TBin {
    let segs = segs.into_iter().map(|(v, sz)| (v, sz.map(num))).collect();
    TBin { segs, open }
}

#[test]
fn binary_fixed_segments_not_exhaustive() {
    let mut um = UsefulnessMatrix::new(&t_binary());
    let p = um.lower(&bin(vec![(TPart::Any, None), (TPart::Any, None)], false));
    assert_eq!(um.find_missing(&[p]).unwrap().as_deref(), Some("_"));
}

#[test]
fn binary_empty_literal_not_exhaustive() {
    let mut um = UsefulnessMatrix::new(&t_binary());
    let p = um.lower(&bin(vec![], false));
    assert_eq!(um.find_missing(&[p]).unwrap().as_deref(), Some("_"));
}

#[test]
fn binary_rest_only_is_exhaustive() {
    let mut um = UsefulnessMatrix::new(&t_binary());
    let p = um.lower(&bin(vec![], true));
    assert!(matches!(p, Pat::Wildcard));
    assert_eq!(um.find_missing(&[p]), Ok(None));
}

#[test]
fn binary_segments_with_rest_not_exhaustive() {
    let mut um = UsefulnessMatrix::new(&t_binary());
    let p = um.lower(&bin(vec![(TPart::Any, None)], true));
    assert_eq!(um.find_missing(&[p]).unwrap().as_deref(), Some("_"));
}

#[test]
fn binary_fixed_then_else_exhaustive() {
    let mut um = UsefulnessMatrix::new(&t_binary());
    let p1 = um.lower(&bin(vec![(TPart::Any, None), (TPart::Any, None)], false));
    assert_eq!(um.find_missing(&[p1, Pat::Wildcard]), Ok(None));
}

#[test]
fn binary_same_shape_redundant() {
    let mut m = UsefulnessMatrix::new(&t_binary());
    let p1 = m.lower(&bin(vec![(TPart::Any, Some("8"))], false));
    let p2 = m.lower(&bin(vec![(TPart::Any, Some("8"))], false));
    m.push(&p1);
    assert!(!m.is_useful(&p2));
}

#[test]
fn binary_different_literal_useful() {
    let mut m = UsefulnessMatrix::new(&t_binary());
    let p1 = m.lower(&bin(vec![(TPart::Int(num("1")), None)], false));
    let p2 = m.lower(&bin(vec![(TPart::Int(num("2")), None)], false));
    m.push(&p1);
    assert!(m.is_useful(&p2));
}

/// String segment values can contain the key's delimiter chars, and two
/// different patterns still must not share a key.
#[test]
fn binary_string_key_is_injective() {
    let mut m = UsefulnessMatrix::new(&t_binary());
    let p1 = m.lower(&bin(vec![(TPart::Str("a'i8,'b"), None)], false));
    let p2 = m.lower(&bin(
        vec![(TPart::Str("a"), Some("8")), (TPart::Str("b"), None)],
        false,
    ));
    m.push(&p1);
    assert!(m.is_useful(&p2));
}

/// An unknown constructor with arguments must keep the pattern and type
/// stacks in line, or the type stack runs out early and every duplicate
/// arm looks useful.
#[test]
fn unknown_ctor_with_args_duplicate_is_redundant() {
    let mut m = UsefulnessMatrix::new(&t_int());
    m.push_s(&ctor("Foo", vec![SPat::Wildcard]));
    assert!(!m.is_useful_s(&ctor("Foo", vec![SPat::Wildcard])));
    assert!(m.is_useful_s(&ctor("Bar", vec![SPat::Wildcard])));
}
