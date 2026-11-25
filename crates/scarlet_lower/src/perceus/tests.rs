use super::*;
use crate::testkit::{
    bind, bound, ctor, func, iff, join, jump, local, matches, ret, tail_call, variant, vref,
};
use crate::{Callee, CaptureIdx, ConstId, CoreBind, FuncIdx, GlobalSlot, Load};
use scarlet_ir::TypeId;
use scarlet_ir::rty::{PrimIds, Repr};

mod ownership;

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
            CoreExpr::If { then, els, .. } => work.extend([&**then, &**els]),
            CoreExpr::Match { arms, .. } => work.extend(arms.iter().map(|(_, arm)| arm)),
            CoreExpr::Ret(_) | CoreExpr::TailCall { .. } | CoreExpr::Jump { .. } => {}
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
    super::perceus(pool, locals, &f.body.always_reached(), f)
}

/// Run the pass with every bind's type in the table, the way `lower`
/// hands it over.
fn perceus(pool: &ResolvedPool, f: CoreFn) -> CoreFn {
    run(pool, &types_of(&f), f).expect("every local is typed")
}

/// The checker finds nothing wrong with `f`.
fn checked(pool: &ResolvedPool, f: &CoreFn, body: ownership::Body) {
    ownership::check(pool, f, body).unwrap_or_else(|why| panic!("{why}\n{f}"));
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
            CoreExpr::Match { arms, .. } => {
                for (pat, arm) in arms {
                    all.extend(pat.binds());
                    work.push(arm);
                }
            }
            CoreExpr::Ret(_) | CoreExpr::TailCall { .. } | CoreExpr::Jump { .. } => {}
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

/// What an arm begins by dropping, in order.
fn drops_first(mut e: &CoreExpr) -> Vec<LocalId> {
    let mut first = Vec::new();
    while let CoreExpr::Drop { local, body } = e {
        first.push(*local);
        e = body;
    }
    first
}

fn arm(fields: Vec<CoreBind>, body: CoreExpr) -> (CorePat, CoreExpr) {
    let variant = variant();
    (CorePat::Ctor { variant, fields }, body)
}

/// An arm with no more use for the value it matched begins by dropping it,
/// whatever it does after. A value that another arm reads and this one does not
/// goes before it, and the fields this one does not read go after it. The VM
/// looks for the drop among those (`code::translate` in scarlet_vm).
#[test]
fn an_arm_drops_its_scrutinee_before_the_fields_it_does_not_read() {
    let mut pool = pool();
    let list = con(&mut pool, 99);
    let fields = || vec![bind(4, list), bind(5, list)];
    let others = || vec![bind(14, list), bind(15, list)];
    let own = Atom::Call {
        callee: Callee::Self_,
        args: vec![local(5)],
    };
    let calls_itself = bound(bind(6, list), own, bound(bind(7, list), ctor(&[6]), ret(7)));
    let reads_it = bound(bind(16, list), ctor(&[0, 1, 14]), ret(16));
    let f = func(
        vec![bind(0, list), bind(1, list)],
        matches(
            1,
            vec![
                arm(fields(), calls_itself),
                arm(Vec::new(), ret(0)),
                arm(others(), reads_it),
            ],
        ),
        list,
    );
    let f = perceus(&pool, f);
    let CoreExpr::Match { arms, .. } = &f.body else {
        panic!("{f}")
    };
    let first: Vec<_> = arms.iter().map(|(_, arm)| drops_first(arm)).collect();
    let locals = |ids: &[u32]| ids.iter().copied().map(local).collect::<Vec<_>>();
    assert_eq!(first[0], locals(&[0, 1, 4]), "{f}");
    assert_eq!(first[1], locals(&[]), "nothing is in a cell there:\n{f}");
    assert_eq!(first[2], locals(&[15]), "the arm reads %1:\n{f}");
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

/// A jump out of a match, past a join that is around it, drops what only that
/// join's continuation would have read.
#[test]
fn a_jump_past_a_join_drops_what_its_continuation_owned() {
    let mut pool = pool();
    let t = con(&mut pool, 99);
    let int = int_ty(&mut pool);
    let params = vec![bind(0, t), bind(1, int), bind(2, t), bind(3, t)];
    let inner = matches(
        0,
        vec![
            arm(vec![bind(5, t)], jump(1, &[])),
            (CorePat::Wild, jump(0, &[1])),
        ],
    );
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
            "    match %0",
            "      | 0.0(%5) ->",
            "        drop %0",
            "        drop %5",
            "        jump j1()",
            "      | _ ->",
            "        drop %2",
            "        drop %0",
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

/// A match inside a join's body keeps its scrutinee when something after the
/// join reads it.
#[test]
fn a_scrutinee_that_is_read_after_the_join_is_kept() {
    let mut pool = pool();
    let t = con(&mut pool, 99);
    let int = int_ty(&mut pool);
    let matched = matches(
        0,
        vec![
            arm(vec![bind(1, t)], jump(0, &[1])),
            arm(Vec::new(), jump(0, &[0])),
        ],
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
        match self.below(9) {
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
            3 => {
                let ty = self.ty();
                let field = self.bind(ty);
                let scrut = self.any(scope);
                let first = self.expr(half, &with(&[field]), joins);
                let second = self.expr(half, scope, joins);
                let mut arms = vec![arm(vec![field], first)];
                // One that is only ever a constructor with no fields there.
                arms.push(match self.types[1..4].contains(&self.of[&scrut]) {
                    true => {
                        let variant = vref(0, 1);
                        let fields = Vec::new();
                        (CorePat::Ctor { variant, fields }, second)
                    }
                    false => (CorePat::Wild, second),
                });
                if self.below(4) == 0 {
                    let all = self.bind(self.of[&scrut]);
                    let third = self.expr(half, &with(&[all]), joins);
                    arms.push((CorePat::Bind(all), third));
                }
                matches(scrut, arms)
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
/// starts, something that another branch needs, or what is matched on, or what
/// the pattern binds. `starts` is what may die where `e` starts, when that is
/// known.
fn drops_are_early(mut e: &CoreExpr, starts: Option<Live>) -> bool {
    let mut may = starts;
    loop {
        match e {
            CoreExpr::Drop { local, body } => {
                if may.as_ref().is_some_and(|may| !may.contains(local)) {
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
            CoreExpr::Match { scrut, arms } => {
                return (0..arms.len()).all(|i| {
                    let (pat, arm) = &arms[i];
                    let mut may = Live::from([*scrut]);
                    may.extend(pat.binds().map(|b| b.id));
                    let mut others = arms.iter().enumerate().filter(|(j, _)| *j != i);
                    let may = others.try_fold(may, |mut may, (_, (_, other))| {
                        may.extend(needed(other)?);
                        Some(may)
                    });
                    drops_are_early(arm, may)
                });
            }
            CoreExpr::Ret(_) | CoreExpr::TailCall { .. } | CoreExpr::Jump { .. } => return true,
        }
    }
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
    let mut seen = [0; 4];
    functions(|seed, p, f| {
        let got = perceus(p, f.clone());
        let checked = ownership::check(p, &got, ownership::Body::Function);
        assert_eq!(checked, Ok(()), "{seed}:\n{got}");
        assert!(
            drops_are_early(&got.body, Some(all_params(f))),
            "{seed}:\n{got}"
        );
        let text = got.body.to_string();
        seen[0] += usize::from(text.contains("join"));
        seen[1] += usize::from(text.contains("drop"));
        seen[2] += usize::from(text.contains("tail call"));
        seen[3] += usize::from(!f.body.always_reached().is_empty());
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
        let types = types_of(f);
        let mut cx = Perceus {
            pool: p,
            locals: &types,
            reached: &BTreeSet::new(),
            entered: BTreeMap::new(),
            around: Vec::new(),
            plain: BTreeSet::new(),
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
    let nothing = |rest| arm(Vec::new(), rest);
    let body = matches(
        0,
        vec![nothing(jump(0, &[])), (CorePat::Wild, jump(0, &[]))],
    );
    let cont = bound(bind(2, int), call(&[0]), ret(2));
    let got = lines(
        &p,
        func(vec![bind(0, t)], join(0, Vec::new(), body, cont), int),
    );
    assert_eq!(got[got.len() - 2], "drop %0");
    let plain = matches(0, vec![nothing(ret(1)), (CorePat::Wild, ret(1))]);
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
