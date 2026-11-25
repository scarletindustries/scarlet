use super::*;
use crate::PrimOp;
use crate::testkit::{bind, ctor, func, local, variant};
use crate::{Callee, CaptureIdx, ConstId, FuncIdx, GlobalSlot, Load};
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
    dropped(pool, f).core
}

fn dropped(pool: &ResolvedPool, f: CoreFn) -> Dropped {
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

/// A value that may hold a handle is left undropped when it is made inside a
/// join, or when its type is a variable and it dies before the function's end,
/// and nowhere else.
#[test]
fn what_is_left_undropped_is_in_a_join_or_of_no_known_type() {
    let mut p = pool();
    let (heap, int) = (con(&mut p, 7), int_ty(&mut p));
    let string = p.prims().string;
    let string = p.mk_con(string, &[]).unwrap();
    let any = p.mk_bound(0).unwrap();
    let call = || Atom::Call {
        callee: Callee::Known(FuncIdx(1)),
        args: vec![local(0)],
    };
    let made = |ty: RTy, rhs: Atom| CoreExpr::Let {
        bind: bind(1, ty),
        rhs,
        body: Box::new(CoreExpr::Tail(Atom::Local(local(0)))),
    };
    let join = |join: CoreExpr| CoreExpr::LetJoin {
        bind: bind(2, int),
        join: Box::new(join),
        body: Box::new(CoreExpr::Tail(Atom::Local(local(2)))),
    };
    let leaves =
        |param: RTy, body: CoreExpr| dropped(&p, func(vec![bind(0, param)], body, int)).undropped;
    let nothing = || CoreExpr::Tail(Atom::Nil);
    assert!(!leaves(heap, nothing()) && !leaves(int, nothing()));
    assert!(leaves(any, nothing()));
    // One that is passed on to the end is not left.
    assert!(!leaves(any, CoreExpr::Tail(Atom::Local(local(0)))));
    assert!(!leaves(any, CoreExpr::Tail(call())));
    assert!(!leaves(any, CoreExpr::Tail(ctor(&[0]))));
    // Where one way passes it on and another does not, it is.
    let on_one_way = |cond: u32| CoreExpr::If {
        cond: local(cond),
        then: Box::new(nothing()),
        els: Box::new(CoreExpr::Tail(Atom::Local(local(0)))),
        ty: int,
    };
    assert!(leaves(any, on_one_way(0)) && !leaves(int, on_one_way(0)));
    for (ty, in_join) in [(heap, true), (int, false), (string, false), (any, true)] {
        assert_eq!(leaves(int, made(ty, call())), ty == any);
        assert_eq!(leaves(int, join(made(ty, call()))), in_join);
        // Wherever in the join it is.
        let nested = CoreExpr::If {
            cond: local(0),
            then: Box::new(nothing()),
            els: Box::new(CoreExpr::LetCont {
                id: JoinId(0),
                cont: Box::new(made(ty, call())),
                body: Box::new(nothing()),
            }),
            ty: int,
        };
        assert_eq!(leaves(int, join(nested)), in_join);
        let bound = |field: RTy| CoreExpr::Match {
            scrut: local(0),
            arms: vec![(
                CorePat::Ctor {
                    variant: variant(),
                    fields: vec![bind(3, field)],
                },
                nothing(),
            )],
            ty: int,
        };
        assert_eq!(leaves(int, bound(ty)), ty == any);
        assert_eq!(leaves(int, join(bound(ty))), in_join);
        assert_eq!(leaves(int, join(join(bound(ty)))), in_join);
    }
    // What a join gives is dropped after it, unless that is in a join too.
    let gives = |ty: RTy| CoreExpr::LetJoin {
        bind: bind(4, ty),
        join: Box::new(nothing()),
        body: Box::new(nothing()),
    };
    assert!(!leaves(int, gives(heap)));
    assert!(leaves(int, join(gives(heap))));
    // A constant, a Bool and what something else goes on holding do not count.
    let in_join = |rhs: Atom| leaves(int, join(made(heap, rhs)));
    let (a, b) = (local(0), local(0));
    let plain = [
        Atom::Nil,
        Atom::Bool(true),
        Atom::Const(ConstId(0)),
        Atom::Load(Load::Global(GlobalSlot(0))),
        Atom::Load(Load::Capture(CaptureIdx(0))),
        ctor(&[]),
        Atom::Closure {
            func_idx: FuncIdx(0),
            captures: Vec::new(),
        },
        Atom::PrimOp {
            op: PrimOp::Eq,
            args: vec![a, b],
        },
    ];
    for rhs in plain {
        assert!(!in_join(rhs.clone()), "{rhs:?}");
    }
    let holding = [
        Atom::Local(a),
        ctor(&[0]),
        Atom::Closure {
            func_idx: FuncIdx(0),
            captures: vec![a],
        },
        Atom::PrimOp {
            op: PrimOp::MakeTuple,
            args: vec![a, b],
        },
    ];
    for rhs in holding {
        assert!(in_join(rhs.clone()), "{rhs:?}");
    }
}

/// What [`dropped`] says of a function, found another way: by going along every
/// way through it with the values that may be a handle, until each is dropped or
/// the end passes it on.
struct Ways<'p> {
    pool: &'p ResolvedPool,
    /// What every jump so far brings to each continuation.
    brought: BTreeMap<JoinId, Live>,
    left: bool,
    /// Whether there is a continuation that nothing jumps to.
    unreached: bool,
}

impl Ways<'_> {
    fn own(&self, b: &CoreBind, owned: &mut Live) {
        if b.global.is_none() && self.pool.prim_of(b.ty).unwrap().is_none() {
            owned.insert(b.id);
        }
    }

    fn go(&mut self, e: &CoreExpr, mut owned: Live) {
        match e {
            CoreExpr::Let { bind, rhs, body } => {
                // A value of a known type gets a drop whatever it holds.
                if !holds_nothing(rhs) || self.pool.is_heap(bind.ty).unwrap() {
                    self.own(bind, &mut owned);
                }
                self.go(body, owned);
            }
            CoreExpr::LetJoin { bind, join, body } => {
                self.left |= self.binds_one(join);
                self.own(bind, &mut owned);
                self.go(body, owned);
            }
            CoreExpr::LetCont { id, cont, body } => {
                self.go(body, owned);
                match self.brought.remove(id) {
                    Some(brought) => self.go(cont, brought),
                    None => self.unreached = true,
                }
            }
            CoreExpr::Drop { local, body } => {
                owned.remove(local);
                self.go(body, owned);
            }
            CoreExpr::Match { arms, .. } => {
                for (pat, arm) in arms {
                    let mut owned = owned.clone();
                    pat.binds().for_each(|b| self.own(b, &mut owned));
                    self.go(arm, owned);
                }
            }
            CoreExpr::If { then, els, .. } => {
                self.go(then, owned.clone());
                self.go(els, owned);
            }
            CoreExpr::Tail(atom) => {
                atom.for_each_operand(|l| {
                    owned.remove(&l);
                });
                self.left |= !owned.is_empty();
            }
            CoreExpr::Goto(id) => self.brought.entry(*id).or_default().extend(owned),
        }
    }

    /// Whether anything in `e`, which is in a join, may be a handle.
    fn binds_one(&self, e: &CoreExpr) -> bool {
        let may = |b: &CoreBind| self.pool.prim_of(b.ty).unwrap().is_none();
        match e {
            CoreExpr::Let { bind, rhs, body } => {
                (may(bind) && !holds_nothing(rhs)) || self.binds_one(body)
            }
            CoreExpr::LetJoin { bind, join, body } => {
                may(bind) || self.binds_one(join) || self.binds_one(body)
            }
            CoreExpr::LetCont { cont, body, .. } => self.binds_one(cont) || self.binds_one(body),
            CoreExpr::Drop { body, .. } => self.binds_one(body),
            CoreExpr::Match { arms, .. } => {
                let arm =
                    |(pat, arm): &(CorePat, CoreExpr)| pat.binds().any(may) || self.binds_one(arm);
                arms.iter().any(arm)
            }
            CoreExpr::If { then, els, .. } => self.binds_one(then) || self.binds_one(els),
            CoreExpr::Tail(_) | CoreExpr::Goto(_) => false,
        }
    }
}

/// Makes a function at random.
struct Maker {
    x: u64,
    next: u32,
    conts: u32,
    types: [RTy; 4],
}

impl Maker {
    fn below(&mut self, n: u32) -> u32 {
        self.x ^= self.x << 13;
        self.x ^= self.x >> 7;
        self.x ^= self.x << 17;
        u32::try_from(self.x >> 33).unwrap() % n
    }

    /// A bind of one of the types, most often the variable.
    fn bind(&mut self) -> CoreBind {
        let at = self.below(6).min(3) as usize;
        self.next += 1;
        bind(self.next, self.types[at])
    }

    fn any(&mut self, scope: &[u32]) -> u32 {
        scope[self.below(u32::try_from(scope.len()).unwrap()) as usize]
    }

    fn atom(&mut self, scope: &[u32]) -> Atom {
        let (a, b) = (self.any(scope), self.any(scope));
        match self.below(6) {
            0 => Atom::Local(local(a)),
            1 => ctor(&[a, b]),
            2 => Atom::Load(Load::Capture(CaptureIdx(0))),
            3 => Atom::Nil,
            _ => Atom::Call {
                callee: Callee::Known(FuncIdx(1)),
                args: vec![local(a), local(b)],
            },
        }
    }

    /// An expression of about `size` nodes, in which `conts` may be jumped to.
    fn expr(&mut self, size: u32, scope: &[u32], conts: &[u32]) -> CoreExpr {
        let half = size / 2;
        let with = |b: &CoreBind| [scope, &[b.id.0]].concat();
        if size == 0 {
            if !conts.is_empty() && self.below(2) == 0 {
                return CoreExpr::Goto(JoinId(self.any(conts)));
            }
            return CoreExpr::Tail(self.atom(scope));
        }
        match self.below(9) {
            0 => {
                let bind = self.bind();
                let join = Box::new(self.expr(half, scope, &[]));
                let body = Box::new(self.expr(half, &with(&bind), conts));
                CoreExpr::LetJoin { bind, join, body }
            }
            1 => {
                self.conts += 1;
                let id = self.conts;
                let cont = Box::new(self.expr(half, scope, conts));
                let body = self.expr(half, scope, &[conts, &[id]].concat());
                CoreExpr::LetCont {
                    id: JoinId(id),
                    cont,
                    body: Box::new(body),
                }
            }
            2 => CoreExpr::If {
                // A Bool, which nothing has to drop.
                cond: local(0),
                then: Box::new(self.expr(half, scope, conts)),
                els: Box::new(self.expr(half, scope, conts)),
                ty: self.types[0],
            },
            3 => {
                let bound = self.bind();
                let arm = self.expr(half, &with(&bound), conts);
                let pat = CorePat::Ctor {
                    variant: variant(),
                    fields: vec![bound],
                };
                CoreExpr::Match {
                    scrut: local(self.any(scope)),
                    arms: vec![(pat, arm), (CorePat::Wild, self.expr(half, scope, conts))],
                    ty: self.types[0],
                }
            }
            _ => {
                let bind = self.bind();
                let rhs = self.atom(scope);
                let body = Box::new(self.expr(size - 1, &with(&bind), conts));
                CoreExpr::Let { bind, rhs, body }
            }
        }
    }
}

/// What Perceus says it left undropped is what it did leave, whatever the
/// function.
#[test]
fn what_is_said_to_be_left_undropped_is() {
    let mut p = pool();
    let (heap, int) = (con(&mut p, 7), int_ty(&mut p));
    let any = p.mk_bound(0).unwrap();
    let (mut left, mut kept) = (0, 0);
    for seed in 1..=30_000u64 {
        let mut of = Maker {
            x: seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1,
            next: 0,
            conts: 0,
            types: [int, heap, heap, any],
        };
        of.below(9);
        let mut params = vec![bind(0, int)];
        params.extend((0..of.below(3)).map(|_| of.bind()));
        let scope: Vec<u32> = params.iter().map(|b| b.id.0).collect();
        let size = of.below(14);
        let body = of.expr(size, &scope, &[]);
        let got = dropped(&p, func(params.clone(), body, int));
        let mut ways = Ways {
            pool: &p,
            brought: BTreeMap::new(),
            left: false,
            unreached: false,
        };
        let mut owned = Live::new();
        params.iter().for_each(|b| ways.own(b, &mut owned));
        ways.go(&got.core.body, owned);
        // Perceus looks at what no way goes through too.
        if !(ways.unreached && got.undropped) {
            assert_eq!(ways.left, got.undropped, "{seed}: {}", got.core.body);
        }
        left += usize::from(got.undropped);
        kept += usize::from(!got.undropped);
    }
    assert!(left > 5000 && kept > 5000, "{left} {kept}");
}
