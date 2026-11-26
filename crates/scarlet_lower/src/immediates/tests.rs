use super::*;
use crate::testkit::{bind, bound, func, join, jump, local, ret, switch_on, vref};
use crate::typed_ir::RTy;

fn im() -> Immediates {
    Immediates {
        true_: vref(1, 0),
        false_: vref(1, 1),
        nil: vref(2, 0),
    }
}

/// A switch on which constructor of `ty` %0 is, with what each case returns.
fn switch(ty: i32, arms: &[(&[u16], u32)], default: Option<u32>) -> CoreExpr {
    let arms = arms.iter().map(|(of, n)| (of.to_vec(), ret(*n))).collect();
    let ty = TypeId(ty);
    switch_on(0, Cases::Variant { ty, arms }, default.map(ret))
}

fn rewritten(body: CoreExpr) -> String {
    let mut f = func(vec![bind(0, RTy(0))], body, RTy(0));
    rewrite(&mut f, &im());
    f.to_string()
}

#[test]
fn true_false_and_nil_become_atoms() {
    let body = |v| {
        let fields = Vec::new();
        bound(bind(1, RTy(0)), Atom::Ctor { variant: v, fields }, ret(1))
    };
    assert!(rewritten(body(vref(1, 0))).contains("= true"));
    assert!(rewritten(body(vref(1, 1))).contains("= false"));
    assert!(rewritten(body(vref(2, 0))).contains("= nil"));
    assert!(rewritten(body(vref(3, 0))).contains("ctor 3.0()"));
}

/// `True` is variant 0 and `False` is variant 1. A case for both, or the default,
/// is what neither has a case of its own for.
#[test]
fn a_switch_on_a_bool_becomes_an_if() {
    let iff = |then: u32, els: u32| {
        format!("fn(%0:0) -> :0\n  if %0\n    ret %{then}\n  else\n    ret %{els}\n")
    };
    let just = |n: u32| format!("fn(%0:0) -> :0\n  ret %{n}\n");
    for (arms, default, want) in [
        (&[(&[0][..], 7), (&[1], 8)][..], None, iff(7, 8)),
        (&[(&[1], 8), (&[0], 7)], None, iff(7, 8)),
        (&[(&[1], 8), (&[0], 7)], Some(9), iff(7, 8)),
        (&[(&[0], 7)], Some(8), iff(7, 8)),
        (&[(&[1], 8)], Some(7), iff(7, 8)),
        (&[(&[0, 1], 7)], None, just(7)),
        (&[(&[1, 0], 7)], None, just(7)),
        (&[], Some(7), just(7)),
        (&[(&[0], 7)], None, just(7)),
        (&[(&[1], 7)], None, just(7)),
    ] {
        assert_eq!(
            rewritten(switch(1, arms, default)),
            want,
            "{arms:?} {default:?}"
        );
    }
    let nothing = rewritten(switch(1, &[], None));
    assert_eq!(nothing, "fn(%0:0) -> :0\n  no arm\n");
}

/// Nil has one value, so its case is all that is left. It is told from `True`,
/// which is the first of its type too.
#[test]
fn a_switch_on_nil_is_its_case() {
    for (arms, default) in [(&[(&[0][..], 7)][..], None), (&[], Some(7))] {
        let got = rewritten(switch(2, arms, default));
        assert_eq!(got, "fn(%0:0) -> :0\n  ret %7\n");
    }
    let both = rewritten(switch(2, &[(&[0], 7)], Some(8)));
    assert!(both.contains("ret %7"), "{both}");
}

/// What is in a case is rewritten too, whatever the switch is on.
#[test]
fn a_switch_on_anything_else_is_kept() {
    let other = rewritten(switch(3, &[(&[0], 7)], Some(8)));
    assert!(other.contains("switch %0"), "{other}");
    let made = || {
        let fields = Vec::new();
        let variant = vref(1, 0);
        bound(bind(5, RTy(0)), Atom::Ctor { variant, fields }, ret(5))
    };
    let ty = TypeId(3);
    let all = [
        Cases::Variant {
            ty,
            arms: vec![(vec![0], made()), (vec![1], made())],
        },
        Cases::Int(vec![(vec![4], made()), (vec![5], made())]),
        Cases::Float(vec![
            (vec![crate::FloatKey::new(1.0)], made()),
            (vec![crate::FloatKey::new(2.0)], made()),
        ]),
        Cases::Text(vec![
            (vec![crate::ConstId(0)], made()),
            (vec![crate::ConstId(1)], made()),
        ]),
    ];
    for cases in all {
        let got = rewritten(switch_on(0, cases.clone(), Some(made())));
        assert_eq!(got.matches("= true").count(), 3, "{got}");
        let got = rewritten(switch_on(0, cases, None));
        assert_eq!(got.matches("= true").count(), 2, "{got}");
    }
    // A switch in what an `If` becomes of one.
    let inner = switch_on(
        0,
        Cases::Variant {
            ty: TypeId(1),
            arms: vec![(vec![0], made())],
        },
        Some(made()),
    );
    let outer = Cases::Variant {
        ty: TypeId(1),
        arms: vec![(vec![1], inner)],
    };
    let got = rewritten(switch_on(0, outer, Some(made())));
    assert_eq!(got.matches("if %0").count(), 2, "{got}");
    assert_eq!(got.matches("= true").count(), 3, "{got}");
}

/// The pass goes into both sides of a join and past a store.
#[test]
fn both_sides_of_a_join_are_rewritten() {
    let made = |id, end| {
        let fields = Vec::new();
        let variant = vref(1, 0);
        bound(bind(id, RTy(0)), Atom::Ctor { variant, fields }, end)
    };
    let stored = CoreExpr::SetGlobal {
        slot: crate::GlobalSlot(3),
        value: local(5),
        body: Box::new(made(6, ret(6))),
    };
    let body = join(
        0,
        vec![bind(4, RTy(0))],
        made(3, jump(0, &[3])),
        made(5, stored),
    );
    let text = rewritten(body);
    assert_eq!(
        text,
        "\
fn(%0:0) -> :0
  join j0(%4:0) after
    let %3:0 = true
    jump j0(%3)
  let %5:0 = true
  set global3 = %5
  let %6:0 = true
  ret %6
"
    );
}
