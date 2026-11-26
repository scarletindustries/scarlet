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
//! around what comes last once it's known. Each branch of an `If` or a `Switch` is
//! sealed on its own, so no `Let` moves across a branch.
//!
//! What a `match` asks and in what order is [`crate::matching`]'s, and [`arms`]
//! writes it. Where a `match` is on what may not be there, like `xs[i]`, the arms of
//! the [`CoreExpr::Probe`] are the arms of the `match`, and no `Option` or `Result` is
//! made. Any other use of it makes one in each arm ([`probes`]).
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

use super::spine::Frame;
use super::{
    Atom, Callee, Const, ConstId, CoreBind, CoreExpr, CoreFn, JoinId, Load, LocalId, PrimOp,
};
use crate::matching::Allowance;
use crate::program::{InternalLinkError, LinkError, PreludeCtors, Table};
use crate::typed_ir::{
    BindingId, RTy, Shapes, TempTys, TypedArm, TypedArrayElem, TypedBinSeg, TypedBind, TypedCallee,
    TypedExpr, TypedFn, TypedInterpPart, ValueRef, each_read,
};
use scarlet_ir::tivec::TiVec;

mod arms;
mod probes;

use probes::{Asked, is_probes};

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

/// What a function's types and constants are, and how its matches are lowered.
#[derive(Clone, Copy)]
pub(crate) struct Around<'a> {
    pub temps: TempTys,
    pub ctors: PreludeCtors,
    pub shapes: &'a Shapes,
    pub consts: &'a [Const],
    pub allowance: Allowance,
}

/// Lower one function. Each param is bound under the [`BindingId`] its
/// [`TypedBind`] carries.
pub(crate) fn lower_fn(around: Around<'_>, f: &TypedFn) -> R<Lowered> {
    let mut lo = Lower::new(around, f.binds);
    each_read(&f.body, |b| {
        if let Some(n) = lo.reads.get_mut(b.0) {
            *n = n.saturating_add(1);
        }
    });
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

/// What a match is on.
enum Scrut {
    Local {
        local: LocalId,
        /// Whether nothing but the match reads it.
        alone: bool,
    },
    /// The value of a probe, which is not made where the match need not have it.
    Probe(Asked),
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

/// What was going on around a segment.
struct Outer {
    spine: Vec<Frame>,
    undo: Vec<(BindingId, Option<LocalId>)>,
}

struct Lower<'a> {
    around: Around<'a>,
    /// How many times the function reads each binding.
    reads: Vec<u32>,
    /// The [`RTy`] of every minted [`LocalId`].
    tys: TiVec<LocalId, RTy>,
    /// Maps `BindingId → LocalId`, densely over `TypedFn::binds`. Different
    /// source bindings have different `BindingId`s, so nothing is shadowed.
    binds: Vec<Option<LocalId>>,
    /// What each binding in the current scope replaced, newest last. A segment
    /// puts these back when it ends ([`Self::leave`]), so a binding made in one
    /// arm or alternative can't be read from the next.
    undo: Vec<(BindingId, Option<LocalId>)>,
    /// The nodes emitted so far in the current straight-line segment.
    /// [`Self::sealed`] folds them around what the segment ends in.
    spine: Vec<Frame>,
    /// How many join labels have been made.
    joins: u32,
}

impl<'a> Lower<'a> {
    fn new(around: Around<'a>, binds: usize) -> Self {
        Lower {
            around,
            reads: vec![0; binds],
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
        let int_t = self.around.temps.int;
        self.let_(int_t, Atom::Const(c))
    }

    /// Start a new spine segment, in a scope of its own.
    fn enter(&mut self) -> Outer {
        Outer {
            spine: std::mem::take(&mut self.spine),
            undo: std::mem::take(&mut self.undo),
        }
    }

    /// End the segment that [`Self::enter`] started, which gives what was pushed in
    /// it. Every binding made in it is undone.
    fn leave(&mut self, outer: Outer) -> Vec<Frame> {
        let inner = std::mem::replace(&mut self.undo, outer.undo);
        // Each entry's slot was in range when it was bound, and the table
        // never shrinks.
        for (b, old) in inner.into_iter().rev() {
            if let Some(slot) = self.binds.get_mut(b.0) {
                *slot = old;
            }
        }
        std::mem::replace(&mut self.spine, outer.spine)
    }

    /// Run `f` in a new spine segment and scope, then fold every node it
    /// pushed around what it produced, innermost first.
    fn sealed(&mut self, f: impl FnOnce(&mut Self) -> R<CoreExpr>) -> R<CoreExpr> {
        let outer = self.enter();
        let e = f(self);
        let segment = self.leave(outer);
        Ok(Frame::wrap_all(segment, e?))
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
                    if let Some((scrut, arms)) = self.matched(e) {
                        return self.match_(scrut, arms, dest);
                    }
                    self.let_bind_at(bind, init, spine)?;
                    e = body;
                }
                TypedExpr::Seq { effect, body, .. } => {
                    self.effect(effect)?;
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
                TypedExpr::Match { scrut, arms, .. } => return self.match_(scrut, arms, dest),
                TypedExpr::And { lhs, rhs, .. } => {
                    return self.short_circuit(lhs, rhs, true, dest);
                }
                TypedExpr::Or { lhs, rhs, .. } => {
                    return self.short_circuit(lhs, rhs, false, dest);
                }
                _ => {
                    if let Some(asked) = self.asked(e)? {
                        return self.value_of(&asked, dest);
                    }
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
    /// that segment, where its value already has a home. And where nothing reads it,
    /// what a probe gives is bound to nothing.
    fn let_bind_at(&mut self, bind: &TypedBind, init: &TypedExpr, spine: SpinePos) -> R<()> {
        let unread = bind.global.is_none() && self.reads.get(bind.id.0) == Some(&0);
        if unread && is_probes(init) {
            return self.effect(init);
        }
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

    /// Lower a `match` on `scrut`.
    fn match_(&mut self, scrut: &TypedExpr, arms: &[TypedArm], dest: Dest) -> R<CoreExpr> {
        let scrut = self.scrut(scrut)?;
        arms::lower(self, scrut, arms, dest)
    }

    /// What a `match` on `e` is on.
    fn scrut(&mut self, e: &TypedExpr) -> R<Scrut> {
        let alone = self.read_once(e);
        let e = self.flattened(e)?;
        Ok(match self.asked(e)? {
            Some(asked) => Scrut::Probe(asked),
            None => Scrut::Local {
                local: self.operand(e)?,
                alone,
            },
        })
    }

    /// Put the `Let`/`Seq` spine that `e` is on the current one, and give its tail.
    fn flattened<'e>(&mut self, mut e: &'e TypedExpr) -> R<&'e TypedExpr> {
        loop {
            if let TypedExpr::Let {
                bind, init, body, ..
            } = e
                && self.matched(e).is_none()
            {
                self.let_bind_at(bind, init, SpinePos::Nested)?;
                e = body;
            } else if let TypedExpr::Seq { effect, body, .. } = e {
                self.effect(effect)?;
                e = body;
            } else {
                return Ok(e);
            }
        }
    }

    /// Lower `e`, whose value nothing reads. That of a probe is not made.
    fn effect(&mut self, e: &TypedExpr) -> R<()> {
        let e = self.flattened(e)?;
        match self.asked(e)? {
            Some(asked) => self.only_ask(&asked),
            None => self.operand(e).map(drop),
        }
    }

    /// Lower a branch whose value is needed further on: each of its ends jumps
    /// to a join, with the value as the argument.
    ///
    /// Where the left side of a short-circuit is a branch too, its join goes in
    /// front, so that a long chain of them is a chain and not a nest.
    /// What a probe is of goes in front too, for the same reason.
    fn join_operand(&mut self, e: &TypedExpr, ty: RTy) -> R<LocalId> {
        if let Some(asked) = self.asked(e)? {
            return self.joined(ty, |lo, dest| lo.value_of(&asked, dest));
        }
        if let Some((scrut, arms)) = self.matched(e) {
            let scrut = self.scrut(scrut)?;
            return self.joined(ty, |lo, dest| {
                lo.sealed(|lo| arms::lower(lo, scrut, arms, dest))
            });
        }
        match sides(e) {
            Some((lhs, rhs, and)) if is_branch(lhs) => {
                self.joined(ty, |lo, dest| lo.short_circuit(lhs, rhs, and, dest))
            }
            Some(_) | None => self.joined(ty, |lo, dest| lo.expr_as(e, dest)),
        }
    }

    /// A new local of type `ty`, which is whatever `body` sends to where it is given.
    fn joined(&mut self, ty: RTy, body: impl FnOnce(&mut Self, Dest) -> R<CoreExpr>) -> R<LocalId> {
        let join = self.fresh_join()?;
        let body = body(self, Dest::Join(join))?;
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
        let e = self.flattened(e)?;
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
            // The one `Let` that gets here is a match on what it binds. No `Seq` does.
            TypedExpr::Let { .. }
            | TypedExpr::Seq { .. }
            | TypedExpr::If { .. }
            | TypedExpr::Match { .. }
            | TypedExpr::And { .. }
            | TypedExpr::Or { .. }
            | TypedExpr::Probe { .. } => Atom::Local(self.join_operand(e, ty)?),
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
        let bool_t = self.around.temps.bool;
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
        let str_t = self.around.temps.string;
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
        let bin_t = self.around.temps.binary;
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

    /// Whether nothing else reads the value of `e`, as far as names say.
    fn read_once(&self, mut e: &TypedExpr) -> bool {
        loop {
            match e {
                TypedExpr::Let { body, .. } | TypedExpr::Seq { body, .. } => e = body,
                TypedExpr::Var {
                    place: ValueRef::Local(b),
                    ..
                } => return self.reads.get(b.0) == Some(&1),
                TypedExpr::Var {
                    place: ValueRef::Global(_) | ValueRef::Capture(_) | ValueRef::SelfClosure,
                    ..
                }
                | TypedExpr::Const { .. }
                | TypedExpr::Nil { .. }
                | TypedExpr::Binary { .. }
                | TypedExpr::Unary { .. }
                | TypedExpr::And { .. }
                | TypedExpr::Or { .. }
                | TypedExpr::Tuple { .. }
                | TypedExpr::TupleIndex { .. }
                | TypedExpr::Field { .. }
                | TypedExpr::Ctor { .. }
                | TypedExpr::Update { .. }
                | TypedExpr::Array { .. }
                | TypedExpr::Probe { .. }
                | TypedExpr::Range { .. }
                | TypedExpr::Interp { .. }
                | TypedExpr::BinLit { .. }
                | TypedExpr::Closure { .. }
                | TypedExpr::Call { .. }
                | TypedExpr::If { .. }
                | TypedExpr::Match { .. } => return true,
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
            | TypedExpr::Probe { .. }
    )
}

#[cfg(test)]
mod tests;
