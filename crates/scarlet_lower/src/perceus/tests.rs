use super::*;
use crate::PrimOp;
use crate::testkit::{bind, ctor, func, local, variant};
use crate::{Callee, ConstId, FuncIdx};
use scarlet_ir::TypeId;
use scarlet_ir::rty::PrimIds;

/// The same kind of arena the elaborator hands `perceus` in the real
/// pipeline.
fn pool() -> ResolvedPool {
    ResolvedPool::new(PrimIds::default())
}

/// A nominal type that isn't a primitive, so it lives on the heap and its
/// allocation width is unknown.
fn con(p: &mut ResolvedPool, id: i32) -> RTy {
    p.mk_con(TypeId(id), &[]).unwrap()
}

/// The unboxed `Int`. It isn't on the heap, so it gets no `Drop`.
fn int_ty(p: &mut ResolvedPool) -> RTy {
    let int = p.prims().int;
    p.mk_con(int, &[]).unwrap()
}

fn count_drops(e: &CoreExpr) -> usize {
    match e {
        CoreExpr::Drop { body, .. } => 1 + count_drops(body),
        CoreExpr::Let { body, .. } => count_drops(body),
        CoreExpr::LetJoin { join, body, .. } => count_drops(join) + count_drops(body),
        CoreExpr::LetCont { cont, body, .. } => count_drops(cont) + count_drops(body),
        CoreExpr::If { then, els, .. } => count_drops(then) + count_drops(els),
        CoreExpr::Match { arms, .. } => arms.iter().map(|(_, b)| count_drops(b)).sum(),
        CoreExpr::Tail(_) | CoreExpr::Goto(_) => 0,
    }
}

fn find_drop(mut e: &CoreExpr, id: LocalId) -> bool {
    loop {
        match e {
            CoreExpr::Drop { local, .. } if *local == id => return true,
            CoreExpr::Drop { body, .. }
            | CoreExpr::Let { body, .. }
            | CoreExpr::LetJoin { body, .. } => e = body,
            CoreExpr::LetCont { cont, body, .. } => {
                return find_drop(cont, id) || find_drop(body, id);
            }
            CoreExpr::Tail(_) | CoreExpr::Goto(_) => return false,
            CoreExpr::If { then, els, .. } => {
                return find_drop(then, id) || find_drop(els, id);
            }
            CoreExpr::Match { arms, .. } => {
                return arms.iter().any(|(_, b)| find_drop(b, id));
            }
        }
    }
}

/// Run the pass with every bind's type in the table, the way `lower`
/// hands it over. An id no bind uses gets the return type, which nothing
/// reads.
fn perceus(pool: &ResolvedPool, f: CoreFn) -> CoreFn {
    fn binds(e: &CoreExpr, out: &mut Vec<CoreBind>) {
        match e {
            CoreExpr::Let { bind, body, .. } => {
                out.push(bind.clone());
                binds(body, out);
            }
            CoreExpr::LetJoin { bind, join, body } => {
                out.push(bind.clone());
                binds(join, out);
                binds(body, out);
            }
            CoreExpr::LetCont { cont, body, .. } => {
                binds(cont, out);
                binds(body, out);
            }
            CoreExpr::Drop { body, .. } => binds(body, out),
            CoreExpr::If { then, els, .. } => {
                binds(then, out);
                binds(els, out);
            }
            CoreExpr::Match { arms, .. } => {
                for (pat, body) in arms {
                    out.extend(pat.binds().cloned());
                    binds(body, out);
                }
            }
            CoreExpr::Tail(_) | CoreExpr::Goto(_) => {}
        }
    }
    let mut all = f.params.clone();
    binds(&f.body, &mut all);
    let len = all.iter().map(|b| b.id.0 + 1).max().unwrap_or(0);
    let mut locals: TiVec<LocalId, RTy> = (0..len).map(|_| f.ret_ty).collect();
    for b in all {
        if let Some(slot) = locals.get_mut(b.id) {
            *slot = b.ty;
        }
    }
    super::perceus(pool, &locals, f).expect("every local is typed")
}

/// A toplevel keeps every value it pins to a global, so Perceus never drops
/// one. A plain temporary still drops.
#[test]
fn pinned_globals_are_never_dropped() {
    let mut pool = pool();
    let obj = con(&mut pool, 7);
    let int = int_ty(&mut pool);
    let mut global = bind(0, obj);
    global.global = Some(crate::typed_ir::GlobalSlot(0));
    let top = func(
        Vec::new(),
        CoreExpr::Let {
            bind: bind(5, int),
            rhs: Atom::Const(ConstId(0)),
            body: Box::new(CoreExpr::Let {
                bind: global,
                rhs: ctor(&[5]),
                body: Box::new(CoreExpr::Let {
                    bind: bind(1, obj),
                    rhs: ctor(&[5]),
                    body: Box::new(CoreExpr::Tail(ctor(&[5]))),
                }),
            }),
        },
        obj,
    );
    let kept = perceus(&pool, top);
    assert!(!find_drop(&kept.body, local(0)), "{}", kept);
    assert!(find_drop(&kept.body, local(1)), "{}", kept);
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

/// An arm with no more use for the value it matched begins by dropping it,
/// whatever it does after. A value that another arm reads and this one does not
/// goes before it, and the fields this one does not read go after it. The VM
/// looks for the drop among those (`code::translate` in scarlet_vm).
#[test]
fn an_arm_drops_its_scrutinee_before_the_fields_it_does_not_read() {
    let mut pool = pool();
    let list = con(&mut pool, 99);
    let arm = |fields: Vec<CoreBind>, body| {
        let variant = variant();
        (CorePat::Ctor { variant, fields }, body)
    };
    let fields = || vec![bind(4, list), bind(5, list)];
    let calls_itself = CoreExpr::Let {
        bind: bind(6, list),
        rhs: Atom::Call {
            callee: Callee::Self_,
            args: vec![local(5)],
        },
        body: Box::new(CoreExpr::Tail(ctor(&[6]))),
    };
    let f = func(
        vec![bind(0, list), bind(1, list)],
        CoreExpr::Match {
            scrut: local(1),
            arms: vec![
                arm(fields(), calls_itself),
                arm(Vec::new(), CoreExpr::Tail(Atom::Local(local(0)))),
                arm(fields(), CoreExpr::Tail(ctor(&[0, 1, 4]))),
            ],
            ty: list,
        },
        list,
    );
    let f = perceus(&pool, f);
    let CoreExpr::Match { arms, .. } = &f.body else {
        panic!("{f}")
    };
    let first: Vec<_> = arms.iter().map(|(_, arm)| drops_first(arm)).collect();
    let locals = |ids: &[u32]| ids.iter().copied().map(local).collect::<Vec<_>>();
    assert_eq!(first[0], locals(&[0, 1, 4]), "{f}");
    assert_eq!(first[1], locals(&[1]), "{f}");
    assert_eq!(first[2], locals(&[5]), "the arm reads %1:\n{f}");
}

/// The shape of `dot_loop`: two constructors used by a call, then a tail call
/// to self. Each turn drops both once the call has read them.
#[test]
fn a_loop_drops_what_a_turn_made() {
    let mut pool = pool();
    let point = con(&mut pool, 99);
    let int = int_ty(&mut pool);
    let body = CoreExpr::Let {
        bind: bind(2, point),
        rhs: ctor(&[0, 0, 0]),
        body: Box::new(CoreExpr::Let {
            bind: bind(3, point),
            rhs: ctor(&[0, 0, 0]),
            body: Box::new(CoreExpr::Let {
                bind: bind(4, int),
                rhs: Atom::Call {
                    callee: Callee::Known(FuncIdx(7)),
                    args: vec![local(2), local(3)],
                },
                body: Box::new(CoreExpr::If {
                    cond: local(0),
                    then: Box::new(CoreExpr::Tail(Atom::Local(local(1)))),
                    els: Box::new(CoreExpr::Tail(Atom::Call {
                        callee: Callee::Self_,
                        args: vec![local(0), local(4)],
                    })),
                    ty: int,
                }),
            }),
        }),
    };
    let f = perceus(&pool, func(vec![bind(0, int), bind(1, int)], body, int));
    assert!(find_drop(&f.body, local(2)), "{}", f.body);
    assert!(find_drop(&f.body, local(3)), "{}", f.body);
    assert_eq!(count_drops(&f.body), 2, "{}", f.body);
}

/// A tail call reads its arguments before its frame's references go, so
/// no argument is dropped ahead of it: that drop would come before the
/// read. Other locals the frame holds still drop as usual.
#[test]
fn a_tail_call_argument_is_not_dropped_before_the_call() {
    let mut pool = pool();
    let list = con(&mut pool, 99);
    for callee in [Callee::Self_, Callee::Known(FuncIdx(3))] {
        let body = CoreExpr::Let {
            bind: bind(1, list),
            rhs: ctor(&[0]),
            body: Box::new(CoreExpr::Tail(Atom::Call {
                callee,
                args: vec![local(1)],
            })),
        };
        let f = perceus(&pool, func(vec![bind(0, list)], body, list));
        assert!(!find_drop(&f.body, local(1)), "{}", f.body);
        assert!(find_drop(&f.body, local(0)), "{}", f.body);
    }
}

#[test]
fn no_drop_for_unboxed_prims() {
    let mut pool = pool();
    let int = int_ty(&mut pool);
    let f = perceus(
        &pool,
        func(
            vec![bind(0, int)],
            CoreExpr::Tail(Atom::Const(ConstId(0))),
            int,
        ),
    );
    assert_eq!(count_drops(&f.body), 0);
}

/// A heap local bound to a call's result that nothing reads is dropped where it
/// is bound.
#[test]
fn a_call_result_nothing_reads_is_dropped() {
    let mut pool = pool();
    let t = con(&mut pool, 99);
    let f = perceus(
        &pool,
        func(
            vec![],
            CoreExpr::Let {
                bind: bind(0, t),
                rhs: Atom::Call {
                    callee: Callee::Known(FuncIdx(0)),
                    args: vec![],
                },
                body: Box::new(CoreExpr::Tail(ctor(&[]))),
            },
            t,
        ),
    );
    let CoreExpr::Let { body, .. } = &f.body else {
        panic!()
    };
    assert_eq!(drops_first(body), vec![local(0)], "{}", f.body);
}

/// A local live into one `If` branch only drops at the head of the other,
/// so both paths release it exactly once.
#[test]
fn if_join_equalises_ownership() {
    let mut pool = pool();
    let t = con(&mut pool, 99);
    let int = int_ty(&mut pool);
    let f = perceus(
        &pool,
        func(
            vec![bind(0, t), bind(1, int)],
            CoreExpr::If {
                cond: local(1),
                then: Box::new(CoreExpr::Tail(Atom::Local(local(0)))),
                els: Box::new(CoreExpr::Tail(Atom::Call {
                    callee: Callee::Known(FuncIdx(0)),
                    args: vec![],
                })),
                ty: t,
            },
            t,
        ),
    );
    let CoreExpr::If { then, els, .. } = &f.body else {
        panic!()
    };
    assert_eq!(count_drops(then), 0, "then reads %0 as its return value");
    assert_eq!(count_drops(els), 1, "els drops %0 at head");
}

/// A rigid quantified variable's representation is unknown, so it is
/// handled at runtime and gets no `Drop`. It is the only type that isn't a
/// primitive and still counts as not on the heap. A dead nominal bind still
/// drops.
#[test]
fn bound_is_not_heap_but_a_nominal_is() {
    let mut pool = pool();
    let generic = pool.mk_bound(0).unwrap();
    let int = int_ty(&mut pool);
    let t = con(&mut pool, 99);
    let f = perceus(
        &pool,
        func(
            vec![bind(0, generic), bind(1, int)],
            CoreExpr::Tail(Atom::Local(local(1))),
            int,
        ),
    );
    assert_eq!(count_drops(&f.body), 0, "rigid Bound owns no cell");

    let f = perceus(
        &pool,
        func(
            vec![bind(0, t), bind(1, int)],
            CoreExpr::Tail(Atom::Local(local(1))),
            int,
        ),
    );
    assert_eq!(count_drops(&f.body), 1, "dead heap param drops at head");
}

/// Ownership evens out across `Goto` edges. `%0` is live into the shared
/// continuation and `%2` isn't. Every owned local is released exactly once
/// on every path, before the edge or inside the continuation, never both.
#[test]
fn goto_edges_equalise_ownership() {
    let mut pool = pool();
    let t = con(&mut pool, 99);
    let int = int_ty(&mut pool);
    let f = perceus(
        &pool,
        func(
            vec![bind(0, t), bind(1, int), bind(2, t)],
            CoreExpr::LetCont {
                id: JoinId(0),
                cont: Box::new(CoreExpr::Tail(Atom::Local(local(0)))),
                body: Box::new(CoreExpr::If {
                    cond: local(1),
                    then: Box::new(CoreExpr::Goto(JoinId(0))),
                    els: Box::new(CoreExpr::Tail(Atom::Local(local(2)))),
                    ty: t,
                }),
            },
            t,
        ),
    );
    let CoreExpr::LetCont { cont, body, .. } = &f.body else {
        panic!("LetCont survives the pass:\n{}", f.body)
    };
    assert_eq!(count_drops(cont), 0, "cont returns %0:\n{}", f.body);
    let CoreExpr::If { then, els, .. } = &**body else {
        panic!("{}", f.body)
    };
    assert!(
        find_drop(then, local(2)),
        "Goto edge drops the local the cont does not consume:\n{}",
        f.body
    );
    assert!(
        !find_drop(then, local(0)),
        "Goto edge must not drop a cont live-in (the cont owns it):\n{}",
        f.body
    );
    assert!(
        find_drop(els, local(0)),
        "non-Goto path drops %0:\n{}",
        f.body
    );
    assert!(!find_drop(els, local(2)), "els returns %2:\n{}", f.body);
}

/// A local whose last use is inside the shared continuation drops there,
/// once, and no edge drops it before its `Goto`.
#[test]
fn drop_inside_shared_cont_not_on_edges() {
    let mut pool = pool();
    let t = con(&mut pool, 99);
    let int = int_ty(&mut pool);
    let cont = CoreExpr::Let {
        bind: bind(2, int),
        rhs: Atom::prim(PrimOp::IntAdd, vec![local(0)]),
        body: Box::new(CoreExpr::Tail(Atom::Local(local(2)))),
    };
    let f = perceus(
        &pool,
        func(
            vec![bind(0, t), bind(1, int)],
            CoreExpr::LetCont {
                id: JoinId(0),
                cont: Box::new(cont),
                body: Box::new(CoreExpr::If {
                    cond: local(1),
                    then: Box::new(CoreExpr::Goto(JoinId(0))),
                    els: Box::new(CoreExpr::Goto(JoinId(0))),
                    ty: int,
                }),
            },
            int,
        ),
    );
    assert_eq!(
        count_drops(&f.body),
        1,
        "exactly one Drop, inside the cont:\n{}",
        f.body
    );
    let CoreExpr::LetCont { cont, .. } = &f.body else {
        panic!("{}", f.body)
    };
    assert!(
        find_drop(cont, local(0)),
        "%0's last use is in the cont, so it drops there:\n{}",
        f.body
    );
}
