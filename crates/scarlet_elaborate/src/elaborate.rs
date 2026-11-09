//! The elaborator turns the checked tree into typed IR. This file holds its
//! skeleton and the code for each kind of expression.
//!
//! It only resolves things: which field index, which variant, which callee and
//! which typed opcode. Evaluation order (ANF) is `lower`'s job. Patterns are
//! handled in [`super::elaborate_pat`], and this module implements the
//! [`PatCtx`] trait it needs.
//!
//! Every node carries an [`RTy`] from the caller's [`ResolvedPool`].
//! [`PreludeTys::resolve_rty`] is the only way a live inference `Ty` gets into
//! that pool, and after that the elaborator never looks at the union-find.
//!
//! Elaboration never fails. It only runs on a module the check walk found no
//! errors in, so every question it asks already has an answer. When one
//! doesn't, [`elaborator_bug`] aborts.

use std::collections::HashMap;

use smallvec::SmallVec;

use super::binop::{BinopKind, ShortCircuitOp, ValueBinop, specialize_binop};
use super::checked::{
    BinderId, CArg, CArm, CArrayElem, CBinSeg, CCallee, CExpr, CInterpPart, CKind, CNode, CPat,
    CPatKind, ClosureFact, NameAt, NameFact,
};
use super::elaborate_pat::{PatCtx, SpecWidth, elaborate_arms, seg_bits};
use super::eta::{FnRTy, FnTable, eta_wrapper};
use super::resolve::{CallForm, Denotation, EtaTarget, ValueForm};
use super::rty::{Arity, RTy, ResolvedNode, ResolvedPool};
use super::slots::slot_labeled;
use super::{
    BindingId, GlobalSlot, TypedArm, TypedArrayElem, TypedBinSeg, TypedBind, TypedCallee,
    TypedExpr, TypedFn, TypedInterpPart, TypedPat, ValueRef,
};
use scarlet_ir::core_ir::{ConstId, FuncIdx, PrimOp, VariantRef};
use scarlet_syntax::ast;
use scarlet_syntax::span::Span;
use scarlet_types::types::{Prim, StrId, Ty};

/// One scheduled node of a block. It holds the node's index in the block's
/// `body` and, for a top-level `fn` or `const`, the entry-frame slot its
/// binding must land in.
type Step = (usize, Option<GlobalSlot>);

/// The `Option`/`Result` shape behind an `or`-expression's left-hand side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrShape {
    /// The failure variant, `None` or `Err`.
    pub fail: VariantRef,
    /// The success variant, `Some` or `Ok`, which always has one field.
    pub ok: VariantRef,
    /// Whether the failure variant carries a payload. `Result`'s does and
    /// `Option`'s doesn't.
    pub err_has_payload: bool,
}

/// Where a field sits in a value of its receiver's type, found by the field's
/// name.
#[derive(Debug, Clone, PartialEq)]
pub enum FieldAt {
    /// The field is at this index in every variant, so a read doesn't need to
    /// know the variant.
    Same(u32),
    /// The field is at different indices in different variants. This holds
    /// one layout per variant, so a read looks at the variant first.
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

/// Abort because the check walk didn't answer a question elaboration had to
/// ask, which means the proof that the module is clean is wrong. `why` names
/// the question, such as `"field access"`. No program should reach this,
/// because a user-facing error would have stopped the module being called
/// clean first.
#[allow(clippy::panic)]
#[cold]
#[inline(never)]
pub(crate) fn elaborator_bug(why: &'static str, span: Span) -> ! {
    panic!(
        "internal compiler error: {why} at {span:?} is well-typed but was not elaborated. \
         Report this as a compiler bug."
    )
}

/// The prelude types a caller can name without a source expression to read
/// them from, plus the one function that resolves a type into a pool. It is
/// split from [`ElabCtx`] because [`super::TempTys::intern`] needs exactly
/// this much and runs before any body is elaborated.
pub trait PreludeTys {
    /// Resolve a live inference `Ty` into `pool`. This never fails, because a
    /// type inference never decided behaves like a polymorphic one at runtime
    /// (see `Zonker::zonk_or_opaque`).
    fn resolve_rty(&mut self, pool: &mut ResolvedPool, t: Ty) -> RTy;

    fn ty_bool(&mut self) -> Ty;
    fn ty_int(&mut self) -> Ty;
    fn ty_string(&mut self) -> Ty;
    fn ty_binary(&mut self) -> Ty;
}

/// Services the elaborator needs from the enclosing compilation.
///
/// There is deliberately no `engine()`. The elaborator makes every decision
/// against the pool, and the engine can only be reached through
/// [`PreludeTys::resolve_rty`].
pub trait ElabCtx: PreludeTys {
    fn intern(&mut self, s: &str) -> StrId;

    /// Parse a numeric literal and pool it.
    fn number_const(&mut self, lit: &ast::NumberLiteral) -> ConstId;
    fn string_const(&mut self, s: &str) -> ConstId;
    fn int_const(&mut self, i: i64) -> ConstId;
    /// Pool a `Binary` constant with the given raw bytes and bit length.
    fn binary_const(&mut self, bytes: Vec<u8>, bit_len: u64) -> ConstId;

    /// The type of the module's own top-level `fn` declaration `name`.
    fn toplevel_fn_ty(&mut self, name: StrId) -> Option<Ty>;
    /// One of this module's own top-level declarations, by its binder, for a
    /// use the module toplevel reaches before the declaration's statement.
    fn decl_denotation(&self, binder: BinderId) -> Option<Denotation>;
    /// Where `.field` sits in a receiver of `receiver` type.
    fn ctor_field(&mut self, receiver: Ty, field: StrId) -> Option<FieldAt>;
    /// The declared order of the field labels of the variant `v` constructs.
    ///
    /// It looks up the [`VariantRef`] the walk resolved, not a source name.
    /// `mod.Ctor(..)` isn't in scope under its bare name, so a name lookup
    /// would miss a constructor the check walk accepted.
    fn ctor_labels(&mut self, v: VariantRef) -> Option<Vec<StrId>>;
    /// The function the check walk made for the top-level `fn`
    /// declaration bound at `slot`, or `None` for a slot holding a plain value.
    fn fn_of_global(&self, slot: GlobalSlot) -> Option<FuncIdx>;
    /// The module's own top-level `fn` and `const` declarations, as `(index in
    /// the module block's `body`, entry-frame slot)` pairs, in the order the
    /// SCC walk visited them.
    ///
    /// The order matters, because a `const` used before its declaration must
    /// be stored before it is read, so declarations aren't walked in source
    /// order. This is only asked while elaborating a module toplevel.
    fn toplevel_decls(&self) -> Vec<(usize, GlobalSlot)>;
    /// The `Option`/`Result` shape behind `expr or default`.
    fn or_shape(&mut self, lhs_ty: Ty) -> Option<OrShape>;

    fn ty_nil(&mut self) -> Ty;
}

/// Elaborate `body` into a [`TypedFn`]. Eta wrappers the body needs are
/// appended to `fns`.
///
/// `toplevel` is set for a program that is a bare expression, whose body is
/// the module toplevel. A module block goes through [`elaborate_toplevel`]
/// instead.
#[allow(clippy::too_many_arguments)]
pub fn elaborate_body<C: ElabCtx>(
    ctx: &mut C,
    pool: &mut ResolvedPool,
    fns: &mut FnTable,
    name: StrId,
    params: &[(StrId, BinderId, Ty)],
    body: &CExpr,
    ret_ty: Ty,
    toplevel: bool,
) -> TypedFn {
    elaborate_fn(ctx, pool, fns, name, params, ret_ty, toplevel, |el| {
        el.expr_as(body, ret_ty)
    })
}

/// Elaborate a module's top level. That is the block's `nodes`, kept at their
/// source indices because its declarations are scheduled by index.
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
    params: &[(StrId, BinderId, Ty)],
    ret_ty: Ty,
    toplevel: bool,
    f: impl FnOnce(&mut Elab<'_, C>) -> TypedExpr,
) -> TypedFn {
    let mut el = Elab::new(ctx, pool, fns, toplevel);
    let ret = el.resolve(ret_ty);
    let params: Vec<TypedBind> = params
        .iter()
        .map(|&(nm, binder, ty)| {
            let t = el.resolve(ty);
            let b = el.new_bind(nm, t);
            el.bind_local(binder, b.id);
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

/// The elaboration of one body's checked tree. It holds the dense `BindingId`
/// table and what each of the body's binders was bound to, and asks
/// [`ElabCtx`] for everything else.
pub struct Elab<'a, C: ElabCtx> {
    ctx: &'a mut C,
    pool: &'a mut ResolvedPool,
    /// The program's function table. Eta wrappers are appended here.
    fns: &'a mut FnTable,
    /// The next `BindingId` to mint. At the end, this is `TypedFn::binds`.
    next_bind: u32,
    /// The resolved type of each bind, indexed by `BindingId.0`.
    binds: Vec<RTy>,
    /// The bind each of the body's binders was elaborated to, indexed by
    /// `BinderId.0`. A binder is one binding, so an entry is set once, when
    /// its binding is reached.
    locals: Vec<Option<BindingId>>,
    /// [`StrId::NONE`], the name for binds no source name reaches, such as an
    /// argument spill, a tuple projection or an `or` expression's unwrapped
    /// payload. `lower` reads it back from [`TypedBind::name`] to tell a
    /// temporary it may fold away from a `let` the source wrote, which owns
    /// its frame slot.
    anon: StrId,
    /// Every eta wrapper's parameters share this name, which is only for
    /// display.
    eta_param: StrId,
    nil: RTy,
    /// Whether this body is a module toplevel, entry script included.
    toplevel: bool,
    /// In a toplevel, this is true until the first [`Self::block`] call. It
    /// decides whether declarations get scheduled.
    at_toplevel: bool,
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
            locals: Vec::new(),
            anon,
            eta_param,
            nil,
            toplevel,
            at_toplevel: toplevel,
        }
    }

    fn nil_expr(&self) -> TypedExpr {
        TypedExpr::Nil { ty: self.nil }
    }

    /// Resolve an inference `Ty` into the pool.
    fn resolve(&mut self, t: Ty) -> RTy {
        self.ctx.resolve_rty(&mut *self.pool, t)
    }

    /// What a name refers to, which is one of this body's binds or a
    /// denotation from outside it. A module-scope value becomes the known
    /// function its slot holds, which a sibling walked later may only now have.
    fn name_ref(&mut self, at: NameAt, span: Span) -> NameRef {
        let den = match at {
            NameAt::Local(binder) => match self.local(binder) {
                Some(id) => return NameRef::Bind(id),
                None if self.toplevel => match self.ctx.decl_denotation(binder) {
                    Some(den) => den,
                    None => {
                        elaborator_bug("toplevel name resolves to no decl and no binding", span)
                    }
                },
                None => elaborator_bug("a name this body binds has no binding", span),
            },
            NameAt::Den(den) => den,
            NameAt::Unbound => elaborator_bug("a name with no runtime binding", span),
        };
        NameRef::Den(den.promoted(|slot| self.ctx.fn_of_global(slot)))
    }

    /// A denotation from outside this body, never one of its own binds.
    fn denotation(&mut self, at: NameAt, span: Span) -> Denotation {
        match self.name_ref(at, span) {
            NameRef::Den(den) => den,
            NameRef::Bind(_) => elaborator_bug("a module member bound as a local", span),
        }
    }

    fn int_rty(&mut self) -> RTy {
        let t = self.ctx.ty_int();
        self.resolve(t)
    }

    /// Everything a constructor call needs, with its field types specialised
    /// to `at`'s type arguments, taken from the denotation the callee walk
    /// resolved. A qualified constructor has no bare name to resolve again,
    /// and resolving one that does again would instantiate its scheme twice.
    ///
    /// `None` means only one thing: `den` is not a constructor.
    fn ctor_of(&mut self, scheme: Ty, den: Denotation, at: RTy, span: Span) -> Option<CtorSig> {
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
        // `from_parts` checks the three widths, each worked out separately,
        // against each other and aborts if they disagree. So callers can zip
        // the slots against `field_tys()` without indexing.
        Some(CtorSig::from_parts(variant, arity, labels, field_tys, span))
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
    /// [`ElabCtx::toplevel_decls`] record scheduled it at. `global` is `None`
    /// outside a module toplevel, such as for a `const` inside a function body.
    fn decl_bind(&mut self, name: StrId, ty: RTy, global: Option<GlobalSlot>) -> TypedBind {
        let mut b = self.new_bind(name, ty);
        b.global = global;
        b
    }

    fn bind_local(&mut self, binder: BinderId, id: BindingId) {
        let i = binder.0 as usize;
        if self.locals.len() <= i {
            self.locals.resize(i + 1, None);
        }
        if let Some(slot) = self.locals.get_mut(i) {
            *slot = Some(id);
        }
    }

    fn local(&self, binder: BinderId) -> Option<BindingId> {
        self.locals.get(binder.0 as usize).copied().flatten()
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

    /// Elaborate `e` where the surrounding context fixes the result type, such
    /// as a function's tail, an `if` arm or a `match` arm's body. Control-flow
    /// forms take `result_ty` instead of their own inferred type. A block's
    /// own type is its tail's, and a module toplevel's is `Nil`, but what the
    /// context wants is the type the result must have.
    fn expr_as(&mut self, e: &CExpr, result_ty: Ty) -> TypedExpr {
        self.dispatch(e, result_ty)
    }

    /// Elaborate `e` in value position, where its type is the one the check
    /// walk inferred for it.
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
            CKind::Match { subject, arms } => self.match_expr(subject, arms, result),
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

    /// An identifier. It is a local, or else a constructor with no fields, a
    /// constructor or builtin used as a value, or a global or capture load.
    fn ident(&mut self, name: StrId, fact: &NameFact, own: Ty, span: Span) -> TypedExpr {
        let den = match self.name_ref(fact.at, span) {
            NameRef::Bind(id) => return self.var(id),
            NameRef::Den(den) => den,
        };
        let ty = self.resolve(own);
        self.value_of(den, name, fact.ty, ty, span)
    }

    /// A resolved name in value position. `ty` is the use site's solved type.
    /// `scheme` is the name's type as the check walk instantiated it, and is
    /// only used when `ty` can't type the node.
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

    /// A constructor with fields, or a builtin, used as a first-class value,
    /// as in `map(Some, xs)`. It becomes a closure with no captures over a
    /// wrapper `TypedFn` added to the program.
    ///
    /// The wrapper is typed from `ty`, the use site's solved type, not from
    /// `scheme`. Zonking an unsolved scheme gives opaque [`ResolvedNode::Bound`]
    /// parameters, which aren't heap types. Perceus, the pass that inserts
    /// reference counting, would then treat the wrapper as all scalars and
    /// drop nothing. `scheme` is only a fallback for the capture path, where
    /// the use site *is* the scheme.
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
    /// `&&` and `||` can't arrive here, because [`BinopKind::of`] sends them
    /// to [`Self::short_circuit`]. That is what lets `specialize_binop` return
    /// a `PrimOp` and not an `Option`.
    fn binary(&mut self, left: &CExpr, right: &CExpr, op: ValueBinop, own: Ty) -> TypedExpr {
        let ty = self.resolve(own);
        let lhs = self.expr(left);
        let rhs = self.expr(right);
        // `None` means the operand really is polymorphic, as in
        // `fn add(a, b) { a + b }`, and the dynamic op is correct there.
        let prim = self.pool.prim_of(lhs.ty());
        TypedExpr::Binary {
            ty,
            op: specialize_binop(op, prim),
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
        }
    }

    /// `a && b` or `a || b`, which is control flow and not an operator. The
    /// right-hand side is a branch, so it is elaborated against the result
    /// type where the branches join.
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

    /// A `fn(...) { ... }` expression. Its body is already checked, set aside
    /// and given a `func_idx`, and the walk recorded how each capture loads
    /// here. A named `fn` *declaration* goes through `fn_of_global` instead.
    fn closure(&mut self, fact: &ClosureFact, ty: RTy, span: Span) -> TypedExpr {
        let mut captures = Vec::with_capacity(fact.captures.len());
        for (cn, cap) in &fact.captures {
            let den = match self.name_ref(cap.at, span) {
                NameRef::Bind(id) => {
                    captures.push(self.var(id));
                    continue;
                }
                NameRef::Den(den) => den,
            };
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
                match seg_bits(self, &seg.width) {
                    SpecWidth::Int(bits) => TypedBinSeg::Int { value, bits },
                    SpecWidth::Bytes(bits) => TypedBinSeg::Binary { value, bits },
                    SpecWidth::Utf8 => TypedBinSeg::Utf8 { value },
                }
            })
            .collect();
        TypedExpr::BinLit { ty, segs }
    }

    /// `module::member` in value position. It is never a local, because `::`
    /// only reaches into a module.
    fn module_access(&mut self, name: StrId, fact: &NameFact, own: Ty, span: Span) -> TypedExpr {
        let ty = self.resolve(own);
        let den = self.denotation(fact.at, span);
        self.value_of(den, name, fact.ty, ty, span)
    }

    /// `recv.label`, where `recv` has type `recv_ty` and the field has type
    /// `ty`. The check walk accepted it because every variant has the field,
    /// but they may hold it in different slots. When they agree, it is one
    /// read. When they don't, it is the `match` a person would write, with an
    /// arm per variant that binds that variant's slot.
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
        // Resolve the callee first, so a constructor call with labels or a
        // spread goes to `ctor_call` before its arguments are reordered.
        match self.callee(callee) {
            Callee::Ctor { scheme, den } => self.ctor_call(scheme, den, args, ty, at),
            Callee::Value {
                callee,
                param_labels,
            } => self.value_call(callee, param_labels, args, ty, at),
        }
    }

    /// A call through a callee value. Labelled arguments are put into the
    /// callee's declared parameter order, which is the order the check walk
    /// typed them in. This uses the same [`slot_labeled`] over the same labels
    /// as the check walk, so the two can't order them differently.
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
                // Only a constructor call takes a spread, so the check walk has
                // already refused this one.
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
            CCallee::Name { fact, span, .. } => match self.name_ref(fact.at, *span) {
                // A local carries no parameter names.
                NameRef::Bind(id) => Callee::Value {
                    callee: TypedCallee::Dynamic(Box::new(self.var(id))),
                    param_labels: None,
                },
                NameRef::Den(den) => self.resolved_callee(fact.ty, den, fact.param_labels.clone()),
            },
            CCallee::Member { fact, span, .. } => {
                let den = self.denotation(fact.at, *span);
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

    /// A constructor call. Labelled arguments are put into declared-field
    /// order, and a `..base` spread fills every slot nobody supplied with a
    /// field read from `base`.
    ///
    /// Each slot a spread fills is `base.field`, read by name exactly as a
    /// written `.field` is. The check walk only accepts a spread when
    /// `base.field` could read every field it fills.
    ///
    /// When any argument is labelled or spread, every supplied argument is
    /// bound to a `Let` in *source* order first. Otherwise putting them in
    /// field order would reorder their side effects.
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
        let field_tys: SmallVec<[RTy; 4]> = SmallVec::from_slice(&cp.field_tys);
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

    /// Bind `v` to a new `Let` and return a reference to it, so reordering it
    /// later can't move where it is evaluated.
    fn spill(&mut self, v: TypedExpr, lets: &mut Vec<(TypedBind, TypedExpr)>) -> TypedExpr {
        let bind = self.new_bind(self.anon, v.ty());
        lets.push((bind, v));
        self.var(bind.id)
    }

    /// `expr or body`, with an optional receiver. Typed IR has no `or`
    /// expression, so this builds the `Match` the `Option` or `Result`
    /// variants describe.
    fn or_expr(
        &mut self,
        lhs: &CExpr,
        receiver: Option<(StrId, BinderId)>,
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
        // The success arm turns `Ok(v)` or `Some(v)` into `v`.
        let ok_bind = self.new_bind(self.anon, ty);
        let ok_body = self.var(ok_bind.id);
        let (fail_fields, body) = if shape.err_has_payload {
            // The payload type must be the real `E`, because `err` may be a
            // heap value the recovery body drops.
            let Some(ety) = self.pool.con_arg(lhs_rty, 1) else {
                elaborator_bug("or-expression on a non-generic Result", span)
            };
            let err_bind = match receiver {
                Some((name, binder)) => {
                    let b = self.new_bind(name, ety);
                    self.bind_local(binder, b.id);
                    b
                }
                None => self.new_bind(self.anon, ety),
            };
            let body = self.expr_as(body, result_ty);
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

    fn match_expr(&mut self, subject: &CExpr, arms: &[CArm], result_ty: Ty) -> TypedExpr {
        let ty = self.resolve(result_ty);
        let scrut = Box::new(self.expr(subject));
        let arms = elaborate_arms(self, arms);
        TypedExpr::Match { ty, scrut, arms }
    }

    /// A block is a right-nested `Let`/`Seq` spine ending in its tail
    /// expression, or [`TypedExpr::Nil`] when it ends in a statement.
    ///
    /// See [`Frame`] for why the spine is assembled flat and folded once.
    fn block(&mut self, nodes: &[CNode], result_ty: Ty, span: Span) -> TypedExpr {
        let is_top = std::mem::replace(&mut self.at_toplevel, false);
        // At a module toplevel, `fn` and `const` declarations can come in any
        // order and refer to each other, so they run in the check walk's
        // dependency (SCC) order. Source order would read a name used before
        // its declaration before it is stored.
        let seq: Vec<Step> = if is_top {
            self.decl_schedule(nodes.len(), span)
        } else {
            (0..nodes.len()).map(|i| (i, None)).collect()
        };
        self.block_nodes(&seq, nodes, result_ty, span)
    }

    /// The module's declarations first, in the order the SCC walk visited them
    /// and each with the entry-frame slot it was given. Then every other node
    /// in source order, with no slot.
    ///
    /// `ElabCtx::toplevel_decls` is empty for a block that is not a module
    /// toplevel, so this becomes plain source order.
    fn decl_schedule(&mut self, len: usize, span: Span) -> Vec<Step> {
        let decls = self.ctx.toplevel_decls();
        let mut placed = vec![false; len];
        let mut seq: Vec<Step> = Vec::with_capacity(len);
        for &(idx, slot) in &decls {
            // Letting either case through would silently miscompile. A
            // duplicate initialises two globals from one node, and an index
            // out of range means the checker and the elaborator disagree about
            // the module.
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
                // The block ended in a statement, or was empty.
                break self.nil_expr();
            };
            let Some(node) = nodes.get(idx) else {
                elaborator_bug("block schedule index out of range", span)
            };
            // A trailing expression is the block's value. Anything else,
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
    /// the entry-frame slot [`Self::decl_schedule`] paired with this node, and
    /// is only `Some` for a top-level `fn` or `const`. A binding takes its
    /// slots from its own `globals` list instead.
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
            } => match pattern.kind {
                CPatKind::Bind { name, binder } => {
                    let init = self.expr(init);
                    let mut bind = self.new_bind(name, init.ty());
                    bind.global = global_of(globals, name);
                    self.bind_local(binder, bind.id);
                    out.push(Frame::Let(bind, init));
                }
                CPatKind::Wild
                | CPatKind::Number(_)
                | CPatKind::String(_)
                | CPatKind::Range { .. }
                | CPatKind::Ctor { .. }
                | CPatKind::Tuple(_)
                | CPatKind::Array { .. }
                | CPatKind::Binary { .. }
                | CPatKind::Or(_)
                | CPatKind::Error => self.destructure(pattern, init, globals, out),
            },
            CNode::Fn { name, binder, span } => {
                // A `fn` declaration is not an expression, so the walk built no
                // node with its type. Its scheme is the module's.
                let Some(fn_ty) = self.ctx.toplevel_fn_ty(*name) else {
                    elaborator_bug("unbound fn declaration", *span)
                };
                let fr = self.resolve(fn_ty);
                // The bind comes first, because its `GlobalSlot` identifies the
                // declaration, and the closure it starts with is whatever
                // function the walk made for that slot.
                let bind = self.decl_bind(*name, fr, global);
                let Some(func_idx) = bind.global.and_then(|g| self.ctx.fn_of_global(g)) else {
                    elaborator_bug("fn declaration with no emitted body", *span)
                };
                let init = TypedExpr::Closure {
                    ty: fr,
                    func_idx,
                    captures: Vec::new(),
                };
                self.bind_local(*binder, bind.id);
                out.push(Frame::Let(bind, init));
            }
            CNode::Const { name, binder, init } => {
                // A `const` gets its own bind even when its init is a bare
                // name, because fn bodies read it from its own entry-frame
                // slot, not the slot of whatever it aliases.
                let init = self.expr(init);
                let bind = self.decl_bind(*name, init.ty(), global);
                self.bind_local(*binder, bind.id);
                out.push(Frame::Let(bind, init));
            }
            // The check walk has already erased types and resolved imports,
            // and a `@vm` fn has no Scarlet body and no runtime binding.
            CNode::Nothing => {}
        }
    }

    /// `let (a, b) = e` or `let Point(x, y) = e`. Bind the scrutinee, then add
    /// one `Let` per field read. Exhaustiveness checking has already proven
    /// the tag, so the field reads need no check.
    fn destructure(
        &mut self,
        pat: &CPat,
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

    /// Read `pat`'s bindings out of the bind `src`, appending one `Let` per
    /// field read. A name sub-pattern is an alias, not a copy.
    fn irrefutable(
        &mut self,
        pat: &CPat,
        src: BindingId,
        globals: &[(StrId, GlobalSlot)],
        lets: &mut Vec<(TypedBind, TypedExpr)>,
    ) {
        let fields = match &pat.kind {
            CPatKind::Wild => return,
            CPatKind::Bind { name, binder } => {
                self.bind_local(*binder, src);
                // A local needs nothing more. Names resolve by `BindingId`, so
                // the field read's bind can stay anonymous and be folded away.
                //
                // A destructured name at module scope takes the source name
                // and a global slot instead, like a top-level `let`. `src` is
                // the field read's own bind, one per field, so exactly one
                // bind is pinned per source name.
                let Some((b, _)) = lets.iter_mut().find(|(b, _)| b.id == src) else {
                    elaborator_bug("destructured name with no projection bind", pat.span)
                };
                if let Some(slot) = global_of(globals, *name) {
                    b.name = *name;
                    b.global = Some(slot);
                }
                return;
            }
            CPatKind::Tuple(elems) => elems,
            CPatKind::Ctor { fields, .. } => fields,
            // A binding can't fail to match, because the check walk rejects a
            // literal, range, array, binary or or-pattern in binding position.
            CPatKind::Number(_)
            | CPatKind::String(_)
            | CPatKind::Range { .. }
            | CPatKind::Array { .. }
            | CPatKind::Binary { .. }
            | CPatKind::Or(_)
            | CPatKind::Error => elaborator_bug("irrefutable pattern", pat.span),
        };
        let tuple = matches!(pat.kind, CPatKind::Tuple(_));
        for (i, sub) in fields.iter().enumerate() {
            if matches!(sub.kind, CPatKind::Wild) {
                continue;
            }
            let ty = self.resolve(sub.ty);
            let b = self.new_bind(self.anon, ty);
            let recv = Box::new(self.var(src));
            let idx = i as u32;
            let proj = if tuple {
                TypedExpr::TupleIndex { ty, recv, idx }
            } else {
                TypedExpr::Field { ty, recv, idx }
            };
            lets.push((b, proj));
            self.irrefutable(sub, b.id, globals, lets);
        }
    }
}

/// What pattern elaboration sees of the walk. Every type that crosses here is
/// an [`RTy`], so a `fresh_var()` can't reach a `TypedPat`.
impl<C: ElabCtx> PatCtx for Elab<'_, C> {
    fn bind(&mut self, binder: BinderId, name: StrId, ty: RTy) -> TypedBind {
        if let Some(id) = self.local(binder) {
            return TypedBind {
                id,
                name,
                ty: self.bind_ty(id),
                global: None,
            };
        }
        let b = self.new_bind(name, ty);
        self.bind_local(binder, b.id);
        b
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

    fn resolve(&mut self, t: Ty) -> RTy {
        Elab::resolve(self, t)
    }

    fn expr(&mut self, e: &CExpr) -> TypedExpr {
        Elab::expr(self, e)
    }
}

/// A constructor's declared fields at one use: the labels in declared order,
/// and each field's type, instantiated against the use's type.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CtorSig {
    variant: VariantRef,
    /// The same length as `field_tys`, which [`CtorSig::from_parts`] checks.
    labels: Vec<StrId>,
    field_tys: Vec<RTy>,
}

impl CtorSig {
    /// If the separate sources disagree about the width, that is a compiler
    /// bug, and this aborts. Guessing a field type would be unsound, because
    /// Perceus uses `is_heap` on the field's type to decide whether to `Dup`
    /// and `Drop` it, so a wrong guess gets the field's reference count wrong.
    fn from_parts(
        variant: VariantRef,
        declared_arity: Arity,
        labels: Vec<StrId>,
        field_tys: Vec<RTy>,
        span: Span,
    ) -> CtorSig {
        let arity = declared_arity.0 as usize;
        if labels.len() != arity || field_tys.len() != arity {
            elaborator_bug("constructor field-type desync", span)
        }
        CtorSig {
            variant,
            labels,
            field_tys,
        }
    }

    fn arity(&self) -> usize {
        self.field_tys.len()
    }

    /// The declared fields in the form [`slot_labeled`] takes, with one
    /// labelled entry per field.
    fn slot_fields(&self) -> SmallVec<[Option<StrId>; 4]> {
        self.labels.iter().map(|&l| Some(l)).collect()
    }
}

/// What a name in this body refers to.
enum NameRef {
    /// One of the body's own binds.
    Bind(BindingId),
    Den(Denotation),
}

/// A callee, or a constructor whose arguments must be put in their slots
/// before it can be one. Keeping these apart means `ctor_call` never has to
/// make up a placeholder [`TypedCallee`].
enum Callee {
    /// A constructor call with all its arguments. It carries the scheme and
    /// denotation so a qualified `mod.Ctor(..)`, whose bare name isn't in
    /// scope, still works.
    Ctor { scheme: Ty, den: Denotation },
    /// A call through a callee value. `param_labels` is only `Some` for a
    /// module `fn` resolved at compile time, which is the only callee whose
    /// arguments may be labelled.
    Value {
        callee: TypedCallee,
        param_labels: Option<Vec<StrId>>,
    },
}

/// One node of a block's spine, before it is folded into the right-nested
/// `Let`/`Seq` tree.
///
/// `Elab::block_nodes` collects these in evaluation order and folds them once.
/// Recursing per statement costs three stack frames each, and a module
/// toplevel has one statement per declaration, so 1200 declarations
/// overflowed the default thread stack.
enum Frame {
    /// `let bind = init;`
    Let(TypedBind, TypedExpr),
    /// A statement run for its side effect.
    Seq(TypedExpr),
}

/// The module-scope slot the check walk gave `name` in this binding.
fn global_of(globals: &[(StrId, GlobalSlot)], name: StrId) -> Option<GlobalSlot> {
    globals.iter().find(|(n, _)| *n == name).map(|&(_, g)| g)
}

/// Fold `lets` around `tail`. The first `Let` pushed is the outermost, so the
/// spine runs in the order they were pushed.
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

/// Line a constructor's return type up against the type it is used at, and
/// return the `Bound(i) → RTy` substitution that specialises its field types.
///
/// `Cons`'s scheme returns `List(Bound(k))`, so used at `List(Int)` the map is
/// `{k ↦ Int}`. When `at` is not a `Con`, the map is empty and the fields stay
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

/// Rebuild `t` with every `Bound(i)` in `m` replaced. A node with no
/// substituted variable in it comes back as it is, so a concrete field type
/// costs no allocation.
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
    /// `Int` and `List(Int)`, which is what Perceus needs to know to emit a
    /// `Drop` for the tail.
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

    /// A use site with an undecided type leaves the fields polymorphic instead
    /// of inventing a type for them.
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

    /// A concrete field type comes back as it is, because substitution
    /// allocates nothing when nothing changes.
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

    /// Nested types are rebuilt all the way down, not copied at the top.
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

    /// `wrap_lets` keeps the source's evaluation order. The first spilled
    /// argument is the outermost `Let`, so it runs first.
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
