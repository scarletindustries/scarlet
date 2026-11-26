use super::*;
use crate::code::tests::program;
use scarlet_ir::core_ir::{LoweredFn, ProbeOp, Program};

fn function<'p>(program: &'p Program, name: &str) -> &'p LoweredFn {
    let mut all = (&program.fns).into_iter();
    all.find(|f| f.name == name).expect("the function")
}

/// The right-hand sides of `e`'s straight run that make an array or a text,
/// with how many operands each has, and where each local is dropped.
fn made(e: &CoreExpr) -> Vec<String> {
    let mut out = Vec::new();
    let mut e = e;
    loop {
        match e {
            CoreExpr::Let { rhs, body, .. } => {
                if let Atom::PrimOp { op, args } = rhs {
                    out.push(format!("{op:?} {}", args.len()));
                }
                e = body;
            }
            CoreExpr::Drop { body, .. } => {
                out.push("drop".to_string());
                e = body;
            }
            CoreExpr::SetGlobal { body, .. }
            | CoreExpr::Open { body, .. }
            | CoreExpr::Join { cont: body, .. } => e = body,
            CoreExpr::Ret(_)
            | CoreExpr::TailCall { .. }
            | CoreExpr::If { .. }
            | CoreExpr::Probe { .. }
            | CoreExpr::Switch { .. }
            | CoreExpr::NoArm
            | CoreExpr::Jump { .. } => {
                return out;
            }
        }
    }
}

fn pieces(src: &str, name: &str) -> Option<Vec<String>> {
    let program = program(src);
    in_pieces(&function(&program, name).core).map(|core| made(&core.body))
}

fn count(of: &[String], what: &str) -> usize {
    of.iter().filter(|s| *s == what).count()
}

fn ints(n: usize) -> String {
    (0..n).map(|i| i.to_string()).collect::<Vec<_>>().join(", ")
}

/// A literal of one piece is left as it is, and a function with no other is
/// not changed.
#[test]
fn a_short_literal_is_left_alone() {
    let src = |n| {
        format!(
            "fn f() Array(Int) {{ [{}] }}\npub fn main() {{ println(f()) }}\n",
            ints(n)
        )
    };
    assert_eq!(pieces(&src(32), "f"), None);
    let found = pieces(&src(33), "f").expect("two pieces");
    assert_eq!(found, ["MakeArray 32", "ArrayAppend 2", "drop"]);
}

/// Every element is in exactly one piece, in order, and each piece but the
/// last has as many as a leaf holds. What was made so far is dropped once the next
/// piece is on its end, though that has taken it.
#[test]
fn a_long_array_is_made_a_piece_at_a_time() {
    let src = format!(
        "fn f() Array(Int) {{ [{}] }}\npub fn main() {{ println(f()) }}\n",
        ints(100)
    );
    let found = pieces(&src, "f").expect("four pieces");
    let want = [
        "MakeArray 32",
        "ArrayAppend 33",
        "drop",
        "ArrayAppend 33",
        "drop",
        "ArrayAppend 5",
        "drop",
    ];
    assert_eq!(found, want);
}

/// The elements after a spread go on its end a piece at a time. Those before
/// one are made into an array a piece at a time, which is joined to it and then
/// given up.
#[test]
fn the_elements_beside_a_spread_are_made_a_piece_at_a_time() {
    let src = |literal: String| {
        format!(
            "fn f(xs Array(Int)) Array(Int) {{ {literal} }}\n\
             pub fn main() {{ println(f([1])) }}\n"
        )
    };
    let after = |n| pieces(&src(format!("[..xs, {}]", ints(n))), "f");
    let before = |n| pieces(&src(format!("[{}, ..xs]", ints(n))), "f");
    assert_eq!(after(32), None);
    assert_eq!(before(32), None);
    // The spread's own drop is the last thing in each.
    let found = after(33).expect("two pieces");
    assert_eq!(found, ["ArrayAppend 33", "ArrayAppend 2", "drop", "drop"]);
    let found = before(33).expect("two pieces");
    let made = ["MakeArray 32", "ArrayAppend 2", "drop", "ArrayConcat 2"];
    assert_eq!(found[..4], made);
    assert_eq!(found[4..], ["drop", "drop"]);
    let found = after(96).expect("three pieces");
    let (next, drop) = ("ArrayAppend 33", "drop");
    assert_eq!(found, [next, next, drop, next, drop, drop]);
    let found = before(96).expect("three pieces");
    assert_eq!(found[..5], ["MakeArray 32", next, drop, next, drop]);
    assert_eq!(found[5..], ["ArrayConcat 2", "drop", "drop"]);
}

/// Each element of an array of tuples dies where its piece is made, so that
/// its register is free for the next, and is dropped nowhere else. Each piece is
/// dropped too, once the next one is made.
#[test]
fn what_a_piece_holds_is_given_up_with_it() {
    let pairs: Vec<String> = (0..64).map(|i| format!("({i}, 'x')")).collect();
    let src = format!(
        "fn f(n Int) Array((Int, String)) {{\n\
         \txs = [{}]\n\
         \tprintln(n)\n\
         \txs\n\
         }}\n\
         pub fn main() {{ println(f(1)) }}\n",
        pairs.join(", ")
    );
    let before = made(&function(&program(&src), "f").core.body);
    let found = pieces(&src, "f").expect("two pieces");
    assert!(count(&before, "drop") >= 64, "{before:?}");
    assert_eq!(count(&found, "drop"), count(&before, "drop") + 1);
    assert_eq!(count(&found, "MakeTuple 2"), 64);
    // The first piece's elements are dropped before the next tuple is made.
    let first = found.iter().position(|s| s == "MakeArray 32");
    let after = &found[first.expect("a piece") + 1..];
    let next = after.iter().position(|s| s == "MakeTuple 2");
    assert_eq!(count(&after[..next.expect("a tuple")], "drop"), 32);

    // One that is read again is dropped where it was.
    let src = src
        .replace("println(n)", "println(xs)")
        .replace("xs = [", "t = (n, 'y')\n\txs = [t, ");
    let src = src.replace("println(xs)", "println(t)");
    let found = pieces(&src, "f").expect("three pieces");
    let first = found.iter().position(|s| s == "MakeArray 32");
    let after = &found[first.expect("a piece") + 1..];
    let next = after.iter().position(|s| s == "MakeTuple 2");
    assert_eq!(count(&after[..next.expect("a tuple")], "drop"), 31);

    let parts = "${n} ".repeat(70);
    let src = format!("fn f(n Int) String {{ '{parts}' }}\npub fn main() {{ println(f(1)) }}\n",);
    let found = pieces(&src, "f").expect("some pieces");
    let joins = found.iter().filter(|s| s.starts_with("StringConcatMany"));
    let joins: Vec<_> = joins.collect();
    assert!(joins.len() > 3, "{found:?}");
    assert_eq!(count(&found, "drop"), joins.len() - 1, "{found:?}");
}

/// Every local a function's straight run reads or drops has its value by then.
#[test]
fn a_piece_goes_after_its_last_element() {
    let pairs: Vec<String> = (0..100).map(|i| format!("(n, '{i}')")).collect();
    let src = format!(
        "fn f(n Int) Array((Int, String)) {{\n\
         \txs = [{}]\n\
         \tprintln(n)\n\
         \txs\n\
         }}\n\
         pub fn main() {{ println(f(1)) }}\n",
        pairs.join(", ")
    );
    assert!(in_order(&src) > 200);
    // The elements here have their values from the start, and what they go
    // beside does not.
    for literal in ["[..array::reverse(xs), {}]", "[{}, ..array::reverse(xs)]"] {
        let src = format!(
            "import scarlet/array\n\
             fn f(xs Array(Int), n Int) Array(Int) {{ {} }}\n\
             pub fn main() {{ println(f([1], 2)) }}\n",
            literal.replace("{}", &["n"; 70].join(", "))
        );
        assert!(in_order(&src) > 3);
    }
}

/// Check that every local `f`'s straight run reads or drops has its value by
/// then, once its literals are in pieces. Gives how many locals it binds.
fn in_order(src: &str) -> usize {
    let program = program(src);
    let core = in_pieces(&function(&program, "f").core).expect("some pieces");
    let mut bound: std::collections::BTreeSet<u32> = core.params.iter().map(|p| p.id.0).collect();
    let mut e = &core.body;
    let mut lets = 0;
    loop {
        match e {
            CoreExpr::Let { bind, rhs, body } => {
                rhs.for_each_operand(|l| assert!(bound.contains(&l.0), "{l} in {bind} = {rhs}"));
                assert!(bound.insert(bind.id.0), "{bind} is bound once");
                lets += 1;
                e = body;
            }
            CoreExpr::Drop { local, body, .. } => {
                assert!(bound.contains(&local.0), "{local} dropped");
                e = body;
            }
            CoreExpr::Ret(_) | CoreExpr::TailCall { .. } => break,
            CoreExpr::SetGlobal { .. }
            | CoreExpr::Join { .. }
            | CoreExpr::If { .. }
            | CoreExpr::Probe { .. }
            | CoreExpr::Switch { .. }
            | CoreExpr::Open { .. }
            | CoreExpr::NoArm
            | CoreExpr::Jump { .. } => panic!("a straight run: {e}"),
        }
    }
    lets
}

/// A literal inside a branch, and one inside another, are made in pieces where
/// they are.
#[test]
fn a_literal_is_cut_wherever_it_is() {
    let src = format!(
        "fn f(c Bool) Array(Array(Int)) {{\n\
         \txs = if c {{ [{a}] }} else {{ [1] }}\n\
         \tmatch xs {{\n\
         \t\t[] -> [[{a}], xs]\n\
         \t\t[_, ..] -> [xs]\n\
         \t}}\n\
         }}\n\
         pub fn main() {{ println(f(True)) }}\n",
        a = ints(40)
    );
    let program = program(&src);
    let core = in_pieces(&function(&program, "f").core).expect("some pieces");
    let text = core.to_string();
    assert_eq!(text.matches("ArrayAppend(").count(), 2, "{text}");
    let mut names = Locals::default();
    names.walk(&core.body);
    assert!(names.highest > 80);
}

/// A literal in either arm of a probe is made in pieces there. What the probe reads and
/// what it finds have their numbers and are counted.
#[test]
fn a_literal_in_the_arms_of_a_probe_is_cut() {
    let ty = RTy(0);
    let local = LocalId;
    let literal = |first: u32| CoreExpr::Let {
        bind: CoreBind::new(local(3), ty),
        rhs: Atom::prim(
            PrimOp::MakeArray,
            [&[local(first)][..], &[local(0); 39]].concat(),
        ),
        body: Box::new(CoreExpr::Ret(local(3))),
    };
    let body = CoreExpr::Probe {
        op: ProbeOp::ArrayGet,
        args: vec![local(0), local(1)],
        binds: vec![Some(CoreBind::new(local(90), ty))],
        hit: Box::new(literal(90)),
        miss: Box::new(literal(1)),
    };
    let mut names = Locals::default();
    names.walk(&body);
    assert_eq!(names.highest, 90);
    let times = [(local(0), 79), (local(1), 2), (local(90), 1), (local(3), 2)];
    assert_eq!(names.times, HashMap::from(times));
    let core = CoreFn {
        params: vec![CoreBind::new(local(0), ty), CoreBind::new(local(1), ty)],
        body,
        ret_ty: ty,
    };
    let text = in_pieces(&core).expect("some pieces").body.to_string();
    assert_eq!(text.matches("ArrayAppend(").count(), 2, "{text}");
    assert!(text.contains("let %91:0 = MakeArray(%90"), "{text}");
}

/// What a piece needs may have its value before a join that every way comes past,
/// and then the piece goes in front of it. It does not go in front of one that
/// some way leaves, where it would be made for nothing, but straight after.
#[test]
fn a_piece_goes_past_a_join_that_is_always_reached() {
    let ty = RTy(0);
    let local = LocalId;
    let jump = || CoreExpr::Jump {
        to: JoinId(0),
        args: vec![local(0)],
    };
    let first = |other: CoreExpr| {
        let bind = CoreBind::new(local(2), ty);
        let literal = CoreExpr::Let {
            bind,
            rhs: Atom::prim(PrimOp::MakeArray, vec![local(0); 40]),
            body: Box::new(CoreExpr::Ret(bind.id)),
        };
        let body = CoreExpr::If {
            cond: local(0),
            then: Box::new(jump()),
            els: Box::new(other),
        };
        let core = CoreFn {
            params: vec![CoreBind::new(local(0), ty)],
            body: CoreExpr::Join {
                id: JoinId(0),
                params: vec![CoreBind::new(local(9), ty)],
                cont: Box::new(literal),
                body: Box::new(body),
            },
            ret_ty: ty,
        };
        let text = in_pieces(&core).expect("two pieces").body.to_string();
        let at = |what: &str| text.find(what).expect("it is there");
        assert!(at("MakeArray") < at("ArrayAppend"), "{text}");
        // It has a number that no local has.
        assert!(text.contains("let %10:0 = MakeArray"), "{text}");
        at("MakeArray") < at("join")
    };
    assert!(first(jump()));
    assert!(!first(CoreExpr::Ret(local(0))));
}
