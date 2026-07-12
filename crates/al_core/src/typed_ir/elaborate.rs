//! AST → Typed IR: the elaborator's skeleton and its expression arms.
//!
//! This is the *resolution* half of what `core_ir::lower` does today. Lowering
//! currently answers two unrelated questions in one walk: "what does this
//! expression mean" — which field index, which variant, which callee, which
//! typed opcode — and "in what order are its subexpressions evaluated" (ANF).
//! Only the first needs the typechecker. Moving it here leaves `lower` a total
//! function from a `TypedProgram` to Core IR.
//!
//! Every node this produces carries an [`RTy`] drawn from the [`ResolvedPool`]
//! the caller owns. The only bridge from a live inference `Ty` into that pool
//! is [`ElabCtx::resolve_rty`] (a [`super::zonk::Zonker`]). Past that bridge the
//! elaborator never consults the union-find: `Int + Int` selects `AddInt`
//! because [`ResolvedPool::prim_of`] says `Int`, and a constructor's field types
//! come from substituting the use site's type arguments into the constructor's
//! resolved signature — not from unifying inside the engine.
//!
//! ## What the elaborator decides, and lowering therefore cannot
//!
//! * `+` on two `Int`s is `Op::AddInt`: [`TypedExpr::Binary`] has no
//!   unspecialised form to fall back to.
//! * `u.id` is a field *index*, and `Some(x)` is a `VariantRef` whose arguments
//!   are already in declared-field order — labels reordered, `..base` spreads
//!   expanded into tag-checked projections.
//! * `x or d` has no [`TypedExpr`] arm at all: it is a [`TypedExpr::Match`] over
//!   the `Option`/`Result` shape [`ElabCtx::or_shape`] reports.
//! * A constructor or builtin in value position is a zero-capture
//!   [`TypedExpr::Closure`] over an eta wrapper appended to the program's `fns`.
//!
//! Patterns are [`super::elaborate_pat`]'s job; this module implements its
//! [`PatCtx`] seam. Name resolution is [`super::resolve::Denotation`]'s.
//!
//! ## Totality
//!
//! [`elaborate_body`] and [`elaborate_toplevel`] return a [`TypedFn`]. They have
//! no error case — which is what makes [`super::TypedProgram`], and therefore
//! `lower`, `perceus` and `emit`, total: there is no half-built function for a
//! caller to discard, and no internal-error diagnostic for a user to see.
//!
//! Three things pay for that.
//!
//! * **The proof.** Elaboration only ever runs on a module the check walk left
//!   free of error diagnostics (`bytecode::compiler`'s `CleanModule`, which
//!   `elaborate_then_materialize` consumes). Every question the elaborator asks
//!   the walk — the type at a span, the denotation of a name, the field index of
//!   a `.field` — the walk has already answered, or it would have reported an
//!   error and denied the proof. That includes [`ast::ErrorNode`], the one form
//!   with no elaboration at all: `compile_expr` restates the parse error, so no
//!   clean module contains one.
//! * **Types that make the question unaskable.** `&&`/`||` are not
//!   [`ValueBinop`](crate::bytecode::ValueBinop)s, so `specialize_binop` cannot be
//!   handed an operator that has no opcode. A [`CtorPat`] has exactly one field
//!   type per declared label or it is not a `CtorPat`, so a constructor's
//!   payload cannot be built with a field type missing. Both were poison sites;
//!   neither is spellable now.
//! * **[`elaborator_bug`]** for what is left: a question the walk did not
//!   answer means the proof above is broken. That is a bug in a pass that has
//!   already run, not a bad program, and it aborts rather than emitting bytecode
//!   for a body it could not build.
//!
//! An *unsolved* type variable is explicitly not one of those: see
//! [`super::zonk::Zonker::zonk_or_opaque`].

use std::collections::HashMap;

use smallvec::SmallVec;

use super::elaborate_pat::{CtorPat, PatCtx, elaborate_arms};
use super::eta::{FnRTy, FnTable, eta_wrapper};
use super::resolve::{CallForm, Denotation, EtaTarget, ValueForm};
use super::rty::{RTy, ResolvedNode, ResolvedPool};
use super::slots::slot_labeled;
use super::{
    BindingId, GlobalSlot, TypedArm, TypedArrayElem, TypedBinSeg, TypedBind, TypedCallee,
    TypedExpr, TypedFn, TypedInterpPart, TypedPat, ValueRef,
};
use crate::ast;
use crate::bytecode::{BinopKind, Op, ShortCircuitOp, Value, ValueBinop, specialize_binop};
use crate::core_ir::{ConstId, FuncIdx, VariantRef};
use crate::span::Span;
use crate::types::{NO_STR, Prim, StrId, Ty};

/// One scheduled node of a block: its index in the block's `body`, and — for a
/// top-level `fn`/`const` — the entry-frame slot its binding must land in. The
/// two travel together, so a declaration cannot be scheduled at one slot and
/// defined at another.
type Step = (usize, Option<GlobalSlot>);

/// The `Option`/`Result` shape behind an `or`-expression's left-hand side.
///
/// `lower` used to ask for this because it built the `Match` itself; now the
/// elaborator does, and `TypedExpr` has no `Or`-expression arm for `lower` to
/// misinterpret.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrShape {
    /// The failure variant (`None`/`Err`).
    pub fail: VariantRef,
    /// The success variant (`Some`/`Ok`), always arity 1.
    pub ok: VariantRef,
    /// Whether the failure variant carries a payload (`Result` yes, `Option`
    /// no).
    pub err_has_payload: bool,
}

/// The check walk did not answer a question elaboration had to ask, so the
/// module's `CleanModule` proof is wrong — see this module's *Totality* section.
///
/// This is not a diagnostic and no program reaches it. A user-facing error
/// would already have denied the proof, and every other pass in the pipeline
/// (`lower`, `perceus`, `emit`) consumes a [`super::TypedProgram`] and is total.
/// So reaching here is a broken invariant, not a bad program, and the compiler
/// aborts rather than emitting bytecode for a body it could not build.
///
/// `why` names the question, e.g. `"field access"`.
///
/// A `panic` in a compiler is a last resort, and the crate denies them; this one
/// is deliberate. There is no value to return (`-> !`) and no user to address:
/// the alternative — a diagnostic asking the user to report a bug in a program
/// that is correct — is the escape hatch the `TypedProgram` pipeline exists to
/// close, and reinstating it would let a real regression hide behind a message.
#[allow(clippy::panic)]
#[cold]
#[inline(never)]
pub fn elaborator_bug(why: &'static str, span: Span) -> ! {
    panic!(
        "internal compiler error: {why} at {span:?} is well-typed but was not elaborated. \
         Please report this as a compiler bug."
    )
}

/// Services the elaborator needs from the enclosing compilation: the same seam
/// `core_ir::lower::LowerCtx` is today, minus everything that existed only to
/// serve ANF, plus [`Self::resolve_rty`].
///
/// There is deliberately no `engine()`. The elaborator runs inside the check
/// phase, but everything it *decides* it decides against the pool; the engine
/// appears only behind [`Self::resolve_rty`], which is where an unsolved
/// variable becomes an honest opaque type rather than a silent `false`.
pub trait ElabCtx {
    /// Resolve a live inference `Ty` into `pool`. Total: an undetermined type is
    /// operationally a polymorphic one (`Zonker::zonk_or_opaque`).
    fn resolve_rty(&mut self, pool: &mut ResolvedPool, t: Ty) -> RTy;

    fn intern(&mut self, s: &str) -> StrId;
    fn str(&self, id: StrId) -> &str;

    /// Pool a constant value; the returned `ConstId` is the `PushConst` operand
    /// `emit` will use verbatim.
    fn add_const(&mut self, v: Value) -> ConstId;
    /// Parse-and-pool a numeric literal.
    fn number_const(&mut self, lit: &ast::NumberLiteral) -> (ConstId, Ty);
    /// Pool a string literal.
    fn string_const(&mut self, s: &str) -> ConstId;
    /// Pool an integer literal.
    fn int_const(&mut self, i: i64) -> ConstId;
    /// Pool a `Binary` constant with the given raw bytes and bit length.
    fn binary_const(&mut self, bytes: Vec<u8>, bit_len: u64) -> ConstId;

    /// Resolve a call/value name to `(instantiated_ty, denotation)`, or `None`
    /// when unbound. The [`Denotation`] is already position-aware: the walk
    /// hands back the pair of loads, not a raw slot the elaborator has to
    /// reinterpret.
    fn resolve_name(&mut self, name: &str) -> Option<(Ty, Denotation)>;
    /// Resolve a `qualifier.member` name. `span` is the member's, for the
    /// internal-error report should the module's interface turn out to be
    /// missing the runtime binding the check walk resolved.
    fn resolve_qualified(
        &mut self,
        qual: &str,
        member: &str,
        span: Span,
    ) -> Option<(Ty, Denotation)>;
    /// Field index for `.field` on a receiver of `receiver` type — the
    /// across-all-variants check the check walk already ran.
    fn ctor_field(&mut self, receiver: Ty, field: &str) -> Option<(u32, Ty)>;
    /// Declared field-label order of the variant `v` constructs.
    ///
    /// Keyed on the [`VariantRef`] the walk already resolved, not on a source
    /// name: `mod.Ctor(..)` names a constructor that is *not* in scope under its
    /// bare name, so a name lookup here would miss one the check walk accepted.
    fn ctor_labels(&mut self, v: VariantRef) -> Option<Vec<StrId>>;
    /// The `(func_idx, captures)` the check walk assigned to the
    /// `FunctionExpression` at `span`, which must be one written directly inside
    /// the body being elaborated — the only lambdas this elaboration can reach.
    fn closure(&mut self, span: Span) -> Option<(FuncIdx, Vec<StrId>)>;
    /// The function body the check walk emitted for the module-scope binding at
    /// `slot`, i.e. a top-level `fn` declaration. `None` for a slot holding a
    /// plain value.
    ///
    /// This is what a `fn` *declaration* has instead of a [`Self::closure`]
    /// entry: its identity at runtime is the entry-frame slot the toplevel
    /// spine stores it into ([`TypedBind::global`]), so def and use cannot name
    /// different functions.
    fn fn_of_global(&self, slot: GlobalSlot) -> Option<FuncIdx>;
    /// The module's own top-level `fn`/`const` declarations: `(index of the
    /// declaration's node in the module block's `body`, its entry-frame slot)`,
    /// in SCC-visit order, dependencies first.
    ///
    /// This both *schedules* the toplevel spine — a forward-referenced `const`
    /// must be stored before it is read, so decls are not walked in source
    /// order — and hands each declaration the slot the check walk already gave
    /// it. One pair, so a decl cannot be scheduled at one slot and defined at
    /// another.
    ///
    /// Empty for a caller that has no module toplevel to schedule, including a
    /// function body's outermost block: source order is then used and no bind
    /// is a global.
    fn toplevel_decls(&self) -> Vec<(usize, GlobalSlot)> {
        Vec::new()
    }
    /// The entry-frame slot the module walk allocated for the *next*
    /// module-scope `let`/destructured binding, or `None` when the block being
    /// elaborated is not a module toplevel.
    ///
    /// The elaborator reads the allocation rather than inventing it: the check
    /// walk hands out the slots and emits every fn body against
    /// `PushGlobal <slot>` before the toplevel is elaborated. Each call
    /// *consumes* one binding, in the order both walks visit the module's
    /// statements, so a name rebound at module scope (`x = 1; f = fn() x;
    /// x = 2`) gets a distinct slot per binding without anyone keying on the
    /// name — which is not an identity here.
    ///
    /// The answer lands in [`TypedBind::global`] and is never looked up again:
    /// no name-keyed queue survives into `lower`, `perceus` or `emit`.
    /// Required rather than defaulted — an `ElabCtx` that allocates entry-frame
    /// slots and forgets to answer here would silently mis-store every global.
    fn next_global_slot(&mut self) -> Option<GlobalSlot>;
    /// `Option`/`Result` shape for `expr or default`.
    fn or_shape(&mut self, lhs_ty: Ty) -> Option<OrShape>;

    fn ty_nil(&mut self) -> Ty;
    fn ty_bool(&mut self) -> Ty;
    fn ty_int(&mut self) -> Ty;
    fn ty_string(&mut self) -> Ty;
    fn ty_binary(&mut self) -> Ty;
}

/// One step of the check walk, recorded in entry order and replayed positionally
/// by the elaborator.
///
/// The walk and the elaborator traverse the same AST, and the elaborator is only
/// correct if it enters *exactly* the nodes the walk entered. Two things can make
/// them disagree: the type of a node, and whether a `name.field` shape is a
/// qualified module member (whose qualifier neither walk enters) or an ordinary
/// field access (whose receiver both do). The first is a value only the walk
/// knows; the second is a decision only the walk may make — it depends on the
/// value env at the point of the access, which has moved on by the time a
/// deferred body is elaborated. So the walk records both, and the elaborator
/// reads rather than re-derives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WalkStep {
    /// The type the walk inferred for the expression it entered here.
    Ty(Ty),
    /// The walk's verdict on a `left.right` shape: `true` when it resolved to a
    /// module member and skipped `left`, `false` when it is a field access.
    Qualified(bool),
}

/// Elaborate `body` into a [`TypedFn`]. `params` are already inferred; each is
/// bound as `BindingId(i)`. Eta wrappers the body needs are appended to `fns`.
///
/// This is also how the entry file's `__main__` is elaborated when it is a bare
/// expression rather than a module block; a block goes through
/// [`elaborate_toplevel`], because the two differ in exactly one thing — whether
/// the check walk entered the body node itself, and so whether `walk_tys` starts
/// with its type.
#[allow(clippy::too_many_arguments)]
pub fn elaborate_body<C: ElabCtx>(
    ctx: &mut C,
    pool: &mut ResolvedPool,
    fns: &mut FnTable,
    name: StrId,
    params: &[(StrId, Ty)],
    body: &ast::Expression,
    ret_ty: Ty,
    walk_tys: &[WalkStep],
) -> TypedFn {
    elaborate_fn(ctx, pool, fns, name, params, ret_ty, walk_tys, |el| {
        el.expr_as(body, ret_ty)
    })
}

/// Elaborate a module's top level: an imported module's initialiser. Callers
/// hold a `BlockExpression`, not an `Expression`, so this runs [`Elab::block`]
/// directly; it is otherwise [`elaborate_body`] with no parameters.
pub fn elaborate_toplevel<C: ElabCtx>(
    ctx: &mut C,
    pool: &mut ResolvedPool,
    fns: &mut FnTable,
    name: StrId,
    block: &ast::BlockExpression,
    ret_ty: Ty,
    walk_tys: &[WalkStep],
) -> TypedFn {
    // No `expr_as`: a module toplevel is walked statement-by-statement, so the
    // check walk never entered the block itself and recorded no type for it.
    elaborate_fn(ctx, pool, fns, name, &[], ret_ty, walk_tys, |el| {
        el.block(block, ret_ty)
    })
}

#[allow(clippy::too_many_arguments)]
fn elaborate_fn<C: ElabCtx>(
    ctx: &mut C,
    pool: &mut ResolvedPool,
    fns: &mut FnTable,
    name: StrId,
    params: &[(StrId, Ty)],
    ret_ty: Ty,
    walk_tys: &[WalkStep],
    f: impl FnOnce(&mut Elab<'_, C>) -> TypedExpr,
) -> TypedFn {
    let mut el = Elab::new(ctx, pool, fns, walk_tys);
    let ret = el.resolve(ret_ty);
    let params: Vec<(StrId, RTy)> = params
        .iter()
        .map(|&(nm, ty)| {
            let t = el.resolve(ty);
            let b = el.new_bind(nm, t);
            el.bind_name(nm, b.id);
            (nm, b.ty)
        })
        .collect();
    let body = f(&mut el);
    // The walk typed exactly the expressions this elaboration entered. A
    // leftover entry means one walk visited a node the other did not, which is
    // a silent mistyping of everything after it — the same class of bug the
    // span-keyed table hid, and the reason this table is positional.
    if el.cursor != el.walk_tys.len() {
        elaborator_bug("body left the check walk's types unconsumed", Span::DUMMY);
    }
    TypedFn {
        name,
        params,
        ret,
        body,
        binds: el.next_bind,
    }
}

/// The elaboration walk. Holds the name scope and the dense `BindingId` table;
/// everything else it asks [`ElabCtx`] for.
pub struct Elab<'a, C: ElabCtx> {
    ctx: &'a mut C,
    pool: &'a mut ResolvedPool,
    /// The program's function table. Eta wrappers are appended here; nothing is
    /// ever appended to a `Program`'s instruction stream mid-walk.
    fns: &'a mut FnTable,
    /// What the check walk saw in this body, in the order it saw it: the type of
    /// every expression it entered, and every `name.field` verdict it reached.
    /// Read strictly in order — never indexed by span, never searched.
    walk_tys: &'a [WalkStep],
    /// How much of [`Self::walk_tys`] has been consumed. `elaborate_fn` checks
    /// it reached the end.
    cursor: usize,
    /// Next `BindingId` to mint. `TypedFn::binds` is this at the end.
    next_bind: u32,
    /// Resolved type of each bind, indexed by `BindingId.0`.
    binds: Vec<RTy>,
    /// `StrId → BindingId` for source-named binds in scope. A stack, so
    /// shadowing is push/pop and lookup is a reverse scan.
    names: Vec<(StrId, BindingId)>,
    /// [`NO_STR`]: binds no source name reaches (an argument spill, a tuple
    /// projection, an `or`-expression's unwrapped payload). `lower` reads it
    /// back off [`TypedBind::name`] to tell an operand temp — free to collapse
    /// into whatever it aliases — from a `let` the source wrote, which owns
    /// its frame slot.
    anon: StrId,
    /// Every eta wrapper's parameters share this name; it is only displayed.
    eta_param: StrId,
    /// `Nil`'s resolved type: what an empty block, a statement-terminated
    /// block, and a constructor call the checker already reported as
    /// under-applied evaluate to.
    nil: RTy,
    /// True until the first [`Self::block`] call, exactly as `lower`'s
    /// `at_toplevel` was. That block is the module's own toplevel when the
    /// entry file's `__main__` or an imported module's initialiser is being
    /// elaborated; every nested block (an `if` arm, an inner `{..}`) sees
    /// `false`. Gates decl scheduling and [`Self::module_scope`].
    ///
    /// It starts `true` for a *function* body too. That body's outermost block
    /// is not a module toplevel, but the elaborator cannot tell the difference
    /// — `__main__` arrives through the same [`elaborate_body`] — and it does
    /// not need to: whether a name is really a module-scope binding is
    /// [`ElabCtx::global_slot`]'s answer, and it answers `None` for every name
    /// bound outside the entry frame. Deciding it here instead would drop the
    /// entry module's globals on the floor.
    at_toplevel: bool,
    /// True while walking the statements of a toplevel block. A source-named
    /// bind minted under it takes whatever [`TypedBind::global`]
    /// [`ElabCtx::global_slot`] hands out; a nested block clears it for its own
    /// body and restores it after, so in `x = { y = 1; y }` `y` can never be
    /// mistaken for the module-scope binding of a name `x`'s initialiser
    /// shadows.
    module_scope: bool,
    /// Span of the `match` whose arms are being elaborated. The [`PatCtx`] seam
    /// carries no spans — `elaborate_pat` asks for a tuple element's or an
    /// array's element type by [`RTy`] alone — so this is the only location an
    /// internal error raised from those queries can be attributed to.
    pat_span: Span,
}

impl<'a, C: ElabCtx> Elab<'a, C> {
    fn new(
        ctx: &'a mut C,
        pool: &'a mut ResolvedPool,
        fns: &'a mut FnTable,
        walk_tys: &'a [WalkStep],
    ) -> Self {
        let anon = NO_STR;
        let eta_param = ctx.intern("_");
        let nil_ty = ctx.ty_nil();
        let nil = ctx.resolve_rty(pool, nil_ty);
        Elab {
            ctx,
            pool,
            fns,
            walk_tys,
            cursor: 0,
            next_bind: 0,
            binds: Vec::new(),
            names: Vec::new(),
            anon,
            eta_param,
            nil,
            at_toplevel: true,
            module_scope: false,
            pat_span: Span::DUMMY,
        }
    }

    fn nil_expr(&self) -> TypedExpr {
        TypedExpr::Nil { ty: self.nil }
    }

    // ── types ──────────────────────────────────────────────────────────────

    /// Bridge an inference `Ty` into the pool.
    fn resolve(&mut self, t: Ty) -> RTy {
        self.ctx.resolve_rty(&mut *self.pool, t)
    }

    /// The type the check walk inferred for the expression this walk is about
    /// to enter. Called exactly once per entered expression, from
    /// [`Self::expr`] / [`Self::expr_as`], which is what keeps the two walks in
    /// step.
    ///
    /// Running out means the elaborator entered an expression the check walk
    /// did not — the two walks have drifted apart, and every type after the
    /// drift would be somebody else's.
    fn take_ty(&mut self, at: Span) -> Ty {
        let Some(&WalkStep::Ty(t)) = self.walk_tys.get(self.cursor) else {
            elaborator_bug("expression with no inferred type", at)
        };
        self.cursor += 1;
        t
    }

    /// The verdict the check walk recorded for the `left.right` shape this walk
    /// is looking at: `true` when `left` names an imported module and neither
    /// walk enters it.
    ///
    /// Recorded rather than re-derived. The decision turns on the value env at
    /// the point of the access — an `import ./one` followed by `one = Box(..)`
    /// makes a later `one.go` a field read — and by the time a deferred body is
    /// elaborated that env has moved on. Re-asking would let the two walks enter
    /// a different number of expressions, and every entry after the
    /// disagreement would belong to another node.
    fn take_qualified(&mut self, at: Span) -> bool {
        let Some(&WalkStep::Qualified(q)) = self.walk_tys.get(self.cursor) else {
            elaborator_bug("property access with no recorded qualifier verdict", at)
        };
        self.cursor += 1;
        q
    }

    /// The type of the expression the walk enters *next*, without entering it.
    ///
    /// The two callers need a sub-expression's type before elaborating it (a
    /// property access's receiver, an `or`'s left side), and in both the check
    /// walk entered that sub-expression first — so it is the entry under the
    /// cursor. Entering it (via [`Self::expr`]) still consumes it.
    fn peek_ty(&mut self, at: Span) -> Ty {
        let Some(&WalkStep::Ty(t)) = self.walk_tys.get(self.cursor) else {
            elaborator_bug("sub-expression with no inferred type", at)
        };
        t
    }

    fn int_rty(&mut self) -> RTy {
        let t = self.ctx.ty_int();
        self.resolve(t)
    }

    /// Everything a constructor's use site needs, with its field types
    /// specialised to `at`'s type arguments.
    ///
    /// The constructor's scheme is resolved once, its return type's arguments
    /// aligned against `at`'s, and the resulting `Bound(i) → RTy` substitution
    /// applied to its parameters. This replaces `lower`'s `ctor_field_tys`,
    /// which achieved the same by *unifying into the engine* — a mutation of
    /// inference state performed by a pass that had supposedly finished
    /// inferring. When `at` is opaque (an undetermined scrutinee) a field type
    /// stays `Bound` and is handled dynamically, exactly as before.
    /// The pattern path, where a constructor is always named bare: `mod.C(x)` is
    /// not a pattern the parser accepts.
    fn ctor_at(&mut self, name: &str, at: RTy, span: Span) -> Option<CtorPat> {
        let (ctor_ty, den) = self.ctx.resolve_name(name)?;
        self.ctor_of(ctor_ty, den, at, span)
    }

    /// The call path, which takes the denotation the callee walk *already*
    /// resolved rather than re-deriving it from a name. A qualified constructor
    /// (`mod.Ctor(x)`) has no bare name to re-resolve, and re-resolving one that
    /// does would instantiate its scheme a second time.
    ///
    /// `None` means exactly one thing: `den` does not denote a constructor.
    /// Once it does, every remaining question has an answer, and a missing or
    /// mismatched label list is a compiler bug rather than a reason to hand the
    /// caller a pattern that matches everything.
    fn ctor_of(&mut self, ctor_ty: Ty, den: Denotation, at: RTy, span: Span) -> Option<CtorPat> {
        let (variant, arity) = den.as_ctor()?;
        let Some(labels) = self.ctx.ctor_labels(variant) else {
            elaborator_bug("constructor with no declared labels", span)
        };
        let sig = self.resolve(ctor_ty);
        let params: SmallVec<[RTy; 4]> = self.pool.fun_params(sig).into();
        // A nullary constructor's scheme is the type itself, not a function.
        let field_tys = match self.pool.fun_ret(sig) {
            Some(ret) => {
                let subst = bound_subst(self.pool, ret, at);
                params
                    .iter()
                    .map(|&p| subst_rty(self.pool, p, &subst))
                    .collect()
            }
            None => params.to_vec(),
        };
        // `CtorPat::from_parts` is the seam that used to be `or_poison(fty,
        // "constructor field")`: rather than handing a caller a `Nil` for the
        // field it cannot type, it checks the three independently-derived
        // widths against each other and aborts on a disagreement. Every caller
        // then indexes nothing — it zips the slots against `field_tys()`.
        Some(CtorPat::from_parts(variant, arity, labels, field_tys, span))
    }

    // ── binds and names ────────────────────────────────────────────────────

    fn new_bind(&mut self, name: StrId, ty: RTy) -> TypedBind {
        let id = BindingId(self.next_bind);
        self.next_bind += 1;
        self.binds.push(ty);
        TypedBind {
            id,
            name,
            ty,
            // The entry-frame slot of a module-level bind comes from the module
            // walk's allocation, via `scoped_bind`; every other bind is a frame
            // local. See `TypedBind::global`.
            global: None,
        }
    }

    /// A bind for a module-scope `let`: it takes the next entry-frame slot the
    /// check walk queued, so def and use carry the same [`GlobalSlot`] and
    /// nothing downstream maps a name back to a slot. A destructured name
    /// takes its slot in [`Self::irrefutable`] instead, and a declaration's
    /// rides on its [`ElabCtx::toplevel_decls`] record.
    fn scoped_bind(&mut self, name: StrId, ty: RTy) -> TypedBind {
        let mut b = self.new_bind(name, ty);
        if self.module_scope {
            b.global = self.ctx.next_global_slot();
        }
        b
    }

    /// A bind for a top-level declaration, pinned to the slot its
    /// [`ElabCtx::toplevel_decls`] record scheduled it at. `None` outside a
    /// module toplevel (a `const` nested in a function body).
    fn decl_bind(&mut self, name: StrId, ty: RTy, global: Option<GlobalSlot>) -> TypedBind {
        let mut b = self.new_bind(name, ty);
        b.global = global;
        b
    }

    fn bind_name(&mut self, name: StrId, id: BindingId) {
        self.names.push((name, id));
    }

    fn lookup_name(&self, name: StrId) -> Option<BindingId> {
        self.names
            .iter()
            .rev()
            .find(|(n, _)| *n == name)
            .map(|&(_, id)| id)
    }

    #[allow(clippy::indexing_slicing)]
    fn bind_ty(&self, id: BindingId) -> RTy {
        self.binds[id.0 as usize]
    }

    fn var(&self, id: BindingId) -> TypedExpr {
        TypedExpr::Var {
            ty: self.bind_ty(id),
            place: ValueRef::Local(id),
        }
    }

    // ── expressions ────────────────────────────────────────────────────────

    /// Elaborate `e` where the surrounding context fixes the result type — a
    /// function's tail, an `if` arm, a `match` arm's body. Control-flow forms
    /// take `result_ty` rather than their own inferred type, because a function
    /// body's block has no type of its own to recover.
    ///
    /// The check walk entered `e` too, so its type is consumed here either way.
    pub fn expr_as(&mut self, e: &ast::Expression, result_ty: Ty) -> TypedExpr {
        let own = self.take_ty(e.span());
        self.dispatch(e, own, result_ty)
    }

    /// Elaborate `e` in value position: its type is the one the check walk
    /// inferred for it, and it is also the type of whatever it evaluates to.
    pub fn expr(&mut self, e: &ast::Expression) -> TypedExpr {
        let own = self.take_ty(e.span());
        self.dispatch(e, own, own)
    }

    /// `own` is `e`'s own inferred type; `result` is the type the context wants
    /// out of it. They differ only for a control-flow form under [`expr_as`],
    /// where `own` is unused.
    ///
    /// [`expr_as`]: Self::expr_as
    fn dispatch(&mut self, e: &ast::Expression, own: Ty, result: Ty) -> TypedExpr {
        use ast::Expression as E;
        match e {
            E::BlockExpression(be) => self.block(be, result),
            E::IfExpression(ie) => self.if_expr(ie, result),
            E::MatchExpression(me) => self.match_expr(me, result),
            E::OrExpression(oe) => self.or_expr(oe, result),
            E::BinaryExpression(be) => match BinopKind::of(be.op) {
                BinopKind::ShortCircuit(sc) => self.short_circuit(be, sc, result),
                // Not control flow: its own type is its type, and `result` only
                // agrees with it.
                BinopKind::Value(op) => self.binary(be, op, own),
            },

            E::NumberLiteral(n) => {
                let ty = self.resolve(own);
                let (value, _) = self.ctx.number_const(n);
                TypedExpr::Const { ty, value }
            }
            E::StringLiteral(s) => {
                let ty = self.resolve(own);
                let value = self.ctx.string_const(&s.value);
                TypedExpr::Const { ty, value }
            }
            E::Identifier(id) => self.ident(id, own),
            E::UnaryExpression(ue) => self.unary(ue, own),
            E::TupleExpression(te) => {
                let ty = self.resolve(own);
                let elems = te.elements.iter().map(|el| self.expr(el)).collect();
                TypedExpr::Tuple { ty, elems }
            }
            E::PropertyAccessExpression(pa) => self.property(pa, own),
            E::FunctionCallExpression(fc) => self.call(fc, own),
            E::RangeExpression(re) => {
                let ty = self.resolve(own);
                let start = Box::new(self.expr(&re.start));
                let end = Box::new(self.expr(&re.end));
                TypedExpr::Range { ty, start, end }
            }
            E::ArrayIndexExpression(ai) => {
                let ty = self.resolve(own);
                let recv = Box::new(self.expr(&ai.expression));
                match ai.index.as_ref() {
                    E::RangeExpression(r) => TypedExpr::Slice {
                        ty,
                        recv,
                        start: Box::new(self.expr(&r.start)),
                        end: Box::new(self.expr(&r.end)),
                    },
                    // `Index` yields `Option(elem)` — that shape is what lets
                    // `xs[i] or d` resolve.
                    other => TypedExpr::Index {
                        ty,
                        recv,
                        index: Box::new(self.expr(other)),
                    },
                }
            }
            E::ArrayExpression(ae) => {
                let ty = self.resolve(own);
                let elems = ae
                    .elements
                    .iter()
                    .map(|el| match el {
                        ast::ArrayElement::Expression(ex) => TypedArrayElem::Elem(self.expr(ex)),
                        ast::ArrayElement::SpreadElement(sp) => {
                            TypedArrayElem::Spread(self.expr(&sp.expression))
                        }
                    })
                    .collect();
                TypedExpr::Array { ty, elems }
            }
            E::InterpolatedString(is) => self.interp(is, own),
            E::FunctionExpression(fe) => {
                let ty = self.resolve(own);
                self.closure(fe.span, ty)
            }
            E::BinaryLiteral(bl) => self.binary_literal(bl, own),
            // The parser builds one only where it also records a parse error,
            // and `compile_expr` restates that error — so no module that earned
            // its `CleanModule` proof carries one.
            E::ErrorNode(err) => elaborator_bug("error node", err.span),
        }
    }

    fn if_expr(&mut self, ie: &ast::IfExpression, result_ty: Ty) -> TypedExpr {
        let ty = self.resolve(result_ty);
        let cond = Box::new(self.expr(&ie.condition));
        let then = Box::new(self.expr_as(&ie.body, result_ty));
        let els = Box::new(self.expr_as(&ie.else_body, result_ty));
        TypedExpr::If {
            ty,
            cond,
            then,
            els,
        }
    }

    /// An identifier that is not a bound local: a nullary constructor, a
    /// first-class constructor/builtin, or a global/capture load.
    fn ident(&mut self, id: &ast::Identifier, own: Ty) -> TypedExpr {
        let sid = self.ctx.intern(&id.name);
        if let Some(bid) = self.lookup_name(sid) {
            return self.var(bid);
        }
        let ty = self.resolve(own);
        let Some((fn_ty, den)) = self.ctx.resolve_name(&id.name) else {
            elaborator_bug("unbound identifier", id.span)
        };
        self.value_of(den, sid, fn_ty, ty, id.span)
    }

    /// A resolved name in value position. `ty` is the use site's solved type;
    /// `scheme` is the (freshly instantiated, unsolved) type `resolve_name`
    /// returned, and is only consulted when `ty` cannot type the node.
    fn value_of(
        &mut self,
        den: Denotation,
        name: StrId,
        scheme: Ty,
        ty: RTy,
        at: Span,
    ) -> TypedExpr {
        match den.as_value() {
            ValueForm::Ref(place) => TypedExpr::Var { ty, place },
            ValueForm::Ctor(variant) => TypedExpr::Ctor {
                ty,
                variant,
                args: vec![],
            },
            ValueForm::Eta(target) => self.eta(name, target, scheme, ty, at),
        }
    }

    /// A non-nullary constructor or a builtin referenced as a first-class value
    /// (`map(Some, xs)`): a zero-capture closure over a wrapper `TypedFn` this
    /// walk appends to the program. Nothing is emitted mid-pass.
    ///
    /// The wrapper is typed from `ty` — the *use site's* type, which the check
    /// walk solved — and not from `scheme`, which is a fresh `instantiate` of
    /// the constructor's or builtin's polymorphic type whose variables nothing
    /// has bound. Zonking that scheme turns each unsolved variable into an
    /// opaque [`ResolvedNode::Bound`], and `Bound` is not heap-shaped, so a
    /// wrapper typed from it would be Perceus-annotated as if every parameter
    /// were a scalar: `map(Some, xs)` would synthesise
    /// `(Bound k) -> Option(Bound k)` and drop nothing. `scheme` is only a
    /// fallback for the capture path, where the use site *is* the scheme.
    fn eta(&mut self, name: StrId, target: EtaTarget, scheme: Ty, ty: RTy, at: Span) -> TypedExpr {
        let f = FnRTy::of(self.pool, ty).or_else(|| {
            let sig = self.ctx.resolve_rty(&mut *self.pool, scheme);
            FnRTy::of(self.pool, sig)
        });
        let Some(f) = f else {
            elaborator_bug("eta-expansion of a non-function", at)
        };
        eta_wrapper(self.fns, name, self.eta_param, target, &f)
    }

    /// An operator that denotes an opcode. `op` is a [`ValueBinop`], so `&&`/`||`
    /// — which branch, and whose right operand may never be evaluated — cannot
    /// arrive here: [`BinopKind::of`] routes them to [`Self::short_circuit`].
    /// That is what lets `specialize_binop` return an `Op` and not an option.
    fn binary(&mut self, be: &ast::BinaryExpression, op: ValueBinop, own: Ty) -> TypedExpr {
        let ty = self.resolve(own);
        let lhs = self.expr(&be.left);
        let rhs = self.expr(&be.right);
        // The operand's type is the checker's, so `Int + Int` selects `AddInt`
        // even when nothing was annotated. `None` is a genuinely polymorphic
        // operand (`fn add(a, b) { a + b }` is `Addable a => (a, a) -> a`) and
        // the dynamic op is the correct choice.
        let prim = self.pool.prim_of(lhs.ty());
        TypedExpr::Binary {
            ty,
            op: specialize_binop(op, prim),
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
        }
    }

    /// `a && b` / `a || b`: control flow, not an operator. The RHS is
    /// elaborated against the join's result type because it is a branch.
    fn short_circuit(
        &mut self,
        be: &ast::BinaryExpression,
        op: ShortCircuitOp,
        result_ty: Ty,
    ) -> TypedExpr {
        let ty = self.resolve(result_ty);
        let lhs = Box::new(self.expr(&be.left));
        let rhs = Box::new(self.expr_as(&be.right, result_ty));
        match op {
            ShortCircuitOp::And => TypedExpr::And { ty, lhs, rhs },
            ShortCircuitOp::Or => TypedExpr::Or { ty, lhs, rhs },
        }
    }

    fn unary(&mut self, ue: &ast::UnaryExpression, own: Ty) -> TypedExpr {
        let ty = self.resolve(own);
        let operand = self.expr(&ue.expression);
        let op = match ue.op {
            ast::UnaryOp::Not => Op::Not,
            ast::UnaryOp::Neg => match self.pool.prim_of(operand.ty()) {
                Some(Prim::Int) => Op::NegInt,
                Some(Prim::Float) => Op::NegFloat,
                _ => Op::Neg,
            },
        };
        TypedExpr::Unary {
            ty,
            op,
            operand: Box::new(operand),
        }
    }

    fn interp(&mut self, is: &ast::InterpolatedString, own: Ty) -> TypedExpr {
        let ty = self.resolve(own);
        if is.parts.is_empty() {
            let value = self.ctx.string_const("");
            return TypedExpr::Const { ty, value };
        }
        let parts = is
            .parts
            .iter()
            .map(|p| match p {
                ast::InterpPart::Literal(sl) => {
                    TypedInterpPart::Str(self.ctx.string_const(&sl.value))
                }
                ast::InterpPart::Expr(ex) => TypedInterpPart::Expr(self.expr(ex)),
            })
            .collect();
        TypedExpr::Interp { ty, parts }
    }

    /// A `fn(...) { ... }` expression: the body is already checked, parked and
    /// assigned a `func_idx`, and the walk recorded its free-name set on the
    /// frame this elaboration is running in. Load each capture in the current
    /// scope. (A named `fn` *declaration* goes through `fn_of_global` instead —
    /// it is not an expression and has no captures.)
    fn closure(&mut self, span: Span, ty: RTy) -> TypedExpr {
        let Some((func_idx, cap_names)) = self.ctx.closure(span) else {
            elaborator_bug("nested closure", span)
        };
        let mut captures = Vec::with_capacity(cap_names.len());
        for cn in cap_names {
            // A capture is a name from *this* scope (or its enclosing frame);
            // resolve it exactly like an identifier reference.
            if let Some(bid) = self.lookup_name(cn) {
                captures.push(self.var(bid));
                continue;
            }
            let name = self.ctx.str(cn).to_owned();
            let Some((t, den)) = self.ctx.resolve_name(&name) else {
                // The walk recorded this name *because* it resolved it.
                elaborator_bug("nested closure capture", span)
            };
            let cty = self.resolve(t);
            captures.push(self.value_of(den, cn, t, cty, span));
        }
        TypedExpr::Closure {
            ty,
            func_idx,
            captures,
        }
    }

    fn binary_literal(&mut self, bl: &ast::BinaryLiteral, own: Ty) -> TypedExpr {
        let ty = self.resolve(own);
        if bl.segments.is_empty() {
            let value = self.ctx.binary_const(vec![], 0);
            return TypedExpr::Const { ty, value };
        }
        let segs = bl
            .segments
            .iter()
            .map(|seg| match &seg.spec {
                ast::BinSpec::Int { bits } => TypedBinSeg::Int {
                    value: self.expr(&seg.value),
                    bits: self.seg_int_bits(bits.as_ref()),
                },
                ast::BinSpec::Binary { bytes } => TypedBinSeg::Binary {
                    value: self.expr(&seg.value),
                    bits: bytes.as_ref().map(|sz| self.seg_bytes_bits(sz)),
                },
                ast::BinSpec::Utf8 => TypedBinSeg::Utf8 {
                    value: self.expr(&seg.value),
                },
            })
            .collect();
        TypedExpr::BinLit { ty, segs }
    }

    /// An `Int` segment's width in bits: the `:N` / `:size(..)` expression,
    /// or the default 8.
    fn seg_int_bits(&mut self, bits: Option<&ast::Expression>) -> TypedExpr {
        match bits {
            None => TypedExpr::Const {
                ty: self.int_rty(),
                value: self.ctx.int_const(8),
            },
            Some(e) => self.expr(e),
        }
    }

    /// A `Binary` segment's `:bytes(..)` size scaled to bits.
    ///
    /// The scale is `Op::Mul`, not `Op::MulInt`. It is a synthesised node, not
    /// one the checker specialised, and today's lowering emits exactly this —
    /// changing it would move the opcode histogram for no semantic gain.
    fn seg_bytes_bits(&mut self, bytes: &ast::Expression) -> TypedExpr {
        let int_r = self.int_rty();
        let v = self.expr(bytes);
        let eight = TypedExpr::Const {
            ty: int_r,
            value: self.ctx.int_const(8),
        };
        TypedExpr::Binary {
            ty: int_r,
            op: Op::Mul,
            lhs: Box::new(v),
            rhs: Box::new(eight),
        }
    }

    /// `qualifier.member` as a module reference, or `None` when it is an
    /// ordinary field access.
    ///
    /// The verdict is the check walk's, replayed off [`Self::walk_tys`] — see
    /// [`Self::take_qualified`]. Every caller of this must correspond to a
    /// `resolve_qualified_member` call in the check walk, in the same order:
    /// a property access entered as an expression, and a call's property-access
    /// callee (which the walk resolves before it enters anything).
    fn qualified(
        &mut self,
        left: &ast::Expression,
        right: &ast::PropertyKey,
        at: Span,
    ) -> Option<(Ty, Denotation, StrId)> {
        if !self.take_qualified(at) {
            return None;
        }
        // The walk only says `true` for the `name.field` shape, and only after
        // resolving the member against the module's interface.
        let (ast::Expression::Identifier(qual), ast::PropertyKey::Field(member)) = (left, right)
        else {
            elaborator_bug("qualified member on a non-`name.field` shape", at)
        };
        let Some((mty, den)) = self
            .ctx
            .resolve_qualified(&qual.name, &member.name, member.span)
        else {
            elaborator_bug("qualified member the check walk resolved", at)
        };
        let sid = self.ctx.intern(&member.name);
        Some((mty, den, sid))
    }

    fn property(&mut self, pa: &ast::PropertyAccessExpression, own: Ty) -> TypedExpr {
        let ty = self.resolve(own);
        // `qualifier.member` module reference? The check walk resolved it the
        // same way and never entered the qualifier, so neither does this.
        if let Some((mty, den, sid)) = self.qualified(&pa.left, &pa.right, pa.span) {
            return self.value_of(den, sid, mty, ty, pa.span);
        }
        let recv_ty = self.peek_ty(pa.left.span());
        let recv = Box::new(self.expr(&pa.left));
        match &pa.right {
            ast::PropertyKey::TupleIndex(num) => {
                // The check walk parsed this same text to find the element's
                // type, and rejected the tuple index it could not.
                let Ok(idx) = num.value.parse::<u32>() else {
                    elaborator_bug("tuple index", pa.span)
                };
                TypedExpr::TupleIndex { ty, recv, idx }
            }
            ast::PropertyKey::Field(field) => {
                // The receiver's type came from the checker, so it is a
                // resolved `Con` and `ctor_field` finds the field. The access
                // is admitted across every variant, so the tag needs no check.
                let Some((idx, _)) = self.ctx.ctor_field(recv_ty, &field.name) else {
                    elaborator_bug("field access", pa.span)
                };
                TypedExpr::Field {
                    ty,
                    recv,
                    idx,
                    checked: false,
                }
            }
        }
    }

    fn call(&mut self, fc: &ast::FunctionCallExpression, own: Ty) -> TypedExpr {
        let ty = self.resolve(own);
        // Resolve the callee first so labeled/spread constructor calls route to
        // `ctor_call` before their arguments are reordered.
        match self.callee(&fc.callee) {
            Callee::Ctor { ty: cty, den } => self.ctor_call(cty, den, &fc.arguments, ty, fc.span),
            Callee::Value(callee) => {
                let args = fc
                    .arguments
                    .iter()
                    .map(|a| match a {
                        ast::CallArg::Positional(e) => self.expr(e),
                        // Labeled/spread on a non-ctor callee is a type error
                        // the check walk already reported; elaborate the value
                        // anyway so the arity still balances.
                        ast::CallArg::Labeled { value, .. } => self.expr(value),
                        ast::CallArg::Spread(e) => self.expr(e),
                    })
                    .collect();
                TypedExpr::Call { ty, callee, args }
            }
        }
    }

    fn callee(&mut self, e: &ast::Expression) -> Callee {
        use ast::Expression as E;
        match e {
            E::Identifier(id) => {
                let sid = self.ctx.intern(&id.name);
                if let Some(bid) = self.lookup_name(sid) {
                    return Callee::Value(TypedCallee::Dynamic(Box::new(self.var(bid))));
                }
                let res = self.ctx.resolve_name(&id.name);
                self.resolved_callee(res, id.span)
            }
            E::PropertyAccessExpression(pa) => {
                if let Some((mty, den, _)) = self.qualified(&pa.left, &pa.right, pa.span) {
                    return self.resolved_callee(Some((mty, den)), pa.span);
                }
                // A computed callee `expr.field(...)`: evaluate, dynamic-call.
                Callee::Value(TypedCallee::Dynamic(Box::new(self.expr(e))))
            }
            _ => Callee::Value(TypedCallee::Dynamic(Box::new(self.expr(e)))),
        }
    }

    fn resolved_callee(&mut self, res: Option<(Ty, Denotation)>, at: Span) -> Callee {
        let Some((fn_ty, den)) = res else {
            elaborator_bug("unbound callee", at)
        };
        let sig = self.resolve(fn_ty);
        match den.as_callee(sig) {
            CallForm::Ctor => Callee::Ctor { ty: fn_ty, den },
            CallForm::Callee(c) => Callee::Value(c),
        }
    }

    /// A constructor call. Labeled args are reordered into declared-field
    /// order; a `..base` spread fills every unsupplied slot with a projection
    /// out of `base`.
    ///
    /// Spread projections always use the tag-checked form, even when the enum
    /// has a single variant and the check is statically redundant: Core carries
    /// no variant-count facts, and the check is one compare against a value
    /// already in a register.
    ///
    /// When any argument is labeled or spread, every supplied argument is bound
    /// to a `Let` in *source* order first: reordering them into field order
    /// would otherwise reorder their side effects.
    fn ctor_call(
        &mut self,
        ctor_ty: Ty,
        den: Denotation,
        args: &[ast::CallArg],
        ty: RTy,
        at: Span,
    ) -> TypedExpr {
        let Some(cp) = self.ctor_of(ctor_ty, den, ty, at) else {
            elaborator_bug("unresolved constructor", at)
        };
        let arity = cp.arity();
        let reorders = args
            .iter()
            .any(|a| !matches!(a, ast::CallArg::Positional(_)));

        let mut lets: Vec<(TypedBind, TypedExpr)> = Vec::new();
        let mut spread: Option<TypedBind> = None;
        let mut supplied: SmallVec<[(Option<StrId>, TypedExpr); 4]> =
            SmallVec::with_capacity(args.len());
        for a in args {
            match a {
                ast::CallArg::Positional(e) => {
                    let v = self.expr(e);
                    let v = if reorders {
                        self.spill(v, &mut lets)
                    } else {
                        v
                    };
                    supplied.push((None, v));
                }
                ast::CallArg::Labeled { label, value } => {
                    let sid = self.ctx.intern(&label.name);
                    let v = self.expr(value);
                    let v = self.spill(v, &mut lets);
                    supplied.push((Some(sid), v));
                }
                ast::CallArg::Spread(e) => {
                    let v = self.expr(e);
                    let b = self.new_bind(self.anon, v.ty());
                    lets.push((b, v));
                    spread = Some(b);
                }
            }
        }

        let (by_pos, errors) = slot_labeled(cp.labels(), arity, supplied);
        // The check walk slotted these same args with the same `slot_labeled`
        // and reported every error it returned; a clean module reslots with
        // none. An error here means the two walks disagree about the call.
        if !errors.is_empty() {
            elaborator_bug("constructor arguments the check walk mis-slotted", at)
        }
        // Exactly `arity` slots and exactly `arity` field types: the zip pairs
        // each unfilled slot with the type of the field it projects.
        let field_tys: SmallVec<[RTy; 4]> = cp.field_tys().into();
        let mut fields: Vec<TypedExpr> = Vec::with_capacity(arity);
        for (i, (slot, fty)) in by_pos.into_iter().zip(field_tys).enumerate() {
            match slot {
                Some(v) => fields.push(v),
                None => {
                    // A slot no argument filled reads from `..base`. With no
                    // spread it is an arity error the check walk reports, so a
                    // clean module cannot reach here — filling the slot with a
                    // guessed `Nil` would hand Perceus a non-heap type for a
                    // possibly-heap field.
                    let Some(base) = spread else {
                        elaborator_bug("constructor field with no argument and no spread", at)
                    };
                    fields.push(TypedExpr::Field {
                        ty: fty,
                        recv: Box::new(self.var(base.id)),
                        idx: i as u32,
                        checked: true,
                    });
                }
            }
        }
        let ctor = TypedExpr::Ctor {
            ty,
            variant: cp.variant,
            args: fields,
        };
        wrap_lets(lets, ctor)
    }

    /// Bind `v` to a fresh `Let` and return a reference to it, so a later
    /// reordering cannot move its evaluation.
    fn spill(&mut self, v: TypedExpr, lets: &mut Vec<(TypedBind, TypedExpr)>) -> TypedExpr {
        let bind = self.new_bind(self.anon, v.ty());
        lets.push((bind, v));
        self.var(bind.id)
    }

    /// `expr or body` (with optional receiver). There is no `Or`-expression in
    /// the typed IR: the checker knows the `Option`/`Result` shape, so it emits
    /// the `Match` those variants describe.
    fn or_expr(&mut self, oe: &ast::OrExpression, result_ty: Ty) -> TypedExpr {
        let ty = self.resolve(result_ty);
        let lhs_ty = self.peek_ty(oe.expression.span());
        let lhs_rty = self.resolve(lhs_ty);
        let lhs = self.expr(&oe.expression);
        let Some(shape) = self.ctx.or_shape(lhs_ty) else {
            // The check walk types `expr or default` only when `expr` is an
            // `Option`/`Result`, and reports it when it is not.
            elaborator_bug("or-expression on non-Option/Result", oe.span)
        };
        // Success arm: `Ok(v)`/`Some(v)` → v.
        let ok_bind = self.new_bind(self.anon, ty);
        let ok_body = self.var(ok_bind.id);
        // Failure arm: bind the payload (`Result`) or drop it (`Option`), then
        // evaluate `body`.
        let (fail_fields, body) = if shape.err_has_payload {
            // `Result(_, E)`'s payload is its second type argument. It must be
            // the real `E`: `err` may be a heap value the recovery body drops.
            let Some(ety) = self.pool.con_arg(lhs_rty, 1) else {
                elaborator_bug("or-expression on a non-generic Result", oe.span)
            };
            let name = match &oe.receiver {
                Some(r) => self.ctx.intern(&r.name),
                None => self.anon,
            };
            let err_bind = self.new_bind(name, ety);
            if oe.receiver.is_some() {
                self.bind_name(name, err_bind.id);
            }
            let body = self.expr_as(&oe.body, result_ty);
            if oe.receiver.is_some() {
                self.names.pop();
            }
            (vec![TypedPat::Bind(err_bind)], body)
        } else {
            (vec![], self.expr_as(&oe.body, result_ty))
        };
        TypedExpr::Match {
            ty,
            scrut: Box::new(lhs),
            arms: vec![
                TypedArm {
                    pat: TypedPat::Ctor {
                        ty: lhs_rty,
                        variant: shape.fail,
                        fields: fail_fields,
                    },
                    guard: None,
                    body,
                },
                TypedArm {
                    pat: TypedPat::Ctor {
                        ty: lhs_rty,
                        variant: shape.ok,
                        fields: vec![TypedPat::Bind(ok_bind)],
                    },
                    guard: None,
                    body: ok_body,
                },
            ],
        }
    }

    fn match_expr(&mut self, me: &ast::MatchExpression, result_ty: Ty) -> TypedExpr {
        let ty = self.resolve(result_ty);
        let scrut_ty = self.peek_ty(me.subject.span());
        let scrut_rty = self.resolve(scrut_ty);
        let scrut = Box::new(self.expr(&me.subject));
        // Arm bodies re-enter `expr`, which may elaborate a nested `match`;
        // save and restore rather than assign, so an error raised after the
        // inner arms are done still names this `match`.
        let outer = std::mem::replace(&mut self.pat_span, me.span);
        let arms = elaborate_arms(self, scrut_rty, &me.arms);
        self.pat_span = outer;
        TypedExpr::Match { ty, scrut, arms }
    }

    // ── blocks ─────────────────────────────────────────────────────────────

    /// A block is a right-nested `Let`/`Seq` spine ending in its tail
    /// expression, or [`TypedExpr::Nil`] when it ends in a statement.
    ///
    /// See [`Frame`] for why the spine is assembled flat and folded once.
    pub fn block(&mut self, be: &ast::BlockExpression, result_ty: Ty) -> TypedExpr {
        let is_top = std::mem::replace(&mut self.at_toplevel, false);
        // Module toplevel: `fn`/`const` decls are order-free and mutually
        // recursive. The check walk initialises them in dependency (SCC) order
        // and only then runs the non-decl nodes; walking `be.body` in raw
        // source order would read a forward reference before it is stored.
        let seq: Vec<Step> = if is_top {
            self.decl_schedule(be.body.len(), be.span)
        } else {
            (0..be.body.len()).map(|i| (i, None)).collect()
        };
        let mark = self.names.len();
        // Only this block's own statements bind globals. A block nested in one
        // of their initialisers (`x = { y = 1; y }`) restores `false` here, so
        // its `y` stays a frame local.
        let outer = std::mem::replace(&mut self.module_scope, is_top);
        let e = self.block_nodes(&seq, &be.body, result_ty, be.span);
        self.module_scope = outer;
        self.names.truncate(mark);
        e
    }

    /// The module's decls first, in SCC-visit order and each carrying the
    /// entry-frame slot it was allocated, then every other node in source
    /// order with no slot.
    ///
    /// `ElabCtx::toplevel_decls` is empty for a block that is not a module
    /// toplevel, which degenerates this to plain source order.
    fn decl_schedule(&mut self, len: usize, span: Span) -> Vec<Step> {
        let decls = self.ctx.toplevel_decls();
        let mut placed = vec![false; len];
        let mut seq: Vec<Step> = Vec::with_capacity(len);
        for &(idx, slot) in &decls {
            match placed.get_mut(idx) {
                Some(p) => *p = true,
                // A decl the check walk saw but this block does not have: the
                // two disagree about what module they are walking. Dropping it
                // would de-globalise a `const` whose slot every already-emitted
                // fn body reads by `PushGlobal` — a silent miscompile.
                None => elaborator_bug("toplevel decl node out of range", span),
            }
            seq.push((idx, Some(slot)));
        }
        for (idx, &p) in placed.iter().enumerate() {
            if !p {
                seq.push((idx, None));
            }
        }
        seq
    }

    /// Elaborate a block's statements into a right-nested `Let`/`Seq` spine.
    ///
    /// Built with a loop and folded once, not by recursing per statement: a
    /// module toplevel is one spine node per declaration and real modules run
    /// to thousands. `lower`, which consumes the spine, is iterative for the
    /// same reason.
    fn block_nodes(
        &mut self,
        seq: &[Step],
        body: &[ast::Node],
        result_ty: Ty,
        span: Span,
    ) -> TypedExpr {
        let mut spine: Vec<Frame> = Vec::with_capacity(seq.len());
        let mut steps = seq.iter().peekable();
        let tail = loop {
            let Some(&(idx, global)) = steps.next() else {
                // Block ended in a statement, or was empty.
                break self.nil_expr();
            };
            let Some(node) = body.get(idx) else {
                // Every schedule index is bounds-checked by `decl_schedule` or
                // ranges over `body` itself; a miss means the schedule was
                // built against a different block.
                elaborator_bug("block schedule index out of range", span)
            };
            match node {
                ast::Node::Expression(e) if steps.peek().is_none() => {
                    break self.expr_as(e, result_ty);
                }
                ast::Node::Expression(e) => {
                    let effect = self.expr(e);
                    spine.push(Frame::Seq(effect));
                }
                ast::Node::Statement(s) => self.statement(s, global, &mut spine),
            }
        };
        spine
            .into_iter()
            .rev()
            .fold(tail, |tail, frame| match frame {
                Frame::Seq(effect) => TypedExpr::Seq {
                    ty: tail.ty(),
                    effect: Box::new(effect),
                    body: Box::new(tail),
                },
                Frame::Let(bind, init) => TypedExpr::Let {
                    ty: tail.ty(),
                    bind,
                    init: Box::new(init),
                    body: Box::new(tail),
                },
            })
    }

    /// Append this statement's spine frames, in evaluation order. `global` is
    /// the entry-frame slot [`Self::decl_schedule`] paired with this node —
    /// always `Some` for a top-level `fn`/`const` of a module toplevel, `None`
    /// for everything else. A `let` takes its slot from [`Self::scoped_bind`]
    /// instead: it has no declaration to hang one on.
    fn statement(&mut self, s: &ast::Statement, global: Option<GlobalSlot>, out: &mut Vec<Frame>) {
        match s {
            ast::Statement::VariableBinding(vb) => {
                let sid = self.ctx.intern(&vb.identifier.name);
                let init = self.expr(&vb.init);
                // A module-scope `let` is a global: the lambdas the check walk
                // compiled read it via `PushGlobal <slot>`, so it needs its own
                // pinned bind. Inner-block `let`s stay collapsible.
                let bind = self.scoped_bind(sid, init.ty());
                self.bind_name(sid, bind.id);
                out.push(Frame::Let(bind, init));
            }
            ast::Statement::TypedDiscard(td) => {
                let effect = self.expr(&td.init);
                out.push(Frame::Seq(effect));
            }
            ast::Statement::TupleDestructuringBinding(tb) => {
                let pat = ast::Pattern::Tuple {
                    elements: tb.patterns.clone(),
                    span: tb.span,
                };
                self.destructure(&pat, &tb.init, out);
            }
            ast::Statement::CtorDestructuringBinding(cb) => {
                self.destructure(&cb.as_pattern(), &cb.init, out);
            }
            ast::Statement::Declaration { decl, .. } => match decl.as_ref() {
                // Types are erased; the walk already registered the ctors.
                ast::Declaration::Type(_) => {}
                // `@vm` fns carry no AL body and no runtime binding.
                ast::Declaration::Function(fd) if matches!(fd.body, ast::FnBody::Vm(_)) => {}
                ast::Declaration::Function(fd) => {
                    // A `fn` *declaration* is not an expression, so the walk
                    // recorded no type for its span; its scheme is in the env.
                    let Some((fn_ty, _)) = self.ctx.resolve_name(&fd.identifier.name) else {
                        elaborator_bug("unbound fn declaration", fd.span)
                    };
                    let fr = self.resolve(fn_ty);
                    let sid = self.ctx.intern(&fd.identifier.name);
                    // The bind comes first: its `GlobalSlot` *is* the decl's
                    // identity, and the closure it initialises is whatever
                    // function the walk emitted for that slot. A top-level fn
                    // captures nothing — siblings are `PushGlobal`.
                    let bind = self.decl_bind(sid, fr, global);
                    let Some(func_idx) = bind.global.and_then(|g| self.ctx.fn_of_global(g)) else {
                        elaborator_bug("fn declaration with no emitted body", fd.span)
                    };
                    let init = TypedExpr::Closure {
                        ty: fr,
                        func_idx,
                        captures: Vec::new(),
                    };
                    self.bind_name(sid, bind.id);
                    out.push(Frame::Let(bind, init));
                }
                ast::Declaration::Const(cb) => {
                    // A `const` gets its own bind even when its init is a bare
                    // identifier: fn bodies address it by its own entry-frame
                    // slot, distinct from whatever it aliases.
                    let init = self.expr(&cb.init);
                    let sid = self.ctx.intern(&cb.identifier.name);
                    let bind = self.decl_bind(sid, init.ty(), global);
                    self.bind_name(sid, bind.id);
                    out.push(Frame::Let(bind, init));
                }
            },
            // Imports were resolved by the check walk.
            ast::Statement::ImportDeclaration(_) => {}
        }
    }

    /// `let (a, b) = e` / `let Point(x, y) = e`: bind the scrutinee, then one
    /// `Let` per projection. Exhaustiveness has proven the tag, so the field
    /// reads are unchecked.
    fn destructure(&mut self, pat: &ast::Pattern, init: &ast::Expression, out: &mut Vec<Frame>) {
        let value = self.expr(init);
        let scrut = self.new_bind(self.anon, value.ty());
        let mut lets = vec![(scrut, value)];
        self.irrefutable(pat, scrut.id, &mut lets);
        out.extend(lets.into_iter().map(|(b, e)| Frame::Let(b, e)));
    }

    /// Project `pat`'s bindings out of the bind `src`, appending one `Let` per
    /// projection. A `Var` sub-pattern is an alias, not a copy.
    fn irrefutable(
        &mut self,
        pat: &ast::Pattern,
        src: BindingId,
        lets: &mut Vec<(TypedBind, TypedExpr)>,
    ) {
        let src_ty = self.bind_ty(src);
        match pat {
            ast::Pattern::Wildcard { .. } => {}
            ast::Pattern::Var { name } => {
                let sid = self.ctx.intern(&name.name);
                self.bind_name(sid, src);
                // `src` is the projection's own bind — each caller mints one
                // per field, and no other name reaches it — so adopting the
                // name here pins exactly one bind per source name, and `lower`
                // gives it a slot of its own rather than collapsing it.
                //
                // A module-scope destructured name is additionally a global,
                // just like a top-level `let`: fn bodies read it via
                // `PushGlobal <slot>`.
                let Some((b, _)) = lets.iter_mut().find(|(b, _)| b.id == src) else {
                    elaborator_bug("destructured name with no projection bind", name.span)
                };
                if self.module_scope {
                    b.name = sid;
                    b.global = self.ctx.next_global_slot();
                }
            }
            ast::Pattern::Tuple { elements, span } => {
                for (i, sub) in elements.iter().enumerate() {
                    let Some(ety) = self.pool.tuple_elem(src_ty, i) else {
                        elaborator_bug("tuple element of a non-tuple type", *span)
                    };
                    let b = self.new_bind(self.anon, ety);
                    let proj = TypedExpr::TupleIndex {
                        ty: ety,
                        recv: Box::new(self.var(src)),
                        idx: i as u32,
                    };
                    lets.push((b, proj));
                    self.irrefutable(sub, b.id, lets);
                }
            }
            ast::Pattern::Constructor {
                qualifier,
                name,
                args,
                ..
            } => {
                let resolved = match qualifier {
                    Some(q) => self
                        .ctx
                        .resolve_qualified(&q.name, &name.name, name.span)
                        .and_then(|(ty, den)| self.ctor_of(ty, den, src_ty, name.span)),
                    None => self.ctor_at(&name.name, src_ty, name.span),
                };
                let Some(cp) = resolved else {
                    elaborator_bug("unresolved constructor pattern", name.span)
                };
                let by_pos = self.slot_pattern_args(&cp, args, name.span);
                let field_tys: SmallVec<[RTy; 4]> = cp.field_tys().into();
                for (i, (sub, fty)) in by_pos.into_iter().zip(field_tys).enumerate() {
                    let Some(sub) = sub else { continue };
                    let b = self.new_bind(self.anon, fty);
                    let proj = TypedExpr::Field {
                        ty: fty,
                        recv: Box::new(self.var(src)),
                        idx: i as u32,
                        checked: false,
                    };
                    lets.push((b, proj));
                    self.irrefutable(sub, b.id, lets);
                }
            }
            // `let`/`(a, b) =` bind irrefutably: the check walk rejects a
            // literal, range, binary or or-pattern in binding position.
            other => elaborator_bug("irrefutable pattern", other.span()),
        }
    }

    /// Slot a constructor pattern's positional/labeled args into declared-field
    /// order, so a slot index is the field index, not the source-arg index.
    /// The check walk slotted the same args and reported every error, so a
    /// slot error on a clean module is a desync between the two walks.
    fn slot_pattern_args<'p>(
        &mut self,
        cp: &CtorPat,
        args: &'p [ast::PatternArg],
        span: Span,
    ) -> SmallVec<[Option<&'p ast::Pattern>; 4]> {
        let mut supplied: SmallVec<[(Option<StrId>, &ast::Pattern); 4]> =
            SmallVec::with_capacity(args.len());
        for a in args {
            match a {
                ast::PatternArg::Positional(p) => supplied.push((None, p)),
                ast::PatternArg::Labeled { label, pattern } => {
                    let sid = self.ctx.intern(&label.name);
                    supplied.push((Some(sid), pattern));
                }
            }
        }
        let (by_pos, errors) = slot_labeled(cp.labels(), cp.arity(), supplied);
        if !errors.is_empty() {
            elaborator_bug("constructor pattern arg desync", span)
        }
        by_pos
    }
}

/// Pattern elaboration's view of the walk. Every type crossing this seam is an
/// [`RTy`]: a `fresh_var()` cannot reach a `TypedPat`.
impl<C: ElabCtx> PatCtx for Elab<'_, C> {
    fn intern(&mut self, name: &str) -> StrId {
        self.ctx.intern(name)
    }

    fn bind(&mut self, name: StrId, ty: RTy) -> TypedBind {
        let b = self.new_bind(name, ty);
        self.bind_name(name, b.id);
        b
    }

    fn scope_mark(&mut self) -> usize {
        self.names.len()
    }

    fn scope_reset(&mut self, mark: usize) {
        self.names.truncate(mark);
    }

    fn number_const(&mut self, lit: &ast::NumberLiteral) -> (ConstId, RTy) {
        let (c, t) = self.ctx.number_const(lit);
        (c, self.resolve(t))
    }

    fn string_const(&mut self, s: &str) -> ConstId {
        self.ctx.string_const(s)
    }

    fn int_const(&mut self, i: i64) -> ConstId {
        self.ctx.int_const(i)
    }

    fn binary_const(&mut self, bytes: Vec<u8>, bit_len: u64) -> ConstId {
        self.ctx.binary_const(bytes, bit_len)
    }

    fn ty_int(&mut self) -> RTy {
        self.int_rty()
    }

    /// A `None` from [`Elab::ctor_at`] means the pattern names no constructor,
    /// which a diagnostics-clean module cannot do. Wildcarding it would make
    /// the arm match everything and drop its bindings on the floor, so it
    /// aborts — exactly as `Elab::ctor_call` and `Elab::irrefutable` do.
    fn resolve_ctor_pat(&mut self, name: &str, scrut: RTy) -> CtorPat {
        let span = self.pat_span;
        match self.ctor_at(name, scrut, span) {
            Some(cp) => cp,
            None => elaborator_bug("unresolved constructor pattern", span),
        }
    }

    fn resolve_ctor_pat_qualified(&mut self, qual: &str, name: &str, scrut: RTy) -> CtorPat {
        let span = self.pat_span;
        let resolved = self
            .ctx
            .resolve_qualified(qual, name, span)
            .and_then(|(ty, den)| self.ctor_of(ty, den, scrut, span));
        match resolved {
            Some(cp) => cp,
            None => elaborator_bug("unresolved qualified constructor pattern", span),
        }
    }

    /// A `None` here means the scrutinee's type is not the tuple the pattern
    /// matched against — a typechecker bug. Defaulting to `Nil` would hand
    /// Perceus a non-heap type for a possibly-heap element and silently lose
    /// its `Drop`.
    fn tuple_elem_ty(&mut self, t: RTy, i: usize) -> RTy {
        match self.pool.tuple_elem(t, i) {
            Some(t) => t,
            None => elaborator_bug("tuple element of a non-tuple type", self.pat_span),
        }
    }

    /// As [`Self::tuple_elem_ty`]: `lower`'s `con_arg(vty, 0, span)`.
    fn array_elem_ty(&mut self, t: RTy) -> RTy {
        match self.pool.con_arg(t, 0) {
            Some(t) => t,
            None => elaborator_bug("type argument of a non-generic type", self.pat_span),
        }
    }

    fn expr(&mut self, e: &ast::Expression) -> TypedExpr {
        Elab::expr(self, e)
    }
}

/// A callee, or the constructor whose arguments must be slotted before it can
/// be one. Keeping these apart is what stops `ctor_call` from having to invent
/// a placeholder [`TypedCallee`].
enum Callee {
    /// A saturated constructor call, with the scheme and denotation the callee
    /// walk resolved. Carrying them is what lets `ctor_call` work for a
    /// qualified `mod.Ctor(..)`, whose bare name is not in scope.
    Ctor {
        ty: Ty,
        den: Denotation,
    },
    Value(TypedCallee),
}

/// One node of a block's spine, before it is folded into the right-nested
/// `Let`/`Seq` tree.
///
/// `Elab::block_nodes` collects these in evaluation order and folds once.
/// Recursing per statement instead — `block_nodes` → `statement` → `let_rest` →
/// `block_nodes` — costs three stack frames per statement, and a module toplevel
/// has one statement per declaration: a 1200-declaration module overflowed the
/// default thread stack before the elaborator reached its tail.
enum Frame {
    /// `let bind = init;`
    Let(TypedBind, TypedExpr),
    /// A statement evaluated for effect.
    Seq(TypedExpr),
}

/// Fold `lets` around `tail`: the first pushed `Let` is the outermost, so the
/// spine's evaluation order is push order.
fn wrap_lets(lets: Vec<(TypedBind, TypedExpr)>, tail: TypedExpr) -> TypedExpr {
    let mut e = tail;
    for (bind, init) in lets.into_iter().rev() {
        e = TypedExpr::Let {
            ty: e.ty(),
            bind,
            init: Box::new(init),
            body: Box::new(e),
        };
    }
    e
}

/// Align a constructor's return type against the type it is used at, yielding
/// the `Bound(i) → RTy` substitution that specialises its field types.
///
/// `Cons`'s scheme returns `List(Bound(k))`; used at `List(Int)` the map is
/// `{k ↦ Int}`. When `at` is not a `Con` — an undetermined scrutinee — the map
/// is empty and the fields stay polymorphic, which is exactly the dynamic
/// handling `ResolvedNode::Bound` prescribes.
fn bound_subst(pool: &ResolvedPool, ret: RTy, at: RTy) -> HashMap<u32, RTy> {
    let mut m = HashMap::new();
    let formals: SmallVec<[RTy; 4]> = pool.con_args(ret).into();
    let actuals = pool.con_args(at);
    for (f, a) in formals.iter().zip(actuals) {
        if let ResolvedNode::Bound(i) = pool.node(*f) {
            m.insert(i, *a);
        }
    }
    m
}

/// Rebuild `t` with every `Bound(i)` in `m` replaced. A node containing no
/// substituted variable is returned as-is, so a concrete field type costs no
/// allocation.
fn subst_rty(pool: &mut ResolvedPool, t: RTy, m: &HashMap<u32, RTy>) -> RTy {
    if m.is_empty() {
        return t;
    }
    match pool.node(t) {
        ResolvedNode::Bound(i) => m.get(&i).copied().unwrap_or(t),
        ResolvedNode::Con { id, name, args } => {
            let kids: SmallVec<[RTy; 4]> = pool.children(args).into();
            let new: SmallVec<[RTy; 4]> = kids.iter().map(|&k| subst_rty(pool, k, m)).collect();
            if new == kids {
                t
            } else {
                pool.mk_con(id, name, &new)
            }
        }
        ResolvedNode::Fun { params, ret } => {
            let kids: SmallVec<[RTy; 4]> = pool.children(params).into();
            let new: SmallVec<[RTy; 4]> = kids.iter().map(|&k| subst_rty(pool, k, m)).collect();
            let nret = subst_rty(pool, ret, m);
            if new == kids && nret == ret {
                t
            } else {
                pool.mk_fun(&new, nret)
            }
        }
        ResolvedNode::Tuple { elems } => {
            let kids: SmallVec<[RTy; 4]> = pool.children(elems).into();
            let new: SmallVec<[RTy; 4]> = kids.iter().map(|&k| subst_rty(pool, k, m)).collect();
            if new == kids { t } else { pool.mk_tuple(&new) }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::type_def::TypeId;
    use crate::types::PrimIds;

    fn pool() -> ResolvedPool {
        ResolvedPool::new(PrimIds {
            int: TypeId(1),
            float: TypeId(2),
            string: TypeId(3),
            array: TypeId(4),
        })
    }

    /// `Cons(head: a, tail: List(a))` used at `List(Int)` must give its fields
    /// `Int` and `List(Int)` — the fact Perceus needs to emit a `Drop` for the
    /// tail. `lower` got this by unifying into the engine; here it is a pure
    /// substitution over the pool.
    #[test]
    fn a_ctors_fields_specialise_to_the_type_it_is_used_at() {
        let mut p = pool();
        let int = p.mk_con(TypeId(1), 0, &[]);
        let a = p.mk_bound(0);
        let list_a = p.mk_con(TypeId(9), 1, &[a]);
        let list_int = p.mk_con(TypeId(9), 1, &[int]);

        let m = bound_subst(&p, list_a, list_int);
        assert_eq!(m.get(&0), Some(&int));

        let head = subst_rty(&mut p, a, &m);
        let tail = subst_rty(&mut p, list_a, &m);
        assert_eq!(p.prim_of(head), Some(Prim::Int));
        assert_eq!(p.con_arg(tail, 0), Some(int));
        assert!(p.is_heap(tail), "a specialised tail is a heap cell");
    }

    /// An undetermined use site leaves the fields polymorphic rather than
    /// inventing a type for them.
    #[test]
    fn an_opaque_use_site_leaves_the_fields_bound() {
        let mut p = pool();
        let a = p.mk_bound(0);
        let list_a = p.mk_con(TypeId(9), 1, &[a]);
        let opaque = p.mk_bound(7);
        let m = bound_subst(&p, list_a, opaque);
        assert!(m.is_empty());
        let unchanged = subst_rty(&mut p, a, &m);
        assert_eq!(p.node(unchanged), ResolvedNode::Bound(0));
    }

    /// A concrete field type is returned as-is: substitution allocates nothing
    /// when nothing changes.
    #[test]
    fn substitution_over_a_concrete_type_allocates_nothing() {
        let mut p = pool();
        let int = p.mk_con(TypeId(1), 0, &[]);
        let tup = p.mk_tuple(&[int, int]);
        let before = p.len();
        let mut m = HashMap::new();
        m.insert(3u32, int);
        assert_eq!(subst_rty(&mut p, tup, &m), tup);
        assert_eq!(p.len(), before);
    }

    /// Nested spines are rebuilt, not shallowly copied.
    #[test]
    fn substitution_rewrites_a_nested_spine() {
        let mut p = pool();
        let int = p.mk_con(TypeId(1), 0, &[]);
        let a = p.mk_bound(0);
        let inner = p.mk_tuple(&[a, int]);
        let outer = p.mk_con(TypeId(4), 2, &[inner]);
        let mut m = HashMap::new();
        m.insert(0u32, int);
        let out = subst_rty(&mut p, outer, &m);
        let t = p.con_arg(out, 0).expect("Array(_)");
        assert_eq!(p.tuple_elem(t, 0), Some(int));
        assert_eq!(p.tuple_elem(t, 1), Some(int));
    }

    /// `wrap_lets` keeps source evaluation order: the first spilled argument is
    /// the outermost `Let`, so it runs first.
    #[test]
    fn spilled_lets_preserve_source_order() {
        let mut p = pool();
        let int = p.mk_con(TypeId(1), 0, &[]);
        let mk = |i: u32| TypedBind {
            id: BindingId(i),
            name: 0,
            ty: int,
            global: None,
        };
        let e = wrap_lets(
            vec![
                (mk(0), TypedExpr::Nil { ty: int }),
                (mk(1), TypedExpr::Nil { ty: int }),
            ],
            TypedExpr::Nil { ty: int },
        );
        let TypedExpr::Let { bind, body, .. } = &e else {
            panic!("outermost is a Let")
        };
        assert_eq!(bind.id, BindingId(0));
        let TypedExpr::Let { bind, .. } = body.as_ref() else {
            panic!("inner is a Let")
        };
        assert_eq!(bind.id, BindingId(1));
    }
}
