use super::*;

fn bind(id: u32, ty: RTy) -> CoreBind {
    CoreBind::new(LocalId(id), ty)
}

fn vref(tid: i32, idx: u16) -> VariantRef {
    VariantRef {
        type_id: TypeId(tid),
        variant_idx: idx,
    }
}

#[test]
fn atom_forms() {
    assert_eq!(Atom::Local(LocalId(3)).to_string(), "%3");
    assert_eq!(Atom::Const(ConstId(7)).to_string(), "c7");
    assert_eq!(
        Atom::prim(PrimOp::IntAdd, vec![LocalId(0), LocalId(1)]).to_string(),
        "IntAdd(%0, %1)"
    );
    assert_eq!(
        Atom::PrimOp {
            op: PrimOp::TupleField(2),
            args: vec![LocalId(0)],
        }
        .to_string(),
        "TupleField(2)(%0)"
    );
    assert_eq!(
        Atom::Closure {
            func_idx: FuncIdx(5),
            captures: vec![LocalId(1)],
        }
        .to_string(),
        "closure fn#5(%1)"
    );
    assert_eq!(
        Atom::Ctor {
            variant: vref(7, 1),
            fields: vec![LocalId(2)],
        }
        .to_string(),
        "ctor 7.1(%2)"
    );
    assert_eq!(
        Atom::Ctor {
            variant: vref(7, 1),
            fields: vec![LocalId(2), LocalId(3)],
        }
        .to_string(),
        "ctor 7.1(%2, %3)"
    );
    assert_eq!(
        Atom::Update {
            base: LocalId(2),
            variant: vref(7, 1),
            sets: vec![(0, LocalId(3)), (4, LocalId(5))],
        }
        .to_string(),
        "update 7.1(..%2, 0: %3, 4: %5)"
    );
    assert_eq!(
        Atom::Call {
            callee: Callee::Known(FuncIdx(9)),
            args: vec![LocalId(0)],
        }
        .to_string(),
        "call fn#9(%0)"
    );
    assert_eq!(
        Atom::Call {
            callee: Callee::Self_,
            args: vec![],
        }
        .to_string(),
        "call self()"
    );
    assert_eq!(
        Atom::Call {
            callee: Callee::Local(LocalId(4)),
            args: vec![LocalId(5), LocalId(6)],
        }
        .to_string(),
        "call %4(%5, %6)"
    );
}

#[test]
fn pat_forms() {
    assert_eq!(CorePat::Wild.to_string(), "_");
    assert_eq!(CorePat::Lit(ConstId(2)).to_string(), "c2");
    assert_eq!(CorePat::Bind(bind(3, RTy(0))).to_string(), "%3");
    assert_eq!(
        CorePat::Ctor {
            variant: vref(4, 0),
            fields: vec![bind(5, RTy(2)), bind(6, RTy(2))],
        }
        .to_string(),
        "4.0(%5, %6)"
    );
}

#[test]
fn let_drop_spine_is_flat() {
    let e = CoreExpr::Let {
        bind: bind(2, RTy(10)),
        rhs: Atom::prim(PrimOp::IntAdd, vec![LocalId(0), LocalId(1)]),
        body: Box::new(CoreExpr::Drop {
            local: LocalId(0),
            body: Box::new(CoreExpr::Let {
                bind: bind(3, RTy(11)),
                rhs: Atom::Ctor {
                    variant: vref(7, 1),
                    fields: vec![LocalId(2)],
                },
                body: Box::new(CoreExpr::Tail(Atom::Local(LocalId(3)))),
            }),
        }),
    };
    assert_eq!(
        e.to_string(),
        "\
let %2:10 = IntAdd(%0, %1)
drop %0
let %3:11 = ctor 7.1(%2)
ret %3
"
    );
}

#[test]
fn if_and_match_indent() {
    let m = CoreExpr::Match {
        scrut: LocalId(0),
        arms: vec![
            (
                CorePat::Ctor {
                    variant: vref(4, 0),
                    fields: vec![bind(5, RTy(2))],
                },
                CoreExpr::Tail(Atom::Call {
                    callee: Callee::Self_,
                    args: vec![LocalId(5)],
                }),
            ),
            (CorePat::Wild, CoreExpr::Tail(Atom::Const(ConstId(0)))),
        ],
        ty: RTy(9),
    };
    let e = CoreExpr::If {
        cond: LocalId(1),
        then: Box::new(m),
        els: Box::new(CoreExpr::Tail(Atom::Local(LocalId(0)))),
        ty: RTy(9),
    };
    assert_eq!(
        e.to_string(),
        "\
if %1 :9
  match %0 :9
    | 4.0(%5) ->
      ret call self(%5)
    | _ ->
      ret c0
else
  ret %0
"
    );
}

#[test]
fn core_fn_header_and_body() {
    let f = CoreFn {
        params: vec![bind(0, RTy(1)), bind(1, RTy(1))],
        body: CoreExpr::Tail(Atom::prim(PrimOp::IntLt, vec![LocalId(0), LocalId(1)])),
        ret_ty: RTy(3),
    };
    assert_eq!(
        f.to_string(),
        "\
fn(%0:1, %1:1) -> :3
  ret IntLt(%0, %1)
"
    );
}

/// An update reads its base and then what it sets, and builds the constructor it
/// updates, wherever it is.
#[test]
fn an_update_reads_its_base_and_builds_its_constructor() {
    let update = |idx| Atom::Update {
        base: LocalId(2),
        variant: vref(7, idx),
        sets: vec![(0, LocalId(3)), (4, LocalId(2))],
    };
    let mut reads = Vec::new();
    update(1).for_each_operand(|l| reads.push(l.0));
    assert_eq!(reads, [2, 3, 2]);
    let e = CoreExpr::Let {
        bind: bind(4, RTy(0)),
        rhs: update(1),
        body: Box::new(CoreExpr::Tail(update(2))),
    };
    let mut built = Vec::new();
    e.for_each_variant(|v| built.push(v));
    built.sort_by_key(|v| v.variant_idx);
    assert_eq!(built, [vref(7, 1), vref(7, 2)]);
}
