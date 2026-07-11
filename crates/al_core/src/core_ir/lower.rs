//! Typed IR → Core IR.
//!
//! [`lower`] turns a [`TypedProgram`] into a [`CoreProgram`]: every
//! subexpression becomes a `Let{bind,rhs,body}`, so the source's implicit
//! evaluation order is an explicit right-nested spine and every operand is a
//! [`LocalId`].
//!
//! ## Total by construction
//!
//! `lower` takes `&TypedProgram` and returns `CoreProgram`. No `&mut`, no
//! `Result`, no `LowerCtx`. That is not a tidy-up: it is the whole point of
//! the typed IR. Everything lowering used to *ask* for it now *reads*.
//!
//! * A type is a [`RTy`] field on the node. There is no `expr_ty(span)` side
//!   table to miss, and [`RTy`]'s arena has no `Var` arm, so `Perceus` can no
//!   longer be told "not heap" because inference lost the type.
//! * A name is a [`ValueRef`]. There is no `resolve_name`, so there is no
//!   "unbound identifier" to report.
//! * A field is an index, a constructor is a `VariantRef`, a callee is a
//!   [`TypedCallee`], an eta-wrapper is an ordinary [`TypedFn`]. Nothing is
//!   re-derived, nothing is synthesised into a `Program` mid-walk.
//!
//! Consequently there is no failure mode left to name *here*: every arm of
//! every match below produces Core, and `LowerError` is gone.
//!
//! Upstream is narrower but not total. [`crate::typed_ir::elaborate_body`] and
//! `elaborate_toplevel` return a [`TypedFn`], not a `Result`, so
//! `DiagnosticCode::InternalError` is gone — a well-typed program is never told
//! it is a compiler bug. What replaced the variant is not a proof, though: a
//! broken invariant reaches [`crate::typed_ir::elaborator_bug`], which
//! **aborts, and does not diagnose**. Read the paragraph above as "`lower` has
//! no failure mode", not as a whole-pipeline guarantee: `or_shape` and
//! `closure` are still *questions* the elaborator asks of the check walk, and a
//! wrong answer kills the process rather than the compile. In-process consumers
//! (`al lsp`, `IncrementalSession`) inherit that. Closing those two — carrying
//! the `OrShape` on the checked node, threading the `ClosureSite` through the
//! AST instead of re-querying it by `Span` — is what would turn the abort into
//! an unrepresentable state.
//!
//! ## Builder discipline
//!
//! ANF construction is inside-out: recursing into a subexpression yields the
//! `LocalId` its value lands in *plus* a list of `Let`s to be prepended. The
//! [`Lower`] builder accumulates those lets in `spine` and [`Lower::seal`]
//! folds them around the eventual tail. Join points (`If`/`Match`) seal each
//! arm independently so no `Let` floats across a branch.
//!
//! ## Joins in operand position
//!
//! An `if`/`match`/short-circuit that appears where a value is needed (not as
//! the function's tail) lowers via [`CoreExpr::LetJoin`]: the join is sealed as
//! its own expression tree whose `Tail`s produce a value, then the result is
//! bound like any other operand.
//!
//! A `Let`/`Seq` spine is *not* one of those: it is straight-line code, so
//! [`Lower::atom`] splices its bindings onto the enclosing spine and takes the
//! spine's tail as the atom. Sealing it as a join would bind the value twice
//! and hand `Perceus` the join's type rather than the value's, turning a shaped
//! `Drop` into a generic one.
//!
//! ## Whole-program, one pass
//!
//! `lower` walks `TypedProgram::fns` in order, so `CoreProgram::fns[i]` is the
//! body of `FuncIdx(i)` and every `TypedCallee::Known(i)` / `Closure{func_idx}`
//! operand is already correct.
//!
//! ## The elaborator owns the constant pool
//!
//! Interning is a mutation, so `lower` does none: it copies `TypedProgram
//! ::consts` to `CoreProgram::consts` verbatim. Every `Atom::Const` it emits
//! names a [`ConstId`] read off the node being lowered. Even the integers no
//! source literal spells — an array pattern's length, a `<<>>` walk's initial
//! bit cursor, a `<<'lit'>>` segment's width — are pooled at elaboration time
//! and carried on `TypedPat::Array`, `TypedPat::Bin` and
//! `TypedBinPatSeg::Utf8Literal` respectively. A constant `lower` wanted but
//! the elaborator had not seen would have nowhere to come from, so the pool it
//! is handed is final by construction.

use std::collections::VecDeque;

use smallvec::SmallVec;

use super::{Atom, Callee, ConstId, CoreBind, CoreExpr, CoreFn, CorePat, CoreProgram, LocalId};
use crate::bytecode::Op;
use crate::typed_ir::{
    BindingId, PatRest, RTy, TempTys, TypedArm, TypedArrayElem, TypedBinPatSeg, TypedBinSeg,
    TypedBind, TypedCallee, TypedExpr, TypedFn, TypedInterpPart, TypedPat, TypedProgram, ValueRef,
};
use crate::types::{NO_STR, StrId};

/// A `TypedFn` read a [`BindingId`] it never bound, or `TypedFn::binds`
/// under-counted its binders. Both are elaborator bugs, and both are silent if
/// papered over: any substituted `LocalId` names a live local, so the program
/// would run and return the wrong answer. Aborts, in release as well as debug —
/// the same trade [`crate::typed_ir::elaborator_bug`] makes, and for the same
/// reason: there is no value to return and no user to address.
#[allow(clippy::panic)]
#[cold]
#[inline(never)]
fn read_before_bind(b: BindingId) -> ! {
    panic!(
        "internal compiler error: {b} was read before it was bound. \
         Please report this as a compiler bug."
    )
}

/// A lowered module plus the entry-frame slot assignments its toplevel needs.
///
/// `emit_toplevel` pins each module-scope `Let` to the slot every already-
/// emitted fn body addresses it by (`PushGlobal slot`). The pairing comes
/// straight out of [`crate::typed_ir::TypedBind::global`], so a def/use
/// mismatch is unspellable: there is no name-keyed queue to dequeue from.
pub struct Lowered {
    pub program: CoreProgram,
    /// `(toplevel Let bind, entry-frame slot)`, in binding order.
    pub toplevel_globals: Vec<(LocalId, i32)>,
}

/// Lower a whole typechecked module.
///
/// Total: a `TypedProgram` is only ever built for a diagnostics-clean module,
/// and every form such a module can take has an arm below.
pub fn lower(p: &TypedProgram) -> CoreProgram {
    lower_all(p).program
}

/// [`lower`], plus the toplevel's `LocalId → entry-frame slot` map.
pub fn lower_all(p: &TypedProgram) -> Lowered {
    let mut fns = Vec::with_capacity(p.fns.len());
    for f in &p.fns {
        fns.push(lower_fn(p.temps, f).0);
    }
    let (top, toplevel_globals) = lower_fn(p.temps, &p.toplevel);
    Lowered {
        program: CoreProgram {
            fns,
            consts: p.consts.clone(),
            toplevel: top.body,
        },
        toplevel_globals,
    }
}

/// Lower one function. `params[i]` binds as `LocalId(i)`, matching
/// `BindingId(i)`.
fn lower_fn(temps: TempTys, f: &TypedFn) -> (CoreFn, Vec<(LocalId, i32)>) {
    let mut lo = Lower::new(temps, f.binds);
    let params: Vec<CoreBind> = f
        .params
        .iter()
        .enumerate()
        .map(|(i, &(_, ty))| {
            let id = lo.fresh(ty);
            lo.bind(BindingId(i as u32), id);
            CoreBind::new(id, ty)
        })
        .collect();
    let body = lo.sealed(|lo| lo.expr_tail(&f.body, f.ret));
    let globals = std::mem::take(&mut lo.globals);
    (
        CoreFn {
            name: f.name,
            params,
            body,
            ret_ty: f.ret,
        },
        globals,
    )
}

/// One pending binding on the spine, sealed around a tail later. `Join` is a
/// `LetJoin`'s deferred rhs (a full expression tree in operand position).
enum Pending {
    Let { bind: CoreBind, rhs: Atom },
    Join { bind: CoreBind, join: CoreExpr },
}

/// What [`Lower::nested_arm_body`] lowers once every residual sub-pattern has
/// been bound.
#[derive(Clone)]
enum ArmLeaf<'p> {
    Body(&'p TypedExpr),
    Guarded {
        guard: &'p TypedExpr,
        body: &'p TypedExpr,
    },
    /// Resume reading a `<<>>` pattern's remaining segments. Reached after the
    /// current segment's value pattern has bound its names — those bindings are
    /// in scope for the next segment's `size(...)` expression.
    BinCont(Box<BinCont<'p>>),
    /// An or-pattern alternative just matched: resume with the sub-patterns
    /// that were queued *after* the or-pattern, then `leaf`, using `outer` —
    /// not the try-next-alternative fall — as the fallthrough. The first
    /// matching alternative commits; guard/rest failure falls to the next arm,
    /// never retries a sibling alternative.
    OrBody {
        rest_nested: Vec<(LocalId, &'p TypedPat)>,
        leaf: Box<ArmLeaf<'p>>,
        outer: ArmFall<'p>,
    },
}

/// Suspended state of a `<<>>` pattern walk: `segs` are the segments still to
/// read at bit offset `cursor` into `bin` (whose bit length is `total`). When
/// `segs` is exhausted the trailing `rest`/exact-length check runs, then
/// `after` (any nested sub-patterns queued after the binary) and finally
/// `then` (the arm body / guard).
#[derive(Clone)]
struct BinCont<'p> {
    bin: LocalId,
    total: LocalId,
    cursor: LocalId,
    /// The pooled `Int` `0` off the enclosing [`TypedPat::Bin`], threaded so a
    /// `:utf8` segment can test its decoded width without interning one.
    zero: ConstId,
    segs: &'p [TypedBinPatSeg],
    rest: PatRest,
    after: Vec<(LocalId, &'p TypedPat)>,
    then: ArmLeaf<'p>,
}

/// Fallthrough continuation for an arm: what to lower when a guard or a nested
/// refutable sub-pattern fails after the arm's `CorePat` head already matched.
#[derive(Clone)]
enum ArmFall<'p> {
    /// Resume matching on the remaining source arms against the original
    /// scrutinee.
    Arms {
        scrut: LocalId,
        rest: &'p [TypedArm],
    },
    /// Try the next alternative of an or-pattern; when `alts` is exhausted,
    /// fall to `outer`.
    OrAlt(Box<OrCont<'p>>),
}

/// Suspended state of an or-pattern walk: `alts` are the alternatives still to
/// try against `scrut`. An alternative that fails to match tries the next, and
/// finally `outer`; an alternative that matches commits — `rest_nested` then
/// `leaf` are lowered with `outer` as the fallthrough, so a subsequent miss
/// never retries a sibling alternative.
///
/// Every alternative of a well-formed or-pattern binds the same names to the
/// same [`BindingId`]s, so re-entering [`Lower::bind`] for the next alternative
/// overwrites exactly the entries the previous one wrote.
#[derive(Clone)]
struct OrCont<'p> {
    scrut: LocalId,
    alts: &'p [TypedPat],
    rest_nested: Vec<(LocalId, &'p TypedPat)>,
    leaf: ArmLeaf<'p>,
    outer: ArmFall<'p>,
}

struct Lower {
    temps: TempTys,
    next: u32,
    /// [`RTy`] for every minted [`LocalId`], indexed by `id.0`. Populated by
    /// [`Self::fresh`] so [`Self::bind_ty`] is a straight index.
    tys: Vec<RTy>,
    /// `BindingId → LocalId`, dense over `TypedFn::binds`. Replaces the old
    /// `Vec<(StrId, LocalId)>` shadowing stack: distinct source bindings have
    /// distinct `BindingId`s, so there is nothing to shadow and nothing to
    /// unwind on the way out of a scope.
    binds: Vec<Option<LocalId>>,
    /// Let-bindings emitted so far in the current linear segment; folded
    /// around the segment's tail by [`Self::seal`].
    spine: Vec<Pending>,
    /// `(bind, entry-frame slot)` for every module-scope binding walked so far.
    globals: Vec<(LocalId, i32)>,
    outer_spine: bool,
}

impl Lower {
    fn new(temps: TempTys, binds: u32) -> Self {
        Lower {
            temps,
            next: 0,
            tys: Vec::new(),
            binds: vec![None; binds as usize],
            spine: Vec::new(),
            globals: Vec::new(),
            outer_spine: true,
        }
    }

    fn fresh(&mut self, ty: RTy) -> LocalId {
        let id = LocalId(self.next);
        self.next += 1;
        self.tys.push(ty);
        debug_assert_eq!(self.tys.len(), self.next as usize);
        id
    }

    /// The `RTy` recorded for `id` at mint time.
    fn bind_ty(&self, id: LocalId) -> RTy {
        self.tys[id.0 as usize]
    }

    /// Record the local `b` names. Indexed, not `get_mut`: `binds` is sized
    /// from [`TypedFn::binds`], so an out-of-range `BindingId` means the
    /// elaborator miscounted the body's binders. Writing nowhere would leave
    /// the later [`Self::local`] to invent a value; aborting says so.
    fn bind(&mut self, b: BindingId, id: LocalId) {
        self.binds[b.0 as usize] = Some(id);
    }

    /// The local a [`BindingId`] names.
    ///
    /// A `TypedProgram` binds before it reads, so this cannot fail — and it
    /// must not *quietly* fail. `lower` being total means "no `Result`", not
    /// "substitute something". Any default here is a live local (`LocalId(0)`
    /// is parameter 0, or the first temp), so a mis-sized `TypedFn::binds` or a
    /// read-before-bind would compile to a program that runs and returns the
    /// wrong answer. Trap instead, in release as well as debug.
    fn local(&self, b: BindingId) -> LocalId {
        self.binds[b.0 as usize].unwrap_or_else(|| read_before_bind(b))
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

    /// Bind an already-pooled `Int` constant to a fresh local. Every integer a
    /// lowered body pushes — an array pattern's length, a `<<>>` walk's initial
    /// bit cursor, a literal segment's width — was interned by the elaborator
    /// and reaches here as a [`ConstId`] on the node.
    fn int_const(&mut self, c: ConstId) -> LocalId {
        let int_t = self.temps.int;
        self.let_(int_t, Atom::Const(c))
    }

    /// A `PrimOp` atom with `imm = args.len()` — for the variadic `Make*`/
    /// `*ConcatN`/`Prepend`/`Append` opcodes whose operand is an argc.
    fn primn(&self, op: Op, args: Vec<LocalId>) -> Atom {
        let imm = args.len() as i32;
        Atom::PrimOp { op, args, imm }
    }

    /// Fold every pending let (from `mark` onward) around `tail`, innermost
    /// first, producing the sealed `CoreExpr` for this segment.
    fn seal(&mut self, mark: usize, tail: CoreExpr) -> CoreExpr {
        let mut e = tail;
        while self.spine.len() > mark {
            match self.spine.pop() {
                Some(Pending::Let { bind, rhs }) => {
                    e = CoreExpr::Let {
                        bind,
                        rhs,
                        body: Box::new(e),
                    };
                }
                Some(Pending::Join { bind, join }) => {
                    e = CoreExpr::LetJoin {
                        bind,
                        join: Box::new(join),
                        body: Box::new(e),
                    };
                }
                None => debug_assert!(false, "seal underflow"),
            }
        }
        e
    }

    /// Run `f` in a fresh spine segment, then seal every `Let` it pushed around
    /// the tail it produced.
    fn sealed(&mut self, f: impl FnOnce(&mut Self) -> CoreExpr) -> CoreExpr {
        let mark = self.spine.len();
        let tail = f(self);
        self.seal(mark, tail)
    }

    // ── expression lowering ────────────────────────────────────────────────

    /// Lower `e` as a self-contained expression tree of result type
    /// `result_ty` — a join arm in operand position, or the function's own
    /// tail. Opens a fresh spine segment.
    fn expr_as(&mut self, e: &TypedExpr, result_ty: RTy) -> CoreExpr {
        self.sealed(|lo| lo.expr_tail(e, result_ty))
    }

    /// Lower `e` in tail position. The `Let`/`Seq` spine is walked iteratively:
    /// a module toplevel is one `Let` per declaration and can be thousands deep.
    fn expr_tail(&mut self, e: &TypedExpr, result_ty: RTy) -> CoreExpr {
        let outer = std::mem::replace(&mut self.outer_spine, false);
        let mut e = e;
        loop {
            match e {
                TypedExpr::Let {
                    bind, init, body, ..
                } => {
                    self.let_bind_g(bind, init, outer);
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

    /// `let bind = init` in a statement/tail spine.
    ///
    /// Two kinds of binding own their slot rather than aliasing whatever local
    /// the initialiser landed in:
    ///
    /// * a module-scope binding — fn bodies address it by its own
    ///   `PushGlobal` slot, distinct from whatever it aliases;
    /// * a source-named `let` at any depth. Collapsing it into a local the
    ///   source already named would silently drop the `StoreLocal`/`PushLocal`
    ///   pair — and, for a heap value, the dead alias's `Drop` — that the
    ///   pre-Core-IR compiler emitted for it.
    ///
    /// An elaborator-minted temp (`bind.name == NO_STR`) — a labelled-argument
    /// spill, a destructuring scrutinee — names no source-level slot, so it
    /// collapses into whatever its initialiser landed in.
    fn let_bind(&mut self, bind: &TypedBind, init: &TypedExpr) {
        self.let_bind_g(bind, init, false)
    }

    fn let_bind_g(&mut self, bind: &TypedBind, init: &TypedExpr, outer: bool) {
        let v = match bind.global {
            Some(slot) => {
                let v = self.let_bound(bind, init);
                self.globals.push((v, slot.0));
                v
            }
            None if outer && bind.name != NO_STR => self.let_bound(bind, init),
            None => self.operand(init),
        };
        self.bind(bind.id, v);
    }

    /// `let bind = <init>` for a binding that owns its slot.
    ///
    /// Normally a `Let` of its own, even when `init` already lowered to a local:
    /// `let y = x` is two frame slots, and an `if`/`match` in initialiser
    /// position lands in the merge temp its `LetJoin` stores to, which the
    /// binding then copies out of — exactly the `PushLocal`/`StoreLocal` pair
    /// the pre-Core-IR compiler emitted.
    ///
    /// A short-circuit is the exception. `a && b` left its result on the
    /// operand stack and was stored once, straight into the binding; ANF has no
    /// stack, so [`Self::atom`] binds it — and that `LetJoin` *is* the
    /// binding's single store. Copying it again would add a store T0 never had,
    /// plus a `Drop` of the alias it leaves dead.
    fn let_bound(&mut self, bind: &TypedBind, init: &TypedExpr) -> LocalId {
        let mark = self.next;
        let (a, _ty) = self.atom(init);
        match a {
            Atom::Local(id)
                if id.0 >= mark && matches!(init, TypedExpr::And { .. } | TypedExpr::Or { .. }) =>
            {
                id
            }
            a => self.let_(bind.ty, a),
        }
    }

    /// Lower `e` to a `LocalId` operand: the value is bound via a `Let` on the
    /// spine (or is already a local), never left as a bare atom.
    fn operand(&mut self, e: &TypedExpr) -> LocalId {
        if let TypedExpr::Var {
            place: ValueRef::Local(b),
            ..
        } = e
        {
            return self.local(*b);
        }
        let (a, ty) = self.atom(e);
        match a {
            Atom::Local(id) => id,
            other => self.let_(ty, other),
        }
    }

    /// Lower a control-flow expression appearing in operand position by
    /// sealing it as its own tree and binding the result via a `LetJoin`.
    fn join_operand(&mut self, e: &TypedExpr, ty: RTy) -> LocalId {
        let join = self.expr_as(e, ty);
        self.let_join(ty, join)
    }

    /// Lower `e` to an `(Atom, RTy)`. Compound subexpressions recurse via
    /// [`Self::operand`] so the returned atom's operands are all `LocalId`s.
    ///
    /// A `Let`/`Seq` spine in operand position is straight-line code, not
    /// control flow: its bindings are spliced onto the *current* spine and the
    /// spine's tail becomes the atom. Sealing it as a join instead would bind
    /// the value twice (once as the join's result, once as the operand) and
    /// hide the value's type behind the join's, which is how a shaped `Drop`
    /// degrades to a generic one.
    fn atom(&mut self, e: &TypedExpr) -> (Atom, RTy) {
        let mut e = e;
        loop {
            match e {
                TypedExpr::Let {
                    bind, init, body, ..
                } => {
                    self.let_bind(bind, init);
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
            TypedExpr::Nil { .. } => Atom::prim(Op::PushNil, vec![]),
            TypedExpr::Var { place, .. } => match *place {
                // A bound local is authoritative: its type was fixed when the
                // bind was minted.
                ValueRef::Local(b) => {
                    let id = self.local(b);
                    return (Atom::Local(id), self.bind_ty(id));
                }
                // A raw frame slot the module walk allocated (a selective
                // `import mod.{x}` binding). Outside lower's `LocalId` space.
                ValueRef::Slot(slot) => Atom::PrimOp {
                    op: Op::PushLocal,
                    args: vec![],
                    imm: slot as i32,
                },
                ValueRef::Global(slot) => Atom::PrimOp {
                    op: Op::PushGlobal,
                    args: vec![],
                    imm: slot.0,
                },
                ValueRef::Capture(idx) => Atom::PrimOp {
                    op: Op::PushCapture,
                    args: vec![],
                    imm: idx as i32,
                },
                ValueRef::SelfClosure => Atom::prim(Op::PushSelf, vec![]),
            },
            // `a && b` / `a || b` in operand position. Only `b` is
            // conditional: `a` runs unconditionally, so it lowers onto the
            // *enclosing* spine and only the `If` over it is sealed as the
            // join. Sealing `a` inside the join too would sink its `Let` — and
            // the `Drop` of its last use — into the join's own tree.
            TypedExpr::And { lhs, rhs, .. } | TypedExpr::Or { lhs, rhs, .. } => {
                let and = matches!(e, TypedExpr::And { .. });
                let join = self.short_circuit(lhs, rhs, and, ty);
                let id = self.let_join(ty, join);
                return (Atom::Local(id), self.bind_ty(id));
            }
            // Control flow in operand position: seal it and bind the join.
            // `Let`/`Seq` never reach here — the splice loop above consumed
            // them — but naming them keeps this match exhaustive without a
            // wildcard, so a new node cannot be silently mis-lowered.
            TypedExpr::Let { .. }
            | TypedExpr::Seq { .. }
            | TypedExpr::If { .. }
            | TypedExpr::Match { .. } => {
                let id = self.join_operand(e, ty);
                return (Atom::Local(id), self.bind_ty(id));
            }
            // The checker already specialised `Add` to `AddInt` where the
            // operand type allowed it, so op selection is a field read.
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
                self.primn(Op::MakeTuple, args)
            }
            TypedExpr::TupleIndex { recv, idx, .. } => {
                let r = self.operand(recv);
                Atom::PrimOp {
                    op: Op::TupleIndex,
                    args: vec![r],
                    imm: *idx as i32,
                }
            }
            TypedExpr::Field {
                recv, idx, checked, ..
            } => {
                let r = self.operand(recv);
                Atom::PrimOp {
                    op: if *checked {
                        Op::GetField
                    } else {
                        Op::GetFieldUnchecked
                    },
                    args: vec![r],
                    imm: *idx as i32,
                }
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
                Atom::prim(Op::Index, vec![r, i])
            }
            TypedExpr::Slice {
                recv, start, end, ..
            } => {
                let r = self.operand(recv);
                let s = self.operand(start);
                let e = self.operand(end);
                Atom::prim(Op::ArraySlice, vec![r, s, e])
            }
            TypedExpr::Range { start, end, .. } => {
                let s = self.operand(start);
                let e = self.operand(end);
                Atom::prim(Op::MakeRange, vec![s, e])
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

    /// A dynamic callee is evaluated *before* the arguments, matching the
    /// source's left-to-right order.
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
            // A `@vm` builtin: the call *is* the opcode.
            TypedCallee::Builtin(op) => {
                let args = self.operands(args);
                Atom::prim(*op, args)
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
            (other, CoreExpr::Tail(Atom::prim(Op::PushFalse, vec![])))
        } else {
            (CoreExpr::Tail(Atom::prim(Op::PushTrue, vec![])), other)
        };
        CoreExpr::If {
            cond,
            then: Box::new(then),
            els: Box::new(els),
            ty: result_ty,
        }
    }

    /// `[a, b, ..xs, c]`: contiguous non-spread runs become `MakeArray`/
    /// `Prepend`/`Append` and spreads `ArrayConcat`, folded left-to-right.
    /// `ty` is the whole literal's `Array(T)`; every intermediate fold step
    /// has it too, so Perceus drops the discarded halves of a concat chain.
    fn array(&mut self, elems: &[TypedArrayElem], ty: RTy) -> Atom {
        let has_spread = elems.iter().any(|e| matches!(e, TypedArrayElem::Spread(_)));
        if !has_spread {
            let mut args = Vec::with_capacity(elems.len());
            for el in elems {
                if let TypedArrayElem::Elem(ex) = el {
                    args.push(self.operand(ex));
                }
            }
            return self.primn(Op::MakeArray, args);
        }

        let mut acc: Option<LocalId> = None;
        let mut i = 0usize;
        while i < elems.len() {
            if let Some(TypedArrayElem::Spread(sp)) = elems.get(i) {
                let s = self.operand(sp);
                acc = Some(match acc {
                    Some(a) => self.let_(ty, Atom::prim(Op::ArrayConcat, vec![a, s])),
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
            for el in elems.get(i..j).unwrap_or_default() {
                if let TypedArrayElem::Elem(ex) = el {
                    run.push(self.operand(ex));
                }
            }
            // Head-run before the first spread: prepend directly onto it.
            if acc.is_none()
                && let Some(TypedArrayElem::Spread(sp)) = elems.get(j)
            {
                let s = self.operand(sp);
                let k = run.len() as i32;
                run.push(s);
                acc = Some(self.let_(
                    ty,
                    Atom::PrimOp {
                        op: Op::Prepend,
                        args: run,
                        imm: k,
                    },
                ));
                i = j + 1;
                continue;
            }
            let a = match acc {
                Some(a) => {
                    let k = run.len() as i32;
                    let mut args = vec![a];
                    args.extend(run);
                    self.let_(
                        ty,
                        Atom::PrimOp {
                            op: Op::Append,
                            args,
                            imm: k,
                        },
                    )
                }
                None => {
                    let a = self.primn(Op::MakeArray, run);
                    self.let_(ty, a)
                }
            };
            acc = Some(a);
            i = j;
        }
        match acc {
            Some(id) => Atom::Local(id),
            None => self.primn(Op::MakeArray, vec![]),
        }
    }

    /// `"a${b}c"`. Literal pieces are already pooled; expression pieces are
    /// stringified. An interpolation with no parts elaborates to a `Const`, so
    /// the empty `StrConcatN` below is unreachable in a real program.
    fn interp(&mut self, parts: &[TypedInterpPart]) -> Atom {
        let str_t = self.temps.string;
        let mut args = Vec::with_capacity(parts.len());
        for part in parts {
            let a = match part {
                TypedInterpPart::Str(c) => self.let_(str_t, Atom::Const(*c)),
                TypedInterpPart::Expr(ex) => {
                    let v = self.operand(ex);
                    self.let_(str_t, Atom::prim(Op::ToString, vec![v]))
                }
            };
            args.push(a);
        }
        match args.len() {
            1 => Atom::Local(args[0]),
            _ => self.primn(Op::StrConcatN, args),
        }
    }

    /// `<<..>>` in value position. Each segment evaluates to a `Binary`,
    /// whatever its source kind — `:int`/`:utf8` encode into one.
    fn bin_lit(&mut self, segs: &[TypedBinSeg]) -> Atom {
        let bin_t = self.temps.binary;
        let mut out = Vec::with_capacity(segs.len());
        for seg in segs {
            let id = match seg {
                TypedBinSeg::Int { value, bits } => {
                    let v = self.operand(value);
                    let b = self.operand(bits);
                    self.let_(bin_t, Atom::prim(Op::BinFromInt, vec![v, b]))
                }
                TypedBinSeg::Binary { value, bits } => {
                    let v = self.operand(value);
                    match bits {
                        Some(b) => {
                            let b = self.operand(b);
                            self.let_(bin_t, Atom::prim(Op::BinTake, vec![v, b]))
                        }
                        None => v,
                    }
                }
                TypedBinSeg::Utf8 { value } => {
                    let v = self.operand(value);
                    self.let_(bin_t, Atom::prim(Op::BinFromString, vec![v]))
                }
            };
            out.push(id);
        }
        match out.len() {
            1 => Atom::Local(out[0]),
            _ => self.primn(Op::BinConcatN, out),
        }
    }

    // ── pattern lowering ───────────────────────────────────────────────────

    /// Lower `arms` against an already-bound `scrut`. A guarded arm lowers to
    /// `pat => if <guard> { body } else { match scrut on remaining arms }`: the
    /// guard-fail path is a fresh nested `Match` over the tail of the arm list,
    /// so guard failure resumes matching exactly as the source does. The tail
    /// arms also remain in the enclosing `Match` for ordinary pattern
    /// fallthrough, so each guard duplicates the trailing arms once.
    /// `arr[i] or <pure atom>` → one [`Op::IndexOr`], no `Option` cell.
    ///
    /// `or_expr` elaborates `x or d` to a two-arm match on `Option`. When `x`
    /// is an `Index`, the ordinary lowering makes `Index` build a `Some` box
    /// that the very next instruction destructures and drops — one heap
    /// allocation per index, on `grid[y] or []`-shaped code.
    ///
    /// Only fuses when the failure arm is a *pure* atom (a constant, or a
    /// variable read): the fused op has no way to skip evaluating it, so
    /// anything with an effect, a cost, or control flow must keep the lazy
    /// match. That is why this is a shape test and not a general one.
    fn index_or(&mut self, scrut: &TypedExpr, arms: &[TypedArm]) -> Option<CoreExpr> {
        let TypedExpr::Index { recv, index, .. } = scrut else {
            return None;
        };
        let [a, b] = arms else { return None };
        if a.guard.is_some() || b.guard.is_some() {
            return None;
        }
        // The payload arm is `Some(v) -> v`; the other is `None -> <atom>`.
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
        // A constant default rides in the operand and never touches the stack:
        // that is the whole win on `a[i] or 0`, whose old fused op *skipped*
        // the default's push on a hit. Any other pure atom is pushed, and
        // `imm = -1` (never a `ConstId`) tells the VM to pop it.
        Some(CoreExpr::Tail(match dflt {
            TypedExpr::Const { value, .. } => Atom::PrimOp {
                op: Op::IndexOr,
                args: vec![r, i],
                imm: value.0 as i32,
            },
            _ => Atom::PrimOp {
                op: Op::IndexOr,
                args: vec![r, i, self.operand(dflt)],
                imm: -1,
            },
        }))
    }

    fn match_arms(&mut self, scrut: LocalId, arms: &[TypedArm], result_ty: RTy) -> CoreExpr {
        let mut core_arms = Vec::with_capacity(arms.len());
        for (i, arm) in arms.iter().enumerate() {
            let mut nested = Vec::new();
            let pat = self.pattern(&arm.pat, scrut, &mut nested);
            let leaf = match &arm.guard {
                None => ArmLeaf::Body(&arm.body),
                Some(guard) => ArmLeaf::Guarded {
                    guard,
                    body: &arm.body,
                },
            };
            let fall = ArmFall::Arms {
                scrut,
                rest: arms.get(i + 1..).unwrap_or_default(),
            };
            let body = self.nested_arm_body(nested.into(), leaf, fall, result_ty);
            core_arms.push((pat, body));
        }
        CoreExpr::Match {
            scrut,
            arms: core_arms,
            ty: result_ty,
        }
    }

    /// Lower `p` to a flat [`CorePat`] head. Any structure `CorePat` cannot
    /// carry — a compound sub-pattern in a `Ctor` field, or a `Tuple`/`Array`
    /// pattern (which have no `CorePat` variant at all) — is bound to a fresh
    /// [`LocalId`] and pushed to `nested` for [`Self::nested_arm_body`] to
    /// flatten into successive `Match`/`Let` nodes inside the arm body.
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
                // `fields` is exactly the variant's arity, so `emit`'s payload
                // binds line up one-for-one with the declared fields.
                let mut binds = Vec::with_capacity(fields.len());
                for fp in fields {
                    let id = self.fresh(fp.ty());
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
            // No `CorePat` head discriminates these; the arm always enters and
            // the destructure (plus any refutable check) happens in the body.
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

    /// Lower the fallthrough continuation for an arm whose guard or nested
    /// refutable sub-pattern failed: resume matching — either on the remaining
    /// source arms, or on the next alternative of an enclosing or-pattern.
    fn arm_fallthrough(&mut self, fall: ArmFall<'_>, result_ty: RTy) -> CoreExpr {
        match fall {
            ArmFall::Arms { scrut, rest } => self.match_arms(scrut, rest, result_ty),
            ArmFall::OrAlt(oc) => self.or_alternatives(*oc, result_ty),
        }
    }

    /// `if cond { then } else { <fallthrough> }`, sealed from `mark`: the shape
    /// every refutable check in a pattern arm lowers to.
    fn guard_if(
        &mut self,
        mark: usize,
        cond: LocalId,
        then: CoreExpr,
        fall: ArmFall<'_>,
        result_ty: RTy,
    ) -> CoreExpr {
        let els = self.arm_fallthrough(fall, result_ty);
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

    /// Lower the remaining alternatives of an or-pattern. Only the alternative
    /// pattern itself uses the try-next-alternative fallthrough; once it
    /// matches, `rest_nested` and `leaf` are lowered with `outer` (via
    /// [`ArmLeaf::OrBody`]) so a guard/trailing-pattern miss falls to the next
    /// arm — commit-on-first-match.
    fn or_alternatives<'p>(&mut self, oc: OrCont<'p>, result_ty: RTy) -> CoreExpr {
        let OrCont {
            scrut,
            alts,
            rest_nested,
            leaf,
            outer,
        } = oc;
        let Some((first, tail)) = alts.split_first() else {
            return self.arm_fallthrough(outer, result_ty);
        };
        let inner_fall = if tail.is_empty() {
            outer.clone()
        } else {
            ArmFall::OrAlt(Box::new(OrCont {
                scrut,
                alts: tail,
                rest_nested: rest_nested.clone(),
                leaf: leaf.clone(),
                outer: outer.clone(),
            }))
        };
        let body_leaf = ArmLeaf::OrBody {
            rest_nested,
            leaf: Box::new(leaf),
            outer,
        };
        self.nested_arm_body(
            VecDeque::from([(scrut, first)]),
            body_leaf,
            inner_fall,
            result_ty,
        )
    }

    /// Flatten `nested` residual sub-patterns into successive `Match`/`Let`
    /// nodes wrapping the arm's body. Irrefutable structure (`Tuple`, and
    /// `Bind`/`Wild` leaves) becomes `Let` projections on the current spine;
    /// refutable structure (`Ctor`, `Lit`, `Array` length) becomes an inner
    /// `Match`/`If` whose fail edge lowers `fall` — the remaining source arms —
    /// so a nested-pattern miss resumes matching exactly as the source does.
    fn nested_arm_body<'p>(
        &mut self,
        mut nested: VecDeque<(LocalId, &'p TypedPat)>,
        leaf: ArmLeaf<'p>,
        fall: ArmFall<'p>,
        result_ty: RTy,
    ) -> CoreExpr {
        let mark = self.spine.len();
        loop {
            let Some((v, p)) = nested.pop_front() else {
                let tail = match leaf {
                    ArmLeaf::Body(e) => self.expr_tail(e, result_ty),
                    ArmLeaf::Guarded { guard, body } => {
                        let cond = self.operand(guard);
                        let then = self.expr_as(body, result_ty);
                        // already sealed from `mark`; the seal below is a no-op
                        self.guard_if(mark, cond, then, fall, result_ty)
                    }
                    ArmLeaf::BinCont(bc) => self.bin_segments(*bc, fall, result_ty),
                    ArmLeaf::OrBody {
                        rest_nested,
                        leaf,
                        outer,
                    } => self.nested_arm_body(rest_nested.into(), *leaf, outer, result_ty),
                };
                return self.seal(mark, tail);
            };
            match p {
                TypedPat::Wild { .. } => {}
                TypedPat::Bind(b) => self.bind(b.id, v),
                TypedPat::Tuple { elems, .. } => {
                    let mut tmp = Vec::with_capacity(elems.len());
                    for (j, elem) in elems.iter().enumerate() {
                        let ej = self.let_(
                            elem.ty(),
                            Atom::PrimOp {
                                op: Op::TupleIndex,
                                args: vec![v],
                                imm: j as i32,
                            },
                        );
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
                    let body = self.nested_arm_body(nested, leaf, fall.clone(), result_ty);
                    let els = self.arm_fallthrough(fall, result_ty);
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
                    let len = self.let_(int_t, Atom::prim(Op::ArrayLen, vec![v]));
                    let n = self.int_const(*len_c);
                    let cmp = match rest {
                        PatRest::None => Op::Eq,
                        PatRest::Discard | PatRest::Bind(_) => Op::Gte,
                    };
                    let ok = self.let_(bool_t, Atom::prim(cmp, vec![len, n]));
                    let then = {
                        let then_mark = self.spine.len();
                        let mut tmp = Vec::with_capacity(pre);
                        for (j, ep) in prefix.iter().enumerate() {
                            let ej = self.let_(
                                *elem_ty,
                                Atom::PrimOp {
                                    op: Op::ElemAt,
                                    args: vec![v],
                                    imm: j as i32,
                                },
                            );
                            tmp.push((ej, ep));
                        }
                        if let PatRest::Bind(b) = rest {
                            let n2 = self.int_const(*len_c);
                            let tail = self.let_(vty, Atom::prim(Op::SeqDrop, vec![v, n2]));
                            self.bind(b.id, tail);
                        }
                        for r in tmp.into_iter().rev() {
                            nested.push_front(r);
                        }
                        let inner = self.nested_arm_body(nested, leaf, fall.clone(), result_ty);
                        self.seal(then_mark, inner)
                    };
                    return self.guard_if(mark, ok, then, fall, result_ty);
                }
                TypedPat::Range { lo, hi, .. } => {
                    let int_t = self.temps.int;
                    let bool_t = self.temps.bool;
                    let lo = self.let_(int_t, Atom::Const(*lo));
                    let ge = self.let_(bool_t, Atom::prim(Op::Gte, vec![v, lo]));
                    let then = {
                        let then_mark = self.spine.len();
                        let hi = self.let_(int_t, Atom::Const(*hi));
                        let lt = self.let_(bool_t, Atom::prim(Op::Lt, vec![v, hi]));
                        let inner = self.nested_arm_body(nested, leaf, fall.clone(), result_ty);
                        self.guard_if(then_mark, lt, inner, fall.clone(), result_ty)
                    };
                    return self.guard_if(mark, ge, then, fall, result_ty);
                }
                TypedPat::Or { alts, .. } => {
                    let rest_nested: Vec<_> = nested.into_iter().collect();
                    let body = self.or_alternatives(
                        OrCont {
                            scrut: v,
                            alts,
                            rest_nested,
                            leaf,
                            outer: fall,
                        },
                        result_ty,
                    );
                    return self.seal(mark, body);
                }
                TypedPat::Bin {
                    zero, segs, rest, ..
                } => {
                    let int_t = self.temps.int;
                    let total = self.let_(int_t, Atom::prim(Op::BinBitSize, vec![v]));
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

    /// Lower one step of a `<<>>` pattern: the head segment of `bc.segs`
    /// (bounds check → read → match its value pattern), then recurse via a
    /// `BinCont` leaf so the value pattern's bindings are in scope for the next
    /// segment's `size(...)`. When `segs` is empty, emit the trailing `..rest`
    /// bind or exact-length check and resume with `bc.after` / `bc.then`.
    fn bin_segments<'p>(&mut self, bc: BinCont<'p>, fall: ArmFall<'p>, result_ty: RTy) -> CoreExpr {
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
                    let rem = self.let_(int_t, Atom::prim(Op::Sub, vec![total, cursor]));
                    let rv = self.let_(bin_ty, Atom::prim(Op::BinView, vec![bin, cursor, rem]));
                    self.bind(b.id, rv);
                    let body = self.nested_arm_body(after.into(), then, fall, result_ty);
                    return self.seal(mark, body);
                }
                PatRest::Discard => {
                    let body = self.nested_arm_body(after.into(), then, fall, result_ty);
                    return self.seal(mark, body);
                }
                PatRest::None => {
                    let ok = self.let_(bool_t, Atom::prim(Op::Eq, vec![cursor, total]));
                    let body = self.nested_arm_body(after.into(), then, fall.clone(), result_ty);
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
                    Atom::prim(Op::BinMatchPrefix, vec![bin, cursor, pfx]),
                );
                let then_e = {
                    let tm = self.spine.len();
                    let w = self.int_const(*bits);
                    let cur2 = self.let_(int_t, Atom::prim(Op::Add, vec![cursor, w]));
                    let inner = self.bin_segments(cont(cur2), fall.clone(), result_ty);
                    self.seal(tm, inner)
                };
                self.guard_if(mark, ok, then_e, fall, result_ty)
            }
            // A `:utf8` codepoint segment: variable width. `BinReadUtf8` yields
            // `(codepoint, nbits)` with `nbits == 0` signalling decode failure.
            TypedBinPatSeg::Utf8 { value } => {
                let pair_ty = self.temps.int_pair;
                let pair = self.let_(pair_ty, Atom::prim(Op::BinReadUtf8, vec![bin, cursor]));
                let cp = self.let_(
                    int_t,
                    Atom::PrimOp {
                        op: Op::TupleIndex,
                        args: vec![pair],
                        imm: 0,
                    },
                );
                let nbits = self.let_(
                    int_t,
                    Atom::PrimOp {
                        op: Op::TupleIndex,
                        args: vec![pair],
                        imm: 1,
                    },
                );
                let z = self.int_const(zero);
                let ok = self.let_(bool_t, Atom::prim(Op::Gt, vec![nbits, z]));
                let then_e = {
                    let tm = self.spine.len();
                    let cur2 = self.let_(int_t, Atom::prim(Op::Add, vec![cursor, nbits]));
                    let leaf2 = ArmLeaf::BinCont(Box::new(cont(cur2)));
                    let inner = self.nested_arm_body(
                        VecDeque::from([(cp, value)]),
                        leaf2,
                        fall.clone(),
                        result_ty,
                    );
                    self.seal(tm, inner)
                };
                self.guard_if(mark, ok, then_e, fall, result_ty)
            }
            // Int / Binary segment. Width is the size expression (in bits), the
            // elaborator having folded in the unit multiplier and the default
            // width; a sizeless `:binary` segment consumes the rest.
            TypedBinPatSeg::Int { bits, value } => {
                let width = self.operand(bits);
                self.bin_read(
                    mark,
                    bin,
                    total,
                    cursor,
                    width,
                    true,
                    Op::BinReadInt,
                    int_t,
                    value,
                    cont,
                    fall,
                    result_ty,
                )
            }
            TypedBinPatSeg::Binary { bits, value } => match bits {
                Some(bits) => {
                    let width = self.operand(bits);
                    self.bin_read(
                        mark,
                        bin,
                        total,
                        cursor,
                        width,
                        true,
                        Op::BinView,
                        bin_ty,
                        value,
                        cont,
                        fall,
                        result_ty,
                    )
                }
                None => {
                    let width = self.let_(int_t, Atom::prim(Op::Sub, vec![total, cursor]));
                    self.bin_read(
                        mark,
                        bin,
                        total,
                        cursor,
                        width,
                        false,
                        Op::BinView,
                        bin_ty,
                        value,
                        cont,
                        fall,
                        result_ty,
                    )
                }
            },
        }
    }

    /// The shared tail of a fixed-width `<<>>` segment: bounds-check
    /// `cursor + width <= total` (unless the width *is* the remainder), read
    /// the value, then match the segment's value pattern and continue.
    #[allow(clippy::too_many_arguments)]
    fn bin_read<'p>(
        &mut self,
        mark: usize,
        bin: LocalId,
        total: LocalId,
        cursor: LocalId,
        width: LocalId,
        need_check: bool,
        read_op: Op,
        val_ty: RTy,
        value: &'p TypedPat,
        cont: impl Fn(LocalId) -> BinCont<'p>,
        fall: ArmFall<'p>,
        result_ty: RTy,
    ) -> CoreExpr {
        let int_t = self.temps.int;
        let bool_t = self.temps.bool;
        let end = self.let_(int_t, Atom::prim(Op::Add, vec![cursor, width]));
        let build_then = |lo: &mut Self, fall: ArmFall<'p>| -> CoreExpr {
            let tm = lo.spine.len();
            let sv = lo.let_(val_ty, Atom::prim(read_op, vec![bin, cursor, width]));
            let leaf2 = ArmLeaf::BinCont(Box::new(cont(end)));
            let inner = lo.nested_arm_body(VecDeque::from([(sv, value)]), leaf2, fall, result_ty);
            lo.seal(tm, inner)
        };
        if need_check {
            let ok = self.let_(bool_t, Atom::prim(Op::Lte, vec![end, total]));
            let then_e = build_then(self, fall.clone());
            self.guard_if(mark, ok, then_e, fall, result_ty)
        } else {
            let then_e = build_then(self, fall);
            self.seal(mark, then_e)
        }
    }
}

/// One slot per declared constructor field. Constructor arity is small, so the
/// common case stays off the heap.
pub(crate) type Slots<T> = SmallVec<[Option<T>; 4]>;

/// A malformed argument list. Each variant hands back the offending item
/// itself, so a caller that needs its span carries the span in `T` rather than
/// re-indexing the sequence it passed in. Elaboration discards these (the
/// typechecker has already reported them); the compiler renders them.
pub(crate) enum SlotError<T> {
    /// Positional item past the last declared field.
    ExtraPositional(T),
    /// Label naming no declared field, with the label.
    UnknownLabel(StrId, T),
    /// Item that landed on an already-filled field, with that field's index.
    Duplicate(T, usize),
}

/// Slot positional and labeled items into declared-field order: positionals
/// fill slots left-to-right, a labeled item lands on its declared index.
/// Overflowing positionals and unknown labels fill no slot. Shared by the
/// elaborator and the typechecker's `slot_ctor_args`, which layers diagnostics
/// on the returned errors. The exhaustiveness checker slots `Pat`s the same
/// way, over its own constructor table.
pub(crate) fn slot_labeled<T>(
    labels: &[StrId],
    arity: usize,
    items: impl IntoIterator<Item = (Option<StrId>, T)>,
) -> (Slots<T>, Vec<SlotError<T>>) {
    let mut by_pos: Slots<T> = (0..arity).map(|_| None).collect();
    let mut errors = Vec::new();
    let mut next_pos = 0usize;
    for (label, val) in items {
        let idx = match label {
            None => {
                let i = next_pos;
                next_pos += 1;
                if i >= arity {
                    errors.push(SlotError::ExtraPositional(val));
                    continue;
                }
                i
            }
            Some(sid) => match labels.iter().position(|&l| l == sid) {
                Some(i) => i,
                None => {
                    errors.push(SlotError::UnknownLabel(sid, val));
                    continue;
                }
            },
        };
        // First item to claim a field keeps it; every later one is reported.
        if by_pos[idx].is_some() {
            errors.push(SlotError::Duplicate(val, idx));
        } else {
            by_pos[idx] = Some(val);
        }
    }
    (by_pos, errors)
}
