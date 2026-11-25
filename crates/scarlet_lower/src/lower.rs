//! Lowering turns typed IR into Core IR.
//!
//! [`lower_fn`] turns one [`TypedFn`] into a [`CoreFn`]. Every subexpression
//! becomes a `Let{bind,rhs,body}`, so evaluation order is spelled out as a
//! right-nested spine of `Let`s and every operand is a [`LocalId`].
//!
//! Lowering never fails. It takes `&TypedFn` and returns `CoreFn` with no
//! `Result`, because everything it would have to ask for is already a field: a
//! type is an [`RTy`], a name is a [`ValueRef`] and a callee is a
//! [`TypedCallee`].
//!
//! ## How the builder works
//!
//! ANF is built from the inside out. Recursing into a subexpression gives back
//! the `LocalId` its value lands in, plus a list of nodes to put in front.
//! [`Lower`] collects those in `spine`, and [`Lower::sealed`] folds them
//! around what comes last once it's known. Each arm of an `If` or a `Match` is
//! sealed on its own, so no `Let` moves across a branch.
//!
//! ## Where a value goes
//!
//! An expression in tail position is returned ([`Dest::Return`]). An `if`,
//! `match` or short-circuit whose value is needed further on is the body of a
//! [`CoreExpr::Join`] with one parameter. Each of its ends jumps there with its
//! value ([`Dest::Join`]), and the parameter is the operand. What it branches on
//! is worked out in the body too, beside the branch, where a backend can make one
//! instruction of the two.
//!
//! ## The constant pool is already final
//!
//! `lower` never interns. Every `Atom::Const` names a [`ConstId`] read from
//! the node. Even integers no source literal writes, like an array pattern's
//! length or a `<<>>` walk's bit cursor, are pooled when the typed IR is made,
//! so the pool is already final when `lower` runs.

use std::collections::{BTreeSet, VecDeque};

use super::spine::Frame;
use super::{
    Atom, Callee, ConstId, CoreBind, CoreExpr, CoreFn, CorePat, JoinId, Load, LocalId, PrimOp,
};
use crate::program::{InternalLinkError, LinkError, Table};
use crate::typed_ir::{
    BindingId, PatRest, RTy, TempTys, TypedArm, TypedArrayElem, TypedBinPatSeg, TypedBinSeg,
    TypedBind, TypedCallee, TypedExpr, TypedFn, TypedInterpPart, TypedPat, ValueRef, or_binds,
    pat_binds,
};
use scarlet_ir::tivec::TiVec;

type R<T> = Result<T, LinkError>;

/// Narrow a [`PrimOp`] field or element index to `u16`, checking it fits.
/// Truncating it instead would read the wrong field.
fn idx16(i: usize) -> R<u16> {
    Ok(u16::try_from(i).map_err(|_| InternalLinkError::IndexTooWide(i))?)
}

/// A lowered function, with the type of every local in it.
pub(crate) struct Lowered {
    pub core: CoreFn,
    pub locals: TiVec<LocalId, RTy>,
}

/// Lower one function. Each param is bound under the [`BindingId`] its
/// [`TypedBind`] carries.
pub(crate) fn lower_fn(temps: TempTys, f: &TypedFn) -> R<Lowered> {
    let mut lo = Lower::new(temps, f.binds);
    let params: Vec<CoreBind> = f
        .params
        .iter()
        .map(|p| {
            let id = lo.fresh(p.ty)?;
            lo.bind(p.id, id)?;
            Ok(CoreBind::new(id, p.ty))
        })
        .collect::<R<_>>()?;
    let body = lo.sealed(|lo| lo.expr_tail(&f.body, Dest::Return, SpinePos::Outer))?;
    Ok(Lowered {
        core: CoreFn {
            params,
            body,
            ret_ty: f.ret,
        },
        locals: lo.tys,
    })
}

/// What becomes of the value of an expression that ends a tree.
#[derive(Clone, Copy)]
enum Dest {
    /// The function returns it.
    Return,
    /// It is the argument of this join.
    Join(JoinId),
}

/// The body of an arm, for where its pattern has matched.
#[derive(Clone, Copy)]
enum ArmBody<'p> {
    /// The pattern matches in one place, and the body goes there.
    Here { body: &'p TypedExpr, dest: Dest },
    /// It may match in several, and each jumps to the body with what it bound.
    Shared(&'p SharedBody),
}

/// A join that is the body of an arm, and the names it takes.
struct SharedBody {
    id: JoinId,
    binds: Vec<TypedBind>,
}

/// What [`Lower::nested_arm_body`] lowers once every remaining sub-pattern
/// has been bound.
#[derive(Clone)]
enum ArmLeaf<'p> {
    Body(ArmBody<'p>),
    Guarded {
        guard: &'p TypedExpr,
        body: ArmBody<'p>,
    },
    /// Carry on reading a `<<>>` pattern's remaining segments. We get here
    /// after the current segment's value pattern has bound its names, so those
    /// bindings are in scope for the next segment's `size(...)` expression.
    BinCont(Box<BinCont<'p>>),
    /// An alternative of an or-pattern that binds names has matched. Carry on
    /// with the sub-patterns queued after the or-pattern, then `leaf`. A failure
    /// there tries the next alternative, because its bindings may pass a guard
    /// this one's failed.
    OrRest {
        rest_nested: Vec<(LocalId, &'p TypedPat)>,
        leaf: Box<ArmLeaf<'p>>,
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
    /// The Int constant 0.
    zero: ConstId,
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

/// A lowered piece of a pattern arm, with the joins of its pattern that it jumps
/// to. A join no subtree jumps to is never declared, so an arm that can't fail
/// stays on the flat path with no continuation.
struct Tree {
    expr: CoreExpr,
    jumps: BTreeSet<JoinId>,
}

impl Tree {
    /// A subtree that jumps to no such join.
    fn leaf(expr: CoreExpr) -> Self {
        Tree {
            expr,
            jumps: BTreeSet::new(),
        }
    }

    /// A jump to `j` with `args`.
    fn jump(j: JoinId, args: Vec<LocalId>) -> Self {
        Tree {
            expr: CoreExpr::Jump { to: j, args },
            jumps: BTreeSet::from([j]),
        }
    }

    /// The failure edge to `j`, which takes nothing.
    fn fail(j: JoinId) -> Self {
        Tree::jump(j, Vec::new())
    }

    fn jumps_to(&self, j: JoinId) -> bool {
        self.jumps.contains(&j)
    }

    /// Declare `j` as `cont`, around this tree. `j`'s edges are all inside, so
    /// it is jumped to from nowhere else.
    fn with_cont(self, j: JoinId, params: Vec<CoreBind>, cont: Tree) -> Tree {
        let mut jumps = self.jumps;
        jumps.remove(&j);
        jumps.extend(cont.jumps);
        Tree {
            expr: CoreExpr::Join {
                id: j,
                params,
                cont: Box::new(cont.expr),
                body: Box::new(self.expr),
            },
            jumps,
        }
    }
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
    /// The [`RTy`] of every minted [`LocalId`].
    tys: TiVec<LocalId, RTy>,
    /// Maps `BindingId → LocalId`, densely over `TypedFn::binds`. Different
    /// source bindings have different `BindingId`s, so nothing is shadowed.
    binds: Vec<Option<LocalId>>,
    /// What each binding in the current scope replaced, newest last. A scope
    /// ([`Self::scoped`]) puts these back when it ends, so a binding made in
    /// one arm or alternative can't be read from the next.
    undo: Vec<(BindingId, Option<LocalId>)>,
    /// The nodes emitted so far in the current straight-line segment.
    /// [`Self::sealed`] folds them around what the segment ends in.
    spine: Vec<Frame>,
    /// How many join labels have been made.
    joins: u32,
}

impl Lower {
    fn new(temps: TempTys, binds: usize) -> Self {
        Lower {
            temps,
            tys: TiVec::new(),
            binds: vec![None; binds],
            undo: Vec::new(),
            spine: Vec::new(),
            joins: 0,
        }
    }

    fn fresh(&mut self, ty: RTy) -> R<LocalId> {
        self.tys
            .push(ty)
            .map_err(|_| LinkError::TooMany(Table::Locals))
    }

    /// The `RTy` recorded for `id` when it was minted.
    fn bind_ty(&self, id: LocalId) -> R<RTy> {
        self.tys
            .get(id)
            .copied()
            .ok_or(InternalLinkError::UntypedLocal(id).into())
    }

    /// Record the local `b` names. A `BindingId` out of range means the
    /// elaborator miscounted the body's binders.
    fn bind(&mut self, b: BindingId, id: LocalId) -> R<()> {
        let slot = self
            .binds
            .get_mut(b.0)
            .ok_or(InternalLinkError::BindingOutOfRange(b))?;
        self.undo.push((b, slot.replace(id)));
        Ok(())
    }

    /// The local a [`BindingId`] names in the current scope. A `TypedFn`
    /// always binds before it reads, so a miss is a compiler bug. It must
    /// never be a default, because any default would be a live local, and
    /// the program would run and return a wrong answer.
    fn local(&self, b: BindingId) -> R<LocalId> {
        match self.binds.get(b.0) {
            Some(Some(id)) => Ok(*id),
            Some(None) => Err(InternalLinkError::UnboundBinding(b).into()),
            None => Err(InternalLinkError::BindingOutOfRange(b).into()),
        }
    }

    /// Push `let %id = rhs` onto the current spine and return `%id`.
    fn let_(&mut self, ty: RTy, rhs: Atom) -> R<LocalId> {
        let id = self.fresh(ty)?;
        self.spine.push(Frame::Let {
            bind: CoreBind::new(id, ty),
            rhs,
        });
        Ok(id)
    }

    /// Mint a new join-point label, which nothing jumps to yet.
    fn fresh_join(&mut self) -> R<JoinId> {
        let j = JoinId(self.joins);
        self.joins = self
            .joins
            .checked_add(1)
            .ok_or(LinkError::TooMany(Table::Joins))?;
        Ok(j)
    }

    /// Bind an already-pooled `Int` constant to a new local. The elaborator
    /// interned every integer a lowered body uses.
    fn int_const(&mut self, c: ConstId) -> R<LocalId> {
        let int_t = self.temps.int;
        self.let_(int_t, Atom::Const(c))
    }

    /// Run `f` in a scope of its own. Every binding it makes is undone at the
    /// end.
    fn scoped<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        let outer = std::mem::take(&mut self.undo);
        let r = f(self);
        let inner = std::mem::replace(&mut self.undo, outer);
        // Each entry's slot was in range when it was bound, and the table
        // never shrinks.
        for (b, old) in inner.into_iter().rev() {
            if let Some(slot) = self.binds.get_mut(b.0) {
                *slot = old;
            }
        }
        r
    }

    /// Run `f` in a new spine segment and scope, then fold every node it
    /// pushed around the tree it produced, innermost first.
    fn sealed_tree(&mut self, f: impl FnOnce(&mut Self) -> R<Tree>) -> R<Tree> {
        let outer = std::mem::take(&mut self.spine);
        let tree = self.scoped(f);
        let segment = std::mem::replace(&mut self.spine, outer);
        let Tree { expr, jumps } = tree?;
        let expr = Frame::wrap_all(segment, expr);
        Ok(Tree { expr, jumps })
    }

    /// [`Self::sealed_tree`] for an expression that jumps nowhere.
    fn sealed(&mut self, f: impl FnOnce(&mut Self) -> R<CoreExpr>) -> R<CoreExpr> {
        Ok(self.sealed_tree(|lo| f(lo).map(Tree::leaf))?.expr)
    }

    /// Lower `e` as a tree of its own, such as a branch. It opens a new spine.
    fn expr_as(&mut self, e: &TypedExpr, dest: Dest) -> R<CoreExpr> {
        self.sealed(|lo| lo.expr_tail(e, dest, SpinePos::Nested))
    }

    /// Lower `e`, whose value goes to `dest`. The `Let`/`Seq` spine is walked in
    /// a loop, because a module toplevel is one `Let` per declaration and can be
    /// thousands deep.
    fn expr_tail(&mut self, e: &TypedExpr, dest: Dest, spine: SpinePos) -> R<CoreExpr> {
        let mut e = e;
        loop {
            match e {
                TypedExpr::Let {
                    bind, init, body, ..
                } => {
                    self.let_bind_at(bind, init, spine)?;
                    e = body;
                }
                TypedExpr::Seq { effect, body, .. } => {
                    let _ = self.operand(effect)?;
                    e = body;
                }
                TypedExpr::If {
                    cond, then, els, ..
                } => {
                    let cond = self.operand(cond)?;
                    let then = self.expr_as(then, dest)?;
                    let els = self.expr_as(els, dest)?;
                    return Ok(CoreExpr::If {
                        cond,
                        then: Box::new(then),
                        els: Box::new(els),
                    });
                }
                TypedExpr::Match {
                    ty, scrut, arms, ..
                } => {
                    if let Some(atom) = self.index_or(scrut, arms)? {
                        return self.end(atom, *ty, dest);
                    }
                    let scrut = self.operand(scrut)?;
                    return self.match_arms(scrut, arms, dest);
                }
                TypedExpr::And { lhs, rhs, .. } => {
                    return self.short_circuit(lhs, rhs, true, dest);
                }
                TypedExpr::Or { lhs, rhs, .. } => {
                    return self.short_circuit(lhs, rhs, false, dest);
                }
                _ => {
                    let (a, ty) = self.atom(e)?;
                    return self.end(a, ty, dest);
                }
            }
        }
    }

    /// The end of a tree whose value is `a`, of type `ty`.
    fn end(&mut self, a: Atom, ty: RTy, dest: Dest) -> R<CoreExpr> {
        Ok(match (a, dest) {
            (Atom::Call { callee, args }, Dest::Return) => CoreExpr::TailCall { callee, args },
            (a, Dest::Return) => CoreExpr::Ret(self.named(a, ty)?),
            (a, Dest::Join(to)) => CoreExpr::Jump {
                to,
                args: vec![self.named(a, ty)?],
            },
        })
    }

    /// The local that `a` is, or a new one that it is bound to.
    fn named(&mut self, a: Atom, ty: RTy) -> R<LocalId> {
        // The wildcard is on purpose. Any atom that isn't a local, now or
        // later, gets bound with a `Let`, and only an existing local skips it.
        #[allow(unknown_lints, wildcard_over_own_enum)]
        match a {
            Atom::Local(id) => Ok(id),
            other => self.let_(ty, other),
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
    fn let_bind_at(&mut self, bind: &TypedBind, init: &TypedExpr, spine: SpinePos) -> R<()> {
        let owns_slot = bind.global.is_some() || (spine == SpinePos::Outer && bind.named);
        let v = if owns_slot {
            self.let_bound(bind, init)?
        } else {
            self.operand(init)?
        };
        self.bind(bind.id, v)
    }

    /// `let bind = <init>` for a binding that owns its slot, and the store of one
    /// at module scope.
    ///
    /// It's normally its own `Let`, even when `init` already lowered to a
    /// local, so `let y = x` is two frame slots. What a join gives is bound by
    /// the join.
    fn let_bound(&mut self, bind: &TypedBind, init: &TypedExpr) -> R<LocalId> {
        let (a, ty) = self.atom(init)?;
        let id = if is_branch(init) {
            self.named(a, ty)?
        } else {
            self.let_(bind.ty, a)?
        };
        if let Some(slot) = bind.global {
            self.spine.push(Frame::SetGlobal { slot, value: id });
        }
        Ok(id)
    }

    /// Lower `e` to a `LocalId` operand, so it never leaves a bare atom.
    fn operand(&mut self, e: &TypedExpr) -> R<LocalId> {
        if let TypedExpr::Var {
            place: ValueRef::Local(b),
            ..
        } = e
        {
            return self.local(*b);
        }
        let (a, ty) = self.atom(e)?;
        self.named(a, ty)
    }

    /// Lower a branch whose value is needed further on: each of its ends jumps
    /// to a join, with the value as the argument.
    ///
    /// Where the left side of a short-circuit is a branch too, its join goes in
    /// front, so that a long chain of them is a chain and not a nest.
    fn join_operand(&mut self, e: &TypedExpr, ty: RTy) -> R<LocalId> {
        let join = self.fresh_join()?;
        let dest = Dest::Join(join);
        let body = match sides(e) {
            Some((lhs, rhs, and)) if is_branch(lhs) => self.short_circuit(lhs, rhs, and, dest)?,
            Some(_) | None => self.expr_as(e, dest)?,
        };
        let id = self.fresh(ty)?;
        self.spine.push(Frame::Join {
            id: join,
            params: vec![CoreBind::new(id, ty)],
            body: Box::new(body),
        });
        Ok(id)
    }

    /// Lower `e` to an `(Atom, RTy)` whose operands are all `LocalId`s.
    ///
    /// A `Let`/`Seq` spine here is straight-line code, so its bindings splice
    /// onto the current spine and its tail becomes the atom.
    fn atom(&mut self, e: &TypedExpr) -> R<(Atom, RTy)> {
        let mut e = e;
        loop {
            match e {
                TypedExpr::Let {
                    bind, init, body, ..
                } => {
                    self.let_bind_at(bind, init, SpinePos::Nested)?;
                    e = body;
                }
                TypedExpr::Seq { effect, body, .. } => {
                    let _ = self.operand(effect)?;
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
                    let id = self.local(b)?;
                    return Ok((Atom::Local(id), self.bind_ty(id)?));
                }
                ValueRef::Global(slot) => Atom::Load(Load::Global(slot)),
                ValueRef::Capture(idx) => Atom::Load(Load::Capture(idx)),
                ValueRef::SelfClosure => Atom::Load(Load::SelfClosure),
            },
            TypedExpr::Match { scrut, arms, .. }
                if let Some(atom) = self.index_or(scrut, arms)? =>
            {
                atom
            }
            // `Let` and `Seq` never get here, but naming them keeps the match
            // exhaustive without a wildcard.
            TypedExpr::Let { .. }
            | TypedExpr::Seq { .. }
            | TypedExpr::If { .. }
            | TypedExpr::Match { .. }
            | TypedExpr::And { .. }
            | TypedExpr::Or { .. } => Atom::Local(self.join_operand(e, ty)?),
            // The elaborator has already specialised `Add` to `IntAdd`, say.
            TypedExpr::Binary { op, lhs, rhs, .. } => {
                let l = self.operand(lhs)?;
                let r = self.operand(rhs)?;
                Atom::prim(*op, vec![l, r])
            }
            TypedExpr::Unary { op, operand, .. } => {
                let v = self.operand(operand)?;
                Atom::prim(*op, vec![v])
            }
            TypedExpr::Tuple { elems, .. } => {
                let args = self.operands(elems)?;
                Atom::prim(PrimOp::MakeTuple, args)
            }
            TypedExpr::TupleIndex { recv, idx, .. } => {
                let r = self.operand(recv)?;
                Atom::prim(PrimOp::TupleField(idx16(*idx)?), vec![r])
            }
            TypedExpr::Field { recv, idx, .. } => {
                let r = self.operand(recv)?;
                Atom::prim(PrimOp::Field(idx16(*idx)?), vec![r])
            }
            TypedExpr::Ctor { variant, args, .. } => {
                let fields = self.operands(args)?;
                Atom::Ctor {
                    variant: *variant,
                    fields,
                }
            }
            TypedExpr::Update {
                variant,
                base,
                sets,
                ..
            } => {
                let base = self.operand(base)?;
                let mut fields = Vec::with_capacity(sets.len());
                for (field, value) in sets {
                    fields.push((*field, self.operand(value)?));
                }
                fields.sort_by_key(|(field, _)| *field);
                Atom::Update {
                    base,
                    variant: *variant,
                    sets: fields,
                }
            }
            TypedExpr::Array { elems, .. } => return Ok((self.array(elems, ty)?, ty)),
            TypedExpr::Index { recv, index, .. } => {
                let r = self.operand(recv)?;
                let i = self.operand(index)?;
                Atom::prim(PrimOp::ArrayIndex, vec![r, i])
            }
            TypedExpr::Slice {
                recv, start, end, ..
            } => {
                let r = self.operand(recv)?;
                let s = self.operand(start)?;
                let e = self.operand(end)?;
                Atom::prim(PrimOp::ArraySlice, vec![r, s, e])
            }
            TypedExpr::Range { start, end, .. } => {
                let s = self.operand(start)?;
                let e = self.operand(end)?;
                Atom::prim(PrimOp::MakeRange, vec![s, e])
            }
            TypedExpr::Interp { parts, .. } => self.interp(parts)?,
            TypedExpr::BinLit { segs, .. } => self.bin_lit(segs)?,
            TypedExpr::Closure {
                func_idx, captures, ..
            } => {
                let captures = self.operands(captures)?;
                Atom::Closure {
                    func_idx: *func_idx,
                    captures,
                }
            }
            TypedExpr::Call { callee, args, .. } => self.call(callee, args)?,
        };
        Ok((a, ty))
    }

    fn operands(&mut self, es: &[TypedExpr]) -> R<Vec<LocalId>> {
        let mut out = Vec::with_capacity(es.len());
        for e in es {
            out.push(self.operand(e)?);
        }
        Ok(out)
    }

    /// A dynamic callee is evaluated first, then the arguments, left to right.
    fn call(&mut self, callee: &TypedCallee, args: &[TypedExpr]) -> R<Atom> {
        Ok(match callee {
            TypedCallee::Known(fi) => {
                let args = self.operands(args)?;
                Atom::Call {
                    callee: Callee::Known(*fi),
                    args,
                }
            }
            TypedCallee::SelfRec => {
                let args = self.operands(args)?;
                Atom::Call {
                    callee: Callee::Self_,
                    args,
                }
            }
            TypedCallee::Builtin { intrinsic } => {
                let args = self.operands(args)?;
                Atom::Intrinsic {
                    intrinsic: *intrinsic,
                    args,
                }
            }
            TypedCallee::Dynamic(f) => {
                let c = self.operand(f)?;
                let args = self.operands(args)?;
                Atom::Call {
                    callee: Callee::Local(c),
                    args,
                }
            }
        })
    }

    /// `a && b` → `if a then b else false`; `a || b` → `if a then true else b`.
    fn short_circuit(
        &mut self,
        lhs: &TypedExpr,
        rhs: &TypedExpr,
        and: bool,
        dest: Dest,
    ) -> R<CoreExpr> {
        let cond = self.operand(lhs)?;
        let other = Box::new(self.expr_as(rhs, dest)?);
        let bool_t = self.temps.bool;
        let known = self.sealed(|lo| lo.end(Atom::Bool(!and), bool_t, dest))?;
        let known = Box::new(known);
        let (then, els) = if and { (other, known) } else { (known, other) };
        Ok(CoreExpr::If { cond, then, els })
    }

    /// `[a, b, ..xs, c]`. Each run of plain elements becomes `MakeArray`,
    /// `ArrayPrepend` or `ArrayAppend`, and each spread becomes `ArrayConcat`,
    /// folded left to right. Every step in between carries the literal's `ty`
    /// too, so Perceus drops the halves of a concat chain that get thrown away.
    fn array(&mut self, elems: &[TypedArrayElem], ty: RTy) -> R<Atom> {
        let has_spread = elems.iter().any(|e| matches!(e, TypedArrayElem::Spread(_)));
        if !has_spread {
            let mut args = Vec::with_capacity(elems.len());
            for el in elems {
                if let TypedArrayElem::Elem(ex) = el {
                    args.push(self.operand(ex)?);
                }
            }
            return Ok(Atom::prim(PrimOp::MakeArray, args));
        }

        let mut acc: Option<LocalId> = None;
        let mut rest = elems;
        while let Some((first, tail)) = rest.split_first() {
            if let TypedArrayElem::Spread(sp) = first {
                let s = self.operand(sp)?;
                acc = Some(match acc {
                    Some(a) => self.let_(ty, Atom::prim(PrimOp::ArrayConcat, vec![a, s]))?,
                    None => s,
                });
                rest = tail;
                continue;
            }
            let mut run: Vec<LocalId> = Vec::new();
            while let Some((TypedArrayElem::Elem(ex), tail)) = rest.split_first() {
                run.push(self.operand(ex)?);
                rest = tail;
            }
            // A run before the first spread is prepended straight onto it.
            if acc.is_none()
                && let Some((TypedArrayElem::Spread(sp), tail)) = rest.split_first()
            {
                let s = self.operand(sp)?;
                run.push(s);
                acc = Some(self.let_(ty, Atom::prim(PrimOp::ArrayPrepend, run))?);
                rest = tail;
                continue;
            }
            let a = match acc {
                Some(a) => {
                    let mut args = vec![a];
                    args.extend(run);
                    self.let_(ty, Atom::prim(PrimOp::ArrayAppend, args))?
                }
                None => {
                    let a = Atom::prim(PrimOp::MakeArray, run);
                    self.let_(ty, a)?
                }
            };
            acc = Some(a);
        }
        Ok(match acc {
            Some(id) => Atom::Local(id),
            None => Atom::prim(PrimOp::MakeArray, vec![]),
        })
    }

    /// `"a${b}c"`. Literal pieces are already pooled, and expression pieces are
    /// turned into strings.
    fn interp(&mut self, parts: &[TypedInterpPart]) -> R<Atom> {
        let str_t = self.temps.string;
        let mut args = Vec::with_capacity(parts.len());
        for part in parts {
            let a = match part {
                TypedInterpPart::Str(c) => self.let_(str_t, Atom::Const(*c))?,
                TypedInterpPart::Expr(ex) => {
                    let v = self.operand(ex)?;
                    self.let_(str_t, Atom::prim(PrimOp::ToString, vec![v]))?
                }
            };
            args.push(a);
        }
        Ok(match args.as_slice() {
            [one] => Atom::Local(*one),
            _ => Atom::prim(PrimOp::StringConcatMany, args),
        })
    }

    /// `<<..>>` in value position. Every segment evaluates to a `Binary`.
    fn bin_lit(&mut self, segs: &[TypedBinSeg]) -> R<Atom> {
        let bin_t = self.temps.binary;
        let mut out = Vec::with_capacity(segs.len());
        for seg in segs {
            let id = match seg {
                TypedBinSeg::Int { value, bits } => {
                    let v = self.operand(value)?;
                    let b = self.operand(bits)?;
                    self.let_(bin_t, Atom::prim(PrimOp::BinaryFromInt, vec![v, b]))?
                }
                TypedBinSeg::Binary { value, bits } => {
                    let v = self.operand(value)?;
                    match bits {
                        Some(b) => {
                            let b = self.operand(b)?;
                            self.let_(bin_t, Atom::prim(PrimOp::BinaryTake, vec![v, b]))?
                        }
                        None => v,
                    }
                }
                TypedBinSeg::Utf8 { value } => {
                    let v = self.operand(value)?;
                    self.let_(bin_t, Atom::prim(PrimOp::BinaryFromString, vec![v]))?
                }
            };
            out.push(id);
        }
        Ok(match out.as_slice() {
            [one] => Atom::Local(*one),
            _ => Atom::prim(PrimOp::BinaryConcatMany, out),
        })
    }

    /// Lower `arr[i] or <pure atom>` to one [`PrimOp::ArrayIndexOr`], with no
    /// `Option` cell. The ordinary lowering boxes a `Some` that the next step
    /// takes apart and drops, which is one heap allocation per index.
    ///
    /// This only fuses the two when the failure arm is a pure atom. The fused
    /// op always evaluates the default, so anything with a side effect or
    /// control flow must keep the lazy match.
    fn index_or(&mut self, scrut: &TypedExpr, arms: &[TypedArm]) -> R<Option<Atom>> {
        let TypedExpr::Index { recv, index, .. } = scrut else {
            return Ok(None);
        };
        let [a, b] = arms else { return Ok(None) };
        if a.guard.is_some() || b.guard.is_some() {
            return Ok(None);
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
            _ => return Ok(None),
        };

        let r = self.operand(recv)?;
        let i = self.operand(index)?;
        let d = self.operand(dflt)?;
        Ok(Some(Atom::prim(PrimOp::ArrayIndexOr, vec![r, i, d])))
    }

    /// Lower `arms` against an already-bound `scrut`.
    ///
    /// Arms are lowered front to back so locals are minted in source order, but
    /// the tree is assembled back to front. Every failure edge of an arm jumps
    /// to one join with no parameters that holds the remaining arms. So each arm
    /// body is lowered once, however many edges carry on there.
    ///
    /// An arm with no failure edge is put at the front of the entry `Match`
    /// instead, so a run of arms that can't fail keeps the flat shape of a
    /// single `Match`. The innermost starting point is a `Match` with no arms,
    /// which the VM loads as a stop for a value no arm matched.
    fn match_arms(&mut self, scrut: LocalId, arms: &[TypedArm], dest: Dest) -> R<CoreExpr> {
        let mut lowered = Vec::with_capacity(arms.len());
        for arm in arms {
            let fall = self.fresh_join()?;
            let (pat, body) = self.scoped(|lo| lo.arm(scrut, arm, fall, dest))?;
            lowered.push((pat, body, fall));
        }

        // The arms after this one, kept unassembled so an arm that can't fail
        // can join the entry match's arms instead of adding another jump.
        let assemble = |entry_arms: VecDeque<(CorePat, CoreExpr)>,
                        wrap: Option<(JoinId, CoreExpr)>| {
            let entry = CoreExpr::Match {
                scrut,
                arms: entry_arms.into(),
            };
            match wrap {
                Some((id, cont)) => CoreExpr::Join {
                    id,
                    params: Vec::new(),
                    cont: Box::new(cont),
                    body: Box::new(entry),
                },
                None => entry,
            }
        };
        let mut entry_arms: VecDeque<(CorePat, CoreExpr)> = VecDeque::new();
        let mut wrap: Option<(JoinId, CoreExpr)> = None;
        for (pat, body, fall) in lowered.into_iter().rev() {
            if body.jumps_to(fall) {
                // With live failure edges, seal everything below as this arm's
                // continuation. A head that can fail to match also needs a
                // wildcard arm that sends a miss to the same join.
                let cont = assemble(entry_arms, wrap.take());
                let head_refutable = matches!(pat, CorePat::Lit(_) | CorePat::Ctor { .. });
                entry_arms = VecDeque::from([(pat, body.expr)]);
                if head_refutable {
                    entry_arms.push_back((CorePat::Wild, Tree::fail(fall).expr));
                }
                wrap = Some((fall, cont));
            } else {
                entry_arms.push_front((pat, body.expr));
            }
        }
        Ok(assemble(entry_arms, wrap))
    }

    /// One arm of a match on `scrut`, whose failure edges jump to `fall`.
    ///
    /// When an or-pattern in it binds names, the pattern can match in several
    /// places, and each has locals of its own for them. The body is then a join
    /// that takes every name the pattern binds.
    fn arm(
        &mut self,
        scrut: LocalId,
        arm: &TypedArm,
        fall: JoinId,
        dest: Dest,
    ) -> R<(CorePat, Tree)> {
        let shared = if or_binds(&arm.pat) {
            let binds = pat_binds(&arm.pat);
            let id = self.fresh_join()?;
            Some(SharedBody { id, binds })
        } else {
            None
        };
        let mut nested = Vec::new();
        let pat = self.pattern(&arm.pat, scrut, &mut nested)?;
        let body = match &shared {
            Some(shared) => ArmBody::Shared(shared),
            None => ArmBody::Here {
                body: &arm.body,
                dest,
            },
        };
        let leaf = match &arm.guard {
            None => ArmLeaf::Body(body),
            Some(guard) => ArmLeaf::Guarded { guard, body },
        };
        let tree = self.nested_arm_body(nested.into(), leaf, fall)?;
        let tree = match shared {
            Some(SharedBody { id, binds }) if tree.jumps_to(id) => {
                let mut params = Vec::with_capacity(binds.len());
                let cont = self.sealed(|lo| {
                    for b in &binds {
                        let param = lo.fresh(b.ty)?;
                        lo.bind(b.id, param)?;
                        params.push(CoreBind::new(param, b.ty));
                    }
                    lo.expr_tail(&arm.body, dest, SpinePos::Nested)
                })?;
                tree.with_cont(id, params, Tree::leaf(cont))
            }
            Some(_) | None => tree,
        };
        Ok((pat, tree))
    }

    /// The body of an arm, where its pattern has matched and its guard held.
    fn arm_body(&mut self, body: ArmBody<'_>) -> R<Tree> {
        Ok(match body {
            ArmBody::Here { body, dest } => {
                Tree::leaf(self.expr_tail(body, dest, SpinePos::Nested)?)
            }
            ArmBody::Shared(SharedBody { id, binds }) => {
                let args = binds.iter().map(|b| self.local(b.id)).collect::<R<_>>()?;
                Tree::jump(*id, args)
            }
        })
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
    ) -> R<CorePat> {
        Ok(match p {
            TypedPat::Wild { .. } => CorePat::Wild,
            TypedPat::Bind(b) => {
                let id = self.fresh(b.ty)?;
                self.bind(b.id, id)?;
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
                    let id = self.fresh(fp.ty())?;
                    // The wildcard is on purpose. Every compound pattern kind,
                    // now or later, goes to the nested queue.
                    #[allow(unknown_lints, wildcard_over_own_enum)]
                    match fp {
                        TypedPat::Wild { .. } => {}
                        TypedPat::Bind(b) => self.bind(b.id, id)?,
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
        })
    }

    /// `if cond { then } else { <fallthrough> }`. Every check in a pattern arm
    /// that can fail lowers to this shape.
    fn guard_if(&mut self, cond: LocalId, then: Tree, fall: JoinId) -> Tree {
        let mut jumps = then.jumps;
        jumps.insert(fall);
        Tree {
            expr: CoreExpr::If {
                cond,
                then: Box::new(then.expr),
                els: Box::new(Tree::fail(fall).expr),
            },
            jumps,
        }
    }

    /// Lower an or-pattern's alternatives the same way [`Self::match_arms`]
    /// lowers arms. The failure edges of each alternative jump to a join that
    /// tries the next one, and the last one falls to `outer`, so none is
    /// lowered twice.
    ///
    /// With no bindings, what comes after is one shared join
    /// ([`ArmLeaf::OrCommit`]), and a miss in it falls to `outer`: every
    /// alternative would lead to the same miss. After an or-pattern that binds
    /// names, the rest of the pattern and the guard are lowered once per
    /// alternative ([`ArmLeaf::OrRest`]), and a miss there tries the next
    /// alternative, as in Gleam and Rust. Only the arm's body is shared then
    /// ([`ArmBody::Shared`]).
    fn or_alternatives<'p>(
        &mut self,
        scrut: LocalId,
        alts: &'p [TypedPat],
        rest_nested: Vec<(LocalId, &'p TypedPat)>,
        leaf: ArmLeaf<'p>,
        outer: JoinId,
    ) -> R<Tree> {
        let commit = if alts.iter().any(|a| !pat_binds(a).is_empty()) {
            None
        } else {
            Some(self.fresh_join()?)
        };

        // Each alternative that can fail, with the join its failures go to.
        let mut chain: Vec<(JoinId, Tree)> = Vec::with_capacity(alts.len());
        // The alternative the chain ends with. With none at all, nothing
        // matches.
        let mut last = Tree::fail(outer);
        for (i, alt) in alts.iter().enumerate() {
            let is_last = i.saturating_add(1) == alts.len();
            let fall = if is_last { outer } else { self.fresh_join()? };
            let alt_leaf = match commit {
                Some(cj) => ArmLeaf::OrCommit(cj),
                None => ArmLeaf::OrRest {
                    rest_nested: rest_nested.clone(),
                    leaf: Box::new(leaf.clone()),
                },
            };
            let t = self.nested_arm_body(VecDeque::from([(scrut, alt)]), alt_leaf, fall)?;
            // An alternative that can't fail ends the chain. Anything written
            // after it can never be reached and is never lowered.
            if !is_last && t.jumps_to(fall) {
                chain.push((fall, t));
            } else {
                last = t;
                break;
            }
        }

        // Assemble back to front: each alternative's join holds the ones
        // after it.
        let cur = chain
            .into_iter()
            .rev()
            .fold(last, |cur, (fall, t)| t.with_cont(fall, Vec::new(), cur));
        Ok(match commit {
            Some(cj) if cur.jumps_to(cj) => {
                let cont = self.nested_arm_body(rest_nested.into(), leaf, outer)?;
                cur.with_cont(cj, Vec::new(), cont)
            }
            Some(_) | None => cur,
        })
    }

    /// Flatten `nested` sub-patterns into `Match` and `Let` nodes around the
    /// arm's leaf. Structure that can't fail becomes `Let` field reads on the
    /// current spine. Structure that can fail becomes an inner `Match` or `If`
    /// whose failure edge is `fall`, so a nested miss carries on matching the
    /// way the source says.
    fn nested_arm_body<'p>(
        &mut self,
        nested: VecDeque<(LocalId, &'p TypedPat)>,
        leaf: ArmLeaf<'p>,
        fall: JoinId,
    ) -> R<Tree> {
        self.sealed_tree(|lo| lo.nested_walk(nested, leaf, fall))
    }

    /// The body of [`Self::nested_arm_body`], inside its segment.
    fn nested_walk<'p>(
        &mut self,
        mut nested: VecDeque<(LocalId, &'p TypedPat)>,
        leaf: ArmLeaf<'p>,
        fall: JoinId,
    ) -> R<Tree> {
        loop {
            let Some((v, p)) = nested.pop_front() else {
                return Ok(match leaf {
                    ArmLeaf::Body(body) => self.arm_body(body)?,
                    ArmLeaf::Guarded { guard, body } => {
                        let cond = self.operand(guard)?;
                        let then = self.sealed_tree(|lo| lo.arm_body(body))?;
                        self.guard_if(cond, then, fall)
                    }
                    ArmLeaf::BinCont(bc) => self.bin_segments(*bc, fall)?,
                    ArmLeaf::OrRest { rest_nested, leaf } => {
                        self.nested_arm_body(rest_nested.into(), *leaf, fall)?
                    }
                    ArmLeaf::OrCommit(j) => Tree::fail(j),
                });
            };
            match p {
                TypedPat::Wild { .. } => {}
                TypedPat::Bind(b) => self.bind(b.id, v)?,
                TypedPat::Tuple { elems, .. } => {
                    let mut tmp = Vec::with_capacity(elems.len());
                    for (j, elem) in elems.iter().enumerate() {
                        let ej = self.let_(
                            elem.ty(),
                            Atom::prim(PrimOp::TupleField(idx16(j)?), vec![v]),
                        )?;
                        tmp.push((ej, elem));
                    }
                    for r in tmp.into_iter().rev() {
                        nested.push_front(r);
                    }
                }
                TypedPat::Ctor { .. } | TypedPat::Lit { .. } => {
                    let mut inner = Vec::new();
                    let sub = self.pattern(p, v, &mut inner)?;
                    for r in inner.into_iter().rev() {
                        nested.push_front(r);
                    }
                    let body = self.nested_arm_body(nested, leaf, fall)?;
                    let mut jumps = body.jumps;
                    jumps.insert(fall);
                    return Ok(Tree {
                        expr: CoreExpr::Match {
                            scrut: v,
                            arms: vec![(sub, body.expr), (CorePat::Wild, Tree::fail(fall).expr)],
                        },
                        jumps,
                    });
                }
                TypedPat::Array {
                    elem_ty,
                    len: len_c,
                    prefix,
                    rest,
                    ..
                } => {
                    let vty = self.bind_ty(v)?;
                    let int_t = self.temps.int;
                    let bool_t = self.temps.bool;
                    let len = self.let_(int_t, Atom::prim(PrimOp::ArrayLen, vec![v]))?;
                    let n = self.int_const(*len_c)?;
                    let cmp = match rest {
                        PatRest::None => PrimOp::IntEq,
                        PatRest::Discard | PatRest::Bind(_) => PrimOp::IntGe,
                    };
                    let ok = self.let_(bool_t, Atom::prim(cmp, vec![len, n]))?;
                    let then = self.sealed_tree(|lo| {
                        let mut tmp = Vec::with_capacity(prefix.len());
                        for (j, ep) in prefix.iter().enumerate() {
                            let ej = lo.let_(
                                *elem_ty,
                                Atom::prim(PrimOp::ArrayElem(idx16(j)?), vec![v]),
                            )?;
                            tmp.push((ej, ep));
                        }
                        if let PatRest::Bind(b) = rest {
                            let n2 = lo.int_const(*len_c)?;
                            let tail = lo.let_(vty, Atom::prim(PrimOp::ArrayDrop, vec![v, n2]))?;
                            lo.bind(b.id, tail)?;
                        }
                        for r in tmp.into_iter().rev() {
                            nested.push_front(r);
                        }
                        lo.nested_arm_body(nested, leaf, fall)
                    })?;
                    return Ok(self.guard_if(ok, then, fall));
                }
                TypedPat::Range { lo, hi, .. } => {
                    let int_t = self.temps.int;
                    let bool_t = self.temps.bool;
                    let lo_c = self.let_(int_t, Atom::Const(*lo))?;
                    let ge = self.let_(bool_t, Atom::prim(PrimOp::IntGe, vec![v, lo_c]))?;
                    let then = self.sealed_tree(|lo| {
                        let hi = lo.let_(int_t, Atom::Const(*hi))?;
                        let lt = lo.let_(bool_t, Atom::prim(PrimOp::IntLt, vec![v, hi]))?;
                        let inner = lo.nested_arm_body(nested, leaf, fall)?;
                        Ok(lo.guard_if(lt, inner, fall))
                    })?;
                    return Ok(self.guard_if(ge, then, fall));
                }
                TypedPat::Or { alts, .. } => {
                    let rest_nested: Vec<_> = nested.into_iter().collect();
                    return self.or_alternatives(v, alts, rest_nested, leaf, fall);
                }
                TypedPat::Bin {
                    zero, segs, rest, ..
                } => {
                    let int_t = self.temps.int;
                    let total = self.let_(int_t, Atom::prim(PrimOp::BinaryBitSize, vec![v]))?;
                    let cursor = self.int_const(*zero)?;
                    let after: Vec<_> = nested.into_iter().collect();
                    return self.bin_segments(
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
                    );
                }
            }
        }
    }

    /// Lower one step of a `<<>>` pattern. Check the bounds, read, match the
    /// segment's value pattern, then recurse through a `BinCont` leaf so those
    /// bindings are in scope for the next segment's `size(...)`. When `segs` is
    /// empty, emit the trailing `..rest` bind or exact-length check.
    fn bin_segments<'p>(&mut self, bc: BinCont<'p>, fall: JoinId) -> R<Tree> {
        self.sealed_tree(|lo| lo.bin_step(bc, fall))
    }

    /// The body of [`Self::bin_segments`], inside its segment.
    fn bin_step<'p>(&mut self, bc: BinCont<'p>, fall: JoinId) -> R<Tree> {
        let int_t = self.temps.int;
        let bool_t = self.temps.bool;
        let bin_ty = self.bind_ty(bc.bin)?;
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
            return Ok(match rest {
                PatRest::Bind(b) => {
                    let rem = self.let_(int_t, Atom::prim(PrimOp::IntSub, vec![total, cursor]))?;
                    let rv = self.let_(
                        bin_ty,
                        Atom::prim(PrimOp::BinaryView, vec![bin, cursor, rem]),
                    )?;
                    self.bind(b.id, rv)?;
                    self.nested_arm_body(after.into(), then, fall)?
                }
                PatRest::Discard => self.nested_arm_body(after.into(), then, fall)?,
                PatRest::None => {
                    let ok = self.let_(bool_t, Atom::prim(PrimOp::IntEq, vec![cursor, total]))?;
                    let body = self.nested_arm_body(after.into(), then, fall)?;
                    self.guard_if(ok, body, fall)
                }
            });
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

        Ok(match seg {
            // A `<<'literal'>>` segment matches its UTF-8 bytes as a prefix.
            TypedBinPatSeg::Utf8Literal { bytes, bits } => {
                let pfx = self.let_(bin_ty, Atom::Const(*bytes))?;
                let ok = self.let_(
                    bool_t,
                    Atom::prim(PrimOp::BinaryMatchPrefix, vec![bin, cursor, pfx]),
                )?;
                let then_e = self.sealed_tree(|lo| {
                    let w = lo.int_const(*bits)?;
                    let cur2 = lo.let_(int_t, Atom::prim(PrimOp::IntAdd, vec![cursor, w]))?;
                    lo.bin_segments(cont(cur2), fall)
                })?;
                self.guard_if(ok, then_e, fall)
            }
            // A `:utf8` codepoint, whose width varies. `BinaryReadUtf8` gives back
            // `(codepoint, nbits)`, where `nbits == 0` means decoding failed.
            TypedBinPatSeg::Utf8 { value } => {
                let pair_ty = self.temps.int_pair;
                let pair = self.let_(
                    pair_ty,
                    Atom::prim(PrimOp::BinaryReadUtf8, vec![bin, cursor]),
                )?;
                let cp = self.let_(int_t, Atom::prim(PrimOp::TupleField(0), vec![pair]))?;
                let nbits = self.let_(int_t, Atom::prim(PrimOp::TupleField(1), vec![pair]))?;
                let z = self.int_const(zero)?;
                let ok = self.let_(bool_t, Atom::prim(PrimOp::IntGt, vec![nbits, z]))?;
                let then_e = self.sealed_tree(|lo| {
                    let cur2 = lo.let_(int_t, Atom::prim(PrimOp::IntAdd, vec![cursor, nbits]))?;
                    let leaf2 = ArmLeaf::BinCont(Box::new(cont(cur2)));
                    lo.nested_arm_body(VecDeque::from([(cp, value)]), leaf2, fall)
                })?;
                self.guard_if(ok, then_e, fall)
            }
            // An Int or Binary segment, whose width is the size expression in
            // bits. A `:binary` segment with no size takes the rest.
            TypedBinPatSeg::Int { bits, value } => self.bin_read(
                BinWalk {
                    bin,
                    total,
                    cursor,
                    zero,
                },
                SegWidth::Sized(bits),
                PrimOp::BinaryReadInt,
                int_t,
                value,
                cont,
                fall,
            )?,
            TypedBinPatSeg::Binary { bits, value } => {
                let width = match bits {
                    Some(bits) => SegWidth::Sized(bits),
                    None => SegWidth::Remainder,
                };
                self.bin_read(
                    BinWalk {
                        bin,
                        total,
                        cursor,
                        zero,
                    },
                    width,
                    PrimOp::BinaryView,
                    bin_ty,
                    value,
                    cont,
                    fall,
                )?
            }
        })
    }

    /// The shared tail of a fixed-position `<<>>` segment. Lower the width,
    /// checking the read's bounds as [`SegWidth`] says, read the value, then
    /// match the segment's value pattern and carry on.
    #[allow(clippy::too_many_arguments)]
    fn bin_read<'p>(
        &mut self,
        walk: BinWalk,
        width: SegWidth<'p>,
        read_op: PrimOp,
        val_ty: RTy,
        value: &'p TypedPat,
        cont: impl Fn(LocalId) -> BinCont<'p>,
        fall: JoinId,
    ) -> R<Tree> {
        let int_t = self.temps.int;
        let bool_t = self.temps.bool;
        let BinWalk {
            bin,
            total,
            cursor,
            zero,
        } = walk;
        let read = |lo: &mut Self, width: LocalId, end: LocalId| -> R<Tree> {
            lo.sealed_tree(|lo| {
                let sv = lo.let_(val_ty, Atom::prim(read_op, vec![bin, cursor, width]))?;
                let leaf2 = ArmLeaf::BinCont(Box::new(cont(end)));
                lo.nested_arm_body(VecDeque::from([(sv, value)]), leaf2, fall)
            })
        };
        match width {
            // A negative width matches nothing, like a read past the end.
            SegWidth::Sized(bits) => {
                let width = self.operand(bits)?;
                let none = self.int_const(zero)?;
                let sized = self.let_(bool_t, Atom::prim(PrimOp::IntGe, vec![width, none]))?;
                let fits = self.sealed_tree(|lo| {
                    let end = lo.let_(int_t, Atom::prim(PrimOp::IntAdd, vec![cursor, width]))?;
                    let ok = lo.let_(bool_t, Atom::prim(PrimOp::IntLe, vec![end, total]))?;
                    let then_e = read(lo, width, end)?;
                    Ok(lo.guard_if(ok, then_e, fall))
                })?;
                Ok(self.guard_if(sized, fits, fall))
            }
            SegWidth::Remainder => {
                let width = self.let_(int_t, Atom::prim(PrimOp::IntSub, vec![total, cursor]))?;
                let end = self.let_(int_t, Atom::prim(PrimOp::IntAdd, vec![cursor, width]))?;
                read(self, width, end)
            }
        }
    }
}

/// The two sides of an `&&` or an `||`, and whether it is an `&&`.
fn sides(e: &TypedExpr) -> Option<(&TypedExpr, &TypedExpr, bool)> {
    if let TypedExpr::And { lhs, rhs, .. } = e {
        return Some((lhs, rhs, true));
    }
    if let TypedExpr::Or { lhs, rhs, .. } = e {
        return Some((lhs, rhs, false));
    }
    None
}

/// Whether `e` is lowered as a branch, whose value a join gives.
fn is_branch(e: &TypedExpr) -> bool {
    matches!(
        e,
        TypedExpr::If { .. }
            | TypedExpr::Match { .. }
            | TypedExpr::And { .. }
            | TypedExpr::Or { .. }
    )
}
