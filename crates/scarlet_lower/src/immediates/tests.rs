use super::*;
use crate::testkit::{bind, bound, func, join, jump, local, matches, ret, vref};
use crate::typed_ir::RTy;

fn im() -> Immediates {
    Immediates {
        true_: vref(1, 0),
        false_: vref(1, 1),
        nil: vref(2, 0),
    }
}

fn ctor(v: VariantRef) -> CorePat {
    CorePat::Ctor {
        variant: v,
        fields: Vec::new(),
    }
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

#[test]
fn a_match_on_a_bool_becomes_an_if() {
    let m = |arms| matches(0, arms);
    let both = rewritten(m(vec![
        (ctor(vref(1, 1)), ret(8)),
        (ctor(vref(1, 0)), ret(7)),
    ]));
    assert!(both.contains("if %0"), "{both}");
    let lines: Vec<&str> = both.lines().map(str::trim).collect();
    let at = |s: &str| lines.iter().position(|l| *l == s).expect(s);
    assert!(
        at("ret %7") < at("ret %8"),
        "the True arm is `then`:\n{both}"
    );

    let wild = rewritten(m(vec![(ctor(vref(1, 0)), ret(7)), (CorePat::Wild, ret(8))]));
    assert!(wild.contains("if %0"), "{wild}");
}

/// `x -> ...` on a Bool binds `x` to the Bool, then runs the arm.
#[test]
fn a_binding_arm_binds_the_value() {
    let text = rewritten(matches(
        0,
        vec![
            (ctor(vref(1, 0)), ret(7)),
            (CorePat::Bind(bind(9, RTy(0))), ret(9)),
        ],
    ));
    assert!(text.contains("if %0"), "{text}");
    assert!(text.contains("let %9:0 = %0"), "{text}");
}

/// A match on Nil has one value to match, so its arm is all that is left.
#[test]
fn a_match_on_nil_is_its_arm() {
    let nil = rewritten(matches(0, vec![(ctor(vref(2, 0)), ret(7))]));
    assert!(!nil.contains("match"), "{nil}");
    assert!(nil.contains("ret %7"), "{nil}");
}

#[test]
fn a_match_on_anything_else_is_left_alone() {
    let other = rewritten(matches(
        0,
        vec![(ctor(vref(3, 0)), ret(7)), (CorePat::Wild, ret(8))],
    ));
    assert!(other.contains("match %0"), "{other}");

    let wild = rewritten(matches(0, vec![(CorePat::Wild, ret(7))]));
    assert!(wild.contains("match %0"), "{wild}");
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
