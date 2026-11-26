use super::*;
use crate::testkit::{
    FIELDS, bind, bound, ctor, field, func, iff, join, jump, local, ownership, probe, ret, switch,
    switch_on, tail_call, variant,
};
use crate::{Callee, CaptureIdx, ConstId, FuncIdx, GlobalSlot, Load};
use scarlet_ir::TypeId;
use scarlet_ir::rty::{PrimIds, Repr};

/// The same kind of arena the elaborator hands `perceus` in the real
/// pipeline.
fn pool() -> ResolvedPool {
    ResolvedPool::new(PrimIds::default())
}

/// A nominal type that isn't a primitive and whose values may be cells.
fn con(p: &mut ResolvedPool, id: i32) -> RTy {
    p.mk_con(TypeId(id), Repr::Counted, &[]).unwrap()
}

/// A type whose constructors have no fields.
fn word(p: &mut ResolvedPool, id: i32) -> RTy {
    p.mk_con(TypeId(id), Repr::Word, &[]).unwrap()
}

/// `Int`, which gets no `Drop`.
fn int_ty(p: &mut ResolvedPool) -> RTy {
    let int = p.prims().int;
    p.mk_con(int, Repr::Counted, &[]).unwrap()
}

/// Every drop in `e`.
fn drops(e: &CoreExpr) -> Vec<LocalId> {
    let mut out = Vec::new();
    let mut work = vec![e];
    while let Some(e) = work.pop() {
        match e {
            CoreExpr::Drop { local, body } => {
                out.push(*local);
                work.push(body);
            }
            CoreExpr::Let { body, .. } | CoreExpr::SetGlobal { body, .. } => work.push(body),
            CoreExpr::Join { cont, body, .. } => work.extend([&**cont, &**body]),
            CoreExpr::If {
                then: a, els: b, ..
            }
            | CoreExpr::Probe {
                hit: a, miss: b, ..
            } => work.extend([&**a, &**b]),
            CoreExpr::Open { body, .. } => work.push(body),
            CoreExpr::Switch { cases, default, .. } => {
                work.extend(cases.arms());
                work.extend(default.as_deref());
            }
            CoreExpr::Ret(_)
            | CoreExpr::TailCall { .. }
            | CoreExpr::Jump { .. }
            | CoreExpr::NoArm => {}
        }
    }
    out
}

fn count_drops(e: &CoreExpr) -> usize {
    drops(e).len()
}

fn find_drop(e: &CoreExpr, id: LocalId) -> bool {
    drops(e).contains(&id)
}

fn run(
    pool: &ResolvedPool,
    locals: &TiVec<LocalId, RTy>,
    f: CoreFn,
) -> Result<CoreFn, InternalLinkError> {
    super::perceus(pool, &shapes(), locals, &f.body.always_reached(), f)
}

/// Type 0 is the one that the tests' constructors are of.
fn shapes() -> Shapes {
    Shapes::new([(TypeId(0), FIELDS.to_vec())])
}

fn fields(ty: TypeId) -> Vec<usize> {
    match ty {
        TypeId(0) => FIELDS.to_vec(),
        _ => Vec::new(),
    }
}

/// Run the pass with every bind's type in the table, the way `lower`
/// hands it over.
fn perceus(pool: &ResolvedPool, f: CoreFn) -> CoreFn {
    run(pool, &types_of(&f), f).expect("every local is typed")
}

/// The checker finds nothing wrong with `f`.
fn checked(pool: &ResolvedPool, f: &CoreFn, body: ownership::Body) {
    ownership::check(pool, &fields, f, body).unwrap_or_else(|why| panic!("{why}\n{f}"));
}

/// What `perceus` makes of `f`, as the lines it prints, without the types. It
/// has to pass the checker.
fn lines(pool: &ResolvedPool, f: CoreFn) -> Vec<String> {
    let f = perceus(pool, f);
    checked(pool, &f, ownership::Body::Function);
    let text = f.body.to_string();
    let untyped = |l: &str| {
        let words = l.split(' ').map(|word| match word.split_once(':') {
            Some((head, ty)) if ty.chars().next().is_some_and(char::is_numeric) => {
                let rest = ty.trim_start_matches(char::is_numeric);
                format!("{head}{rest}")
            }
            _ => word.to_string(),
        });
        words.collect::<Vec<_>>().join(" ")
    };
    text.lines().map(untyped).collect()
}

/// The type of each local of `f`. An id no bind uses gets the return type, which
/// nothing reads.
fn types_of(f: &CoreFn) -> TiVec<LocalId, RTy> {
    let mut all = f.params.clone();
    let mut work = vec![&f.body];
    while let Some(e) = work.pop() {
        match e {
            CoreExpr::Let { bind, body, .. } => {
                all.push(*bind);
                work.push(body);
            }
            CoreExpr::Join {
                params, cont, body, ..
            } => {
                all.extend(params);
                work.extend([&**cont, &**body]);
            }
            CoreExpr::SetGlobal { body, .. } | CoreExpr::Drop { body, .. } => work.push(body),
            CoreExpr::If { then, els, .. } => work.extend([&**then, &**els]),
            CoreExpr::Probe {
                binds, hit, miss, ..
            } => {
                all.extend(binds.iter().flatten());
                work.extend([&**hit, &**miss]);
            }
            CoreExpr::Open { fields, body, .. } => {
                all.extend(fields.iter().map(|(_, b)| *b));
                work.push(body);
            }
            CoreExpr::Switch { cases, default, .. } => {
                work.extend(cases.arms());
                work.extend(default.as_deref());
            }
            CoreExpr::Ret(_)
            | CoreExpr::TailCall { .. }
            | CoreExpr::Jump { .. }
            | CoreExpr::NoArm => {}
        }
    }
    let len = all.iter().map(|b| b.id.0 + 1).max().unwrap_or(0);
    let mut locals: TiVec<LocalId, RTy> = (0..len).map(|_| f.ret_ty).collect();
    for b in all {
        if let Some(slot) = locals.get_mut(b.id) {
            *slot = b.ty;
        }
    }
    locals
}

/// A call of another function.
fn call(args: &[u32]) -> Atom {
    Atom::Call {
        callee: Callee::Known(FuncIdx(0)),
        args: args.iter().copied().map(local).collect(),
    }
}

/// A toplevel gives up its own reference to what it stores in a global, at its
/// last use. The slot has another.
#[test]
fn a_stored_value_is_dropped_where_the_toplevel_last_reads_it() {
    let mut pool = pool();
    let obj = con(&mut pool, 7);
    let store = |slot, value, rest| CoreExpr::SetGlobal {
        slot: GlobalSlot(slot),
        value: local(value),
        body: Box::new(rest),
    };
    let body = store(0, 0, store(1, 1, bound(bind(2, obj), ctor(&[1]), ret(2))));
    let body = bound(
        bind(0, obj),
        call(&[]),
        bound(bind(1, obj), call(&[]), body),
    );
    let f = perceus(&pool, func(Vec::new(), body, obj));
    checked(&pool, &f, ownership::Body::Toplevel);
    let text = f.body.to_string();
    let lines: Vec<&str> = text.lines().skip(2).collect();
    assert_eq!(
        lines,
        [
            "set global0 = %0",
            "drop %0",
            "set global1 = %1",
            "let %2:0 = ctor 0.0(%1)",
            "drop %1",
            "ret %2",
        ]
    );
}

/// A case that reads every field it needs of what it is for, and nothing else of it,
/// takes it apart. A value that another case needs and this one does not is dropped in
/// front of that, and a field that nothing reads is not there. A case that needs
/// nothing of it drops it, unless there is nothing in a cell.
#[test]
fn a_case_takes_its_scrutinee_apart_where_it_dies() {
    let mut pool = pool();
    let list = con(&mut pool, 99);
    let own = Atom::Call {
        callee: Callee::Self_,
        args: vec![local(5)],
    };
    let calls_itself = bound(bind(6, list), own, bound(bind(7, list), ctor(&[6]), ret(7)));
    // It reads a field that it has no use for, the two fields in no order, and one
    // of them twice.
    let calls_itself = field(bind(8, list), 1, 2, calls_itself);
    let calls_itself = field(bind(5, list), 1, 1, calls_itself);
    let calls_itself = field(bind(4, list), 1, 0, calls_itself);
    let reads_it = bound(bind(16, list), ctor(&[0, 1, 14]), ret(16));
    let reads_it = field(bind(14, list), 1, 1, reads_it);
    let twice = bound(bind(23, list), ctor(&[20, 21, 22]), ret(23));
    let twice = field(bind(22, list), 1, 0, twice);
    let twice = field(bind(21, list), 1, 1, twice);
    let twice = field(bind(20, list), 1, 0, twice);
    let nothing = bound(bind(30, list), call(&[]), ret(30));
    let arms = vec![(vec![0], calls_itself), (vec![1], ret(0)), (vec![2], twice)];
    let f = func(
        vec![bind(0, list), bind(1, list)],
        switch(1, arms, Some(nothing)),
        list,
    );
    assert_eq!(
        lines(&pool, f),
        [
            "switch %1",
            "  | 0.0 ->",
            "    drop %0",
            "    open %1 0.0(1: %5)",
            "    let %6 = call self(%5)",
            "    drop %5",
            "    let %7 = ctor 0.0(%6)",
            "    drop %6",
            "    ret %7",
            "  | 0.1 ->",
            "    ret %0",
            "  | 0.2 ->",
            "    drop %0",
            "    open %1 0.2(0: %20, 1: %21)",
            "    let %22 = %20",
            "    let %23 = ctor 0.0(%20, %21, %22)",
            "    drop %20",
            "    drop %21",
            "    drop %22",
            "    ret %23",
            // Only the constructor without fields that no case names is left.
            "  | _ ->",
            "    drop %0",
            "    let %30 = call fn#0()",
            "    ret %30",
        ]
    );
    // One that is read on is not taken apart, and one that several constructors share
    // is not known to be one.
    let f = func(
        vec![bind(0, list), bind(1, list)],
        switch(
            1,
            vec![(vec![0, 2], field(bind(9, list), 1, 0, ret(9)))],
            Some(reads_it),
        ),
        list,
    );
    assert_eq!(
        lines(&pool, f),
        [
            "switch %1",
            "  | 0.0, 0.2 ->",
            "    drop %0",
            "    let %9 = Field(0)(%1)",
            "    drop %1",
            "    ret %9",
            "  | _ ->",
            "    let %14 = Field(1)(%1)",
            "    let %16 = ctor 0.0(%0, %1, %14)",
            "    drop %0",
            "    drop %14",
            "    ret %16",
        ]
    );
}

/// Where a cell is taken apart is where the last run of reads of it starts, in a case
/// inside the one that says what it is, and past a guard. A read between, or of
/// another cell, is no end of a run.
#[test]
fn a_cell_is_taken_apart_at_its_last_run_of_reads() {
    let mut pool = pool();
    let (t, int) = (con(&mut pool, 99), int_ty(&mut pool));
    // %0 is constructor 0 and its field 0, %1, is constructor 2.
    let ends = bound(bind(6, t), ctor(&[2, 3, 4]), ret(6));
    let reads = field(bind(4, t), 0, 2, ends);
    let reads = field(bind(3, t), 1, 1, reads);
    let reads = field(bind(2, t), 0, 1, reads);
    let inner = switch(1, vec![(vec![2], reads)], Some(ret(9)));
    let body = field(bind(1, t), 0, 0, inner);
    let outer = switch(0, vec![(vec![0], body)], Some(ret(9)));
    assert_eq!(
        lines(&pool, func(vec![bind(0, t), bind(9, t)], outer, t)),
        [
            "switch %0",
            "  | 0.0 ->",
            "    let %1 = Field(0)(%0)",
            "    switch %1",
            "      | 0.2 ->",
            "        drop %9",
            "        open %0 0.0(1: %2, 2: %4)",
            "        open %1 0.2(1: %3)",
            "        let %6 = ctor 0.0(%2, %3, %4)",
            "        drop %2",
            "        drop %3",
            "        drop %4",
            "        ret %6",
            "      | _ ->",
            "        drop %0",
            "        drop %1",
            "        ret %9",
            "  | _ ->",
            "    drop %0",
            "    ret %9",
        ]
    );
    // A call between two reads is the end of a run, and so the first is a read.
    let ends = bound(bind(6, t), ctor(&[2, 3]), ret(6));
    let ends = field(bind(3, t), 0, 1, ends);
    let called = bound(bind(5, int), call(&[]), ends);
    let reads = field(bind(2, t), 0, 0, called);
    let f = func(
        vec![bind(0, t)],
        switch(0, vec![(vec![0], reads)], Some(CoreExpr::NoArm)),
        t,
    );
    assert_eq!(
        lines(&pool, f),
        [
            "switch %0",
            "  | 0.0 ->",
            "    let %2 = Field(0)(%0)",
            "    let %5 = call fn#0()",
            "    open %0 0.0(1: %3)",
            "    let %6 = ctor 0.0(%2, %3)",
            "    drop %2",
            "    drop %3",
            "    ret %6",
            // Nothing is given up where the run stops.
            "  | _ ->",
            "    no arm",
        ]
    );
    // Outside the case nothing says what it is.
    let read = field(bind(3, t), 0, 0, ret(3));
    let cont = join(
        0,
        Vec::new(),
        switch(0, vec![(vec![0], jump(0, &[]))], Some(jump(0, &[]))),
        read,
    );
    let got = lines(&pool, func(vec![bind(0, t)], cont, t));
    assert!(!got.iter().any(|l| l.contains("open")), "{got:?}");
    // A switch on the same value in a case for several says which of them it is, and
    // only in its own cases.
    let inner = switch(
        0,
        vec![(vec![0], field(bind(1, t), 0, 0, ret(1)))],
        Some(field(bind(2, t), 0, 0, ret(2))),
    );
    let f = func(
        vec![bind(0, t)],
        switch(0, vec![(vec![0, 2], inner)], Some(ret(0))),
        t,
    );
    let got = lines(&pool, f);
    let opens: Vec<&String> = got.iter().filter(|l| l.contains("open")).collect();
    assert_eq!(opens, ["        open %0 0.0(0: %1)"], "{got:?}");
}

/// A case, or a default, for constructors that all have no fields drops nothing of
/// what it is for. One for a constructor that the tables do not know does.
#[test]
fn what_is_for_constructors_with_no_fields_drops_nothing() {
    let mut pool = pool();
    let (t, int) = (con(&mut pool, 99), int_ty(&mut pool));
    let one = |arms: &[&[u16]], default: bool| {
        let arms = arms.iter().map(|of| (of.to_vec(), ret(1))).collect();
        let body = switch(0, arms, default.then(|| ret(1)));
        let got = lines(&pool, func(vec![bind(0, t), bind(1, int)], body, int));
        let drops = |lines: &[String]| lines.iter().any(|l| l.contains("drop"));
        let all = got.join("\n");
        let arms: Vec<String> = all.split("| ").skip(1).map(str::to_string).collect();
        arms.iter()
            .map(|arm| drops(std::slice::from_ref(arm)))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        one(&[&[0], &[1], &[2], &[3]], false),
        [true, false, true, false]
    );
    assert_eq!(one(&[&[1, 3], &[0, 2]], false), [false, true]);
    assert_eq!(one(&[&[1, 0], &[2, 3]], false), [true, true]);
    assert_eq!(one(&[&[0], &[2]], true), [true, true, false]);
    assert_eq!(one(&[&[0, 2], &[1]], true), [true, false, false]);
    assert_eq!(one(&[&[0]], true), [true, true]);
    assert_eq!(one(&[&[1], &[3]], true), [false, false, true]);
    // A type that the tables do not know.
    let ty = TypeId(5);
    let arms = vec![
        (vec![0], ret(1)),
        (vec![1], field(bind(2, t), 0, 0, ret(1))),
    ];
    let body = switch_on(0, Cases::Variant { ty, arms }, Some(ret(1)));
    let got = perceus(&pool, func(vec![bind(0, t), bind(1, int)], body, int));
    assert_eq!(
        drops(&got.body),
        [local(0), local(0), local(2), local(0)].to_vec(),
        "{got}"
    );
    // Ints, Floats and texts say nothing.
    for cases in [
        Cases::Int(vec![(vec![1], ret(1)), (vec![2], ret(1))]),
        Cases::Float(vec![
            (vec![crate::FloatKey::new(1.0)], ret(1)),
            (vec![], ret(1)),
        ]),
        Cases::Text(vec![(vec![ConstId(1)], ret(1)), (vec![], ret(1))]),
    ] {
        let body = switch_on(0, cases, Some(ret(1)));
        let got = perceus(&pool, func(vec![bind(0, t), bind(1, int)], body, int));
        assert_eq!(drops(&got.body), [local(0); 3].to_vec(), "{got}");
    }
}

/// The shape of `dot_loop`: two constructors used by a call, then a tail call
/// to self. Each turn drops both once the call has read them.
#[test]
fn a_loop_drops_what_a_turn_made() {
    let mut pool = pool();
    let point = con(&mut pool, 99);
    let int = int_ty(&mut pool);
    let turn = tail_call(Callee::Self_, &[0, 4]);
    let body = bound(bind(4, int), call(&[2, 3]), iff(0, ret(1), turn));
    let body = bound(bind(3, point), ctor(&[0, 0, 0]), body);
    let body = bound(bind(2, point), ctor(&[0, 0, 0]), body);
    let f = func(vec![bind(0, int), bind(1, int)], body, int);
    let got = lines(&pool, f);
    assert_eq!(
        got[2..5],
        ["let %4 = call fn#0(%2, %3)", "drop %2", "drop %3"]
    );
    assert_eq!(got.iter().filter(|l| l.contains("drop")).count(), 2);
}

/// A tail call reads its arguments before its frame's references go, so
/// no argument is dropped ahead of it: that drop would come before the
/// read. Other locals the frame holds still drop as usual.
#[test]
fn a_tail_call_argument_is_not_dropped_before_the_call() {
    let mut pool = pool();
    let list = con(&mut pool, 99);
    for callee in [
        Callee::Self_,
        Callee::Known(FuncIdx(3)),
        Callee::Local(local(2)),
    ] {
        let body = bound(bind(1, list), ctor(&[0]), tail_call(callee, &[1]));
        let f = func(vec![bind(0, list), bind(2, list)], body, list);
        let f = perceus(&pool, f);
        checked(&pool, &f, ownership::Body::Function);
        assert!(!find_drop(&f.body, local(1)), "{}", f.body);
        assert!(find_drop(&f.body, local(0)), "{}", f.body);
        let called = callee == Callee::Local(local(2));
        assert_eq!(find_drop(&f.body, local(2)), !called, "{}", f.body);
    }
}

#[test]
fn no_drop_for_unboxed_prims() {
    let mut pool = pool();
    let int = int_ty(&mut pool);
    let body = bound(bind(1, int), Atom::Const(ConstId(0)), ret(1));
    let f = perceus(&pool, func(vec![bind(0, int)], body, int));
    assert_eq!(count_drops(&f.body), 0);
}

/// A heap local bound to a call's result that nothing reads is dropped where it
/// is bound.
#[test]
fn a_call_result_nothing_reads_is_dropped() {
    let mut pool = pool();
    let t = con(&mut pool, 99);
    let body = bound(bind(0, t), call(&[]), bound(bind(1, t), ctor(&[]), ret(1)));
    let got = lines(&pool, func(vec![], body, t));
    assert_eq!(got[1], "drop %0");
}

/// A local live into one `If` branch only drops at the head of the other,
/// so both paths release it exactly once.
#[test]
fn branches_equalise_ownership() {
    let mut pool = pool();
    let t = con(&mut pool, 99);
    let int = int_ty(&mut pool);
    let params = vec![bind(0, t), bind(1, int)];
    let els = tail_call(Callee::Known(FuncIdx(0)), &[]);
    let f = perceus(&pool, func(params, iff(1, ret(0), els), t));
    let CoreExpr::If { then, els, .. } = &f.body else {
        panic!()
    };
    assert_eq!(count_drops(then), 0, "then reads %0 as its return value");
    assert_eq!(count_drops(els), 1, "els drops %0 at head");
}

/// A value whose type is a variable is dropped like any other that may be a
/// cell, and one of a type that is never a cell is not.
#[test]
fn a_value_of_a_variable_type_is_dropped() {
    let mut pool = pool();
    let generic = pool.mk_bound(0).unwrap();
    let int = int_ty(&mut pool);
    let (t, flag) = (con(&mut pool, 99), word(&mut pool, 98));
    for (ty, want) in [(generic, 1), (t, 1), (int, 0), (flag, 0)] {
        let params = vec![bind(0, ty), bind(1, int)];
        let f = perceus(&pool, func(params, ret(1), int));
        assert_eq!(count_drops(&f.body), want, "{f}");
        // Whatever gives it.
        for rhs in [call(&[]), Atom::Load(Load::Capture(CaptureIdx(0)))] {
            let body = bound(bind(0, ty), rhs, ret(1));
            let f = perceus(&pool, func(vec![bind(1, int)], body, int));
            assert_eq!(count_drops(&f.body), want, "{f}");
        }
    }
}

/// Ownership evens out across jumps. `%0` is live into the continuation and `%2`
/// isn't. Every owned local is released exactly once on every path, before the
/// jump or after it, never both.
#[test]
fn jumps_equalise_ownership() {
    let mut pool = pool();
    let t = con(&mut pool, 99);
    let int = int_ty(&mut pool);
    let params = vec![bind(0, t), bind(1, int), bind(2, t)];
    let body = join(0, Vec::new(), iff(1, jump(0, &[]), ret(2)), ret(0));
    assert_eq!(
        lines(&pool, func(params, body, t)),
        [
            "join j0() after",
            "  if %1",
            "    drop %2",
            "    jump j0()",
            "  else",
            "    drop %0",
            "    ret %2",
            "ret %0",
        ]
    );
}

/// A local whose last use is inside the continuation drops there, once, and
/// nothing drops it before a jump.
#[test]
fn a_local_the_continuation_reads_is_dropped_there() {
    let mut pool = pool();
    let t = con(&mut pool, 99);
    let int = int_ty(&mut pool);
    let cont = bound(bind(2, int), call(&[0]), ret(2));
    let params = vec![bind(0, t), bind(1, int)];
    let body = join(0, Vec::new(), iff(1, jump(0, &[]), jump(0, &[])), cont);
    let got = lines(&pool, func(params, body, int));
    assert_eq!(got[5..], ["let %2 = call fn#0(%0)", "drop %0", "ret %2"]);
    assert!(!got[..5].iter().any(|l| l.contains("drop")), "{got:?}");
}

/// Inside the body of a join that gives a value, what dies there is dropped
/// there: what a branch made, and what came from outside and only one branch
/// reads.
#[test]
fn the_body_of_a_join_drops_what_dies_in_it() {
    let mut pool = pool();
    let t = con(&mut pool, 99);
    let int = int_ty(&mut pool);
    let params = vec![bind(0, t), bind(1, int), bind(2, t)];
    let made = bound(bind(4, int), call(&[3, 0]), jump(0, &[4]));
    let made = bound(bind(3, t), call(&[0]), made);
    let body = iff(1, made, jump(0, &[1]));
    let body = join(
        0,
        vec![bind(5, int)],
        body,
        bound(bind(6, int), call(&[2]), ret(5)),
    );
    assert_eq!(
        lines(&pool, func(params, body, int)),
        [
            "join j0(%5) after",
            "  if %1",
            "    let %3 = call fn#0(%0)",
            "    let %4 = call fn#0(%3, %0)",
            "    drop %0",
            "    drop %3",
            "    jump j0(%4)",
            "  else",
            "    drop %0",
            "    jump j0(%1)",
            "let %6 = call fn#0(%2)",
            "drop %2",
            "ret %5",
        ]
    );
}

/// What a jump passes is handed over and not dropped. When the continuation
/// reads it under its own name too, it is dropped there as well as its
/// parameter is. A parameter that nothing reads is dropped where the
/// continuation starts.
#[test]
fn a_jump_hands_over_its_arguments() {
    let mut pool = pool();
    let t = con(&mut pool, 99);
    let int = int_ty(&mut pool);
    let params = vec![bind(0, t), bind(1, int), bind(2, t)];
    let ends = bound(bind(7, int), call(&[4, 0]), ret(7));
    let body = iff(1, jump(0, &[0, 2, 2]), jump(0, &[2, 0, 2]));
    let joined = vec![bind(4, t), bind(5, t), bind(6, t)];
    let f = func(params, join(0, joined, body, ends), int);
    assert_eq!(
        lines(&pool, f),
        [
            "join j0(%4, %5, %6) after",
            "  if %1",
            "    jump j0(%0, %2, %2)",
            "  else",
            "    jump j0(%2, %0, %2)",
            "drop %5",
            "drop %6",
            "let %7 = call fn#0(%4, %0)",
            "drop %0",
            "drop %4",
            "ret %7",
        ]
    );
}

/// A jump out of a switch, past a join that is around it, drops what only that
/// join's continuation would have read.
#[test]
fn a_jump_past_a_join_drops_what_its_continuation_owned() {
    let mut pool = pool();
    let t = con(&mut pool, 99);
    let int = int_ty(&mut pool);
    let params = vec![bind(0, t), bind(1, int), bind(2, t), bind(3, t)];
    let first = field(bind(5, t), 0, 0, jump(1, &[]));
    let inner = switch(0, vec![(vec![0], first)], Some(jump(0, &[1])));
    let inner = join(
        1,
        Vec::new(),
        inner,
        bound(bind(6, int), call(&[2]), jump(0, &[6])),
    );
    let body = join(
        0,
        vec![bind(7, int)],
        inner,
        bound(bind(8, int), call(&[3]), ret(8)),
    );
    assert_eq!(
        lines(&pool, func(params, body, int)),
        [
            "join j0(%7) after",
            "  join j1() after",
            "    switch %0",
            "      | 0.0 ->",
            "        drop %0",
            "        jump j1()",
            "      | _ ->",
            "        drop %0",
            "        drop %2",
            "        jump j0(%1)",
            "  let %6 = call fn#0(%2)",
            "  drop %2",
            "  jump j0(%6)",
            "let %8 = call fn#0(%3)",
            "drop %3",
            "ret %8",
        ]
    );
}

/// A switch inside a join's body keeps its scrutinee when something after the
/// join reads it.
#[test]
fn a_scrutinee_that_is_read_after_the_join_is_kept() {
    let mut pool = pool();
    let t = con(&mut pool, 99);
    let int = int_ty(&mut pool);
    let first = field(bind(1, t), 0, 0, jump(0, &[1]));
    let matched = switch(
        0,
        vec![(vec![0], first), (vec![1, 2, 3], jump(0, &[0]))],
        None,
    );
    let cont = bound(bind(3, int), call(&[0, 2]), ret(3));
    let body = join(0, vec![bind(2, t)], matched, cont);
    let got = lines(&pool, func(vec![bind(0, t)], body, int));
    let dropped: Vec<&str> = got
        .iter()
        .map(|l| l.trim())
        .filter(|l| l.contains("drop"))
        .collect();
    assert_eq!(dropped, ["drop %0", "drop %2"], "{got:?}");
    assert_eq!(got[got.len() - 2], "drop %2");
}

/// A jump to a join that is not around it is refused, from a continuation and
/// after one.
#[test]
fn a_jump_out_of_scope_is_refused() {
    let mut pool = pool();
    let int = int_ty(&mut pool);
    for body in [
        jump(3, &[]),
        join(3, Vec::new(), ret(0), jump(3, &[])),
        join(3, Vec::new(), jump(3, &[]), jump(3, &[])),
        join(
            4,
            Vec::new(),
            join(3, Vec::new(), jump(4, &[]), ret(0)),
            jump(3, &[]),
        ),
        join(
            4,
            Vec::new(),
            jump(3, &[]),
            join(3, Vec::new(), jump(3, &[]), ret(0)),
        ),
    ] {
        let f = func(vec![bind(0, int)], body, int);
        let got = run(&pool, &types_of(&f), f);
        assert_eq!(got.unwrap_err(), InternalLinkError::UnboundJoin(JoinId(3)));
    }
}

/// A local that nothing typed is refused.
#[test]
fn an_untyped_local_is_refused() {
    let mut pool = pool();
    let int = int_ty(&mut pool);
    let f = func(vec![bind(0, int)], ret(0), int);
    let got = run(&pool, &TiVec::new(), f.clone());
    assert!(got.is_ok(), "nothing dies");
    let f = func(vec![bind(0, int), bind(1, int)], ret(0), int);
    let got = run(&pool, &TiVec::new(), f);
    assert_eq!(got.unwrap_err(), InternalLinkError::UntypedLocal(local(1)));
}

/// Makes a function at random.
struct Maker {
    x: u64,
    next: u32,
    joins: u32,
    /// `Int`, two that get drops, one that is never a cell, and a variable.
    types: [RTy; 5],
    /// The type of each local.
    of: BTreeMap<u32, RTy>,
    /// What each join takes.
    takes: BTreeMap<u32, Vec<RTy>>,
    /// What is known to be a constructor with fields where it is, and which.
    known: Vec<(u32, u16)>,
    /// Which constructors a value can be where it is, the innermost switch last.
    can_be: Vec<(u32, Vec<u16>)>,
}

impl Maker {
    fn below(&mut self, n: usize) -> usize {
        self.x ^= self.x << 13;
        self.x ^= self.x >> 7;
        self.x ^= self.x << 17;
        usize::try_from(self.x >> 33).unwrap() % n
    }

    fn ty(&mut self) -> RTy {
        let at = self.below(7).min(4);
        self.types[at]
    }

    fn bind(&mut self, ty: RTy) -> CoreBind {
        self.next += 1;
        self.of.insert(self.next, ty);
        bind(self.next, ty)
    }

    fn any(&mut self, scope: &[u32]) -> u32 {
        scope[self.below(scope.len())]
    }

    fn atom(&mut self, scope: &[u32]) -> Atom {
        let (a, b) = (self.any(scope), self.any(scope));
        if !self.known.is_empty() && self.below(2) == 0 {
            let at = self.below(self.known.len());
            let (cell, variant) = self.known[at];
            return self.part(cell, variant);
        }
        match self.below(7) {
            0 => Atom::Local(local(a)),
            1 => ctor(&[a, b]),
            2 => Atom::Load(Load::Capture(CaptureIdx(0))),
            3 => Atom::Nil,
            4 => Atom::Update {
                base: local(a),
                variant: variant(),
                sets: vec![(1, local(b))],
            },
            _ => call(&[a, b]),
        }
    }

    /// A read of a field of `cell`, which is this constructor.
    fn part(&mut self, cell: u32, variant: u16) -> Atom {
        let i = self.below(FIELDS[usize::from(variant)]);
        Atom::prim(PrimOp::Field(u16::try_from(i).unwrap()), vec![local(cell)])
    }

    /// A case for `variants` of `scrut`. One for a single constructor with fields
    /// begins by reading some, and one for what `scrut` cannot be stops.
    fn case(
        &mut self,
        size: usize,
        scrut: u32,
        variants: &[u16],
        scope: &[u32],
        joins: &[u32],
    ) -> CoreExpr {
        let before = self.can_be.iter().rev().find(|(l, _)| *l == scrut);
        let can_be: Vec<u16> = match before {
            Some((_, all)) => variants
                .iter()
                .copied()
                .filter(|v| all.contains(v))
                .collect(),
            None => variants.to_vec(),
        };
        if can_be.is_empty() {
            return CoreExpr::NoArm;
        }
        self.can_be.push((scrut, can_be));
        let body = self.possible_case(size, scrut, variants, scope, joins);
        self.can_be.pop();
        body
    }

    fn possible_case(
        &mut self,
        size: usize,
        scrut: u32,
        variants: &[u16],
        scope: &[u32],
        joins: &[u32],
    ) -> CoreExpr {
        let [v] = variants else {
            return self.expr(size, scope, joins);
        };
        if FIELDS[usize::from(*v)] == 0 {
            return self.expr(size, scope, joins);
        }
        self.known.push((scrut, *v));
        let mut scope = scope.to_vec();
        let mut reads = Vec::new();
        for _ in 0..self.below(4) {
            let ty = self.ty();
            let part = match self.below(3) {
                0 => {
                    let at = self.below(self.known.len());
                    let (cell, variant) = self.known[at];
                    self.part(cell, variant)
                }
                _ => self.part(scrut, *v),
            };
            let new = self.bind(ty);
            scope.push(new.id.0);
            reads.push((new, part));
        }
        let rest = self.expr(size, &scope, joins);
        self.known.pop();
        let reads = reads.into_iter().rev();
        reads.fold(rest, |rest, (bind, rhs)| bound(bind, rhs, rest))
    }

    /// A local of `scope` with type `ty`, or a new one that `lets` gets the `let`
    /// of.
    fn of_type(&mut self, ty: RTy, scope: &[u32], lets: &mut Vec<(CoreBind, Atom)>) -> u32 {
        let fit: Vec<u32> = scope.iter().copied().filter(|l| self.of[l] == ty).collect();
        if !fit.is_empty() && self.below(4) != 0 {
            return self.any(&fit);
        }
        let new = self.bind(ty);
        let rhs = self.atom(scope);
        lets.push((new, rhs));
        new.id.0
    }

    /// A return, a tail call or a jump to one of `joins`.
    fn end(&mut self, scope: &[u32], joins: &[u32]) -> CoreExpr {
        let mut lets = Vec::new();
        let end = if !joins.is_empty() && self.below(4) != 0 {
            let to = self.any(joins);
            let takes = self.takes[&to].clone();
            let args: Vec<u32> = takes
                .iter()
                .map(|ty| self.of_type(*ty, scope, &mut lets))
                .collect();
            jump(to, &args)
        } else if self.below(3) == 0 {
            let (a, b, f) = (self.any(scope), self.any(scope), self.any(scope));
            let callee = match self.below(3) {
                0 => Callee::Self_,
                1 => Callee::Known(FuncIdx(1)),
                _ => Callee::Local(local(f)),
            };
            tail_call(callee, &[a, b])
        } else {
            let int = self.types[0];
            ret(self.of_type(int, scope, &mut lets))
        };
        let lets = lets.into_iter().rev();
        lets.fold(end, |rest, (bind, rhs)| bound(bind, rhs, rest))
    }

    /// An expression of about `size` nodes, in which `joins` may be jumped to.
    fn expr(&mut self, size: usize, scope: &[u32], joins: &[u32]) -> CoreExpr {
        let half = size / 2;
        let with = |more: &[CoreBind]| -> Vec<u32> {
            let more = more.iter().map(|b| b.id.0);
            scope.iter().copied().chain(more).collect()
        };
        if size == 0 {
            return self.end(scope, joins);
        }
        match self.below(12) {
            0 | 1 => {
                self.joins += 1;
                let id = self.joins;
                let params: Vec<CoreBind> = (0..self.below(4))
                    .map(|_| {
                        let ty = self.ty();
                        self.bind(ty)
                    })
                    .collect();
                let takes = params.iter().map(|p| p.ty).collect();
                self.takes.insert(id, takes);
                let cont = self.expr(half, &with(&params), joins);
                // Often the one it is for is all that its body may jump to.
                let inside = match self.below(2) {
                    0 => vec![id],
                    _ => [joins, &[id]].concat(),
                };
                let body = self.expr(half, scope, &inside);
                join(id, params, body, cont)
            }
            2 => {
                let then = self.expr(half, scope, joins);
                // A Bool, which nothing has to drop.
                iff(0, then, self.expr(half, scope, joins))
            }
            3 | 4 => {
                // Only a type whose values may be cells has a constructor with fields.
                let heap = self.types[1];
                let cells: Vec<u32> = scope
                    .iter()
                    .copied()
                    .filter(|l| self.of[l] == heap)
                    .collect();
                if cells.is_empty() {
                    return self.end(scope, joins);
                }
                let scrut = self.any(&cells);
                let mut left: Vec<u16> = vec![0, 1, 2, 3];
                let mut arms = Vec::new();
                while !left.is_empty() && self.below(4) != 0 {
                    let mut of = vec![left.swap_remove(self.below(left.len()))];
                    if !left.is_empty() && self.below(4) == 0 {
                        of.push(left.swap_remove(self.below(left.len())));
                    }
                    let body = self.case(half / 2, scrut, &of, scope, joins);
                    arms.push((of, body));
                }
                let default = match left.is_empty() {
                    true => None,
                    false if self.below(8) == 0 => Some(CoreExpr::NoArm),
                    false => Some(self.case(half / 2, scrut, &left, scope, joins)),
                };
                switch(scrut, arms, default)
            }
            5 | 6 => {
                let ty = self.ty();
                let found = self.bind(ty);
                let (op, n) = [
                    (ProbeOp::ArrayGet, 2),
                    (ProbeOp::ArraySlice, 3),
                    (ProbeOp::ArraySet, 3),
                    (ProbeOp::MapGet, 2),
                ][self.below(4)];
                let args: Vec<u32> = (0..n).map(|_| self.any(scope)).collect();
                let hit = self.expr(half, &with(&[found]), joins);
                probe(op, &args, Some(found), hit, self.expr(half, scope, joins))
            }
            _ => {
                let ty = self.ty();
                let new = self.bind(ty);
                let rhs = self.atom(scope);
                let rest = self.expr(size - 1, &with(&[new]), joins);
                bound(new, rhs, rest)
            }
        }
    }
}

/// Some functions made at random, with the pool their types are in.
fn functions(mut each: impl FnMut(u64, &ResolvedPool, &CoreFn)) {
    let mut p = pool();
    let (heap, int, flag) = (con(&mut p, 7), int_ty(&mut p), word(&mut p, 8));
    let any = p.mk_bound(0).unwrap();
    for seed in 1..=30_000u64 {
        let mut of = Maker {
            x: seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1,
            next: 0,
            joins: 0,
            types: [int, heap, heap, flag, any],
            of: BTreeMap::from([(0, int)]),
            takes: BTreeMap::new(),
            known: Vec::new(),
            can_be: Vec::new(),
        };
        of.below(9);
        let mut params = vec![bind(0, int)];
        for _ in 0..of.below(3) {
            let ty = of.ty();
            params.push(of.bind(ty));
        }
        let scope: Vec<u32> = params.iter().map(|b| b.id.0).collect();
        let size = of.below(26);
        let body = of.expr(size, &scope, &[]);
        each(seed, &p, &func(params, body, int));
    }
}

/// Whether each drop in `e` is of something that could not have been dropped
/// before: what the line in front of it reads or makes, or, where a branch
/// starts, something that another branch needs, or what the branch is on.
/// `starts` is what may die where `e` starts, when that is known. Nothing in `e` reads a
/// field that nothing reads.
fn drops_are_early(mut e: &CoreExpr, starts: Option<Live>) -> bool {
    let mut may = starts;
    loop {
        match e {
            CoreExpr::Drop { local, body } => {
                let late = may.as_ref().is_some_and(|may| !may.contains(local));
                if late {
                    return false;
                }
                e = body;
            }
            CoreExpr::Let { bind, rhs, body } => {
                let mut here = Vec::new();
                operands(rhs, &mut here);
                here.push(bind.id);
                may = Some(here.into_iter().collect());
                e = body;
            }
            CoreExpr::SetGlobal { value, body, .. } => {
                may = Some(Live::from([*value]));
                e = body;
            }
            CoreExpr::Join {
                params, cont, body, ..
            } => {
                if !drops_are_early(body, Some(Live::new())) {
                    return false;
                }
                may = Some(params.iter().map(|p| p.id).collect());
                e = cont;
            }
            CoreExpr::If { cond, then, els } => {
                let with = |other: &CoreExpr| {
                    let mut may = needed(other)?;
                    may.insert(*cond);
                    Some(may)
                };
                return drops_are_early(then, with(els)) && drops_are_early(els, with(then));
            }
            // Nothing that a hit finds is dropped at once: it is put nowhere instead.
            CoreExpr::Probe {
                args, hit, miss, ..
            } => {
                let with = |other: &CoreExpr| {
                    let mut may = needed(other)?;
                    may.extend(args);
                    Some(may)
                };
                return drops_are_early(hit, with(miss)) && drops_are_early(miss, with(hit));
            }
            CoreExpr::Open { fields, body, .. } => {
                may = Some(fields.iter().map(|(_, b)| b.id).collect());
                e = body;
            }
            CoreExpr::Switch {
                scrut,
                cases,
                default,
            } => {
                let arms: Vec<_> = cases.arms().into_iter().chain(default.as_deref()).collect();
                return (0..arms.len()).all(|i| {
                    let mut others = arms.iter().enumerate().filter(|(j, _)| *j != i);
                    let may = others.try_fold(Live::from([*scrut]), |mut may, (_, other)| {
                        may.extend(needed(other)?);
                        Some(may)
                    });
                    drops_are_early(arms[i], may)
                });
            }
            CoreExpr::Ret(_)
            | CoreExpr::TailCall { .. }
            | CoreExpr::Jump { .. }
            | CoreExpr::NoArm => return true,
        }
    }
}

/// `f` without the reads of fields that nothing reads, and without what never runs. Such a
/// read goes when its cell dies there, and then the cell's drop is after what was in front
/// of it.
fn without_unread(f: &CoreFn) -> CoreFn {
    let mut f = f.clone();
    while take_unread(&mut f.body) {}
    f
}

/// Whether there were any in `e`, which are gone now.
fn take_unread(e: &mut CoreExpr) -> bool {
    let (read, never) = all_read(e);
    let mut any = false;
    let mut work = vec![e];
    while let Some(e) = work.pop() {
        while let CoreExpr::Let { bind, rhs, body } = e
            && is_field(rhs)
            && !read.contains(&bind.id)
        {
            *e = std::mem::replace(body, CoreExpr::NoArm);
            any = true;
        }
        match e {
            CoreExpr::Let { body, .. }
            | CoreExpr::SetGlobal { body, .. }
            | CoreExpr::Drop { body, .. }
            | CoreExpr::Open { body, .. } => work.push(body),
            CoreExpr::Join { id, cont, body, .. } => {
                if never.contains(id) {
                    **cont = CoreExpr::NoArm;
                }
                work.extend([&mut **cont, &mut **body]);
            }
            CoreExpr::If {
                then: a, els: b, ..
            }
            | CoreExpr::Probe {
                hit: a, miss: b, ..
            } => work.extend([&mut **a, &mut **b]),
            CoreExpr::Switch { cases, default, .. } => {
                work.extend(cases.arms_mut());
                work.extend(default.as_deref_mut());
            }
            CoreExpr::Ret(_)
            | CoreExpr::TailCall { .. }
            | CoreExpr::Jump { .. }
            | CoreExpr::NoArm => {}
        }
    }
    any
}

fn is_field(rhs: &Atom) -> bool {
    matches!(
        rhs,
        Atom::PrimOp {
            op: PrimOp::Field(_),
            ..
        }
    )
}

/// Every local that something in `e` reads, and the joins that nothing jumps to, whose
/// continuations do not count.
fn all_read(e: &CoreExpr) -> (Live, BTreeSet<JoinId>) {
    let mut read = Live::new();
    let mut conts = BTreeMap::new();
    let mut work = vec![e];
    while let Some(e) = work.pop() {
        match e {
            CoreExpr::Let { rhs, body, .. } => {
                rhs.for_each_operand(|l| {
                    read.insert(l);
                });
                work.push(body);
            }
            CoreExpr::SetGlobal { value, body, .. } => {
                read.insert(*value);
                work.push(body);
            }
            CoreExpr::Drop { body, .. } | CoreExpr::Open { body, .. } => work.push(body),
            CoreExpr::Join { id, cont, body, .. } => {
                conts.insert(*id, &**cont);
                work.push(body);
            }
            CoreExpr::If { cond, then, els } => {
                read.insert(*cond);
                work.extend([&**then, &**els]);
            }
            CoreExpr::Probe {
                args, hit, miss, ..
            } => {
                read.extend(args);
                work.extend([&**hit, &**miss]);
            }
            CoreExpr::Switch {
                scrut,
                cases,
                default,
            } => {
                read.insert(*scrut);
                work.extend(cases.arms());
                work.extend(default.as_deref());
            }
            CoreExpr::Ret(value) => {
                read.insert(*value);
            }
            CoreExpr::TailCall { callee, args } => {
                read.extend(callee.local());
                read.extend(args);
            }
            CoreExpr::Jump { to, args } => {
                read.extend(args);
                work.extend(conts.remove(to));
            }
            CoreExpr::NoArm => {}
        }
    }
    (read, conts.into_keys().collect())
}

/// What [`runs`] finds of the reads of fields.
#[derive(Default)]
struct Runs {
    /// The run that each local is read in.
    of: BTreeMap<LocalId, usize>,
    /// The run that is straight in front of the `let` of a local, and straight behind.
    in_front: BTreeMap<LocalId, usize>,
    behind: BTreeMap<LocalId, usize>,
    count: usize,
}

/// The runs of reads of fields in `e`, with nothing between two of one run.
fn runs(e: &CoreExpr) -> Runs {
    let mut out = Runs::default();
    let mut work = vec![e];
    while let Some(mut e) = work.pop() {
        // The run that is going on, and the last `let` that was no read.
        let (mut run, mut last) = (None, None);
        while let CoreExpr::Let { bind, rhs, body } = e {
            if is_field(rhs) {
                let id = *run.get_or_insert(out.count);
                out.count = id + 1;
                out.of.insert(bind.id, id);
                last.take().map(|l| out.behind.insert(l, id));
            } else {
                run.take().map(|id| out.in_front.insert(bind.id, id));
                last = Some(bind.id);
            }
            e = body;
        }
        match e {
            CoreExpr::SetGlobal { body, .. }
            | CoreExpr::Drop { body, .. }
            | CoreExpr::Open { body, .. }
            | CoreExpr::Let { body, .. } => work.push(body),
            CoreExpr::Join { cont, body, .. } => work.extend([&**cont, &**body]),
            CoreExpr::If {
                then: a, els: b, ..
            }
            | CoreExpr::Probe {
                hit: a, miss: b, ..
            } => work.extend([&**a, &**b]),
            CoreExpr::Switch { cases, default, .. } => {
                work.extend(cases.arms());
                work.extend(default.as_deref());
            }
            CoreExpr::Ret(_)
            | CoreExpr::TailCall { .. }
            | CoreExpr::Jump { .. }
            | CoreExpr::NoArm => {}
        }
    }
    out
}

/// Whether each `open` in `e` is of fields that one of `runs` read, and stands where
/// that run stood.
fn opens_stand_where_they_read(e: &CoreExpr, runs: &Runs) -> bool {
    let mut work = vec![e];
    // The runs that are over, and those that have not begun, as far as a walk that comes
    // to the parts of `e` in any order can say. A run is on one spine.
    let (mut over, mut begun) = (BTreeSet::<usize>::new(), BTreeSet::<usize>::new());
    while let Some(e) = work.pop() {
        match e {
            CoreExpr::Open { fields, body, .. } => {
                let of: BTreeSet<_> = fields.iter().map(|(_, b)| runs.of.get(&b.id)).collect();
                let [Some(run)] = of.into_iter().collect::<Vec<_>>()[..] else {
                    return false;
                };
                if over.contains(run) {
                    return false;
                }
                begun.insert(*run);
                work.push(body);
            }
            CoreExpr::Let { bind, body, .. } => {
                if let Some(run) = runs.behind.get(&bind.id)
                    && begun.contains(run)
                {
                    return false;
                }
                over.extend(runs.in_front.get(&bind.id));
                work.push(body);
            }
            CoreExpr::SetGlobal { body, .. } | CoreExpr::Drop { body, .. } => work.push(body),
            CoreExpr::Join { cont, body, .. } => work.extend([&**cont, &**body]),
            CoreExpr::If {
                then: a, els: b, ..
            }
            | CoreExpr::Probe {
                hit: a, miss: b, ..
            } => work.extend([&**a, &**b]),
            CoreExpr::Switch { cases, default, .. } => {
                work.extend(cases.arms());
                work.extend(default.as_deref());
            }
            CoreExpr::Ret(_)
            | CoreExpr::TailCall { .. }
            | CoreExpr::Jump { .. }
            | CoreExpr::NoArm => {}
        }
    }
    true
}

/// What `e` reads or drops, unless it jumps out: what the continuation needs is
/// not known here.
fn needed(e: &CoreExpr) -> Option<Live> {
    let free = ownership::free(e);
    let inside = free.jumps.is_empty();
    inside.then_some(free.named)
}

/// Whatever the function, every way through it gives up each value once and
/// reads nothing it gave up, and each drop is as early as it can be.
#[test]
fn every_way_gives_up_each_value_once() {
    let mut seen = [0; 8];
    functions(|seed, p, f| {
        let got = perceus(p, f.clone());
        let checked = ownership::check(p, &fields, &got, ownership::Body::Function);
        assert_eq!(checked, Ok(()), "{seed}:\n{got}");
        let plain = without_unread(f);
        let plain_got = perceus(p, plain.clone());
        let early = drops_are_early(&plain_got.body, Some(all_params(f)));
        assert!(early, "{seed}:\n{plain_got}");
        let in_place = opens_stand_where_they_read(&plain_got.body, &runs(&plain.body));
        assert!(in_place, "{seed}:\n{plain}\n{plain_got}");
        let text = got.body.to_string();
        seen[0] += usize::from(text.contains("join"));
        seen[1] += usize::from(text.contains("drop"));
        seen[2] += usize::from(text.contains("tail call"));
        seen[3] += usize::from(!f.body.always_reached().is_empty());
        seen[4] += text.matches("open").count();
        seen[5] += usize::from(text.contains("switch"));
        seen[6] += usize::from(text.contains("hit(%"));
        seen[7] += usize::from(text.contains("hit(_)"));
    });
    assert!(seen.iter().all(|n| *n > 5000), "{seen:?}");
}

fn all_params(f: &CoreFn) -> Live {
    f.params.iter().map(|p| p.id).collect()
}

/// The pass is where drops come from, so a function that has one already is one
/// that something else was wrong with.
#[test]
fn a_function_with_a_drop_is_refused() {
    let mut p = pool();
    let t = con(&mut p, 7);
    let drop = |local, body| CoreExpr::Drop {
        local: LocalId(local),
        body: Box::new(body),
    };
    for body in [
        drop(0, ret(1)),
        bound(bind(2, t), call(&[1]), drop(0, ret(2))),
        iff(1, ret(1), drop(0, ret(1))),
        join(0, vec![bind(2, t)], drop(0, jump(0, &[1])), ret(2)),
    ] {
        let f = func(vec![bind(0, t), bind(1, t)], body, t);
        let types = types_of(&f);
        let got = run(&p, &types, f);
        assert_eq!(got.unwrap_err(), InternalLinkError::DropsGiven(LocalId(0)));
    }
}

/// That a join is always reached is only a shorter way to say what is live in its
/// body: without it every drop is the same.
#[test]
fn what_is_live_around_a_join_is_found_either_way() {
    let mut reached = 0;
    functions(|seed, p, f| {
        // On a way that stops the run, what is dropped does not matter.
        if f.body.to_string().contains("no arm") {
            return;
        }
        let types = types_of(f);
        let shapes = shapes();
        let mut cx = Perceus {
            pool: p,
            shapes: &shapes,
            locals: &types,
            reached: &BTreeSet::new(),
            entered: BTreeMap::new(),
            around: Vec::new(),
            plain: BTreeSet::new(),
            known: BTreeMap::new(),
        };
        let (body, live) = cx.drop_pass(f.body.clone()).expect("it is well made");
        let dead = all_params(f);
        let body = cx
            .wrap_drops(without(&dead, &live), body)
            .expect("it is typed");
        assert_eq!(
            body.to_string(),
            perceus(p, f.clone()).body.to_string(),
            "{seed}"
        );
        reached += f.body.always_reached().len();
    });
    assert!(reached > 5000, "{reached}");
}

/// The drops are those there would be if every value could be a cell, less those
/// of what can be none.
#[test]
fn what_can_be_no_cell_gets_no_drop() {
    let mut fewer = 0;
    functions(|seed, p, f| {
        let got = perceus(p, f.clone());
        let mut q = pool();
        let (_, _, _) = (con(&mut q, 7), int_ty(&mut q), con(&mut q, 8));
        q.mk_bound(0).unwrap();
        let every = perceus(&q, f.clone());
        let words = |l: &LocalId| p.repr(*types_of(f).get(*l).unwrap()).unwrap() == Repr::Word;
        let mut want = drops(&every.body);
        let all = want.len();
        want.retain(|l| !words(l));
        assert_eq!(drops(&got.body), want, "{seed}:\n{got}\n{every}");
        fewer += usize::from(want.len() < all);
    });
    assert!(fewer > 5000, "{fewer}");
}

/// A probe takes nothing, so what it is the last to read is dropped where a hit goes on
/// and where a miss does. So is what only the other of the two reads, and what a hit
/// finds and nothing reads is put nowhere.
#[test]
fn a_probe_leaves_each_way_to_drop_its_own() {
    let mut p = pool();
    let (t, int) = (con(&mut p, 1), int_ty(&mut p));
    let set = |found, hit, miss| {
        let body = probe(ProbeOp::ArraySet, &[0, 1, 2], Some(found), hit, miss);
        let params = vec![bind(0, t), bind(1, int), bind(2, t)];
        lines(&p, func(params, body, t))
    };
    // The array is what a miss gives.
    assert_eq!(
        set(bind(4, t), ret(4), ret(0)),
        [
            "probe ArraySet(%0, %1, %2)",
            "  | hit(%4) ->",
            "    drop %0",
            "    drop %2",
            "    ret %4",
            "  | miss ->",
            "    drop %2",
            "    ret %0",
        ]
    );
    // A hit reads the array again, a miss the value, and nothing what was found.
    let again = || bound(bind(5, t), call(&[0]), ret(5));
    assert_eq!(
        set(bind(4, t), again(), ret(2)),
        [
            "probe ArraySet(%0, %1, %2)",
            "  | hit(_) ->",
            "    drop %2",
            "    let %5 = call fn#0(%0)",
            "    drop %0",
            "    ret %5",
            "  | miss ->",
            "    drop %0",
            "    ret %2",
        ]
    );
    // What is found is dropped where it dies, as anything is.
    assert_eq!(
        set(bind(4, t), iff(1, ret(4), ret(0)), CoreExpr::NoArm),
        [
            "probe ArraySet(%0, %1, %2)",
            "  | hit(%4) ->",
            "    drop %2",
            "    if %1",
            "      drop %0",
            "      ret %4",
            "    else",
            "      drop %4",
            "      ret %0",
            "  | miss ->",
            "    no arm",
        ]
    );
    // One local may be every operand, and one that is read after the join that the
    // probe is in is dropped by neither way.
    let body = probe(
        ProbeOp::ArraySlice,
        &[0, 0, 0],
        Some(bind(4, t)),
        jump(0, &[4]),
        jump(0, &[0]),
    );
    let after = bound(bind(6, t), call(&[3, 5]), ret(6));
    let f = func(
        vec![bind(0, t), bind(3, t)],
        join(0, vec![bind(5, t)], body, after),
        t,
    );
    assert_eq!(
        lines(&p, f),
        [
            "join j0(%5) after",
            "  probe ArraySlice(%0, %0, %0)",
            "    | hit(%4) ->",
            "      drop %0",
            "      jump j0(%4)",
            "    | miss ->",
            "      jump j0(%0)",
            "let %6 = call fn#0(%3, %5)",
            "drop %3",
            "drop %5",
            "ret %6",
        ]
    );
}

/// The checker of finished IR finds a probe that gives up too little or too much, on
/// either way, and one that is made of the wrong things.
#[test]
fn a_probe_that_is_wrong_is_found() {
    let mut p = pool();
    let (t, int) = (con(&mut p, 1), int_ty(&mut p));
    let drop = |local, body| CoreExpr::Drop {
        local: LocalId(local),
        body: Box::new(body),
    };
    let is_wrong = |body: CoreExpr| {
        let f = func(vec![bind(0, t), bind(1, int)], body, t);
        ownership::check(&p, &fields, &f, ownership::Body::Function).is_err()
    };
    let get = |args: &[u32], found: Option<CoreBind>, hit, miss| {
        probe(ProbeOp::ArrayGet, args, found, hit, miss)
    };
    let found = || Some(bind(2, t));
    let right = || drop(0, ret(2));
    assert!(!is_wrong(get(&[0, 1], found(), right(), ret(0))));
    assert!(!is_wrong(get(&[0, 1], None, ret(0), ret(0))));
    // The array, and what was found, left behind.
    assert!(is_wrong(get(&[0, 1], found(), ret(2), ret(0))));
    assert!(is_wrong(get(&[0, 1], found(), ret(0), ret(0))));
    // What was found, read where nothing was.
    assert!(is_wrong(get(&[0, 1], found(), right(), ret(2))));
    // The array given up on a miss and read after.
    assert!(is_wrong(get(&[0, 1], found(), right(), drop(0, ret(0)))));
    // An operand that is not there, or was given up.
    assert!(is_wrong(get(&[0, 7], found(), right(), ret(0))));
    assert!(is_wrong(drop(0, get(&[0, 1], found(), ret(2), ret(2)))));
    // Too few operands, and no place for what it finds.
    assert!(is_wrong(get(&[0], found(), right(), ret(0))));
    let mut none = get(&[0, 1], None, ret(0), ret(0));
    if let CoreExpr::Probe { binds, .. } = &mut none {
        binds.clear();
    }
    assert!(is_wrong(none));
}

/// An update reads its base and its values, so each that is read last there is
/// dropped behind it, and the VM then takes it.
#[test]
fn an_update_drops_what_it_is_the_last_to_read() {
    let mut p = pool();
    let t = con(&mut p, 1);
    let update = Atom::Update {
        base: local(0),
        variant: variant(),
        sets: vec![(0, local(1)), (2, local(2))],
    };
    let body = bound(bind(3, t), update, bound(bind(4, t), ctor(&[2, 3]), ret(4)));
    let params = (0..3).map(|i| bind(i, t)).collect();
    assert_eq!(
        lines(&p, func(params, body, t)),
        [
            "let %3 = update 0.0(..%0, 0: %1, 2: %2)",
            "drop %0",
            "drop %1",
            "let %4 = ctor 0.0(%2, %3)",
            "drop %2",
            "drop %3",
            "ret %4",
        ]
    );
}

/// A branch for values with no fields drops nothing of what it matched, and past
/// a join it may be a cell again.
#[test]
fn a_plain_scrutinee_is_dropped_only_past_a_join() {
    let mut p = pool();
    let t = con(&mut p, 1);
    let int = int_ty(&mut p);
    let body = switch(0, vec![(vec![1], jump(0, &[]))], Some(jump(0, &[])));
    let cont = bound(bind(2, int), call(&[0]), ret(2));
    let got = lines(
        &p,
        func(vec![bind(0, t)], join(0, Vec::new(), body, cont), int),
    );
    assert_eq!(got[got.len() - 2], "drop %0");
    let plain = switch(0, vec![(vec![1], ret(1))], Some(ret(1)));
    let got = lines(&p, func(vec![bind(0, t), bind(1, int)], plain, int));
    assert_eq!(
        got.iter().filter(|l| l.contains("drop %0")).count(),
        1,
        "{got:?}"
    );
}

/// `n` joins one after another, with `values` made in front of them and read at
/// the end, and one in each branch.
fn chain(n: u32, values: u32, t: RTy, int: RTy) -> CoreFn {
    let held: Vec<u32> = (1..=values).collect();
    let mut rest = bound(bind(0, t), call(&held), ret(1_000_000));
    for i in 0..n {
        let at = 2_000_000 + 3 * i;
        let made = bound(bind(at, t), call(&[]), jump(i, &[at]));
        let body = iff(
            1_000_000,
            made,
            bound(bind(at + 1, t), call(&[]), jump(i, &[at + 1])),
        );
        rest = join(i, vec![bind(at + 2, t)], body, rest);
    }
    for v in held {
        rest = bound(bind(v, t), call(&[]), rest);
    }
    func(vec![bind(1_000_000, int)], rest, int)
}

/// A long chain of joins takes no stack for its length and a time that grows with
/// it, however much is held over all of them.
#[test]
fn a_long_chain_of_joins_is_walked_in_a_loop() {
    let small = std::thread::Builder::new().stack_size(512 * 1024);
    let run = small.spawn(|| {
        let mut p = pool();
        let (t, int) = (con(&mut p, 7), int_ty(&mut p));
        let f = chain(20_000, 20_000, t, int);
        let types = types_of(&f);
        let got = run(&p, &types, f).expect("it is well made");
        let count = count_drops(&got.body);
        // Taking it apart by hand, since a `Box` drops what is in it from inside.
        take_apart(got.body);
        count
    });
    let count = run.expect("a thread").join().expect("it ends");
    assert_eq!(count, 20_000 + 20_000 + 1);
}

fn take_apart(e: CoreExpr) {
    let mut e = e;
    while let Ok((_, rest)) = peel(e) {
        e = rest;
    }
}

/// Joins in the bodies of joins take a little stack each, far less than it took to
/// lower them.
#[test]
fn joins_in_joins_take_a_little_stack() {
    let small = std::thread::Builder::new().stack_size(2 * 1024 * 1024);
    let run = small.spawn(|| {
        let mut p = pool();
        let (t, int) = (con(&mut p, 7), int_ty(&mut p));
        let mut body = bound(bind(1, int), call(&[0]), jump(0, &[1]));
        for i in 0..100 {
            let param = bind(100 + i, int);
            let end = if i == 99 {
                ret(100 + i)
            } else {
                jump(i + 1, &[100 + i])
            };
            body = join(i, vec![param], iff(2, body, jump(i, &[2])), end);
        }
        let f = func(vec![bind(0, t), bind(2, int)], body, int);
        let got = perceus(&p, f);
        checked(&p, &got, ownership::Body::Function);
        count_drops(&got.body)
    });
    // It is dropped where it is read, and on each way that does not go there.
    assert_eq!(run.expect("a thread").join().expect("it ends"), 101);
}
