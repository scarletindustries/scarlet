use super::*;
use crate::testkit::{
    bind, bound, ctor, func, iff, join, jump, local, matches, ret, tail_call as tail,
};
use crate::{ConstId, CorePat, FuncIdx};
use scarlet_ir::TypeId;
use scarlet_ir::rty::{PrimIds, RTy, Repr};

/// The types the tests' locals have: `Int`, `String`, and a type that is on the
/// heap.
const INT: RTy = RTy(0);
const STRING: RTy = RTy(1);
const CELL: RTy = RTy(2);

fn pool() -> ResolvedPool {
    let mut pool = ResolvedPool::new(PrimIds::default());
    let prims = pool.prims();
    for id in [prims.int, prims.string, TypeId(40)] {
        pool.mk_con(id, Repr::Counted, &[]).unwrap();
    }
    pool
}

/// One `let` of a spine: what it binds, its type, and what it is.
type Line = (u32, RTy, Atom);

fn int(id: u32, op: PrimOp, args: &[u32]) -> Line {
    (id, INT, prim(op, args))
}

fn prim(op: PrimOp, args: &[u32]) -> Atom {
    Atom::PrimOp {
        op,
        args: args.iter().copied().map(local).collect(),
    }
}

fn add(id: u32, a: u32, b: u32) -> Line {
    int(id, PrimOp::IntAdd, &[a, b])
}

fn constant(id: u32) -> Line {
    (id, INT, Atom::Const(ConstId(id)))
}

fn call(id: u32, args: &[u32]) -> Line {
    let callee = Callee::Known(FuncIdx(9));
    let args = args.iter().copied().map(local).collect();
    (id, CELL, Atom::Call { callee, args })
}

fn spine(lines: &[Line], end: CoreExpr) -> CoreExpr {
    lines.iter().rev().fold(end, |body, (id, ty, rhs)| {
        bound(bind(*id, *ty), rhs.clone(), body)
    })
}

fn run(f: CoreFn) -> Result<CoreFn, InternalLinkError> {
    schedule(&pool(), &f.body.always_reached(), f)
}

fn scheduled(params: u32, body: CoreExpr) -> CoreExpr {
    let params = (0..params).map(|id| bind(id, INT)).collect();
    let f = run(func(params, body, INT));
    f.expect("every type is in the pool").body
}

/// What the spine of `e` binds, in order.
fn order_of(mut e: &CoreExpr) -> Vec<u32> {
    let mut out = Vec::new();
    loop {
        match e {
            CoreExpr::Let { bind, body, .. } => {
                out.push(bind.id.0);
                e = body;
            }
            CoreExpr::Join { params, cont, .. } => {
                out.extend(params.iter().map(|p| p.id.0));
                e = cont;
            }
            CoreExpr::SetGlobal { body, .. } | CoreExpr::Drop { body, .. } => e = body,
            CoreExpr::If { .. }
            | CoreExpr::Match { .. }
            | CoreExpr::Ret(_)
            | CoreExpr::TailCall { .. }
            | CoreExpr::Jump { .. } => return out,
        }
    }
}

/// The order `lines` come in before a function of `params` parameters calls
/// itself with `args`.
fn order(params: u32, lines: &[Line], args: &[u32]) -> Vec<u32> {
    let body = spine(lines, tail(Callee::Self_, args));
    order_of(&scheduled(params, body))
}

/// `go(n - 1, acc + n)`: the new `n` is made after `acc + n` has read the old.
#[test]
fn an_argument_is_made_after_the_last_read_of_what_it_replaces() {
    let lines = [constant(2), int(3, PrimOp::IntSub, &[0, 2]), add(4, 1, 0)];
    assert_eq!(order(2, &lines, &[3, 4]), [4, 2, 3]);
    // The same for a call to another function.
    let body = spine(&lines, tail(Callee::Known(FuncIdx(3)), &[3, 4]));
    assert_eq!(order_of(&scheduled(2, body)), [4, 2, 3]);
    // With nothing to gain, it stays.
    assert_eq!(order(2, &lines, &[4, 3]), [2, 3, 4]);
    // A constant that something else reads too stays where it is.
    let lines = [
        constant(2),
        int(3, PrimOp::IntSub, &[0, 2]),
        add(4, 1, 0),
        add(5, 4, 2),
    ];
    assert_eq!(order(2, &lines, &[3, 5]), [2, 4, 3, 5]);
}

/// It moves past a call and past a join that read the parameter, which keep
/// their order.
#[test]
fn an_argument_moves_past_what_reads_its_parameter() {
    let lines = [add(2, 0, 0), call(3, &[0]), call(4, &[1])];
    assert_eq!(order(2, &lines, &[2, 1]), [3, 2, 4]);
    // Whatever its branches do, and one may jump from inside another join.
    let branches = iff(
        1,
        jump(0, &[0]),
        join(
            1,
            Vec::new(),
            iff(1, jump(1, &[]), jump(0, &[1])),
            bound(bind(4, INT), call(4, &[1]).2, jump(0, &[4])),
        ),
    );
    let go = tail(Callee::Self_, &[2, 3]);
    let after = join(0, vec![bind(3, INT)], branches, go);
    let body = spine(&[add(2, 0, 0)], after);
    assert_eq!(order_of(&scheduled(2, body)), [3, 2]);
    // A join of several parameters makes them all there.
    let go = tail(Callee::Self_, &[2, 4, 5]);
    let body = join(0, vec![bind(4, INT), bind(5, INT)], jump(0, &[0, 1]), go);
    let body = spine(&[add(2, 0, 0)], body);
    assert_eq!(order_of(&scheduled(3, body)), [4, 5, 2]);
    // One that it makes is not itself moved, and what reads it stays after it.
    let go = tail(Callee::Self_, &[4, 6]);
    let body = bound(bind(6, INT), prim(PrimOp::IntAdd, &[0, 4]), go);
    let body = join(0, vec![bind(4, INT)], jump(0, &[1]), body);
    assert_eq!(order_of(&scheduled(2, body)), [4, 6]);
}

/// Of arguments that each read what the next one replaces, the first to be made
/// is the one nothing else waits for.
#[test]
fn arguments_go_in_the_order_they_read_each_other() {
    // 3 replaces 0, 4 replaces 1 and reads 0, 5 replaces 2 and reads 1.
    let lines = [add(3, 0, 0), add(4, 1, 0), add(5, 2, 1)];
    assert_eq!(order(3, &lines, &[3, 4, 5]), [5, 4, 3]);
    // Already in order.
    let lines = [add(5, 2, 1), add(4, 1, 0), add(3, 0, 0)];
    assert_eq!(order(3, &lines, &[3, 4, 5]), [5, 4, 3]);
    // One goes no further than it has to.
    let lines = [add(3, 0, 0), add(4, 1, 0), call(5, &[])];
    assert_eq!(order(3, &lines, &[3, 4, 5]), [4, 3, 5]);
}

/// When two arguments read what the other replaces, one of them has to be moved
/// whatever the order, so they stay as written.
#[test]
fn a_cycle_stays_as_written() {
    let swap = [add(2, 0, 1), int(3, PrimOp::IntSub, &[0, 1])];
    assert_eq!(order(2, &swap, &[2, 3]), [2, 3]);
    let three = [add(3, 0, 1), add(4, 1, 2), add(5, 2, 0)];
    assert_eq!(order(3, &three, &[3, 4, 5]), [3, 4, 5]);
    // One beside the cycle still moves.
    let lines = [add(4, 3, 3), add(5, 0, 3), add(6, 1, 0), add(7, 0, 1)];
    assert_eq!(order(4, &lines, &[6, 7, 2, 4]), [5, 4, 6, 7]);
}

/// An argument does not move past a value for a parameter that it reads, when
/// that value stays where it is: it would then need the parameter that the value
/// had replaced.
#[test]
fn an_argument_does_not_keep_a_parameter_longer() {
    // 2 replaces 0 and reads 1; the call reads 0 and makes what replaces 1.
    let lines = [add(2, 0, 1), call(3, &[0])];
    assert_eq!(order(2, &lines, &[2, 3]), [2, 3]);
    // The value that replaces 1 is made before it, so it may.
    let lines = [call(3, &[]), add(2, 0, 1), call(4, &[0])];
    assert_eq!(order(2, &lines, &[2, 3]), [3, 4, 2]);
}

/// The same where a join makes the value that stays.
#[test]
fn an_argument_does_not_keep_a_parameter_past_a_join_for_it() {
    // 2 replaces 0 and reads 1; the join reads 0 and makes what replaces 1.
    let go = tail(Callee::Self_, &[2, 3]);
    let end = join(0, vec![bind(3, CELL)], jump(0, &[0]), go);
    let body = spine(&[add(2, 1, 1)], end);
    assert_eq!(order_of(&scheduled(2, body)), [2, 3]);
}

/// What an expression reads is what was bound outside it, on every way through it.
#[test]
fn what_a_join_reads_is_bound_outside_it() {
    let inner = join(
        1,
        vec![bind(5, INT)],
        iff(6, jump(1, &[7]), tail(Callee::Local(local(8)), &[9])),
        bound(
            bind(10, INT),
            prim(PrimOp::IntAdd, &[5, 11]),
            jump(0, &[10, 12]),
        ),
    );
    let arm = |id| {
        let pat = CorePat::Bind(bind(id, INT));
        (pat, ret(id))
    };
    let first = |rest| bound(bind(1, INT), call(1, &[2]).2, rest);
    let dropped = CoreExpr::Drop {
        local: local(1),
        body: Box::new(matches(3, vec![arm(13), (CorePat::Wild, inner)])),
    };
    let stored = CoreExpr::SetGlobal {
        slot: crate::GlobalSlot(0),
        value: local(14),
        body: Box::new(first(dropped)),
    };
    let got: Vec<u32> = reads(&stored).into_iter().map(|l| l.0).collect();
    assert_eq!(got, [2, 3, 6, 7, 8, 9, 11, 12, 14]);
}

/// Only what has no effect, cannot fail and reads only numbers moves.
#[test]
fn only_arithmetic_and_constants_move() {
    let moves = |ty: RTy, rhs: Atom| {
        let lines = [(2, ty, rhs), call(3, &[0])];
        order(2, &lines, &[2, 3]) == [3, 2]
    };
    for op in [
        PrimOp::IntAdd,
        PrimOp::IntDiv,
        PrimOp::IntRem,
        PrimOp::IntLe,
        PrimOp::FloatMul,
        PrimOp::FloatLt,
    ] {
        assert!(moves(INT, prim(op, &[0, 0])), "{op:?}");
    }
    assert!(moves(INT, prim(PrimOp::IntNeg, &[0])));
    for op in [
        PrimOp::Add,
        PrimOp::Eq,
        PrimOp::StringConcat,
        PrimOp::MakeTuple,
        PrimOp::ArrayConcat,
        PrimOp::ArrayLen,
        PrimOp::Field(0),
    ] {
        assert!(!moves(INT, prim(op, &[0, 0])), "{op:?}");
    }
    assert!(moves(INT, Atom::Const(ConstId(0))));
    assert!(!moves(STRING, Atom::Const(ConstId(0))));
    assert!(moves(CELL, Atom::Nil));
    assert!(moves(CELL, Atom::Bool(true)));
    assert!(moves(CELL, ctor(&[])));
    assert!(!moves(CELL, ctor(&[0])));
    assert!(!moves(CELL, Atom::Local(local(0))));
    assert!(!moves(CELL, call(0, &[0]).2));
}

/// A value that is read before the call or passed twice stays.
#[test]
fn a_value_with_another_use_stays() {
    let read = [add(2, 0, 0), call(3, &[0, 2])];
    assert_eq!(order(2, &read, &[2, 3]), [2, 3]);
    let twice = [add(2, 0, 0), call(3, &[0])];
    assert_eq!(order(2, &twice, &[2, 2]), [2, 3]);
}

/// A parameter that the call passes on is read to the end, so nothing is gained.
#[test]
fn a_parameter_that_is_passed_on_is_left_alone() {
    let lines = [add(2, 0, 0), call(3, &[0])];
    assert_eq!(order(2, &lines, &[2, 0]), [2, 3]);
}

/// A function that holds many values goes on holding no more: the order that
/// would is not taken. One that holds few may hold a few more.
#[test]
fn an_order_that_holds_too_much_is_not_taken() {
    // `many` values are held to the last line. The sum reads two others, that
    // nothing reads after it, and three more are made before the last read of
    // what it replaces.
    let moved = |many: u32, sum: Line| {
        let held = 20..20 + many;
        let mut lines: Vec<Line> = held.clone().map(|id| call(id, &[])).collect();
        lines.extend([call(10, &[]), call(11, &[]), sum]);
        lines.extend([12, 13, 14].map(|id| call(id, &[])));
        lines.push(call(6, &[0]));
        let all: Vec<u32> = held.chain([12, 13, 14, 6]).collect();
        lines.push(call(7, &all));
        let order = order(1, &lines, &[5]);
        let at = |id: u32| order.iter().position(|l| *l == id);
        at(5) > at(6)
    };
    for many in [3, 40, 58] {
        assert!(moved(many, add(5, 10, 11)), "{many}");
    }
    for many in [59, 70, 200] {
        assert!(!moved(many, add(5, 10, 11)), "{many}");
        // One that holds no more is taken however much it holds.
        assert!(moved(many, add(5, 0, 0)), "{many}");
        assert!(moved(many, add(5, 20, 0)), "{many}");
    }
}

/// Where many values are held, nothing that stays may have more around it than
/// it had, even when that is fewer than somewhere else.
#[test]
fn no_line_that_stays_has_more_around_it() {
    let moved = |before: u32, sum: Line| {
        let (held, more) = (20..20 + before, 200..240);
        let mut lines = vec![call(10, &[]), call(11, &[]), sum];
        lines.extend(held.clone().map(|id| call(id, &[])));
        lines.push(call(6, &[0]));
        lines.extend(more.clone().map(|id| call(id, &[])));
        let all: Vec<u32> = held.chain(more).chain([6]).collect();
        lines.push(call(7, &all));
        let order = order(1, &lines, &[5]);
        let at = |id: u32| order.iter().position(|l| *l == id);
        at(5) > at(6)
    };
    for before in [3, 40, 60] {
        assert!(moved(before, add(5, 10, 11)), "{before}");
    }
    for before in [63, 100] {
        assert!(!moved(before, add(5, 10, 11)), "{before}");
        assert!(moved(before, add(5, 0, 0)), "{before}");
    }
}

/// An argument goes behind the drops of the line it goes after, which release
/// what that line read, and a drop is no read. What has been dropped is not read
/// again.
#[test]
fn an_argument_goes_behind_the_drops() {
    let drop = |of: u32, body: CoreExpr| CoreExpr::Drop {
        local: local(of),
        body: Box::new(body),
    };
    let one = |(id, ty, rhs): Line, body: CoreExpr| bound(bind(id, ty), rhs, body);
    let text = |e: &CoreExpr| -> Vec<String> {
        let mut e = e;
        let mut out = Vec::new();
        loop {
            match e {
                CoreExpr::Let { bind, body, .. } => {
                    out.push(format!("{}", bind.id.0));
                    e = body;
                }
                CoreExpr::Drop { local, body } => {
                    out.push(format!("drop {}", local.0));
                    e = body;
                }
                _ => return out,
            }
        }
    };
    // 4 replaces 0, which the call that makes 5 is the last to read.
    let end = || tail(Callee::Self_, &[4, 5]);
    let after = drop(0, drop(3, one(call(6, &[]), drop(6, end()))));
    let body = one(call(3, &[]), one(constant(4), one(call(5, &[0, 3]), after)));
    let got = scheduled(2, body);
    assert_eq!(
        text(&got),
        ["3", "5", "drop 0", "drop 3", "4", "6", "drop 6"]
    );
    // A drop of the parameter itself, with no read after the argument.
    let body = one(constant(4), drop(0, one(call(5, &[]), end())));
    assert_eq!(text(&scheduled(2, body)), ["4", "drop 0", "5"]);
}

/// A join may hold any number of values inside, so nothing more is held around
/// one.
#[test]
fn nothing_more_is_held_around_a_join() {
    let body = |lines: &[Line]| {
        let end = join(
            0,
            vec![bind(8, INT)],
            jump(0, &[0]),
            tail(Callee::Self_, &[5]),
        );
        order_of(&scheduled(1, spine(lines, end)))
    };
    let calls = [call(3, &[]), call(4, &[])];
    let with = |sum: Line| [&calls[..], &[sum]].concat();
    assert_eq!(body(&with(add(5, 3, 4))), [3, 4, 5, 8]);
    assert_eq!(body(&with(add(5, 3, 3))), [3, 4, 8, 5]);
    assert_eq!(body(&with(add(5, 0, 0))), [3, 4, 8, 5]);
}

/// No more is held at any point of a join either. Fewer held where it starts
/// do not make up for more held past it.
#[test]
fn nothing_more_is_held_past_a_join() {
    // A join that reads `reads`, in front of a call of itself.
    let moved = |params: u32, lines: &[Line], reads: &[u32], args: &[u32]| {
        let made = bound(bind(80, INT), call(0, reads).2, jump(0, &[80]));
        let end = join(0, vec![bind(90, INT)], made, tail(Callee::Self_, args));
        let order = order_of(&scheduled(params, spine(lines, end)));
        order.last() != Some(&90)
    };
    // As written, the sum of 1 and 2 is held past the join and 1 and 2 are not,
    // though the join reads them. Moved, both are, and fewer are held before it.
    assert!(!moved(4, &[add(5, 1, 2)], &[1, 2, 3], &[0, 5, 1, 90]));
    assert!(!moved(4, &[add(5, 1, 2)], &[1, 2, 3], &[0, 5, 90, 90]));
    // Held for one value that it replaces, it may.
    assert!(moved(4, &[add(5, 1, 1)], &[1, 2, 3], &[0, 5, 90, 90]));
    // So it may where what it reads is held past the join anyway.
    assert!(moved(4, &[add(5, 1, 2)], &[1, 2, 3], &[0, 5, 2, 90]));
    // One that the join reads in place of one that it does not: with the join's
    // reads all still to come no more is held, and with them all done no more.
    assert!(moved(2, &[add(5, 1, 1)], &[1], &[90, 5]));
}

/// Whatever is held around a join as written and in another order, the one is
/// found to be no more than the other when that is so at every point of the
/// join, which is after it has made any of its reads.
#[test]
fn what_is_held_around_a_join_is_compared_at_every_point() {
    // Sets of seven values, of which the join reads the first three.
    let around = |past: u32| Around {
        most: 0,
        past: (past >> 3).count_ones() as usize,
        again: (0..3).filter(|i| past & (1 << i) != 0).map(local).collect(),
    };
    for was in 0..128u32 {
        for now in 0..128u32 {
            let held = |past: u32, reads: u32| (past | reads).count_ones();
            let no_more = (0..8).all(|reads| held(now, reads) <= held(was, reads));
            let got = around(now).around_no_more_than(&around(was));
            assert_eq!(got, no_more, "{was:07b} {now:07b}");
        }
    }
}

/// A constructor that takes each of its fields is better left to take them.
#[test]
fn a_constructor_goes_on_taking_its_fields() {
    let lines = |fields: &[u32]| [add(2, 0, 0), (3, CELL, ctor(fields))];
    assert_eq!(order(2, &lines(&[0, 1]), &[2, 3]), [2, 3]);
    // One that shares a field anyway does not matter.
    assert_eq!(order(3, &lines(&[0, 1]), &[2, 1, 3]), [3, 2]);
    let mut shared = lines(&[0, 1]).to_vec();
    shared.push(call(4, &[1]));
    assert_eq!(order(2, &shared, &[2, 3]), [3, 2, 4]);
}

/// So is an update that takes its base and what it sets.
#[test]
fn an_update_goes_on_taking_its_operands() {
    let update = |base: u32, value: u32| {
        let update = Atom::Update {
            base: local(base),
            variant: crate::testkit::variant(),
            sets: vec![(1, local(value))],
        };
        [add(2, 0, 0), (3, CELL, update)]
    };
    assert_eq!(order(2, &update(1, 0), &[2, 3]), [2, 3]);
    assert_eq!(order(2, &update(0, 1), &[2, 3]), [2, 3]);
    // One that shares its base, or a value, anyway does not matter.
    assert_eq!(order(3, &update(1, 0), &[2, 1, 3]), [3, 2]);
    assert_eq!(order(3, &update(0, 1), &[2, 1, 3]), [3, 2]);
}

/// The calls in the branches of a function are each put in order, and what is
/// before the branch is not touched. A call of a function value is not.
#[test]
fn every_tail_call_is_put_in_order_and_nothing_else() {
    let lines = [add(3, 0, 0), add(4, 1, 0)];
    let arm = || spine(&lines, tail(Callee::Self_, &[3, 4]));
    let inner = matches(1, vec![(CorePat::Wild, arm())]);
    let inner = iff(1, jump(0, &[]), inner);
    let branch = iff(0, arm(), join(0, Vec::new(), inner, arm()));
    let body = scheduled(3, spine(&[add(6, 0, 0), add(7, 0, 1)], branch));
    assert_eq!(order_of(&body), [6, 7]);
    let text = body.to_string();
    let lets = |of: &str| text.matches(&format!("let %{of}:0")).count();
    assert_eq!((lets("3"), lets("4")), (3, 3));
    let first = |a: &str, b: &str| {
        text.split(&format!("let %{a}:0"))
            .skip(1)
            .filter(|after| after.contains(&format!("let %{b}:0")))
            .count()
    };
    // In each of the three, 4 comes before 3.
    assert_eq!(first("4", "3"), 3, "{text}");
    assert_eq!(first("3", "4"), 2, "{text}");
    let value = spine(&lines, tail(Callee::Local(local(2)), &[3, 4]));
    assert_eq!(order_of(&scheduled(3, value)), [3, 4]);
}

/// What comes before a join that some way leaves is not moved past it.
#[test]
fn nothing_moves_past_a_join_that_some_way_leaves() {
    let moved = |body| {
        let after = spine(&[add(4, 1, 0)], tail(Callee::Self_, &[3, 4]));
        let body = spine(&[add(3, 0, 0)], join(0, Vec::new(), body, after));
        order_of(&scheduled(2, body)) == [4, 3]
    };
    assert!(moved(jump(0, &[])));
    assert!(moved(iff(0, jump(0, &[]), jump(0, &[]))));
    assert!(!moved(iff(0, jump(0, &[]), ret(1))));
    assert!(!moved(iff(0, jump(0, &[]), tail(Callee::Self_, &[0, 1]))));
}

/// A type that is not in the pool is an error and not a guess.
#[test]
fn a_type_of_another_pool_is_refused() {
    let lines = [(2, RTy(77), Atom::Const(ConstId(0))), call(3, &[0])];
    let body = spine(&lines, tail(Callee::Self_, &[2, 3]));
    let params = vec![bind(0, INT), bind(1, INT)];
    let got = run(func(params, body, INT));
    let foreign = InternalLinkError::ForeignRTy(scarlet_ir::rty::ForeignRTy(RTy(77)));
    assert_eq!(got.map(|_| ()).unwrap_err(), foreign);
}

/// Numbers from a generator that gives the same ones each time.
fn numbers(seed: u64) -> impl FnMut(usize) -> usize {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    move |n| {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        usize::try_from(x >> 20).unwrap() % n
    }
}

/// How many arguments are made while something still reads what they replace,
/// which is how many need a register of their own.
fn out_of_place(params: u32, lines: &[Line], args: &[u32]) -> usize {
    let made = |v: u32| lines.iter().position(|(id, ..)| *id == v);
    let mut out = 0;
    for (param, arg) in (0..params).zip(args) {
        let mut last = args.contains(&param).then_some(lines.len());
        for (at, (_, _, rhs)) in lines.iter().enumerate() {
            rhs.for_each_operand(|l| {
                if l.0 == param {
                    last = last.max(Some(at));
                }
            });
        }
        out += usize::from(*arg != param && last > made(*arg));
    }
    out
}

/// Whatever the function: each line is still there once and after what it reads,
/// what may have an effect keeps its order, and no more arguments need a register
/// of their own.
#[test]
fn an_order_is_always_one_that_computes_the_same() {
    let mut moved = 0;
    let mut gained = 0;
    for seed in 0..40_000 {
        let mut below = numbers(seed + 1);
        let params = 1 + u32::try_from(below(5)).unwrap();
        let mut lines: Vec<Line> = Vec::new();
        for i in 0..1 + below(9) {
            let id = params + u32::try_from(i).unwrap();
            let mut any = || u32::try_from(below(id as usize)).unwrap();
            let (a, b) = (any(), any());
            lines.push(match below(7) {
                0 => constant(id),
                1 => call(id, &[a]),
                2 => (id, CELL, ctor(&[a, b])),
                3 => int(id, PrimOp::IntNeg, &[a]),
                _ => add(id, a, b),
            });
        }
        let all = params as usize + lines.len();
        let args: Vec<u32> = (0..params)
            .map(|p| match below(3) {
                0 => p,
                _ => u32::try_from(below(all)).unwrap(),
            })
            .collect();
        let body = spine(&lines, tail(Callee::Self_, &args));
        let after = order_of(&scheduled(params, body));
        let line = |id: &u32| lines.iter().find(|(l, ..)| l == id).unwrap().clone();
        let now: Vec<Line> = after.iter().map(line).collect();
        let what = format!("{seed}: {lines:?} {args:?} {after:?}");
        assert_eq!(now.len(), lines.len(), "{what}");
        let mut made: BTreeSet<u32> = (0..params).collect();
        for (id, _, rhs) in &now {
            rhs.for_each_operand(|l| assert!(made.contains(&l.0), "{what}"));
            assert!(made.insert(*id), "{what}");
        }
        let effects = |lines: &[Line]| -> Vec<u32> {
            let has = |(_, _, rhs): &&Line| matches!(rhs, Atom::Call { .. });
            lines.iter().filter(has).map(|(id, ..)| *id).collect()
        };
        assert_eq!(effects(&lines), effects(&now), "{what}");
        let (was, is) = (
            out_of_place(params, &lines, &args),
            out_of_place(params, &now, &args),
        );
        assert!(is <= was, "{what}: {was} {is}");
        moved += usize::from(after.windows(2).any(|w| w[0] > w[1]));
        gained += was - is;
    }
    assert!(moved > 1000 && gained > 1000, "{moved} {gained}");
}

/// A call of many arguments is put in order in time that grows with how many
/// there are: each reads what the one before it replaces.
#[test]
fn many_arguments_are_put_in_order() {
    let n = 250;
    let mut lines: Vec<Line> = vec![add(n, 0, 0)];
    lines.extend((1..n).map(|i| add(n + i, i, i - 1)));
    let args: Vec<u32> = (n..2 * n).collect();
    let want: Vec<u32> = (n..2 * n).rev().collect();
    assert_eq!(order(n, &lines, &args), want);
}
