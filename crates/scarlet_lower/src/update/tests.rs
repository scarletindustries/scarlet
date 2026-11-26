use super::*;
use scarlet_ir::core_ir::{ConstId, FuncIdx};

use crate::testkit::variant;
use crate::typed_ir::{
    BindingId, Gives, RTy, TypedBinPatSeg, TypedBinSeg, TypedBind, TypedCallee, TypedInterpPart,
    ValueRef,
};

const TY: RTy = RTy(0);

fn name(id: usize) -> TypedBind {
    TypedBind {
        id: BindingId(id),
        named: true,
        ty: TY,
        global: None,
    }
}

fn var() -> TypedExpr {
    TypedExpr::Var {
        ty: TY,
        place: ValueRef::Local(BindingId(0)),
    }
}

fn boxed(e: &TypedExpr) -> Box<TypedExpr> {
    Box::new(e.clone())
}

/// A call, which is never brief.
fn call() -> TypedExpr {
    TypedExpr::Call {
        ty: TY,
        callee: TypedCallee::Known(FuncIdx(0)),
        args: Vec::new(),
    }
}

fn probe(op: ProbeOp, args: Vec<TypedExpr>) -> TypedExpr {
    TypedExpr::Probe {
        ty: TY,
        op,
        args,
        found: TY,
        gives: Gives::OrFirst,
    }
}

fn binary(op: PrimOp, lhs: &TypedExpr, rhs: &TypedExpr) -> TypedExpr {
    TypedExpr::Binary {
        ty: TY,
        op,
        lhs: boxed(lhs),
        rhs: boxed(rhs),
    }
}

fn arm(pat: TypedPat, guard: Option<&TypedExpr>, body: &TypedExpr) -> TypedArm {
    TypedArm {
        pat,
        guard: guard.cloned(),
        body: body.clone(),
    }
}

fn matching(scrut: &TypedExpr, arms: Vec<TypedArm>) -> TypedExpr {
    TypedExpr::Match {
        ty: TY,
        scrut: boxed(scrut),
        arms,
    }
}

fn wild() -> TypedPat {
    TypedPat::Wild { ty: TY }
}

/// What makes an expression around another.
type Around = Box<dyn Fn(&TypedExpr) -> TypedExpr>;

/// An expression of each kind that is brief when what is in it is, as a function
/// of one of its parts. The rest of it is brief.
fn around() -> Vec<(&'static str, Around)> {
    vec![
        (
            "let",
            Box::new(|e| TypedExpr::Let {
                ty: TY,
                bind: name(1),
                init: boxed(e),
                body: boxed(&var()),
            }),
        ),
        (
            "let body",
            Box::new(|e| TypedExpr::Let {
                ty: TY,
                bind: name(1),
                init: boxed(&var()),
                body: boxed(e),
            }),
        ),
        (
            "seq",
            Box::new(|e| TypedExpr::Seq {
                ty: TY,
                effect: boxed(e),
                body: boxed(&var()),
            }),
        ),
        (
            "seq body",
            Box::new(|e| TypedExpr::Seq {
                ty: TY,
                effect: boxed(&var()),
                body: boxed(e),
            }),
        ),
        ("left", Box::new(|e| binary(PrimOp::FloatAdd, e, &var()))),
        ("right", Box::new(|e| binary(PrimOp::FloatLt, &var(), e))),
        (
            "not",
            Box::new(|e| TypedExpr::Unary {
                ty: TY,
                op: PrimOp::Not,
                operand: boxed(e),
            }),
        ),
        (
            "and",
            Box::new(|e| TypedExpr::And {
                ty: TY,
                lhs: boxed(e),
                rhs: boxed(&var()),
            }),
        ),
        (
            "or",
            Box::new(|e| TypedExpr::Or {
                ty: TY,
                lhs: boxed(&var()),
                rhs: boxed(e),
            }),
        ),
        (
            "tuple",
            Box::new(|e| TypedExpr::Tuple {
                ty: TY,
                elems: vec![var(), e.clone()],
            }),
        ),
        (
            "element",
            Box::new(|e| TypedExpr::TupleIndex {
                ty: TY,
                recv: boxed(e),
                idx: 1,
            }),
        ),
        (
            "field",
            Box::new(|e| TypedExpr::Field {
                ty: TY,
                recv: boxed(e),
                idx: 1,
            }),
        ),
        (
            "constructor",
            Box::new(|e| TypedExpr::Ctor {
                ty: TY,
                variant: variant(),
                args: vec![var(), e.clone()],
            }),
        ),
        (
            "array",
            Box::new(|e| TypedExpr::Array {
                ty: TY,
                elems: vec![TypedArrayElem::Elem(var()), TypedArrayElem::Elem(e.clone())],
            }),
        ),
        (
            "indexed",
            Box::new(|e| probe(ProbeOp::ArrayGet, vec![e.clone(), var()])),
        ),
        (
            "index",
            Box::new(|e| probe(ProbeOp::ArrayGet, vec![var(), e.clone()])),
        ),
        (
            "range",
            Box::new(|e| TypedExpr::Range {
                ty: TY,
                start: boxed(e),
                end: boxed(&var()),
            }),
        ),
        (
            "range end",
            Box::new(|e| TypedExpr::Range {
                ty: TY,
                start: boxed(&var()),
                end: boxed(e),
            }),
        ),
        (
            "capture",
            Box::new(|e| TypedExpr::Closure {
                ty: TY,
                func_idx: FuncIdx(0),
                captures: vec![var(), e.clone()],
            }),
        ),
        (
            "cond",
            Box::new(|e| TypedExpr::If {
                ty: TY,
                cond: boxed(e),
                then: boxed(&var()),
                els: boxed(&var()),
            }),
        ),
        (
            "then",
            Box::new(|e| TypedExpr::If {
                ty: TY,
                cond: boxed(&var()),
                then: boxed(e),
                els: boxed(&var()),
            }),
        ),
        (
            "else",
            Box::new(|e| TypedExpr::If {
                ty: TY,
                cond: boxed(&var()),
                then: boxed(&var()),
                els: boxed(e),
            }),
        ),
        (
            "subject",
            Box::new(|e| matching(e, vec![arm(wild(), None, &var())])),
        ),
        (
            "guard",
            Box::new(|e| matching(&var(), vec![arm(wild(), Some(e), &var())])),
        ),
        (
            "arm",
            Box::new(|e| {
                let first = arm(wild(), Some(&var()), &var());
                matching(&var(), vec![first, arm(wild(), None, e)])
            }),
        ),
    ]
}

/// What is made of what is brief is brief, and nothing made of a call is.
#[test]
fn an_expression_is_brief_when_everything_in_it_is() {
    let constant = TypedExpr::Const {
        ty: TY,
        value: ConstId(0),
    };
    for plain in [var(), constant, TypedExpr::Nil { ty: TY }] {
        assert!(brief(&plain), "{plain:?}");
        for (what, part) in around() {
            assert!(brief(&part(&plain)), "{what}");
            assert!(brief(&part(&part(&plain))), "{what}");
        }
    }
    for (what, part) in around() {
        assert!(!brief(&part(&call())), "{what}");
        assert!(!brief(&part(&part(&call()))), "{what}");
    }
}

/// What may write in place, call something or take as long as its operands are
/// big is not brief, whatever it is made of.
#[test]
fn what_takes_long_or_writes_is_not_brief() {
    let update = TypedExpr::Update {
        ty: TY,
        variant: variant(),
        base: boxed(&var()),
        sets: vec![(0, var())],
    };
    let slice = probe(ProbeOp::ArraySlice, vec![var(), var(), var()]);
    let write = probe(ProbeOp::ArraySet, vec![var(), var(), var()]);
    let lookup = probe(ProbeOp::MapGet, vec![var(), var()]);
    let text = TypedExpr::Interp {
        ty: TY,
        parts: vec![TypedInterpPart::Expr(var())],
    };
    let bits = TypedExpr::BinLit {
        ty: TY,
        segs: vec![TypedBinSeg::Utf8 { value: var() }],
    };
    let spread = TypedExpr::Array {
        ty: TY,
        elems: vec![TypedArrayElem::Spread(var())],
    };
    let built_in = TypedExpr::Call {
        ty: TY,
        callee: TypedCallee::Dynamic(boxed(&var())),
        args: vec![var()],
    };
    for e in [
        update,
        slice,
        write,
        lookup,
        text,
        bits,
        spread,
        built_in,
        call(),
    ] {
        assert!(!brief(&e), "{e:?}");
    }
    for op in [
        PrimOp::IntAdd,
        PrimOp::IntMul,
        PrimOp::IntEq,
        PrimOp::IntLt,
        PrimOp::Add,
        PrimOp::Lt,
        PrimOp::Eq,
        PrimOp::Ne,
        PrimOp::StringConcat,
    ] {
        assert!(!brief(&binary(op, &var(), &var())), "{op:?}");
    }
    for op in [PrimOp::IntNeg, PrimOp::Neg] {
        let e = TypedExpr::Unary {
            ty: TY,
            op,
            operand: boxed(&var()),
        };
        assert!(!brief(&e), "{op:?}");
    }
}

/// A pattern is brief when it only looks, however it is nested. One that takes the
/// rest of an array may cut it in place, and one on a binary reads it.
#[test]
fn a_pattern_that_takes_the_rest_is_not_brief() {
    let array = |prefix: Vec<TypedPat>, rest: PatRest| TypedPat::Array {
        ty: TY,
        elem_ty: TY,
        prefix,
        rest,
    };
    let bits = TypedPat::Bin {
        ty: TY,
        zero: ConstId(0),
        segs: vec![TypedBinPatSeg::Utf8 { value: wild() }],
        rest: PatRest::None,
    };
    let within = |pat: &TypedPat| {
        vec![
            pat.clone(),
            TypedPat::Ctor {
                ty: TY,
                variant: variant(),
                fields: vec![wild(), pat.clone()],
            },
            TypedPat::Tuple {
                ty: TY,
                elems: vec![wild(), pat.clone()],
            },
            TypedPat::Or {
                ty: TY,
                alts: vec![wild(), pat.clone()],
            },
            array(vec![wild(), pat.clone()], PatRest::Discard),
        ]
    };
    let asks = |pat: TypedPat| brief(&matching(&var(), vec![arm(pat, None, &var())]));
    for pat in [
        wild(),
        TypedPat::Bind(name(1)),
        TypedPat::Lit {
            ty: TY,
            value: ConstId(0),
        },
        TypedPat::Range {
            ty: TY,
            lo: ConstId(0),
            hi: ConstId(1),
        },
        array(vec![], PatRest::None),
        array(vec![wild()], PatRest::Discard),
    ] {
        assert!(within(&pat).into_iter().all(asks), "{pat:?}");
    }
    for pat in [array(vec![], PatRest::Bind(name(2))), bits] {
        assert!(!within(&pat).into_iter().any(asks), "{pat:?}");
    }
}

#[test]
fn an_update_pays_by_what_it_sets_and_keeps() {
    for sets in 0..40usize {
        for kept in 0..100 {
            let want = sets > 0 && kept > 0 && (sets < 4 || kept >= 2 * sets);
            assert_eq!(pays(sets, kept), want, "{sets} {kept}");
        }
    }
}
