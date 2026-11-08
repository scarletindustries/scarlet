//! Checked tree → Typed IR: the elaborator's skeleton and its expression arms.
//!
//! Resolution only — which field index, which variant, which callee, which
//! typed opcode. Evaluation order (ANF) is `core_ir::lower`'s job. Patterns are
//! [`super::elaborate_pat`]'s; this module implements its [`PatCtx`] seam.
//!
//! Every node carries an [`RTy`] from the caller's [`ResolvedPool`].
//! [`PreludeTys::resolve_rty`] is the only bridge from a live inference `Ty`
//! into that pool; past it the elaborator never consults the union-find.
//!
//! Elaboration is total. It only runs on a module the check walk left free of
//! error diagnostics, so every question it asks already has an answer. When one
//! does not, [`elaborator_bug`] aborts.

use std::collections::HashMap;

use smallvec::SmallVec;

use super::binop::{BinopKind, ShortCircuitOp, ValueBinop, specialize_binop};
use super::checked::{
    CArg, CArm, CArrayElem, CBinSeg, CCallee, CExpr, CInterpPart, CKind, CNode, ClosureFact,
    NameAt, NameFact,
};
use super::elaborate_pat::{
    CtorPat, PatCtx, SpecWidth, elaborate_arms, seg_bits, slot_pattern_args,
};
use super::eta::{FnRTy, FnTable, eta_wrapper};
use super::resolve::{CallForm, Denotation, EtaTarget, ValueForm};
use super::rty::{RTy, ResolvedNode, ResolvedPool};
use super::slots::slot_labeled;
use super::{
    BindingId, GlobalSlot, TypedArm, TypedArrayElem, TypedBinSeg, TypedBind, TypedCallee,
    TypedExpr, TypedFn, TypedInterpPart, TypedPat, ValueRef,
};
use scarlet_ir::core_ir::{ConstId, FuncIdx, PrimOp, VariantRef};
use scarlet_syntax::ast;
use scarlet_syntax::span::Span;
use scarlet_types::types::{Prim, StrId, Ty};

/// One scheduled node of a block: its index in the block's `body`, and — for a
/// top-level `fn`/`const` — the entry-frame slot its binding must land in.
type Step = (usize, Option<GlobalSlot>);

/// The `Option`/`Result` shape behind an `or`-expression's left-hand side.
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

/// Where a field sits in a value of its receiver's type, found by the field's
/// name.
#[derive(Debug, Clone, PartialEq)]
pub enum FieldAt {
    /// At this index in every variant, so a read needs no variant.
    Same(u32),
    /// At different indices in different variants: each variant's layout, one
    /// per variant, so a read looks at the variant first.
    PerVariant(Vec<VariantLayout>),
}

/// One variant of a receiver's type, as a read of one of its fields sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct VariantLayout {
    pub variant: VariantRef,
    /// Every field's type, instantiated at the receiver's type arguments.
    pub fields: Vec<Ty>,
    /// Which of `fields` the read is after.
    pub slot: u32,
}

/// Abort: the check walk did not answer a question elaboration had to ask, so
/// the module's clean-module proof is wrong. `why` names the question, e.g.
/// `"field access"`. No program reaches this — a user-facing error would have
/// denied the proof first.
#[allow(clippy::panic)]
#[cold]
#[inline(never)]
pub fn elaborator_bug(why: &'static str, span: Span) -> ! {
    panic!(
        "internal compiler error: {why} at {span:?} is well-typed but was not elaborated. \
         Report this as a compiler bug."
    )
}

/// The prelude types a caller can name without a source expression to read
/// them off, plus the one bridge that resolves them into a pool. Split from
/// [`ElabCtx`] because [`super::TempTys::intern`] needs exactly this much and
/// runs before any body is elaborated.
pub trait PreludeTys {
    /// Resolve a live inference `Ty` into `pool`. Total: an undetermined type is
    /// operationally a polymorphic one (`Zonker::zonk_or_opaque`).
    fn resolve_rty(&mut self, pool: &mut ResolvedPool, t: Ty) -> RTy;

    fn ty_bool(&mut self) -> Ty;
    fn ty_int(&mut self) -> Ty;
    fn ty_string(&mut self) -> Ty;
    fn ty_binary(&mut self) -> Ty;
}

/// Services the elaborator needs from the enclosing compilation.
///
/// There is deliberately no `engine()`: everything the elaborator decides it
/// decides against the pool, and the engine is reachable only behind
/// [`PreludeTys::resolve_rty`].
pub trait ElabCtx: PreludeTys {
    fn intern(&mut self, s: &str) -> StrId;

    /// Parse-and-pool a numeric literal.
    fn number_const(&mut self, lit: &ast::NumberLiteral) -> ConstId;
    /// Pool a string literal.
    fn string_const(&mut self, s: &str) -> ConstId;
    /// Pool an integer literal.
    fn int_const(&mut self, i: i64) -> ConstId;
    /// Pool a `Binary` constant with the given raw bytes and bit length.
    fn binary_const(&mut self, bytes: Vec<u8>, bit_len: u64) -> ConstId;

    /// A constructor named by its bare name in a pattern, as
    /// `(instantiated scheme, denotation)`.
    fn resolve_ctor(&mut self, name: &str) -> Option<(Ty, Denotation)>;
    /// The type of the module's own top-level `fn` declaration `name`.
    fn toplevel_fn_ty(&mut self, name: StrId) -> Option<Ty>;
    /// One of this module's own top-level declarations, for a name the module
    /// toplevel reaches before the declaration's statement binds it.
    fn decl_denotation(&self, name: StrId) -> Option<Denotation>;
    /// Resolve a `qualifier.member` name. `span` is the member's, for the
    /// internal-error report.
    fn resolve_qualified(
        &mut self,
        qual: &str,
        member: &str,
        span: Span,
    ) -> Option<(Ty, Denotation)>;
    /// Where `.field` sits in a receiver of `receiver` type.
    fn ctor_field(&mut self, receiver: Ty, field: StrId) -> Option<FieldAt>;
    /// Declared field-label order of the variant `v` constructs.
    ///
    /// Keyed on the [`VariantRef`] the walk resolved, not on a source name:
    /// `mod.Ctor(..)` is not in scope under its bare name, so a name lookup
    /// would miss a constructor the check walk accepted.
    fn ctor_labels(&mut self, v: VariantRef) -> Option<Vec<StrId>>;
    /// The function body the check walk emitted for the top-level `fn`
    /// declaration bound at `slot`. `None` for a slot holding a plain value.
    fn fn_of_global(&self, slot: GlobalSlot) -> Option<FuncIdx>;
    /// The module's own top-level `fn`/`const` declarations as `(index in the
    /// module block's `body`, entry-frame slot)`, in SCC-visit order.
    ///
    /// Order matters: a forward-referenced `const` must be stored before it is
    /// read, so decls are not walked in source order. Only asked while
    /// elaborating a module toplevel.
    fn toplevel_decls(&self) -> Vec<(usize, GlobalSlot)>;
    /// `Option`/`Result` shape for `expr or default`.
    fn or_shape(&mut self, lhs_ty: Ty) -> Option<OrShape>;

    fn ty_nil(&mut self) -> Ty;
}

/// Elaborate `body` into a [`TypedFn`]. Eta wrappers the body needs are
/// appended to `fns`.
///
/// `toplevel` is set for a bare-expression program, whose body is the module
/// toplevel; a module block goes through [`elaborate_toplevel`] instead.
#[allow(clippy::too_many_arguments)]
pub fn elaborate_body<C: ElabCtx>(
    ctx: &mut C,
    pool: &mut ResolvedPool,
    fns: &mut FnTable,
    name: StrId,
    params: &[(StrId, Ty)],
    body: &CExpr,
    ret_ty: Ty,
    toplevel: bool,
) -> TypedFn {
    elaborate_fn(ctx, pool, fns, name, params, ret_ty, toplevel, |el| {
        el.expr_as(body, ret_ty)
    })
}

/// Elaborate a module's top level: the block's `nodes`, at their source
/// indices, which its declarations are scheduled by.
#[allow(clippy::too_many_arguments)]
pub fn elaborate_toplevel<C: ElabCtx>(
    ctx: &mut C,
    pool: &mut ResolvedPool,
    fns: &mut FnTable,
    name: StrId,
    nodes: &[CNode],
    span: Span,
    ret_ty: Ty,
) -> TypedFn {
    elaborate_fn(ctx, pool, fns, name, &[], ret_ty, true, |el| {
        el.block(nodes, ret_ty, span)
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
    toplevel: bool,
    f: impl FnOnce(&mut Elab<'_, C>) -> TypedExpr,
) -> TypedFn {
    let mut el = Elab::new(ctx, pool, fns, toplevel);
    let ret = el.resolve(ret_ty);
    let params: Vec<TypedBind> = params
        .iter()
        .map(|&(nm, ty)| {
            let t = el.resolve(ty);
            let b = el.new_bind(nm, t);
            el.bind_name(nm, b.id);
            b
        })
        .collect();
    let body = f(&mut el);
    TypedFn {
        name,
        params,
        ret,
        body,
        binds: el.next_bind,
    }
}

/// The elaboration of one body's checked tree. Holds the name scope and the
/// dense `BindingId` table; everything else it asks [`ElabCtx`] for.
pub struct Elab<'a, C: ElabCtx> {
    ctx: &'a mut C,
    pool: &'a mut ResolvedPool,
    /// The program's function table. Eta wrappers are appended here.
    fns: &'a mut FnTable,
    /// Next `BindingId` to mint. `TypedFn::binds` is this at the end.
    next_bind: u32,
    /// Resolved type of each bind, indexed by `BindingId.0`.
    binds: Vec<RTy>,
    /// `StrId → BindingId` for source-named binds in scope. A stack, so
    /// shadowing is push/pop and lookup is a reverse scan.
    names: Vec<(StrId, BindingId)>,
    /// [`StrId::NONE`]: binds no source name reaches (an argument spill, a tuple
    /// projection, an `or`-expression's unwrapped payload). `lower` reads it
    /// back off [`TypedBind::name`] to tell an operand temp it may collapse
    /// from a `let` the source wrote, which owns its frame slot.
    anon: StrId,
    /// Every eta wrapper's parameters share this name; it is only displayed.
    eta_param: StrId,
    nil: RTy,
    /// Whether this body is a module toplevel, entry script included.
    toplevel: bool,
    /// True, in a toplevel, until the first [`Self::block`] call. Gates decl
    /// scheduling.
    at_toplevel: bool,
    /// Span of the `match` whose arms are being elaborated. The [`PatCtx`] seam
    /// carries no spans, so this is the only location an internal error raised
    /// from a pattern query can be attributed to.
    pat_span: Span,
}

impl<'a, C: ElabCtx> Elab<'a, C> {
    fn new(
        ctx: &'a mut C,
        pool: &'a mut ResolvedPool,
        fns: &'a mut FnTable,
        toplevel: bool,
    ) -> Self {
        let anon = StrId::NONE;
        let eta_param = ctx.intern("_");
        let nil_ty = ctx.ty_nil();
        let nil = ctx.resolve_rty(pool, nil_ty);
        Elab {
            ctx,
            pool,
            fns,
            next_bind: 0,
            binds: Vec::new(),
            names: Vec::new(),
            anon,
            eta_param,
            nil,
            toplevel,
            at_toplevel: toplevel,
            pat_span: Span::DUMMY,
        }
    }

    fn nil_expr(&self) -> TypedExpr {
        TypedExpr::Nil { ty: self.nil }
    }

    /// Bridge an inference `Ty` into the pool.
    fn resolve(&mut self, t: Ty) -> RTy {
        self.ctx.resolve_rty(&mut *self.pool, t)
    }

    /// The denotation of a name this body's own scope does not bind. A
    /// module-scope value is promoted to the known function its slot holds,
    /// which a sibling walked later may only now have.
    fn denotation(&mut self, name: StrId, at: NameAt, span: Span) -> Denotation {
        match at {
            NameAt::Den(den) => den.promoted(|slot| self.ctx.fn_of_global(slot)),
            NameAt::Bound if self.toplevel => match self.ctx.decl_denotation(name) {
                Some(den) => den,
                None => elaborator_bug("toplevel name resolves to no decl and no binding", span),
            },
            NameAt::Bound => elaborator_bug("a name this body binds has no binding", span),
        }
    }

    fn int_rty(&mut self) -> RTy {
        let t = self.ctx.ty_int();
        self.resolve(t)
    }

    /// Everything a constructor's use site needs, with its field types
    /// specialised to `at`'s type arguments. When `at` is opaque (an
    /// undetermined scrutinee) a field type stays `Bound` and is handled
    /// dynamically.
    ///
    /// The bare-name path. Qualified constructors go through [`Self::ctor_of`];
    /// `mod.C(x)` is not a pattern the parser accepts.
    fn ctor_at(&mut self, name: &str, at: RTy, span: Span) -> Option<CtorPat> {
        let (scheme, den) = self.ctx.resolve_ctor(name)?;
        self.ctor_of(scheme, den, at, span)
    }

    /// As [`Self::ctor_at`], but from the denotation the callee walk already
    /// resolved. A qualified constructor has no bare name to re-resolve, and
    /// re-resolving one that does would instantiate its scheme twice.
    ///
    /// `None` means exactly one thing: `den` does not denote a constructor.
    fn ctor_of(&mut self, scheme: Ty, den: Denotation, at: RTy, span: Span) -> Option<CtorPat> {
        let (variant, arity) = den.as_ctor()?;
        let Some(labels) = self.ctx.ctor_labels(variant) else {
            elaborator_bug("constructor with no declared labels", span)
        };
        let sig = self.resolve(scheme);
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
        // `from_parts` checks the three independently-derived widths against
        // each other and aborts on a disagreement, so callers can zip the slots
        // against `field_tys()` without indexing.
        Some(CtorPat::from_parts(variant, arity, labels, field_tys, span))
    }

    fn new_bind(&mut self, name: StrId, ty: RTy) -> TypedBind {
        let id = BindingId(self.next_bind);
        self.next_bind += 1;
        self.binds.push(ty);
        TypedBind {
            id,
            name,
            ty,
            global: None,
        }
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

    /// Elaborate `e` where the surrounding context fixes the result type — a
    /// function's tail, an `if` arm, a `match` arm's body. Control-flow forms
    /// take `result_ty` rather than their own inferred type: a block's own type
    /// is its tail's, and a module toplevel's is `Nil`, but what the context
    /// wants out of it is the type the result must have.
    fn expr_as(&mut self, e: &CExpr, result_ty: Ty) -> TypedExpr {
        self.dispatch(e, result_ty)
    }

    /// Elaborate `e` in value position: its type is the one the check walk
    /// inferred for it.
    fn expr(&mut self, e: &CExpr) -> TypedExpr {
        self.dispatch(e, e.ty)
    }

    /// `result` is the type the context wants out of `e`. It differs from
    /// `e.ty` only for a control-flow form under [`Self::expr_as`].
    fn dispatch(&mut self, e: &CExpr, result: Ty) -> TypedExpr {
        let own = e.ty;
        let span = e.span;
        match &e.kind {
            CKind::Block(nodes) => self.block(nodes, result, span),
            CKind::If { cond, then, els } => self.if_expr(cond, then, els, result),
            CKind::Match { subject, arms } => self.match_expr(subject, arms, result, span),
            CKind::Or {
                lhs,
                receiver,
                body,
            } => self.or_expr(lhs, *receiver, body, result, span),
            CKind::Binary { op, left, right } => match BinopKind::of(*op) {
                BinopKind::ShortCircuit(sc) => self.short_circuit(left, right, sc, result),
                BinopKind::Value(op) => self.binary(left, right, op, own),
            },

            CKind::Number(n) => {
                let ty = self.resolve(own);
                let value = self.ctx.number_const(n);
                TypedExpr::Const { ty, value }
            }
            CKind::String(s) => {
                let ty = self.resolve(own);
                let value = self.ctx.string_const(s);
                TypedExpr::Const { ty, value }
            }
            CKind::Name { name, fact } => self.ident(*name, fact, own, span),
            CKind::Member { name, fact } => self.module_access(*name, fact, own, span),
            CKind::Unary { op, operand } => self.unary(*op, operand, own),
            CKind::Tuple(elems) => {
                let ty = self.resolve(own);
                let elems = elems.iter().map(|el| self.expr(el)).collect();
                TypedExpr::Tuple { ty, elems }
            }
            CKind::Field { recv, label } => {
                let ty = self.resolve(own);
                let recv_ty = recv.ty;
                let recv = self.expr(recv);
                self.field(recv, recv_ty, *label, ty, span)
            }
            CKind::TupleIndex { recv, idx } => {
                let ty = self.resolve(own);
                let recv = Box::new(self.expr(recv));
                TypedExpr::TupleIndex {
                    ty,
                    recv,
                    idx: *idx,
                }
            }
            CKind::Call { callee, args } => self.call(callee, args, own, span),
            CKind::Range { start, end } => {
                let ty = self.resolve(own);
                let start = Box::new(self.expr(start));
                let end = Box::new(self.expr(end));
                TypedExpr::Range { ty, start, end }
            }
            CKind::Slice { recv, start, end } => {
                let ty = self.resolve(own);
                let recv = Box::new(self.expr(recv));
                TypedExpr::Slice {
                    ty,
                    recv,
                    start: Box::new(self.expr(start)),
                    end: Box::new(self.expr(end)),
                }
            }
            // `Index` yields `Option(elem)`, which is what lets `xs[i] or d`
            // resolve.
            CKind::Index { recv, index } => {
                let ty = self.resolve(own);
                let recv = Box::new(self.expr(recv));
                TypedExpr::Index {
                    ty,
                    recv,
                    index: Box::new(self.expr(index)),
                }
            }
            CKind::Array(elems) => {
                let ty = self.resolve(own);
                let elems = elems
                    .iter()
                    .map(|el| match el {
                        CArrayElem::Elem(ex) => TypedArrayElem::Elem(self.expr(ex)),
                        CArrayElem::Spread(ex) => TypedArrayElem::Spread(self.expr(ex)),
                    })
                    .collect();
                TypedExpr::Array { ty, elems }
            }
            CKind::Interp(parts) => self.interp(parts, own),
            CKind::Closure(fact) => {
                let ty = self.resolve(own);
                self.closure(fact, ty, span)
            }
            CKind::BinaryLiteral(segs) => self.binary_literal(segs, own),
            CKind::Error => elaborator_bug("expression the check walk rejected", span),
        }
    }

    fn if_expr(&mut self, cond: &CExpr, then: &CExpr, els: &CExpr, result_ty: Ty) -> TypedExpr {
        let ty = self.resolve(result_ty);
        let cond = Box::new(self.expr(cond));
        let then = Box::new(self.expr_as(then, result_ty));
        let els = Box::new(self.expr_as(els, result_ty));
        TypedExpr::If {
            ty,
            cond,
            then,
            els,
        }
    }

    /// An identifier: a local, or else a nullary constructor, a first-class
    /// constructor/builtin, or a global/capture load.
    fn ident(&mut self, name: StrId, fact: &NameFact, own: Ty, span: Span) -> TypedExpr {
        if let Some(bid) = self.lookup_name(name) {
            return self.var(bid);
        }
        let ty = self.resolve(own);
        let den = self.denotation(name, fact.at, span);
        self.value_of(den, name, fact.ty, ty, span)
    }

    /// A resolved name in value position. `ty` is the use site's solved type;
    /// `scheme` is the name's type as the check walk instantiated it, and is
    /// only consulted when `ty` cannot type the node.
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
    /// (`map(Some, xs)`): a zero-capture closure over a wrapper `TypedFn`
    /// appended to the program.
    ///
    /// Type the wrapper from `ty`, the use site's solved type, not from
    /// `scheme`. Zonking an unsolved scheme yields opaque
    /// [`ResolvedNode::Bound`] parameters, which are not heap-shaped, so
    /// Perceus would annotate the wrapper as all-scalar and drop nothing.
    /// `scheme` is only a fallback for the capture path, where the use site
    /// *is* the scheme.
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

    /// An operator that denotes a [`PrimOp`]. `op` is a [`ValueBinop`], so
    /// `&&`/`||` cannot arrive here — [`BinopKind::of`] routes them to
    /// [`Self::short_circuit`]. That is what lets `specialize_binop` return a
    /// `PrimOp` and not an option.
    fn binary(&mut self, left: &CExpr, right: &CExpr, op: ValueBinop, own: Ty) -> TypedExpr {
        let ty = self.resolve(own);
        let lhs = self.expr(left);
        let rhs = self.expr(right);
        // `None` is a genuinely polymorphic operand (`fn add(a, b) { a + b }`
        // is `Addable a => (a, a) -> a`); the dynamic op is correct there.
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
        left: &CExpr,
        right: &CExpr,
        op: ShortCircuitOp,
        result_ty: Ty,
    ) -> TypedExpr {
        let ty = self.resolve(result_ty);
        let lhs = Box::new(self.expr(left));
        let rhs = Box::new(self.expr_as(right, result_ty));
        match op {
            ShortCircuitOp::And => TypedExpr::And { ty, lhs, rhs },
            ShortCircuitOp::Or => TypedExpr::Or { ty, lhs, rhs },
        }
    }

    fn unary(&mut self, op: ast::UnaryOp, operand: &CExpr, own: Ty) -> TypedExpr {
        let ty = self.resolve(own);
        let operand = self.expr(operand);
        let op = match op {
            ast::UnaryOp::Not => PrimOp::Not,
            ast::UnaryOp::Neg => match self.pool.prim_of(operand.ty()) {
                Some(Prim::Int) => PrimOp::IntNeg,
                Some(Prim::Float) => PrimOp::FloatNeg,
                _ => PrimOp::Neg,
            },
        };
        TypedExpr::Unary {
            ty,
            op,
            operand: Box::new(operand),
        }
    }

    fn interp(&mut self, parts: &[CInterpPart], own: Ty) -> TypedExpr {
        let ty = self.resolve(own);
        if parts.is_empty() {
            let value = self.ctx.string_const("");
            return TypedExpr::Const { ty, value };
        }
        let parts = parts
            .iter()
            .map(|p| match p {
                CInterpPart::Literal(s) => TypedInterpPart::Str(self.ctx.string_const(s)),
                CInterpPart::Expr(ex) => TypedInterpPart::Expr(self.expr(ex)),
            })
            .collect();
        TypedExpr::Interp { ty, parts }
    }

    /// A `fn(...) { ... }` expression: the body is already checked, parked and
    /// assigned a `func_idx`, and the walk recorded how each capture loads here.
    /// A named `fn` *declaration* goes through `fn_of_global` instead.
    fn closure(&mut self, fact: &ClosureFact, ty: RTy, span: Span) -> TypedExpr {
        let mut captures = Vec::with_capacity(fact.captures.len());
        for (cn, cap) in &fact.captures {
            if let Some(bid) = self.lookup_name(*cn) {
                captures.push(self.var(bid));
                continue;
            }
            let den = self.denotation(*cn, cap.at, span);
            let cty = self.resolve(cap.ty);
            captures.push(self.value_of(den, *cn, cap.ty, cty, span));
        }
        TypedExpr::Closure {
            ty,
            func_idx: fact.func_idx,
            captures,
        }
    }

    fn binary_literal(&mut self, segs: &[CBinSeg], own: Ty) -> TypedExpr {
        let ty = self.resolve(own);
        if segs.is_empty() {
            let value = self.ctx.binary_const(vec![], 0);
            return TypedExpr::Const { ty, value };
        }
        let segs = segs
            .iter()
            .map(|seg| {
                let value = self.expr(&seg.value);
                match seg_bits(self, &seg.spec, seg.size.as_ref(), seg.value.span) {
                    SpecWidth::Int(bits) => TypedBinSeg::Int { value, bits },
                    SpecWidth::Bytes(bits) => TypedBinSeg::Binary { value, bits },
                    SpecWidth::Utf8 => TypedBinSeg::Utf8 { value },
                }
            })
            .collect();
        TypedExpr::BinLit { ty, segs }
    }

    /// `module::member` in value position. Never a local: `::` reaches only
    /// into a module.
    fn module_access(&mut self, name: StrId, fact: &NameFact, own: Ty, span: Span) -> TypedExpr {
        let ty = self.resolve(own);
        let den = self.denotation(name, fact.at, span);
        self.value_of(den, name, fact.ty, ty, span)
    }

    /// `recv.label`, where `recv` has type `recv_ty` and the field has type
    /// `ty`. The check walk admitted it because every variant has the field,
    /// but they may hold it in different slots. When they agree it is one
    /// read. When they don't it is the `match` a person would write: an arm
    /// per variant, each binding that variant's slot.
    fn field(
        &mut self,
        recv: TypedExpr,
        recv_ty: Ty,
        label: StrId,
        ty: RTy,
        at: Span,
    ) -> TypedExpr {
        let Some(place) = self.ctx.ctor_field(recv_ty, label) else {
            elaborator_bug("field access", at)
        };
        let layouts = match place {
            FieldAt::Same(idx) => {
                return TypedExpr::Field {
                    ty,
                    recv: Box::new(recv),
                    idx,
                };
            }
            FieldAt::PerVariant(layouts) => layouts,
        };
        let scrut_ty = recv.ty();
        let mut arms = Vec::with_capacity(layouts.len());
        for layout in layouts {
            let b = self.new_bind(self.anon, ty);
            let mut fields = Vec::with_capacity(layout.fields.len());
            for (j, &fty) in layout.fields.iter().enumerate() {
                fields.push(if j == layout.slot as usize {
                    TypedPat::Bind(b)
                } else {
                    TypedPat::Wild {
                        ty: self.resolve(fty),
                    }
                });
            }
            arms.push(TypedArm {
                pat: TypedPat::Ctor {
                    ty: scrut_ty,
                    variant: layout.variant,
                    fields,
                },
                guard: None,
                body: self.var(b.id),
            });
        }
        TypedExpr::Match {
            ty,
            scrut: Box::new(recv),
            arms,
        }
    }

    fn call(&mut self, callee: &CCallee, args: &[CArg], own: Ty, at: Span) -> TypedExpr {
        let ty = self.resolve(own);
        // Resolve the callee first so labeled/spread constructor calls route to
        // `ctor_call` before their arguments are reordered.
        match self.callee(callee) {
            Callee::Ctor { scheme, den } => self.ctor_call(scheme, den, args, ty, at),
            Callee::Value {
                callee,
                param_labels,
            } => self.value_call(callee, param_labels, args, ty, at),
        }
    }

    /// A call through a callee value. Labelled arguments are reordered into the
    /// callee's declared parameter order — the order the check walk typed them
    /// against — by the same [`slot_labeled`] over the same labels the check
    /// walk used, so the two cannot permute differently.
    ///
    /// As in [`Self::ctor_call`], a call that reorders binds every supplied
    /// argument to a `Let` in *source* order first, or reordering them would
    /// reorder their side effects.
    fn value_call(
        &mut self,
        callee: TypedCallee,
        param_labels: Option<Vec<StrId>>,
        args: &[CArg],
        ty: RTy,
        at: Span,
    ) -> TypedExpr {
        if args.iter().all(|a| matches!(a, CArg::Positional(_))) {
            let args = args
                .iter()
                .map(|a| match a {
                    CArg::Positional(e) => self.expr(e),
                    CArg::Labeled { .. } | CArg::Spread(_) => {
                        elaborator_bug("non-positional argument on a positional call", at)
                    }
                })
                .collect::<Vec<_>>();
            return TypedExpr::Call { ty, callee, args };
        }
        let Some(labels) = param_labels else {
            elaborator_bug(
                "labeled or spread argument on a callee with no parameter names",
                at,
            )
        };

        let mut lets: Vec<(TypedBind, TypedExpr)> = Vec::new();
        let mut supplied: SmallVec<[(Option<StrId>, TypedExpr); 4]> =
            SmallVec::with_capacity(args.len());
        for a in args {
            match a {
                CArg::Positional(e) => {
                    let v = self.expr(e);
                    let v = self.spill(v, &mut lets);
                    supplied.push((None, v));
                }
                CArg::Labeled { label, value } => {
                    let v = self.expr(value);
                    let v = self.spill(v, &mut lets);
                    supplied.push((Some(*label), v));
                }
                // Only a constructor call takes a spread; the check walk has
                // refused this one.
                CArg::Spread(_) => {
                    elaborator_bug("spread argument on a non-constructor callee", at)
                }
            }
        }

        let slots: SmallVec<[Option<StrId>; 4]> = labels.iter().map(|&l| Some(l)).collect();
        let (by_pos, errors) = slot_labeled(&slots, supplied);
        if !errors.is_empty() {
            elaborator_bug("call arguments the check walk mis-slotted", at)
        }
        let mut out: Vec<TypedExpr> = Vec::with_capacity(by_pos.len());
        for slot in by_pos {
            match slot {
                Some(v) => out.push(v),
                // A call takes no spread, so an unfilled parameter is an arity
                // error the check walk has already reported.
                None => elaborator_bug("call parameter with no argument", at),
            }
        }
        wrap_lets(
            lets,
            TypedExpr::Call {
                ty,
                callee,
                args: out,
            },
        )
    }

    fn callee(&mut self, callee: &CCallee) -> Callee {
        match callee {
            CCallee::Name { name, fact, span } => {
                if let Some(bid) = self.lookup_name(*name) {
                    // A binding in scope shadows any module `fn` of that name,
                    // exactly as it does for the check walk's `env` lookup, and
                    // carries no parameter names.
                    let callee = TypedCallee::Dynamic(Box::new(self.var(bid)));
                    return Callee::Value {
                        callee,
                        param_labels: None,
                    };
                }
                let den = self.denotation(*name, fact.at, *span);
                self.resolved_callee(fact.ty, den, fact.param_labels.clone())
            }
            CCallee::Member { name, fact, span } => {
                let den = self.denotation(*name, fact.at, *span);
                self.resolved_callee(fact.ty, den, fact.param_labels.clone())
            }
            CCallee::Expr(e) => Callee::Value {
                callee: TypedCallee::Dynamic(Box::new(self.expr(e))),
                param_labels: None,
            },
        }
    }

    fn resolved_callee(
        &mut self,
        fn_ty: Ty,
        den: Denotation,
        param_labels: Option<Vec<StrId>>,
    ) -> Callee {
        let sig = self.resolve(fn_ty);
        match den.as_callee(sig) {
            CallForm::Ctor => Callee::Ctor { scheme: fn_ty, den },
            CallForm::Callee(c) => Callee::Value {
                callee: c,
                param_labels,
            },
        }
    }

    /// A constructor call. Labeled args are reordered into declared-field
    /// order; a `..base` spread fills every unsupplied slot with a projection
    /// out of `base`.
    ///
    /// Each slot a spread fills is `base.field`, read by name exactly as a
    /// written `.field` is: the check walk admits a spread only when every
    /// field it fills is one `base.field` could read.
    ///
    /// When any argument is labeled or spread, every supplied argument is bound
    /// to a `Let` in *source* order first, or reordering them into field order
    /// would reorder their side effects.
    fn ctor_call(
        &mut self,
        scheme: Ty,
        den: Denotation,
        args: &[CArg],
        ty: RTy,
        at: Span,
    ) -> TypedExpr {
        let Some(cp) = self.ctor_of(scheme, den, ty, at) else {
            elaborator_bug("unresolved constructor", at)
        };
        let arity = cp.arity();
        let reorders = args.iter().any(|a| !matches!(a, CArg::Positional(_)));

        let mut lets: Vec<(TypedBind, TypedExpr)> = Vec::new();
        let mut spread: Option<(TypedBind, Ty)> = None;
        let mut supplied: SmallVec<[(Option<StrId>, TypedExpr); 4]> =
            SmallVec::with_capacity(args.len());
        for a in args {
            match a {
                CArg::Positional(e) => {
                    let v = self.expr(e);
                    let v = if reorders {
                        self.spill(v, &mut lets)
                    } else {
                        v
                    };
                    supplied.push((None, v));
                }
                CArg::Labeled { label, value } => {
                    let v = self.expr(value);
                    let v = self.spill(v, &mut lets);
                    supplied.push((Some(*label), v));
                }
                CArg::Spread(e) => {
                    let v = self.expr(e);
                    let b = self.new_bind(self.anon, v.ty());
                    lets.push((b, v));
                    spread = Some((b, e.ty));
                }
            }
        }

        let labels = cp.slot_fields();
        let (by_pos, errors) = slot_labeled(&labels, supplied);
        if !errors.is_empty() {
            elaborator_bug("constructor arguments the check walk mis-slotted", at)
        }
        let field_tys: SmallVec<[RTy; 4]> = cp.field_tys().into();
        let mut fields: Vec<TypedExpr> = Vec::with_capacity(arity);
        for (i, (slot, fty)) in by_pos.into_iter().zip(field_tys).enumerate() {
            match slot {
                Some(v) => fields.push(v),
                None => {
                    // A slot no argument filled reads from `..base`.
                    let (Some((base, base_ty)), Some(&Some(label))) = (spread, labels.get(i))
                    else {
                        elaborator_bug("constructor field with no argument and no spread", at)
                    };
                    let recv = self.var(base.id);
                    fields.push(self.field(recv, base_ty, label, fty, at));
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
    /// the typed IR: this emits the `Match` the `Option`/`Result` variants
    /// describe.
    fn or_expr(
        &mut self,
        lhs: &CExpr,
        receiver: Option<StrId>,
        body: &CExpr,
        result_ty: Ty,
        span: Span,
    ) -> TypedExpr {
        let ty = self.resolve(result_ty);
        let lhs_ty = lhs.ty;
        let lhs_rty = self.resolve(lhs_ty);
        let lhs = self.expr(lhs);
        let Some(shape) = self.ctx.or_shape(lhs_ty) else {
            elaborator_bug("or-expression on non-Option/Result", span)
        };
        // Success arm: `Ok(v)`/`Some(v)` → v.
        let ok_bind = self.new_bind(self.anon, ty);
        let ok_body = self.var(ok_bind.id);
        let (fail_fields, body) = if shape.err_has_payload {
            // The payload type must be the real `E`: `err` may be a heap value
            // the recovery body drops.
            let Some(ety) = self.pool.con_arg(lhs_rty, 1) else {
                elaborator_bug("or-expression on a non-generic Result", span)
            };
            let err_bind = self.new_bind(receiver.unwrap_or(self.anon), ety);
            // The receiver is in scope for the recovery body and nothing else.
            let mark = self.names.len();
            if let Some(name) = receiver {
                self.bind_name(name, err_bind.id);
            }
            let body = self.expr_as(body, result_ty);
            self.names.truncate(mark);
            (vec![TypedPat::Bind(err_bind)], body)
        } else {
            (vec![], self.expr_as(body, result_ty))
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

    fn match_expr(
        &mut self,
        subject: &CExpr,
        arms: &[CArm],
        result_ty: Ty,
        span: Span,
    ) -> TypedExpr {
        let ty = self.resolve(result_ty);
        let scrut_rty = self.resolve(subject.ty);
        let scrut = Box::new(self.expr(subject));
        // Save and restore rather than assign: an arm body may hold a nested
        // `match`, and an error raised after it still names this one.
        let outer = std::mem::replace(&mut self.pat_span, span);
        let arms = elaborate_arms(self, scrut_rty, arms);
        self.pat_span = outer;
        TypedExpr::Match { ty, scrut, arms }
    }

    /// A block is a right-nested `Let`/`Seq` spine ending in its tail
    /// expression, or [`TypedExpr::Nil`] when it ends in a statement.
    ///
    /// See [`Frame`] for why the spine is assembled flat and folded once.
    fn block(&mut self, nodes: &[CNode], result_ty: Ty, span: Span) -> TypedExpr {
        let is_top = std::mem::replace(&mut self.at_toplevel, false);
        // Module toplevel: `fn`/`const` decls are order-free and mutually
        // recursive, so they run in the check walk's dependency (SCC) order.
        // Raw source order would read a forward reference before it is stored.
        let seq: Vec<Step> = if is_top {
            self.decl_schedule(nodes.len(), span)
        } else {
            (0..nodes.len()).map(|i| (i, None)).collect()
        };
        let mark = self.names.len();
        let e = self.block_nodes(&seq, nodes, result_ty, span);
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
            // Either abort is a silent miscompile if it is allowed through: a
            // duplicate initialises two globals from one node, and an
            // out-of-range index means the checker and the elaborator disagree
            // about the module.
            match placed.get_mut(idx) {
                Some(p) if !*p => *p = true,
                Some(_) => elaborator_bug("toplevel decl scheduled twice", span),
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
    /// See [`Frame`] for why this loops instead of recursing per statement.
    fn block_nodes(
        &mut self,
        seq: &[Step],
        nodes: &[CNode],
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
            let Some(node) = nodes.get(idx) else {
                elaborator_bug("block schedule index out of range", span)
            };
            // A trailing expression is the block's value; anything else,
            // including an expression before the end, is a statement.
            if let CNode::Expr(e) = node
                && steps.peek().is_none()
            {
                break self.expr_as(e, result_ty);
            }
            self.statement(node, global, &mut spine);
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
    /// the entry-frame slot [`Self::decl_schedule`] paired with this node:
    /// `Some` only for a top-level `fn`/`const`. A `let` takes its slot from
    /// [`Self::scoped_bind`] instead.
    fn statement(&mut self, node: &CNode, global: Option<GlobalSlot>, out: &mut Vec<Frame>) {
        match node {
            CNode::Expr(e) | CNode::Effect(e) => {
                let effect = self.expr(e);
                out.push(Frame::Seq(effect));
            }
            CNode::Binding {
                pattern,
                init,
                globals,
            } => match &pattern.pattern {
                ast::Pattern::Var { name } => {
                    let sid = self.ctx.intern(&name.name);
                    let init = self.expr(init);
                    let mut bind = self.new_bind(sid, init.ty());
                    bind.global = global_of(globals, sid);
                    self.bind_name(sid, bind.id);
                    out.push(Frame::Let(bind, init));
                }
                pat => self.destructure(pat, init, globals, out),
            },
            CNode::Fn { name, span } => {
                // A `fn` declaration is not an expression, so the walk built no
                // node with its type; its scheme is in the env.
                let Some(fn_ty) = self.ctx.toplevel_fn_ty(*name) else {
                    elaborator_bug("unbound fn declaration", *span)
                };
                let fr = self.resolve(fn_ty);
                // The bind comes first: its `GlobalSlot` is the decl's
                // identity, and the closure it initialises is whatever
                // function the walk emitted for that slot.
                let bind = self.decl_bind(*name, fr, global);
                let Some(func_idx) = bind.global.and_then(|g| self.ctx.fn_of_global(g)) else {
                    elaborator_bug("fn declaration with no emitted body", *span)
                };
                let init = TypedExpr::Closure {
                    ty: fr,
                    func_idx,
                    captures: Vec::new(),
                };
                self.bind_name(*name, bind.id);
                out.push(Frame::Let(bind, init));
            }
            CNode::Const { name, init } => {
                // A `const` gets its own bind even when its init is a bare
                // identifier: fn bodies address it by its own entry-frame
                // slot, distinct from whatever it aliases.
                let init = self.expr(init);
                let bind = self.decl_bind(*name, init.ty(), global);
                self.bind_name(*name, bind.id);
                out.push(Frame::Let(bind, init));
            }
            // Types are erased and imports resolved by the check walk; a `@vm`
            // fn carries no Scarlet body and no runtime binding.
            CNode::Nothing => {}
        }
    }

    /// `let (a, b) = e` / `let Point(x, y) = e`: bind the scrutinee, then one
    /// `Let` per projection. Exhaustiveness has proven the tag, so the field
    /// reads need no check.
    fn destructure(
        &mut self,
        pat: &ast::Pattern,
        init: &CExpr,
        globals: &[(StrId, GlobalSlot)],
        out: &mut Vec<Frame>,
    ) {
        let value = self.expr(init);
        let scrut = self.new_bind(self.anon, value.ty());
        let mut lets = vec![(scrut, value)];
        self.irrefutable(pat, scrut.id, globals, &mut lets);
        out.extend(lets.into_iter().map(|(b, e)| Frame::Let(b, e)));
    }

    /// Project `pat`'s bindings out of the bind `src`, appending one `Let` per
    /// projection. A `Var` sub-pattern is an alias, not a copy.
    fn irrefutable(
        &mut self,
        pat: &ast::Pattern,
        src: BindingId,
        globals: &[(StrId, GlobalSlot)],
        lets: &mut Vec<(TypedBind, TypedExpr)>,
    ) {
        let src_ty = self.bind_ty(src);
        match pat {
            // Write-only `_`-prefixed names: nothing may read them, so the
            // projection bind stays anonymous.
            ast::Pattern::Var { name } if name.name.starts_with('_') => {}
            ast::Pattern::Var { name } => {
                let sid = self.ctx.intern(&name.name);
                self.bind_name(sid, src);
                // A local needs nothing more: names resolve by `BindingId`, so
                // the projection bind can stay anonymous and collapsible.
                //
                // A module-scope destructured name instead adopts the source
                // name and a global slot, like a top-level `let`. `src` is the
                // projection's own bind, one per field, so exactly one bind is
                // pinned per source name.
                let Some((b, _)) = lets.iter_mut().find(|(b, _)| b.id == src) else {
                    elaborator_bug("destructured name with no projection bind", name.span)
                };
                if let Some(slot) = global_of(globals, sid) {
                    b.name = sid;
                    b.global = Some(slot);
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
                    self.irrefutable(sub, b.id, globals, lets);
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
                let by_pos = slot_pattern_args(self, &cp.slot_fields(), args, name.span);
                let field_tys: SmallVec<[RTy; 4]> = cp.field_tys().into();
                for (i, (sub, fty)) in by_pos.into_iter().zip(field_tys).enumerate() {
                    let Some(sub) = sub else { continue };
                    let b = self.new_bind(self.anon, fty);
                    let proj = TypedExpr::Field {
                        ty: fty,
                        recv: Box::new(self.var(src)),
                        idx: i as u32,
                    };
                    lets.push((b, proj));
                    self.irrefutable(sub, b.id, globals, lets);
                }
            }
            // `let`/`(a, b) =` bind irrefutably: the check walk rejects a
            // literal, range, binary or or-pattern in binding position.
            other => elaborator_bug("irrefutable pattern", other.span()),
        }
    }
}

/// Pattern elaboration's view of the walk. Every type crossing this seam is an
/// [`RTy`], so a `fresh_var()` cannot reach a `TypedPat`.
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

    fn number_const(&mut self, lit: &ast::NumberLiteral) -> ConstId {
        self.ctx.number_const(lit)
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

    /// Aborts on `None`: wildcarding an unresolved constructor pattern would
    /// make the arm match everything and drop its bindings.
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

    /// Aborts on `None`: defaulting to `Nil` would hand Perceus a non-heap type
    /// for a possibly-heap element and silently lose its `Drop`.
    fn tuple_elem_ty(&mut self, t: RTy, i: usize) -> RTy {
        match self.pool.tuple_elem(t, i) {
            Some(t) => t,
            None => elaborator_bug("tuple element of a non-tuple type", self.pat_span),
        }
    }

    /// As [`Self::tuple_elem_ty`], for an array's element type.
    fn array_elem_ty(&mut self, t: RTy) -> RTy {
        match self.pool.con_arg(t, 0) {
            Some(t) => t,
            None => elaborator_bug("type argument of a non-generic type", self.pat_span),
        }
    }

    fn expr(&mut self, e: &CExpr) -> TypedExpr {
        Elab::expr(self, e)
    }
}

/// A callee, or the constructor whose arguments must be slotted before it can
/// be one. Keeping these apart stops `ctor_call` from having to invent a
/// placeholder [`TypedCallee`].
enum Callee {
    /// A saturated constructor call. It carries the scheme and denotation so a
    /// qualified `mod.Ctor(..)`, whose bare name is not in scope, still works.
    Ctor { scheme: Ty, den: Denotation },
    /// A call through a callee value. `param_labels` is `Some` only for a
    /// statically resolved module `fn`, which is the only callee whose
    /// arguments may be labelled.
    Value {
        callee: TypedCallee,
        param_labels: Option<Vec<StrId>>,
    },
}

/// One node of a block's spine, before it is folded into the right-nested
/// `Let`/`Seq` tree.
///
/// `Elab::block_nodes` collects these in evaluation order and folds once.
/// Recursing per statement costs three stack frames each, and a module toplevel
/// has one statement per declaration: 1200 declarations overflowed the default
/// thread stack.
enum Frame {
    /// `let bind = init;`
    Let(TypedBind, TypedExpr),
    /// A statement evaluated for effect.
    Seq(TypedExpr),
}

/// Fold `lets` around `tail`: the first pushed `Let` is the outermost, so the
/// spine's evaluation order is push order.
/// The module-scope slot the check walk gave `name` in this binding.
fn global_of(globals: &[(StrId, GlobalSlot)], name: StrId) -> Option<GlobalSlot> {
    globals.iter().find(|(n, _)| *n == name).map(|&(_, g)| g)
}

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
/// `{k ↦ Int}`. When `at` is not a `Con` the map is empty and the fields stay
/// polymorphic.
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
        ResolvedNode::Con { id, args } => {
            let kids: SmallVec<[RTy; 4]> = pool.children(args).into();
            let new: SmallVec<[RTy; 4]> = kids.iter().map(|&k| subst_rty(pool, k, m)).collect();
            if new == kids {
                t
            } else {
                pool.mk_con(id, &new)
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
    use scarlet_types::type_def::TypeId;
    use scarlet_types::types::PrimIds;

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
    /// tail.
    #[test]
    fn a_ctors_fields_specialise_to_the_type_it_is_used_at() {
        let mut p = pool();
        let int = p.mk_con(TypeId(1), &[]);
        let a = p.mk_bound(0);
        let list_a = p.mk_con(TypeId(9), &[a]);
        let list_int = p.mk_con(TypeId(9), &[int]);

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
        let list_a = p.mk_con(TypeId(9), &[a]);
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
        let int = p.mk_con(TypeId(1), &[]);
        let tup = p.mk_tuple(&[int, int]);
        let before = p.node_count();
        let mut m = HashMap::new();
        m.insert(3u32, int);
        assert_eq!(subst_rty(&mut p, tup, &m), tup);
        assert_eq!(p.node_count(), before);
    }

    /// Nested spines are rebuilt, not shallowly copied.
    #[test]
    fn substitution_rewrites_a_nested_spine() {
        let mut p = pool();
        let int = p.mk_con(TypeId(1), &[]);
        let a = p.mk_bound(0);
        let inner = p.mk_tuple(&[a, int]);
        let outer = p.mk_con(TypeId(4), &[inner]);
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
        let int = p.mk_con(TypeId(1), &[]);
        let mk = |i: u32| TypedBind {
            id: BindingId(i),
            name: StrId(0),
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
