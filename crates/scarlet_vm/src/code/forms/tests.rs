use super::*;
use scarlet_ir::core_ir::{TypeNames, VariantNames};

/// A type whose variants have these many fields.
fn a_type(variants: &[usize]) -> TypeNames {
    let variant = |fields: &usize| VariantNames {
        name: "A".to_string(),
        fields: vec!["f".to_string(); *fields],
    };
    TypeNames {
        name: "T".to_string(),
        variants: variants.iter().map(variant).collect(),
    }
}

fn v(type_id: i32, variant_idx: u16) -> VariantRef {
    VariantRef {
        type_id: TypeId(type_id),
        variant_idx,
    }
}

/// The constructor of a form that has fields.
fn ctor(form: Result<Form, Broken>) -> Ctor {
    match form {
        Ok(Form::Fields { ctor, .. }) => ctor,
        other => panic!("{other:?} has no fields"),
    }
}

/// Every constructor with fields has a class of its own, and the class leads
/// back to it, whether or not two types spell it the same. One with no fields
/// has none.
#[test]
fn every_constructor_with_fields_has_its_own_class() {
    let types = Types::from([
        (TypeId(-3), a_type(&[2, 0, 2])),
        (TypeId(5), a_type(&[2, 1])),
        (TypeId(6), a_type(&[])),
        (TypeId(7), a_type(&[0, 300])),
    ]);
    let (forms, ctors) = Forms::new(&types).expect("a few constructors");
    let with_fields = [
        (v(-3, 0), 2),
        (v(-3, 2), 2),
        (v(5, 0), 2),
        (v(5, 1), 1),
        (v(7, 1), 300),
    ];
    let mut seen = Vec::new();
    for (name, fields) in with_fields {
        let made = ctor(forms.of(name, fields));
        assert_eq!(made.variant(), name.variant_idx);
        assert_eq!(ctors.name(made), Some(name));
        assert!(!seen.contains(&made), "{name:?}");
        seen.push(made);
    }
    for name in [v(-3, 1), v(7, 0)] {
        assert_eq!(forms.of(name, 0), Ok(Form::Plain(name)));
    }
}

/// A use of a constructor as one with another number of fields is refused,
/// and so is one the program does not list.
#[test]
fn a_constructor_is_used_as_it_is_listed() {
    let types = Types::from([(TypeId(5), a_type(&[2, 0]))]);
    let (forms, _) = Forms::new(&types).expect("a few constructors");
    let fields = |ctor, got, want| Err(Broken::CtorFields { ctor, got, want });
    assert_eq!(forms.of(v(5, 0), 1), fields(v(5, 0), 1, 2));
    assert_eq!(forms.of(v(5, 0), 0), fields(v(5, 0), 0, 2));
    assert_eq!(forms.of(v(5, 1), 2), fields(v(5, 1), 2, 0));
    for unlisted in [v(5, 2), v(5, u16::MAX), v(6, 0)] {
        assert_eq!(forms.of(unlisted, 0), Err(Broken::NoCtor(unlisted)));
        assert_eq!(forms.of(unlisted, 2), Err(Broken::NoCtor(unlisted)));
    }
    // The constructors the VM builds go through the same table.
    let mut named = &forms;
    assert_eq!(
        named.plain(v(5, 1)).map(Value::bits),
        Ok(Value::nullary(v(5, 1)).bits())
    );
    let wrong = fields(v(5, 0), 0, 2).err();
    assert_eq!(named.plain(v(5, 0)).map(Value::bits).err(), wrong);
    let made = named.fields::<2>(v(5, 0)).map(CtorOf::ctor);
    assert_eq!(made, Ok(ctor(forms.of(v(5, 0), 2))));
    let made = named.fields::<1>(v(5, 0)).map(CtorOf::ctor);
    assert_eq!(
        made,
        Err(Broken::CtorFields {
            ctor: v(5, 0),
            got: 1,
            want: 2
        })
    );
    let made = named.fields::<3>(v(5, 0)).map(CtorOf::ctor);
    assert_eq!(
        made,
        Err(Broken::CtorFields {
            ctor: v(5, 0),
            got: 3,
            want: 2
        })
    );
    let made = named.fields::<2>(v(5, 1)).map(CtorOf::ctor);
    assert_eq!(
        made,
        Err(Broken::CtorFields {
            ctor: v(5, 1),
            got: 2,
            want: 0
        })
    );
    let made = named.fields::<2>(v(9, 1)).map(CtorOf::ctor);
    assert_eq!(made, Err(Broken::NoCtor(v(9, 1))));
}

/// A test of a variant is a test of the top half of a word, so a variant's tag
/// is the one its values have, and no two variants of one type share one.
#[test]
fn a_forms_tag_is_its_values() {
    let types = Types::from([(TypeId(5), a_type(&[0, 2, 0, 2]))]);
    let (forms, ctors) = Forms::new(&types).expect("a few constructors");
    let mut heap = crate::heap::Heap::new(ctors);
    let tags: Vec<Tag> = [0, 2, 0, 2]
        .into_iter()
        .zip(0..)
        .map(|(fields, i)| {
            let form = forms.of(v(5, i), fields).expect("it is listed");
            let value = match form {
                Form::Plain(v) => Value::nullary(v),
                Form::Fields { ctor, .. } => {
                    Value::cell(heap.ctor(ctor, &[Value::NIL; 2]).expect("room"))
                }
            };
            assert_eq!(value.tag(), form.tag());
            form.tag()
        })
        .collect();
    for (i, tag) in tags.iter().enumerate() {
        let same = tags.iter().filter(|other| *other == tag).count();
        assert_eq!((same, tag.parts()), (1, (i as u16, i % 2 == 1)));
    }
}

/// There are 65,521 classes for constructors. One with no fields takes none.
#[test]
fn a_program_with_too_many_constructors_does_not_load() {
    let most = 65_521;
    let with = |fields: &[usize]| {
        let types = Types::from([
            (TypeId(1), a_type(&vec![1; most - most / 2])),
            (TypeId(2), a_type(&vec![0; 60_000])),
            (TypeId(3), a_type(&vec![3; most / 2])),
            (TypeId(4), a_type(fields)),
        ]);
        Forms::new(&types)
    };
    let (forms, ctors) = with(&[0]).expect("as many as there can be");
    let last = v(3, u16::try_from(most / 2 - 1).expect("small"));
    let made = ctor(forms.of(last, 3));
    assert_eq!(ctors.name(made), Some(last));
    let Err(stop) = with(&[0, 1]) else {
        panic!("one more constructor than there are classes");
    };
    assert_eq!(stop, Stop::TooManyConstructors);
}
