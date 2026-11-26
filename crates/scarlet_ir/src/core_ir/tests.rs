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

/// The two zeros are one key, and every other Float is its own.
#[test]
fn a_float_key_is_what_double_equals_sees() {
    assert_eq!(FloatKey::new(-0.0), FloatKey::new(0.0));
    assert_eq!(FloatKey::new(-0.0).get().to_bits(), 0.0f64.to_bits());
    for x in [
        1.0,
        -1.0,
        5e-324,
        -5e-324,
        f64::MAX,
        f64::MIN,
        f64::INFINITY,
    ] {
        assert_eq!(FloatKey::new(x).get().to_bits(), x.to_bits());
        assert_ne!(FloatKey::new(x), FloatKey::new(0.0));
        assert_ne!(FloatKey::new(x), FloatKey::new(-x));
    }
    assert_eq!(FloatKey::new(-0.5).to_string(), "-0.5");
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

fn switch(cases: Cases, default: Option<CoreExpr>) -> CoreExpr {
    CoreExpr::Switch {
        scrut: LocalId(0),
        cases,
        default: default.map(Box::new),
    }
}

/// Every kind of case, with and without a default, and what a case opens.
#[test]
fn if_and_switch_indent() {
    let opened = CoreExpr::Open {
        cell: LocalId(0),
        variant: vref(4, 1),
        fields: vec![(0, bind(5, RTy(2))), (2, bind(6, RTy(3)))],
        body: Box::new(CoreExpr::TailCall {
            callee: Callee::Self_,
            args: locals(&[5, 6]),
        }),
    };
    let variants = Cases::Variant {
        ty: TypeId(4),
        arms: vec![(vec![1], opened), (vec![0, 3], ret())],
    };
    let texts = Cases::Text(vec![(vec![ConstId(7)], CoreExpr::NoArm)]);
    let floats = Cases::Float(vec![(
        vec![FloatKey::new(-0.0), FloatKey::new(1.5)],
        switch(texts, Some(ret())),
    )]);
    let ints = Cases::Int(vec![
        (vec![-3, i64::MAX], switch(floats, Some(ret()))),
        (vec![4], ret()),
    ]);
    assert_eq!(
        iff(switch(variants, None), switch(ints, Some(ret()))).to_string(),
        "\
if %1
  switch %0
    | 4.1 ->
      open %0 4.1(0: %5:2, 2: %6:3)
      tail call self(%5, %6)
    | 4.0, 4.3 ->
      ret %0
else
  switch %0
    | -3, 9223372036854775807 ->
      switch %0
        | 0.0, 1.5 ->
          switch %0
            | c7 ->
              no arm
            | _ ->
              ret %0
        | _ ->
          ret %0
    | 4 ->
      ret %0
    | _ ->
      ret %0
"
    );
}

/// Each variant a switch names counts, and so does one that is taken apart.
#[test]
fn a_switch_and_an_open_mention_their_variants() {
    let open = |idx, body| CoreExpr::Open {
        cell: LocalId(0),
        variant: vref(7, idx),
        fields: vec![(0, bind(1, RTy(0)))],
        body: Box::new(body),
    };
    let inner = Cases::Variant {
        ty: TypeId(8),
        arms: vec![(vec![2, 5], ret()), (vec![1], open(3, ret()))],
    };
    let ints = Cases::Int(vec![(vec![1], switch(inner, Some(open(4, ret()))))]);
    let mut all = Vec::new();
    switch(ints, Some(ret())).for_each_variant(|v| all.push((v.type_id.0, v.variant_idx)));
    all.sort_unstable();
    assert_eq!(all, [(7, 3), (7, 4), (8, 1), (8, 2), (8, 5)]);
}

/// Every arm of every kind of case is one of `arms`, in order, and `map` and
/// `arms_mut` go over the same.
#[test]
fn cases_go_over_their_arms_in_order() {
    let numbered = |n| CoreExpr::Ret(LocalId(n));
    let all = [
        Cases::Variant {
            ty: TypeId(1),
            arms: vec![(vec![0], numbered(1)), (vec![1, 2], numbered(2))],
        },
        Cases::Int(vec![(vec![5], numbered(1)), (vec![6], numbered(2))]),
        Cases::Float(vec![
            (vec![FloatKey::new(1.0)], numbered(1)),
            (vec![FloatKey::new(2.0)], numbered(2)),
        ]),
        Cases::Text(vec![
            (vec![ConstId(0)], numbered(1)),
            (vec![ConstId(1)], numbered(2)),
        ]),
    ];
    let shown = |cases: &Cases| switch(cases.clone(), None).to_string();
    for mut cases in all {
        let names =
            |cases: &Cases| -> Vec<String> { cases.arms().iter().map(|e| e.to_string()).collect() };
        assert_eq!(names(&cases), ["ret %1\n", "ret %2\n"]);
        assert_eq!(shown(&cases.map(Clone::clone)), shown(&cases));
        let mapped = cases.map(|e| iff(e.clone(), e.clone()));
        for (i, arm) in cases.arms_mut().into_iter().enumerate() {
            assert_eq!(arm.to_string(), format!("ret %{}\n", i + 1));
            *arm = iff(arm.clone(), arm.clone());
        }
        assert_eq!(shown(&mapped), shown(&cases));
    }
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
    let stops = || CoreExpr::NoArm;
    assert!(reached(one(stops())).is_empty());
    assert!(reached(one(switch(Cases::Int(Vec::new()), None))).is_empty());
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
    // A switch's cases and its default count, and lets, drops, stores and cells
    // taken apart are gone past.
    let arms = |last| {
        switch(
            Cases::Int(vec![(vec![1], jump(1, &[])), (vec![2], last)]),
            None,
        )
    };
    let default = |last| switch(Cases::Int(vec![(vec![1], jump(1, &[]))]), Some(last));
    let past = |e| CoreExpr::Let {
        bind: bind(9, RTy(0)),
        rhs: Atom::Nil,
        body: Box::new(CoreExpr::SetGlobal {
            slot: GlobalSlot(0),
            value: LocalId(9),
            body: Box::new(CoreExpr::Open {
                cell: LocalId(8),
                variant: vref(0, 0),
                fields: Vec::new(),
                body: Box::new(CoreExpr::Drop {
                    local: LocalId(9),
                    body: Box::new(e),
                }),
            }),
        }),
    };
    for of in [arms as fn(CoreExpr) -> CoreExpr, default] {
        assert_eq!(reached(one(past(of(jump(1, &[]))))), [1]);
        assert_eq!(reached(one(past(of(stops())))), [1]);
        assert!(reached(one(past(of(ret())))).is_empty());
    }
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
