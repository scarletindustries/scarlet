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
                body: Box::new(CoreExpr::Ret(LocalId(3))),
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

fn locals(ids: &[u32]) -> Vec<LocalId> {
    ids.iter().copied().map(LocalId).collect()
}

fn jump(to: u32, args: &[u32]) -> CoreExpr {
    CoreExpr::Jump {
        to: JoinId(to),
        args: locals(args),
    }
}

fn join(id: u32, params: Vec<CoreBind>, body: CoreExpr, cont: CoreExpr) -> CoreExpr {
    CoreExpr::Join {
        id: JoinId(id),
        params,
        cont: Box::new(cont),
        body: Box::new(body),
    }
}

fn iff(then: CoreExpr, els: CoreExpr) -> CoreExpr {
    CoreExpr::If {
        cond: LocalId(1),
        then: Box::new(then),
        els: Box::new(els),
    }
}

fn ret() -> CoreExpr {
    CoreExpr::Ret(LocalId(0))
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
                CoreExpr::TailCall {
                    callee: Callee::Self_,
                    args: locals(&[5]),
                },
            ),
            (CorePat::Wild, CoreExpr::Ret(LocalId(2))),
        ],
    };
    assert_eq!(
        iff(m, ret()).to_string(),
        "\
if %1
  match %0
    | 4.0(%5) ->
      tail call self(%5)
    | _ ->
      ret %2
else
  ret %0
"
    );
}

/// A join prints its body first, as it runs first, and then its continuation
/// level with it.
#[test]
fn a_join_prints_in_the_order_it_runs() {
    let inner = join(
        2,
        Vec::new(),
        iff(jump(1, &[0, 1]), jump(2, &[])),
        jump(1, &[1, 0]),
    );
    let stored = CoreExpr::SetGlobal {
        slot: GlobalSlot(7),
        value: LocalId(3),
        body: Box::new(ret()),
    };
    let e = join(1, vec![bind(3, RTy(0)), bind(4, RTy(5))], inner, stored);
    assert_eq!(
        e.to_string(),
        "\
join j1(%3:0, %4:5) after
  join j2() after
    if %1
      jump j1(%0, %1)
    else
      jump j2()
  jump j1(%1, %0)
set global7 = %3
ret %0
"
    );
}

/// A join is always reached when every way through its body ends in a jump to
/// it, through as many joins inside as there are.
#[test]
fn a_join_is_always_reached_when_no_way_leaves_its_body() {
    let reached = |e: CoreExpr| -> Vec<u32> {
        let all = e.always_reached();
        all.into_iter().map(|j| j.0).collect()
    };
    let one = |body| join(1, Vec::new(), body, ret());
    assert_eq!(reached(one(jump(1, &[]))), [1]);
    assert_eq!(reached(one(iff(jump(1, &[]), jump(1, &[])))), [1]);
    assert!(reached(one(iff(jump(1, &[]), ret()))).is_empty());
    let call = || CoreExpr::TailCall {
        callee: Callee::Self_,
        args: Vec::new(),
    };
    assert!(reached(one(iff(call(), jump(1, &[])))).is_empty());
    // One that nothing jumps to is not.
    let stops = || CoreExpr::Match {
        scrut: LocalId(0),
        arms: Vec::new(),
    };
    assert!(reached(one(stops())).is_empty());
    // A way that stops the run leaves it nowhere.
    assert_eq!(reached(one(iff(stops(), jump(1, &[])))), [1]);
    // What a body jumps to inside itself goes on to the end of that.
    let two = |inner, cont| one(join(2, Vec::new(), inner, cont));
    assert_eq!(reached(two(jump(2, &[]), jump(1, &[]))), [1, 2]);
    assert_eq!(
        reached(two(iff(jump(2, &[]), jump(1, &[])), jump(1, &[]))),
        [1]
    );
    assert_eq!(reached(two(jump(2, &[]), ret())), [2]);
    assert!(reached(two(iff(jump(2, &[]), ret()), jump(1, &[]))).is_empty());
    assert_eq!(reached(two(jump(1, &[]), ret())), [] as [u32; 0]);
    // A chain, whose joins are each reached or not by themselves.
    let chain = join(
        1,
        Vec::new(),
        jump(1, &[]),
        two(iff(jump(2, &[]), ret()), ret()),
    );
    assert!(chain.always_reached().contains(&JoinId(1)));
    // A jump to nothing leaves.
    assert!(reached(one(iff(jump(9, &[]), jump(1, &[])))).is_empty());
    // A match's arms count, and lets, drops and stores are gone past.
    let arms = |last| CoreExpr::Match {
        scrut: LocalId(0),
        arms: vec![(CorePat::Wild, jump(1, &[])), (CorePat::Wild, last)],
    };
    let past = |e| CoreExpr::Let {
        bind: bind(9, RTy(0)),
        rhs: Atom::Nil,
        body: Box::new(CoreExpr::SetGlobal {
            slot: GlobalSlot(0),
            value: LocalId(9),
            body: Box::new(CoreExpr::Drop {
                local: LocalId(9),
                body: Box::new(e),
            }),
        }),
    };
    assert_eq!(reached(one(past(arms(jump(1, &[]))))), [1]);
    assert!(reached(one(past(arms(ret())))).is_empty());
}

#[test]
fn core_fn_header_and_body() {
    let f = CoreFn {
        params: vec![bind(0, RTy(1)), bind(1, RTy(1))],
        body: CoreExpr::Ret(LocalId(1)),
        ret_ty: RTy(3),
    };
    assert_eq!(
        f.to_string(),
        "\
fn(%0:1, %1:1) -> :3
  ret %1
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
    let made = |id, idx, end| CoreExpr::Let {
        bind: bind(id, RTy(0)),
        rhs: update(idx),
        body: Box::new(end),
    };
    // On either side of a join.
    let e = join(1, Vec::new(), made(4, 1, jump(1, &[])), made(5, 2, ret()));
    let mut built = Vec::new();
    e.for_each_variant(|v| built.push(v));
    built.sort_by_key(|v| v.variant_idx);
    assert_eq!(built, [vref(7, 1), vref(7, 2)]);
}
