//! Lowering turns typed IR into Core IR.
//!
//! [`lower`] turns a [`TypedProgram`] into a [`CoreProgram`]. Every
//! subexpression becomes a `Let{bind,rhs,body}`, so evaluation order is spelled
//! out as a right-nested spine of `Let`s and every operand is a [`LocalId`].
//!
//! Lowering never fails. It takes `&TypedProgram` and returns `CoreProgram`
//! with no `&mut` and no `Result`, because everything it would have to ask for
//! is already a field: a type is an [`RTy`], a name is a [`ValueRef`] and a
//! callee is a [`TypedCallee`]. The earlier stages can fail, though. A broken
//! invariant there reaches the elaborator's `elaborator_bug`, which aborts
//! instead of reporting a diagnostic.
//!
//! ## How the builder works
//!
//! ANF is built from the inside out. Recursing into a subexpression gives back
//! the `LocalId` its value lands in, plus a list of `Let`s to put in front.
//! [`Lower`] collects those in `spine`, and [`Lower::seal`] folds them around
//! the tail once it's known. Join points (`If` and `Match`) seal each arm on
//! its own, so no `Let` moves across a branch.
//!
//! ## Joins in operand position
//!
//! An `if`, `match` or short-circuit where a value is needed lowers through
//! [`CoreExpr::LetJoin`]. It is sealed as its own tree whose `Tail`s produce a
//! value, and then bound like any other operand.
//!
//! A `Let`/`Seq` spine is not one of those. It is straight-line code, so
//! [`Lower::atom`] splices its bindings onto the enclosing spine. Sealing it as
//! a join would bind the value twice and hand Perceus the join's type instead
//! of the value's, which turns a `Drop` that knows the value's shape into a
//! generic one.
//!
//! ## One pass over the whole program
//!
//! `lower` walks `TypedProgram::fns` in order, so `CoreProgram::fns[i]` is the
//! body of `FuncIdx(i)` and every `TypedCallee::Known(i)` operand is already
//! correct.
//!
//! ## The elaborator owns the constant pool
//!
//! Interning changes the pool, so `lower` never interns. Every `Atom::Const`
//! names a [`ConstId`] read from the node. Even integers no source literal
//! writes, like an array pattern's length or a `<<>>` walk's bit cursor, are
//! pooled during elaboration, so the pool is already final when `lower` runs.

use std::collections::VecDeque;

use super::{
    Atom, Callee, ConstId, CoreBind, CoreExpr, CoreFn, CorePat, JoinId, Load, LocalId,
    PrimOp,
};
use crate::typed_ir::{
    BindingId, PatRest, RTy, TempTys, TypedArm, TypedArrayElem, TypedBinPatSeg, TypedBinSeg,
    TypedBind, TypedCallee, TypedExpr, TypedFn, TypedInterpPart, TypedPat, ValueRef, pat_binds,
};
use scarlet_ir::tivec::TiVec;

/// Narrow a [`PrimOp`] field or element index to `u16`, checking it fits. An
/// index that doesn't fit aborts, because silently truncating it would read
/// the wrong field.
#[allow(clippy::panic)]
fn idx16<T: TryInto<u16> + Copy + std::fmt::Debug>(i: T) -> u16 {
    match i.try_into() {
        Ok(v) => v,
        Err(_) => panic!(
            "internal compiler error: index immediate {i:?} exceeds u16. \
             Report this as a compiler bug."
        ),
    }
}

/// A `TypedFn` misused a [`BindingId`], by reading it before binding it or by
/// not counting it in `TypedFn::binds`. Carrying on would hide the bug,
/// because any `LocalId` we put in its place names a live local and the
/// program returns a wrong answer. So this aborts in release builds too.
#[allow(clippy::panic)]
#[cold]
#[inline(never)]
fn binding_bug(b: BindingId, why: &str) -> ! {
    panic!("internal compiler error: {b} {why}. Report this as a compiler bug.")
}

/// Lower one function. Each param is bound under the [`BindingId`] its
/// [`TypedBind`] carries.
pub fn lower_fn(temps: TempTys, f: &TypedFn) -> CoreFn {
    let mut lo = Lower::new(temps, f.binds);
    let params: Vec<CoreBind> = f
        .params
        .iter()
        .map(|p| {
            let id = lo.fresh(p.ty);
            lo.bind(p.id, id);
            CoreBind::new(id, p.ty)
        })
        .collect();
    let body = lo.sealed(|lo| lo.expr_tail(&f.body, f.ret, SpinePos::Outer));
    CoreFn {
        params,
        body,
        ret_ty: f.ret,
    }
}

/// One binding waiting on the spine, to be sealed around a tail later. `Join`
/// holds a `LetJoin`'s right-hand side, which is a full expression tree in
/// operand position.
enum Pending {
    Let { bind: CoreBind, rhs: Atom },
    Join { bind: CoreBind, join: CoreExpr },
}

/// What [`Lower::nested_arm_body`] lowers once every remaining sub-pattern
/// has been bound.
#[derive(Clone)]
enum ArmLeaf<'p> {
    Body(&'p TypedExpr),
    Guarded {
        guard: &'p TypedExpr,
        body: &'p TypedExpr,
    },
    /// Carry on reading a `<<>>` pattern's remaining segments. We get here
    /// after the current segment's value pattern has bound its names, so those
    /// bindings are in scope for the next segment's `size(...)` expression.
    BinCont(Box<BinCont<'p>>),
    /// An alternative of an or-pattern that binds names has matched, so its
    /// commit path can't be shared. Carry on with the sub-patterns queued
    /// after the or-pattern, then `leaf`. A failure falls to `outer` and not to
    /// the next alternative, because the first match commits and a later
    /// failure goes to the next arm.
    OrBody {
        rest_nested: Vec<(LocalId, &'p TypedPat)>,
        leaf: Box<ArmLeaf<'p>>,
        outer: ArmFall,
    },
    /// An alternative of an or-pattern that binds nothing has matched. Jump to
    /// the commit continuation, which is lowered once for all alternatives.
    OrCommit(JoinId),
}

/// The paused state of a `<<>>` pattern walk. `segs` are the segments still
/// to read, starting at bit offset `cursor` into `bin`, whose bit length is
/// `total`. When `segs` runs out, the trailing `rest` or exact-length check
/// runs, then `after` (any nested sub-patterns queued after the binary), and
/// finally `then` (the arm body or guard).
#[derive(Clone)]
struct BinCont<'p> {
    bin: LocalId,
    total: LocalId,
    cursor: LocalId,
    /// The pooled `Int` `0` from the enclosing [`TypedPat::Bin`], so a `:utf8`
    /// segment can test its decoded width without interning one.
    zero: ConstId,
    segs: &'p [TypedBinPatSeg],
    rest: PatRest,
    after: Vec<(LocalId, &'p TypedPat)>,
    then: ArmLeaf<'p>,
}

/// The position of a `<<>>` walk at one segment boundary. The locals are
/// bundled so [`Lower::bin_read`] can't take them, all of one type, in the
/// wrong order.
#[derive(Clone, Copy)]
struct BinWalk {
    bin: LocalId,
    total: LocalId,
    cursor: LocalId,
}

/// How wide a fixed-position `<<>>` segment read is. A `Sized` read gets the
/// `end <= total` bounds check. A `Remainder` read is always `total - cursor`
/// wide, so its check could never fail and is skipped.
enum SegWidth<'p> {
    /// The segment's size in bits, with the unit multiplier already folded in.
    Sized(&'p TypedExpr),
    /// A trailing `:binary` segment with no size, which takes the rest of the
    /// binary.
    Remainder,
}

/// Where control goes when a guard or nested sub-pattern fails after the arm's
/// `CorePat` head matched. It is a [`JoinId`] label, so an arm with many
/// failure edges lowers its fallthrough once and `Goto`s it from each.
#[derive(Clone, Copy)]
enum ArmFall {
    /// Jump to the join that carries on matching, with the remaining arms or
    /// the next alternative of an enclosing or-pattern.
    Join(JoinId),
    /// No join was minted, because [`arm_fallible`] said the arm can't fail.
    /// Using it aborts instead of emitting a failure edge we proved can't
    /// exist.
    Unreachable,
}

/// Whether a [`Lower::expr_tail`] walk is on the function's outermost
/// `Let`/`Seq` spine or a nested one. Only bindings on the outermost spine own
/// frame slots (see [`Lower::let_bind_at`]). [`lower_fn`] passes `Outer`, and
/// every other caller passes `Nested`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SpinePos {
    Outer,
    Nested,
}

struct Lower {
    temps: TempTys,
    next: u32,
    /// The [`RTy`] of every minted [`LocalId`], indexed by `id.0`.
    tys: Vec<RTy>,
    /// Maps `BindingId → LocalId`, densely over `TypedFn::binds`. Different
    /// source bindings have different `BindingId`s, so nothing is shadowed and
    /// nothing has to be undone when a scope ends.
    binds: Vec<Option<LocalId>>,
    /// The `Let`s emitted so far in the current straight-line segment.
    /// [`Self::seal`] folds them around the segment's tail.
    spine: Vec<Pending>,
    /// How many `Goto`s target each minted [`JoinId`]. A join nothing targets
    /// is never turned into a `LetCont`. So an arm whose shape could fail, but
    /// never actually does, stays on the flat path with no continuation.
    join_uses: TiVec<JoinId, u32>,
}

impl Lower {
    fn new(temps: TempTys, binds: u32) -> Self {
        Lower {
            temps,
            next: 0,
            tys: Vec::new(),
            binds: vec![None; binds as usize],
            spine: Vec::new(),
            join_uses: TiVec::new(),
        }
    }

    fn fresh(&mut self, ty: RTy) -> LocalId {
        let id = LocalId(self.next);
        self.next += 1;
        self.tys.push(ty);
        debug_assert_eq!(self.tys.len(), self.next as usize);
        id
    }

    /// The `RTy` recorded for `id` when it was minted.
    fn bind_ty(&self, id: LocalId) -> RTy {
        self.tys[id.0 as usize]
    }

    /// Record the local `b` names. A `BindingId` out of range means the
    /// elaborator miscounted the body's binders, so this aborts instead of
    /// letting [`Self::local`] make up a value later.
    fn bind(&mut self, b: BindingId, id: LocalId) {
        match self.binds.get_mut(b.0 as usize) {
            Some(slot) => *slot = Some(id),
            None => binding_bug(b, "is out of range for this fn's binder table"),
        }
    }

    /// The local a [`BindingId`] names. A `TypedProgram` always binds before it
    /// reads, so this can't fail. It must not fail quietly either, because any
    /// default would be a live local, and the program would run and return a
    /// wrong answer.
    fn local(&self, b: BindingId) -> LocalId {
        match self.binds.get(b.0 as usize) {
            Some(Some(id)) => *id,
            Some(None) => binding_bug(b, "was read before it was bound"),
            None => binding_bug(b, "is out of range for this fn's binder table"),
        }
    }

    /// Push `let %id = rhs` onto the current spine and return `%id`.
    fn let_(&mut self, ty: RTy, rhs: Atom) -> LocalId {
        let id = self.fresh(ty);
        self.spine.push(Pending::Let {
            bind: CoreBind::new(id, ty),
            rhs,
        });
        id
    }

    /// Push `letj %id = <join>` onto the current spine and return `%id`.
    fn let_join(&mut self, ty: RTy, join: CoreExpr) -> LocalId {
        let id = self.fresh(ty);
        self.spine.push(Pending::Join {
            bind: CoreBind::new(id, ty),
            join,
        });
        id
    }

    /// Mint a new join-point label, which no `Goto` targets yet.
    fn fresh_join(&mut self) -> JoinId {
        self.join_uses.push(0)
    }

    /// A `Goto` edge into `j`, counted so [`Self::join_used`] can tell a live
    /// continuation from a dead one.
    fn goto_join(&mut self, j: JoinId) -> CoreExpr {
        self.join_uses[j] += 1;
        CoreExpr::Goto(j)
    }

    /// Whether any failure edge targeted `j`.
    fn join_used(&self, j: JoinId) -> bool {
        self.join_uses[j] > 0
    }

    /// Take back the `Goto` counts inside a subtree that assembly is throwing
    /// away. Without this, a join whose remaining edges were all thrown away
    /// would still be turned into a dead `LetCont`.
    fn retract_gotos(&mut self, e: &CoreExpr) {
        match e {
            CoreExpr::Goto(j) => self.join_uses[*j] -= 1,
            CoreExpr::Let { body, .. } | CoreExpr::Drop { body, .. } => self.retract_gotos(body),
            CoreExpr::LetJoin { join, body, .. } => {
                self.retract_gotos(join);
                self.retract_gotos(body);
            }
            CoreExpr::LetCont { cont, body, .. } => {
                self.retract_gotos(cont);
                self.retract_gotos(body);
            }
            CoreExpr::Match { arms, .. } => {
                for (_, arm) in arms {
                    self.retract_gotos(arm);
                }
            }
            CoreExpr::If { then, els, .. } => {
                self.retract_gotos(then);
                self.retract_gotos(els);
            }
            CoreExpr::Tail(_) => {}
        }
    }

    /// Push `let %id = rhs` for a source binding that owns its slot. The
    /// [`CoreBind`] carries `bind.global` from the moment it's built, so the
    /// entry-frame pin is never patched on afterwards.
    fn let_pinned(&mut self, bind: &TypedBind, rhs: Atom) -> LocalId {
        let id = self.fresh(bind.ty);
        self.spine.push(Pending::Let {
            bind: CoreBind {
                id,
                ty: bind.ty,
                global: bind.global,
            },
            rhs,
        });
        id
    }

    /// Push `letj %id = <join>` carrying `bind.global`, as [`Self::let_pinned`]
    /// does for a plain `Let`. `ty` is the join's own result type.
    fn let_join_pinned(&mut self, bind: &TypedBind, ty: RTy, join: CoreExpr) -> LocalId {
        let id = self.fresh(ty);
        self.spine.push(Pending::Join {
            bind: CoreBind {
                id,
                ty,
                global: bind.global,
            },
            join,
        });
        id
    }

    /// Bind an already-pooled `Int` constant to a new local. The elaborator
    /// interned every integer a lowered body uses.
    fn int_const(&mut self, c: ConstId) -> LocalId {
        let int_t = self.temps.int;
        self.let_(int_t, Atom::Const(c))
    }

    /// Fold every waiting `Let` from `mark` onward around `tail`, innermost
    /// first, and return the sealed `CoreExpr` for this segment.
    fn seal(&mut self, mark: usize, tail: CoreExpr) -> CoreExpr {
        let mut e = tail;
        for p in self.spine.drain(mark..).rev() {
            e = match p {
                Pending::Let { bind, rhs } => CoreExpr::Let {
                    bind,
                    rhs,
                    body: Box::new(e),
                },
                Pending::Join { bind, join } => CoreExpr::LetJoin {
                    bind,
                    join: Box::new(join),
                    body: Box::new(e),
                },
            };
        }
        e
    }

    /// Run `f` in a new spine segment, then seal every `Let` it pushed around
    /// the tail it produced.
    fn sealed(&mut self, f: impl FnOnce(&mut Self) -> CoreExpr) -> CoreExpr {
        let mark = self.spine.len();
        let tail = f(self);
        self.seal(mark, tail)
    }

    /// Lower `e` as a self-contained tree with result type `result_ty`, such as
    /// a join arm in operand position or the function's tail. It opens a new
    /// spine.
    fn expr_as(&mut self, e: &TypedExpr, result_ty: RTy) -> CoreExpr {
        self.sealed(|lo| lo.expr_tail(e, result_ty, SpinePos::Nested))
    }

    /// Lower `e` in tail position. The `Let`/`Seq` spine is walked in a loop,
    /// because a module toplevel is one `Let` per declaration and can be
    /// thousands deep.
    fn expr_tail(&mut self, e: &TypedExpr, result_ty: RTy, spine: SpinePos) -> CoreExpr {
        let mut e = e;
        loop {
            match e {
                TypedExpr::Let {
                    bind, init, body, ..
                } => {
                    self.let_bind_at(bind, init, spine);
                    e = body;
                }
                TypedExpr::Seq { effect, body, .. } => {
                    let _ = self.operand(effect);
                    e = body;
                }
                TypedExpr::If {
                    cond, then, els, ..
                } => {
                    let cond = self.operand(cond);
                    let then = self.expr_as(then, result_ty);
                    let els = self.expr_as(els, result_ty);
                    return CoreExpr::If {
                        cond,
                        then: Box::new(then),
                        els: Box::new(els),
                        ty: result_ty,
                    };
                }
                TypedExpr::Match { scrut, arms, .. } => {
                    if let Some(atom) = self.index_or(scrut, arms) {
                        return atom;
                    }
                    let scrut = self.operand(scrut);
                    return self.match_arms(scrut, arms, result_ty);
                }
                TypedExpr::And { lhs, rhs, .. } => {
                    return self.short_circuit(lhs, rhs, true, result_ty);
                }
                TypedExpr::Or { lhs, rhs, .. } => {
                    return self.short_circuit(lhs, rhs, false, result_ty);
                }
                _ => {
                    let (a, _ty) = self.atom(e);
                    return CoreExpr::Tail(a);
                }
            }
        }
    }

    /// `let bind = init` on the spine at `spine` position.
    ///
    /// Two kinds of binding own their slot instead of reusing the local the
    /// initialiser landed in. One is a module-scope binding, which fn bodies
    /// read from its own entry-frame slot. The other is a `let` with a source
    /// name on the outermost spine, which gets a local of its own, as written.
    ///
    /// Everything else folds into its initialiser. An elaborator temporary
    /// names no source slot, and a named `let` on a nested spine is local to
    /// that segment, where its value already has a home.
    fn let_bind_at(&mut self, bind: &TypedBind, init: &TypedExpr, spine: SpinePos) {
        let owns_slot =
            bind.global.is_some() || (spine == SpinePos::Outer && bind.named);
        let v = if owns_slot {
            self.let_bound(bind, init)
        } else {
            self.operand(init)
        };
        self.bind(bind.id, v);
    }

    /// `let bind = <init>` for a binding that owns its slot. Both branches mint
    /// the `CoreBind` themselves, so a module-scope binding carries its
    /// entry-frame pin from the start.
    ///
    /// It's normally its own `Let`, even when `init` already lowered to a
    /// local, so `let y = x` is two frame slots.
    ///
    /// A short-circuit is the exception. Its `LetJoin` is already the
    /// binding's one store, so copying again would add a store and leave a
    /// dead alias to drop.
    fn let_bound(&mut self, bind: &TypedBind, init: &TypedExpr) -> LocalId {
        match init {
            // Like `atom`'s short-circuit arm, except the join is the binding.
            TypedExpr::And { lhs, rhs, .. } | TypedExpr::Or { lhs, rhs, .. } => {
                let and = matches!(init, TypedExpr::And { .. });
                let ty = init.ty();
                let join = self.short_circuit(lhs, rhs, and, ty);
                self.let_join_pinned(bind, ty, join)
            }
            _ => {
                let (a, _ty) = self.atom(init);
                self.let_pinned(bind, a)
            }
        }
    }

    /// Lower `e` to a `LocalId` operand, so it never leaves a bare atom.
    fn operand(&mut self, e: &TypedExpr) -> LocalId {
        if let TypedExpr::Var {
            place: ValueRef::Local(b),
            ..
        } = e
        {
            return self.local(*b);
        }
        let (a, ty) = self.atom(e);
        // The wildcard is on purpose. Any atom that isn't a local, now or
        // later, gets bound with a `Let`, and only an existing local skips it.
        #[allow(unknown_lints, wildcard_over_own_enum)]
        match a {
            Atom::Local(id) => id,
            other => self.let_(ty, other),
        }
    }

    /// Lower a control-flow expression in operand position by sealing it as
    /// its own tree and binding the result with a `LetJoin`.
    fn join_operand(&mut self, e: &TypedExpr, ty: RTy) -> LocalId {
        let join = self.expr_as(e, ty);
        self.let_join(ty, join)
    }

    /// Lower `e` to an `(Atom, RTy)` whose operands are all `LocalId`s.
    ///
    /// A `Let`/`Seq` spine here is straight-line code, so its bindings splice
    /// onto the current spine and its tail becomes the atom. Sealing it as a
    /// join would bind the value twice and hide its type behind the join's,
    /// which turns a `Drop` that knows the value's shape into a generic one.
    fn atom(&mut self, e: &TypedExpr) -> (Atom, RTy) {
        let mut e = e;
        loop {
            match e {
                TypedExpr::Let {
                    bind, init, body, ..
                } => {
                    self.let_bind_at(bind, init, SpinePos::Nested);
                    e = body;
                }
                TypedExpr::Seq { effect, body, .. } => {
                    let _ = self.operand(effect);
                    e = body;
                }
                _ => break,
            }
        }
        let ty = e.ty();
        let a = match e {
            TypedExpr::Const { value, .. } => Atom::Const(*value),
            TypedExpr::Nil { .. } => Atom::Nil,
            TypedExpr::Var { place, .. } => match *place {
                // A bound local's type was fixed when its bind was minted.
                ValueRef::Local(b) => {
                    let id = self.local(b);
                    return (Atom::Local(id), self.bind_ty(id));
                }
                ValueRef::Global(slot) => Atom::Load(Load::Global(slot)),
                ValueRef::Capture(idx) => Atom::Load(Load::Capture(idx)),
                ValueRef::SelfClosure => Atom::Load(Load::SelfClosure),
            },
            // Only `b` is conditional, so `a` lowers onto the enclosing spine
            // and only the `If` is sealed as the join. Sealing `a` inside would
            // push its `Let`, and the `Drop` after its last use, into the join.
            TypedExpr::And { lhs, rhs, .. } | TypedExpr::Or { lhs, rhs, .. } => {
                let and = matches!(e, TypedExpr::And { .. });
                let join = self.short_circuit(lhs, rhs, and, ty);
                let id = self.let_join(ty, join);
                return (Atom::Local(id), self.bind_ty(id));
            }
            // Control flow in operand position is sealed and bound as a join.
            // `Let` and `Seq` never get here, but naming them keeps the match
            // exhaustive without a wildcard.
            TypedExpr::Let { .. }
            | TypedExpr::Seq { .. }
            | TypedExpr::If { .. }
            | TypedExpr::Match { .. } => {
                let id = self.join_operand(e, ty);
                return (Atom::Local(id), self.bind_ty(id));
            }
            // The elaborator has already specialised `Add` to `IntAdd`, say.
            TypedExpr::Binary { op, lhs, rhs, .. } => {
                let l = self.operand(lhs);
                let r = self.operand(rhs);
                Atom::prim(*op, vec![l, r])
            }
            TypedExpr::Unary { op, operand, .. } => {
                let v = self.operand(operand);
                Atom::prim(*op, vec![v])
            }
            TypedExpr::Tuple { elems, .. } => {
                let args = self.operands(elems);
                Atom::prim(PrimOp::MakeTuple, args)
            }
            TypedExpr::TupleIndex { recv, idx, .. } => {
                let r = self.operand(recv);
                Atom::prim(PrimOp::TupleField(idx16(*idx)), vec![r])
            }
            TypedExpr::Field { recv, idx, .. } => {
                let r = self.operand(recv);
                Atom::prim(PrimOp::Field(idx16(*idx)), vec![r])
            }
            TypedExpr::Ctor { variant, args, .. } => {
                let fields = self.operands(args);
                Atom::Ctor {
                    variant: *variant,
                    fields,
                    reuse: None,
                }
            }
            TypedExpr::Array { elems, .. } => return (self.array(elems, ty), ty),
            TypedExpr::Index { recv, index, .. } => {
                let r = self.operand(recv);
                let i = self.operand(index);
                Atom::prim(PrimOp::ArrayIndex, vec![r, i])
            }
            TypedExpr::Slice {
                recv, start, end, ..
            } => {
                let r = self.operand(recv);
                let s = self.operand(start);
                let e = self.operand(end);
                Atom::prim(PrimOp::ArraySlice, vec![r, s, e])
            }
            TypedExpr::Range { start, end, .. } => {
                let s = self.operand(start);
                let e = self.operand(end);
                Atom::prim(PrimOp::MakeRange, vec![s, e])
            }
            TypedExpr::Interp { parts, .. } => self.interp(parts),
            TypedExpr::BinLit { segs, .. } => self.bin_lit(segs),
            TypedExpr::Closure {
                func_idx, captures, ..
            } => {
                let captures = self.operands(captures);
                Atom::Closure {
                    func_idx: *func_idx,
                    captures,
                }
            }
            TypedExpr::Call { callee, args, .. } => self.call(callee, args),
        };
        (a, ty)
    }

    fn operands(&mut self, es: &[TypedExpr]) -> Vec<LocalId> {
        let mut out = Vec::with_capacity(es.len());
        for e in es {
            out.push(self.operand(e));
        }
        out
    }

    /// A dynamic callee is evaluated first, then the arguments, left to right.
    fn call(&mut self, callee: &TypedCallee, args: &[TypedExpr]) -> Atom {
        match callee {
            TypedCallee::Known(fi) => {
                let args = self.operands(args);
                Atom::Call {
                    callee: Callee::Known(*fi),
                    args,
                }
            }
            TypedCallee::SelfRec => {
                let args = self.operands(args);
                Atom::Call {
                    callee: Callee::Self_,
                    args,
                }
            }
            TypedCallee::Builtin { intrinsic } => {
                let args = self.operands(args);
                Atom::Intrinsic {
                    intrinsic: *intrinsic,
                    args,
                }
            }
            TypedCallee::Dynamic(f) => {
                let c = self.operand(f);
                let args = self.operands(args);
                Atom::Call {
                    callee: Callee::Local(c),
                    args,
                }
            }
        }
    }

    /// `a && b` → `if a then b else false`; `a || b` → `if a then true else b`.
    fn short_circuit(
        &mut self,
        lhs: &TypedExpr,
        rhs: &TypedExpr,
        and: bool,
        result_ty: RTy,
    ) -> CoreExpr {
        let cond = self.operand(lhs);
        let other = self.expr_as(rhs, result_ty);
        let (then, els) = if and {
            (other, CoreExpr::Tail(Atom::Bool(false)))
        } else {
            (CoreExpr::Tail(Atom::Bool(true)), other)
        };
        CoreExpr::If {
            cond,
            then: Box::new(then),
            els: Box::new(els),
            ty: result_ty,
        }
    }

    /// `[a, b, ..xs, c]`. Each run of plain elements becomes `MakeArray`,
    /// `ArrayPrepend` or `ArrayAppend`, and each spread becomes `ArrayConcat`,
    /// folded left to right. Every step in between carries the literal's `ty`
    /// too, so Perceus drops the halves of a concat chain that get thrown away.
    fn array(&mut self, elems: &[TypedArrayElem], ty: RTy) -> Atom {
        let has_spread = elems.iter().any(|e| matches!(e, TypedArrayElem::Spread(_)));
        if !has_spread {
            let mut args = Vec::with_capacity(elems.len());
            for el in elems {
                if let TypedArrayElem::Elem(ex) = el {
                    args.push(self.operand(ex));
                }
            }
            return Atom::prim(PrimOp::MakeArray, args);
        }

        let mut acc: Option<LocalId> = None;
        let mut i = 0usize;
        while i < elems.len() {
            if let Some(TypedArrayElem::Spread(sp)) = elems.get(i) {
                let s = self.operand(sp);
                acc = Some(match acc {
                    Some(a) => self.let_(ty, Atom::prim(PrimOp::ArrayConcat, vec![a, s])),
                    None => s,
                });
                i += 1;
                continue;
            }
            let mut j = i;
            while j < elems.len() && !matches!(elems.get(j), Some(TypedArrayElem::Spread(_))) {
                j += 1;
            }
            let mut run: Vec<LocalId> = Vec::with_capacity(j - i);
            for el in &elems[i..j] {
                if let TypedArrayElem::Elem(ex) = el {
                    run.push(self.operand(ex));
                }
            }
            // A run before the first spread is prepended straight onto it.
            if acc.is_none()
                && let Some(TypedArrayElem::Spread(sp)) = elems.get(j)
            {
                let s = self.operand(sp);
                run.push(s);
                acc = Some(self.let_(ty, Atom::prim(PrimOp::ArrayPrepend, run)));
                i = j + 1;
                continue;
            }
            let a = match acc {
                Some(a) => {
                    let mut args = vec![a];
                    args.extend(run);
                    self.let_(ty, Atom::prim(PrimOp::ArrayAppend, args))
                }
                None => {
                    let a = Atom::prim(PrimOp::MakeArray, run);
                    self.let_(ty, a)
                }
            };
            acc = Some(a);
            i = j;
        }
        match acc {
            Some(id) => Atom::Local(id),
            None => Atom::prim(PrimOp::MakeArray, vec![]),
        }
    }

    /// `"a${b}c"`. Literal pieces are already pooled, and expression pieces are
    /// turned into strings.
    fn interp(&mut self, parts: &[TypedInterpPart]) -> Atom {
        let str_t = self.temps.string;
        let mut args = Vec::with_capacity(parts.len());
        for part in parts {
            let a = match part {
                TypedInterpPart::Str(c) => self.let_(str_t, Atom::Const(*c)),
                TypedInterpPart::Expr(ex) => {
                    let v = self.operand(ex);
                    self.let_(str_t, Atom::prim(PrimOp::ToString, vec![v]))
                }
            };
            args.push(a);
        }
        match args.len() {
            1 => Atom::Local(args[0]),
            _ => Atom::prim(PrimOp::StringConcatMany, args),
        }
    }

    /// `<<..>>` in value position. Every segment evaluates to a `Binary`.
    fn bin_lit(&mut self, segs: &[TypedBinSeg]) -> Atom {
        let bin_t = self.temps.binary;
        let mut out = Vec::with_capacity(segs.len());
        for seg in segs {
            let id = match seg {
                TypedBinSeg::Int { value, bits } => {
                    let v = self.operand(value);
                    let b = self.operand(bits);
                    self.let_(bin_t, Atom::prim(PrimOp::BinaryFromInt, vec![v, b]))
                }
                TypedBinSeg::Binary { value, bits } => {
                    let v = self.operand(value);
                    match bits {
                        Some(b) => {
                            let b = self.operand(b);
                            self.let_(bin_t, Atom::prim(PrimOp::BinaryTake, vec![v, b]))
                        }
                        None => v,
                    }
                }
                TypedBinSeg::Utf8 { value } => {
                    let v = self.operand(value);
                    self.let_(bin_t, Atom::prim(PrimOp::BinaryFromString, vec![v]))
                }
            };
            out.push(id);
        }
        match out.len() {
            1 => Atom::Local(out[0]),
            _ => Atom::prim(PrimOp::BinaryConcatMany, out),
        }
    }

    /// Lower `arr[i] or <pure atom>` to one [`PrimOp::ArrayIndexOr`], with no
    /// `Option` cell. The ordinary lowering boxes a `Some` that the next step
    /// takes apart and drops, which is one heap allocation per index.
    ///
    /// This only fuses the two when the failure arm is a pure atom. The fused
    /// op always evaluates the default, so anything with a side effect or
    /// control flow must keep the lazy match.
    fn index_or(&mut self, scrut: &TypedExpr, arms: &[TypedArm]) -> Option<CoreExpr> {
        let TypedExpr::Index { recv, index, .. } = scrut else {
            return None;
        };
        let [a, b] = arms else { return None };
        if a.guard.is_some() || b.guard.is_some() {
            return None;
        }
        // The payload arm is `Some(v) -> v`, the other `None -> <atom>`.
        let payload = |arm: &TypedArm| match (&arm.pat, &arm.body) {
            (
                TypedPat::Ctor { fields, .. },
                TypedExpr::Var {
                    place: ValueRef::Local(read),
                    ..
                },
            ) => match fields.as_slice() {
                [TypedPat::Bind(bind)] if bind.id == *read => Some(()),
                _ => None,
            },
            _ => None,
        };
        let empty_ctor =
            |p: &TypedPat| matches!(p, TypedPat::Ctor { fields, .. } if fields.is_empty());
        let pure = |e: &TypedExpr| matches!(e, TypedExpr::Const { .. } | TypedExpr::Var { .. });

        let dflt = match (payload(a), payload(b)) {
            (Some(()), None) if empty_ctor(&b.pat) && pure(&b.body) => &b.body,
            (None, Some(())) if empty_ctor(&a.pat) && pure(&a.body) => &a.body,
            _ => return None,
        };

        let r = self.operand(recv);
        let i = self.operand(index);
        let d = self.operand(dflt);
        Some(CoreExpr::Tail(Atom::prim(
            PrimOp::ArrayIndexOr,
            vec![r, i, d],
        )))
    }

    /// Lower `arms` against an already-bound `scrut`.
    ///
    /// Arms are lowered front to back so locals are minted in source order, but
    /// the tree is assembled back to front. Every failure edge of an arm that
    /// can fail `Goto`s one [`CoreExpr::LetCont`] with no parameters that
    /// holds the remaining arms. So each arm body is lowered once, however many
    /// edges carry on there.
    ///
    /// An arm with no failure edge is put at the front of the entry `Match`
    /// instead, so a run of arms that can't fail keeps the flat shape of a
    /// single `Match`. The innermost starting point is a `Match` with no arms,
    /// which the VM loads as a stop for a value no arm matched.
    fn match_arms(&mut self, scrut: LocalId, arms: &[TypedArm], result_ty: RTy) -> CoreExpr {
        let mut lowered = Vec::with_capacity(arms.len());
        for arm in arms {
            let fall = if arm_fallible(arm) {
                ArmFall::Join(self.fresh_join())
            } else {
                ArmFall::Unreachable
            };
            let mut nested = Vec::new();
            let pat = self.pattern(&arm.pat, scrut, &mut nested);
            let leaf = match &arm.guard {
                None => ArmLeaf::Body(&arm.body),
                Some(guard) => ArmLeaf::Guarded {
                    guard,
                    body: &arm.body,
                },
            };
            let body = self.nested_arm_body(nested.into(), leaf, fall, result_ty);
            lowered.push((pat, body, fall));
        }

        // The arms after this one, kept unassembled so an arm that can't fail
        // can join the entry match's arms instead of adding another jump.
        let assemble = |entry_arms: VecDeque<(CorePat, CoreExpr)>,
                        wrap: Option<(JoinId, CoreExpr)>| {
            let entry = CoreExpr::Match {
                scrut,
                arms: entry_arms.into(),
                ty: result_ty,
            };
            match wrap {
                Some((id, cont)) => CoreExpr::LetCont {
                    id,
                    cont: Box::new(cont),
                    body: Box::new(entry),
                },
                None => entry,
            }
        };
        let mut entry_arms: VecDeque<(CorePat, CoreExpr)> = VecDeque::new();
        let mut wrap: Option<(JoinId, CoreExpr)> = None;
        for (pat, body, fall) in lowered.into_iter().rev() {
            match fall {
                // With live failure edges, seal everything below as this arm's
                // continuation. A head that can fail to match also needs a
                // wildcard arm that sends a miss to the same join.
                ArmFall::Join(j) if self.join_used(j) => {
                    let cont = assemble(entry_arms, wrap.take());
                    let head_refutable = matches!(pat, CorePat::Lit(_) | CorePat::Ctor { .. });
                    entry_arms = VecDeque::from([(pat, body)]);
                    if head_refutable {
                        entry_arms.push_back((CorePat::Wild, self.goto_join(j)));
                    }
                    wrap = Some((j, cont));
                }
                // No failure edge could fire, so the arm stays flat. `Join`
                // lands here too when nothing used its join.
                ArmFall::Join(_) | ArmFall::Unreachable => entry_arms.push_front((pat, body)),
            }
        }
        assemble(entry_arms, wrap)
    }

    /// Lower `p` to a flat [`CorePat`] head. Structure `CorePat` can't carry,
    /// such as a compound `Ctor` field or a `Tuple` or `Array` pattern, is
    /// bound to a new local and pushed to `nested`. [`Self::nested_arm_body`]
    /// then flattens it into `Match` and `Let` nodes inside the arm body.
    fn pattern<'p>(
        &mut self,
        p: &'p TypedPat,
        scrut: LocalId,
        nested: &mut Vec<(LocalId, &'p TypedPat)>,
    ) -> CorePat {
        match p {
            TypedPat::Wild { .. } => CorePat::Wild,
            TypedPat::Bind(b) => {
                let id = self.fresh(b.ty);
                self.bind(b.id, id);
                CorePat::Bind(CoreBind::new(id, b.ty))
            }
            TypedPat::Lit { value, .. } => CorePat::Lit(*value),
            TypedPat::Ctor {
                variant, fields, ..
            } => {
                // `fields` is exactly the variant's arity, so the payload binds
                // line up with the declared fields.
                let mut binds = Vec::with_capacity(fields.len());
                for fp in fields {
                    let id = self.fresh(fp.ty());
                    // The wildcard is on purpose. Every compound pattern kind,
                    // now or later, goes to the nested queue.
                    #[allow(unknown_lints, wildcard_over_own_enum)]
                    match fp {
                        TypedPat::Wild { .. } => {}
                        TypedPat::Bind(b) => self.bind(b.id, id),
                        inner => nested.push((id, inner)),
                    }
                    binds.push(CoreBind::new(id, fp.ty()));
                }
                CorePat::Ctor {
                    variant: *variant,
                    fields: binds,
                }
            }
            // No `CorePat` head discriminates these, so the arm always enters
            // and the destructure happens in the body.
            TypedPat::Tuple { .. }
            | TypedPat::Array { .. }
            | TypedPat::Bin { .. }
            | TypedPat::Or { .. }
            | TypedPat::Range { .. } => {
                nested.push((scrut, p));
                CorePat::Wild
            }
        }
    }

    /// The failure edge, which is a `Goto` to the arm's fallthrough join.
    /// [`ArmFall::Unreachable`] means [`arm_fallible`] proved no such edge
    /// exists, so getting here is a lowering bug. Any expression put in its
    /// place would change what a failed match evaluates to, so this aborts.
    #[allow(clippy::panic)]
    fn arm_fallthrough(&mut self, fall: ArmFall) -> CoreExpr {
        match fall {
            ArmFall::Join(j) => self.goto_join(j),
            ArmFall::Unreachable => panic!(
                "internal compiler error: failure edge lowered for a match arm classified \
                 infallible. Report this as a compiler bug."
            ),
        }
    }

    /// `if cond { then } else { <fallthrough> }`, sealed from `mark`. Every
    /// check in a pattern arm that can fail lowers to this shape.
    fn guard_if(
        &mut self,
        mark: usize,
        cond: LocalId,
        then: CoreExpr,
        fall: ArmFall,
        result_ty: RTy,
    ) -> CoreExpr {
        let els = self.arm_fallthrough(fall);
        self.seal(
            mark,
            CoreExpr::If {
                cond,
                then: Box::new(then),
                els: Box::new(els),
                ty: result_ty,
            },
        )
    }

    /// Lower an or-pattern's alternatives the same way [`Self::match_arms`]
    /// lowers arms. The failure edges of each alternative that can fail `Goto`
    /// a join that tries the next one, and the last one falls to `outer`, so
    /// none is lowered twice.
    ///
    /// The commit path always falls to `outer`, never to a try-the-next join,
    /// because the first matching alternative commits and a later miss goes to
    /// the next arm. With no bindings, the commit path is one shared join
    /// ([`ArmLeaf::OrCommit`]). An or-pattern that binds names lowers it once
    /// per alternative ([`ArmLeaf::OrBody`]). Each alternative binds the same
    /// [`BindingId`]s to its own locals, and a continuation with no parameters
    /// can't receive them.
    fn or_alternatives<'p>(
        &mut self,
        scrut: LocalId,
        alts: &'p [TypedPat],
        rest_nested: Vec<(LocalId, &'p TypedPat)>,
        leaf: ArmLeaf<'p>,
        outer: ArmFall,
        result_ty: RTy,
    ) -> CoreExpr {
        let commit = (!alts.iter().any(|a| !pat_binds(a).is_empty())).then(|| self.fresh_join());

        let mut lowered: Vec<(Option<JoinId>, CoreExpr)> = Vec::with_capacity(alts.len());
        for (i, alt) in alts.iter().enumerate() {
            // An alternative that can't fail ends the chain. Anything written
            // after it can never be reached and is never lowered.
            let terminal = i + 1 == alts.len() || !nested_fallible(alt);
            let fall_join = (!terminal).then(|| self.fresh_join());
            let fall = match fall_join {
                Some(j) => ArmFall::Join(j),
                None => outer,
            };
            let alt_leaf = match commit {
                Some(cj) => ArmLeaf::OrCommit(cj),
                None => ArmLeaf::OrBody {
                    rest_nested: rest_nested.clone(),
                    leaf: Box::new(leaf.clone()),
                    outer,
                },
            };
            let e = self.nested_arm_body(VecDeque::from([(scrut, alt)]), alt_leaf, fall, result_ty);
            lowered.push((fall_join, e));
            if terminal {
                break;
            }
        }

        // Assemble back to front. An alternative whose join some failure edge
        // targeted seals the ones after it as that join's continuation. One
        // whose join went unused replaces them, and the thrown-away subtree's
        // `Goto` counts are taken back.
        let Some((_, mut cur)) = lowered.pop() else {
            return self.arm_fallthrough(outer);
        };
        while let Some((fall_join, e)) = lowered.pop() {
            cur = match fall_join {
                Some(j) if self.join_used(j) => CoreExpr::LetCont {
                    id: j,
                    cont: Box::new(cur),
                    body: Box::new(e),
                },
                _ => {
                    self.retract_gotos(&cur);
                    e
                }
            };
        }
        match commit {
            Some(cj) => {
                let cont = self.nested_arm_body(rest_nested.into(), leaf, outer, result_ty);
                CoreExpr::LetCont {
                    id: cj,
                    cont: Box::new(cont),
                    body: Box::new(cur),
                }
            }
            None => cur,
        }
    }

    /// Flatten `nested` sub-patterns into `Match` and `Let` nodes around the
    /// arm body. Structure that can't fail becomes `Let` field reads on the
    /// current spine. Structure that can fail becomes an inner `Match` or `If`
    /// whose failure edge is `fall`, so a nested miss carries on matching the
    /// way the source says.
    fn nested_arm_body<'p>(
        &mut self,
        mut nested: VecDeque<(LocalId, &'p TypedPat)>,
        leaf: ArmLeaf<'p>,
        fall: ArmFall,
        result_ty: RTy,
    ) -> CoreExpr {
        let mark = self.spine.len();
        loop {
            let Some((v, p)) = nested.pop_front() else {
                let tail = match leaf {
                    ArmLeaf::Body(e) => self.expr_tail(e, result_ty, SpinePos::Nested),
                    ArmLeaf::Guarded { guard, body } => {
                        let cond = self.operand(guard);
                        let then = self.expr_as(body, result_ty);
                        // This is already sealed from `mark`, so the seal below
                        // does nothing.
                        self.guard_if(mark, cond, then, fall, result_ty)
                    }
                    ArmLeaf::BinCont(bc) => self.bin_segments(*bc, fall, result_ty),
                    ArmLeaf::OrBody {
                        rest_nested,
                        leaf,
                        outer,
                    } => self.nested_arm_body(rest_nested.into(), *leaf, outer, result_ty),
                    ArmLeaf::OrCommit(j) => self.goto_join(j),
                };
                return self.seal(mark, tail);
            };
            match p {
                TypedPat::Wild { .. } => {}
                TypedPat::Bind(b) => self.bind(b.id, v),
                TypedPat::Tuple { elems, .. } => {
                    let mut tmp = Vec::with_capacity(elems.len());
                    for (j, elem) in elems.iter().enumerate() {
                        let ej =
                            self.let_(elem.ty(), Atom::prim(PrimOp::TupleField(idx16(j)), vec![v]));
                        tmp.push((ej, elem));
                    }
                    for r in tmp.into_iter().rev() {
                        nested.push_front(r);
                    }
                }
                TypedPat::Ctor { .. } | TypedPat::Lit { .. } => {
                    let mut inner = Vec::new();
                    let sub = self.pattern(p, v, &mut inner);
                    for r in inner.into_iter().rev() {
                        nested.push_front(r);
                    }
                    let body = self.nested_arm_body(nested, leaf, fall, result_ty);
                    let els = self.arm_fallthrough(fall);
                    let m = CoreExpr::Match {
                        scrut: v,
                        arms: vec![(sub, body), (CorePat::Wild, els)],
                        ty: result_ty,
                    };
                    return self.seal(mark, m);
                }
                TypedPat::Array {
                    elem_ty,
                    len: len_c,
                    prefix,
                    rest,
                    ..
                } => {
                    let vty = self.bind_ty(v);
                    let int_t = self.temps.int;
                    let bool_t = self.temps.bool;
                    let pre = prefix.len();
                    let len = self.let_(int_t, Atom::prim(PrimOp::ArrayLen, vec![v]));
                    let n = self.int_const(*len_c);
                    let cmp = match rest {
                        PatRest::None => PrimOp::IntEq,
                        PatRest::Discard | PatRest::Bind(_) => PrimOp::IntGe,
                    };
                    let ok = self.let_(bool_t, Atom::prim(cmp, vec![len, n]));
                    let then = {
                        let then_mark = self.spine.len();
                        let mut tmp = Vec::with_capacity(pre);
                        for (j, ep) in prefix.iter().enumerate() {
                            let ej = self
                                .let_(*elem_ty, Atom::prim(PrimOp::ArrayElem(idx16(j)), vec![v]));
                            tmp.push((ej, ep));
                        }
                        if let PatRest::Bind(b) = rest {
                            let n2 = self.int_const(*len_c);
                            let tail = self.let_(vty, Atom::prim(PrimOp::ArrayDrop, vec![v, n2]));
                            self.bind(b.id, tail);
                        }
                        for r in tmp.into_iter().rev() {
                            nested.push_front(r);
                        }
                        let inner = self.nested_arm_body(nested, leaf, fall, result_ty);
                        self.seal(then_mark, inner)
                    };
                    return self.guard_if(mark, ok, then, fall, result_ty);
                }
                TypedPat::Range { lo, hi, .. } => {
                    let int_t = self.temps.int;
                    let bool_t = self.temps.bool;
                    let lo = self.let_(int_t, Atom::Const(*lo));
                    let ge = self.let_(bool_t, Atom::prim(PrimOp::IntGe, vec![v, lo]));
                    let then = {
                        let then_mark = self.spine.len();
                        let hi = self.let_(int_t, Atom::Const(*hi));
                        let lt = self.let_(bool_t, Atom::prim(PrimOp::IntLt, vec![v, hi]));
                        let inner = self.nested_arm_body(nested, leaf, fall, result_ty);
                        self.guard_if(then_mark, lt, inner, fall, result_ty)
                    };
                    return self.guard_if(mark, ge, then, fall, result_ty);
                }
                TypedPat::Or { alts, .. } => {
                    let rest_nested: Vec<_> = nested.into_iter().collect();
                    let body = self.or_alternatives(v, alts, rest_nested, leaf, fall, result_ty);
                    return self.seal(mark, body);
                }
                TypedPat::Bin {
                    zero, segs, rest, ..
                } => {
                    let int_t = self.temps.int;
                    let total = self.let_(int_t, Atom::prim(PrimOp::BinaryBitSize, vec![v]));
                    let cursor = self.int_const(*zero);
                    let after: Vec<_> = nested.into_iter().collect();
                    let body = self.bin_segments(
                        BinCont {
                            bin: v,
                            total,
                            cursor,
                            zero: *zero,
                            segs,
                            rest: *rest,
                            after,
                            then: leaf,
                        },
                        fall,
                        result_ty,
                    );
                    return self.seal(mark, body);
                }
            }
        }
    }

    /// Lower one step of a `<<>>` pattern. Check the bounds, read, match the
    /// segment's value pattern, then recurse through a `BinCont` leaf so those
    /// bindings are in scope for the next segment's `size(...)`. When `segs` is
    /// empty, emit the trailing `..rest` bind or exact-length check.
    fn bin_segments<'p>(&mut self, bc: BinCont<'p>, fall: ArmFall, result_ty: RTy) -> CoreExpr {
        let mark = self.spine.len();
        let int_t = self.temps.int;
        let bool_t = self.temps.bool;
        let bin_ty = self.bind_ty(bc.bin);
        let BinCont {
            bin,
            total,
            cursor,
            zero,
            segs,
            rest,
            after,
            then,
        } = bc;

        let Some((seg, tail_segs)) = segs.split_first() else {
            match rest {
                PatRest::Bind(b) => {
                    let rem = self.let_(int_t, Atom::prim(PrimOp::IntSub, vec![total, cursor]));
                    let rv = self.let_(
                        bin_ty,
                        Atom::prim(PrimOp::BinaryView, vec![bin, cursor, rem]),
                    );
                    self.bind(b.id, rv);
                    let body = self.nested_arm_body(after.into(), then, fall, result_ty);
                    return self.seal(mark, body);
                }
                PatRest::Discard => {
                    let body = self.nested_arm_body(after.into(), then, fall, result_ty);
                    return self.seal(mark, body);
                }
                PatRest::None => {
                    let ok = self.let_(bool_t, Atom::prim(PrimOp::IntEq, vec![cursor, total]));
                    let body = self.nested_arm_body(after.into(), then, fall, result_ty);
                    return self.guard_if(mark, ok, body, fall, result_ty);
                }
            }
        };

        let cont = |cursor: LocalId| BinCont {
            bin,
            total,
            cursor,
            zero,
            segs: tail_segs,
            rest,
            after: after.clone(),
            then: then.clone(),
        };

        match seg {
            // A `<<'literal'>>` segment matches its UTF-8 bytes as a prefix.
            TypedBinPatSeg::Utf8Literal { bytes, bits } => {
                let pfx = self.let_(bin_ty, Atom::Const(*bytes));
                let ok = self.let_(
                    bool_t,
                    Atom::prim(PrimOp::BinaryMatchPrefix, vec![bin, cursor, pfx]),
                );
                let then_e = {
                    let tm = self.spine.len();
                    let w = self.int_const(*bits);
                    let cur2 = self.let_(int_t, Atom::prim(PrimOp::IntAdd, vec![cursor, w]));
                    let inner = self.bin_segments(cont(cur2), fall, result_ty);
                    self.seal(tm, inner)
                };
                self.guard_if(mark, ok, then_e, fall, result_ty)
            }
            // A `:utf8` codepoint, whose width varies. `BinaryReadUtf8` gives back
            // `(codepoint, nbits)`, where `nbits == 0` means decoding failed.
            TypedBinPatSeg::Utf8 { value } => {
                let pair_ty = self.temps.int_pair;
                let pair = self.let_(
                    pair_ty,
                    Atom::prim(PrimOp::BinaryReadUtf8, vec![bin, cursor]),
                );
                let cp = self.let_(int_t, Atom::prim(PrimOp::TupleField(0), vec![pair]));
                let nbits = self.let_(int_t, Atom::prim(PrimOp::TupleField(1), vec![pair]));
                let z = self.int_const(zero);
                let ok = self.let_(bool_t, Atom::prim(PrimOp::IntGt, vec![nbits, z]));
                let then_e = {
                    let tm = self.spine.len();
                    let cur2 = self.let_(int_t, Atom::prim(PrimOp::IntAdd, vec![cursor, nbits]));
                    let leaf2 = ArmLeaf::BinCont(Box::new(cont(cur2)));
                    let inner =
                        self.nested_arm_body(VecDeque::from([(cp, value)]), leaf2, fall, result_ty);
                    self.seal(tm, inner)
                };
                self.guard_if(mark, ok, then_e, fall, result_ty)
            }
            // An Int or Binary segment, whose width is the size expression in
            // bits. A `:binary` segment with no size takes the rest.
            TypedBinPatSeg::Int { bits, value } => self.bin_read(
                mark,
                BinWalk { bin, total, cursor },
                SegWidth::Sized(bits),
                PrimOp::BinaryReadInt,
                int_t,
                value,
                cont,
                fall,
                result_ty,
            ),
            TypedBinPatSeg::Binary { bits, value } => {
                let width = match bits {
                    Some(bits) => SegWidth::Sized(bits),
                    None => SegWidth::Remainder,
                };
                self.bin_read(
                    mark,
                    BinWalk { bin, total, cursor },
                    width,
                    PrimOp::BinaryView,
                    bin_ty,
                    value,
                    cont,
                    fall,
                    result_ty,
                )
            }
        }
    }

    /// The shared tail of a fixed-position `<<>>` segment. Lower the width,
    /// checking the read's bounds as [`SegWidth`] says, read the value, then
    /// match the segment's value pattern and carry on.
    #[allow(clippy::too_many_arguments)]
    fn bin_read<'p>(
        &mut self,
        mark: usize,
        walk: BinWalk,
        width: SegWidth<'p>,
        read_op: PrimOp,
        val_ty: RTy,
        value: &'p TypedPat,
        cont: impl Fn(LocalId) -> BinCont<'p>,
        fall: ArmFall,
        result_ty: RTy,
    ) -> CoreExpr {
        let int_t = self.temps.int;
        let bool_t = self.temps.bool;
        let BinWalk { bin, total, cursor } = walk;
        let read = |lo: &mut Self, width: LocalId, end: LocalId, fall: ArmFall| -> CoreExpr {
            let tm = lo.spine.len();
            let sv = lo.let_(val_ty, Atom::prim(read_op, vec![bin, cursor, width]));
            let leaf2 = ArmLeaf::BinCont(Box::new(cont(end)));
            let inner = lo.nested_arm_body(VecDeque::from([(sv, value)]), leaf2, fall, result_ty);
            lo.seal(tm, inner)
        };
        match width {
            SegWidth::Sized(bits) => {
                let width = self.operand(bits);
                let end = self.let_(int_t, Atom::prim(PrimOp::IntAdd, vec![cursor, width]));
                let ok = self.let_(bool_t, Atom::prim(PrimOp::IntLe, vec![end, total]));
                let then_e = read(self, width, end, fall);
                self.guard_if(mark, ok, then_e, fall, result_ty)
            }
            SegWidth::Remainder => {
                let width = self.let_(int_t, Atom::prim(PrimOp::IntSub, vec![total, cursor]));
                let end = self.let_(int_t, Atom::prim(PrimOp::IntAdd, vec![cursor, width]));
                let then_e = read(self, width, end, fall);
                self.seal(mark, then_e)
            }
        }
    }
}

// These answer "does lowering this arm ever use its `ArmFall`?" by looking at
// the syntax. `match_arms` asks before lowering, so an arm that can't fail
// mints no join. The answer is only safe one way: `false` must mean no failure
// edge is lowered, but `true` may be wrong and cost an unused `JoinId`.

/// Whether lowering `arm` can produce a failure edge after its `CorePat` head
/// matched. That happens with a guard, or with structure that can fail and
/// that the flat head can't carry.
fn arm_fallible(arm: &TypedArm) -> bool {
    arm.guard.is_some() || head_fallible(&arm.pat)
}

/// [`arm_fallible`] for the arm's own pattern. The `Match` itself checks a
/// `Lit` head, so a miss there never reaches a failure continuation. Only
/// compound `Ctor` fields make it into the nested queue.
fn head_fallible(p: &TypedPat) -> bool {
    // The wildcard is on purpose. Anything that isn't a bare head, new
    // pattern kinds included, goes to the general nested check.
    #[allow(unknown_lints, wildcard_over_own_enum)]
    match p {
        TypedPat::Wild { .. } | TypedPat::Bind(_) | TypedPat::Lit { .. } => false,
        TypedPat::Ctor { fields, .. } => fields.iter().any(nested_fallible),
        _ => nested_fallible(p),
    }
}

/// Whether a remaining sub-pattern can fail once it's queued. This follows
/// [`Lower::nested_arm_body`]. A `Tuple` only reads fields, but a nested
/// `Ctor` or `Lit` always lowers a failure edge, even for a type with one
/// variant.
fn nested_fallible(p: &TypedPat) -> bool {
    match p {
        TypedPat::Wild { .. } | TypedPat::Bind(_) => false,
        TypedPat::Tuple { elems, .. } => elems.iter().any(nested_fallible),
        TypedPat::Lit { .. }
        | TypedPat::Ctor { .. }
        | TypedPat::Array { .. }
        | TypedPat::Bin { .. }
        | TypedPat::Or { .. }
        | TypedPat::Range { .. } => true,
    }
}

