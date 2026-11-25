use super::*;
use crate::testkit::{bind, func, local, vref};

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

fn ret(n: u32) -> CoreExpr {
    CoreExpr::Tail(Atom::Local(local(n)))
}

fn rewritten(body: CoreExpr) -> String {
    let f = func(vec![bind(0, RTy(0))], body, RTy(0));
    rewrite(f, &im()).to_string()
}

#[test]
fn true_false_and_nil_become_atoms() {
    let body = |v| CoreExpr::Let {
        bind: bind(1, RTy(0)),
        rhs: Atom::Ctor {
            variant: v,
            fields: Vec::new(),
        },
        body: Box::new(ret(1)),
    };
    assert!(rewritten(body(vref(1, 0))).contains("= true"));
    assert!(rewritten(body(vref(1, 1))).contains("= false"));
    assert!(rewritten(body(vref(2, 0))).contains("= nil"));
    assert!(rewritten(body(vref(3, 0))).contains("ctor 3.0()"));
}

#[test]
fn a_match_on_a_bool_becomes_an_if() {
    let m = |arms| CoreExpr::Match {
        scrut: local(0),
        arms,
        ty: RTy(0),
    };
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
    let text = rewritten(CoreExpr::Match {
        scrut: local(0),
        arms: vec![
            (ctor(vref(1, 0)), ret(7)),
            (CorePat::Bind(bind(9, RTy(0))), ret(9)),
        ],
        ty: RTy(0),
    });
    assert!(text.contains("if %0"), "{text}");
    assert!(text.contains("let %9:0 = %0"), "{text}");
}

/// A match on Nil has one value to match, so its arm is all that is left.
#[test]
fn a_match_on_nil_is_its_arm() {
    let nil = rewritten(CoreExpr::Match {
        scrut: local(0),
        arms: vec![(ctor(vref(2, 0)), ret(7))],
        ty: RTy(0),
    });
    assert!(!nil.contains("match"), "{nil}");
    assert!(nil.contains("ret %7"), "{nil}");
}

#[test]
fn a_match_on_anything_else_is_left_alone() {
    let other = rewritten(CoreExpr::Match {
        scrut: local(0),
        arms: vec![(ctor(vref(3, 0)), ret(7)), (CorePat::Wild, ret(8))],
        ty: RTy(0),
    });
    assert!(other.contains("match %0"), "{other}");

    let wild = rewritten(CoreExpr::Match {
        scrut: local(0),
        arms: vec![(CorePat::Wild, ret(7))],
        ty: RTy(0),
    });
    assert!(wild.contains("match %0"), "{wild}");
}
