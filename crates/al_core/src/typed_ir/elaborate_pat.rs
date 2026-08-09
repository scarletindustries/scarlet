//! Pattern elaboration: `ast::Pattern` → [`TypedPat`], `ast::MatchArm` →
//! [`TypedArm`].
//!
//! The resolution half of `core_ir::lower`'s pattern machinery; `lower` keeps
//! the ANF half. Everything `lower` would otherwise ask the inference engine
//! is decided here, so it never sees an unsolved type variable:
//!
//! * A [`TypedPat::Ctor`]'s `fields` is exactly the variant's arity, in
//!   declared-field order. A slot no source argument filled is a
//!   [`TypedPat::Wild`] carrying that field's resolved type.
//! * A [`TypedPat::Array`] carries `elem_ty` projected out of its scrutinee.
//! * Interning is a mutation, so `lower` cannot do it: every `ConstId` it
//!   emits — including ones with no source literal, like an array pattern's
//!   length or a `<<>>` walk's initial bit cursor — is minted here.
//!
//! ## Or-patterns bind once
//!
//! `Cons(x, _) | Wrap(x)` binds one `x`. The arm's guard and body are one
//! tree, so which [`BindingId`] a name means must not depend on which
//! alternative matched: the first occurrence mints it and every occurrence in
//! a later alternative reuses it. A repeat *within* one alternative
//! (`Pair(x, x)`) is what that reuse must never absorb; the check walk rejects
//! it, so elaboration aborts on one.

use std::collections::{HashMap, HashSet};

use smallvec::SmallVec;

use crate::ast;
use crate::bytecode::Op;
use crate::core_ir::ConstId;
use crate::core_ir::VariantRef;
use crate::span::Span;
use crate::types::StrId;

use super::rty::{Arity, RTy};
use super::slots::{Slots, slot_labeled};
use super::{PatRest, TypedArm, TypedBinPatSeg, TypedBind, TypedExpr, TypedPat, elaborator_bug};

/// Everything a constructor pattern head needs, resolved against the
/// scrutinee's type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CtorPat {
    pub variant: VariantRef,
    /// Declared field labels, in declared order. Private, and the same length
    /// as `field_tys` — see [`CtorPat::new`].
    labels: Vec<StrId>,
    /// Field types, instantiated against the scrutinee, index-aligned with
    /// `labels`.
    field_tys: Vec<RTy>,
}

impl CtorPat {
    /// One `(label, type)` pair per declared field, in declared order.
    ///
    /// The only constructor, so a `labels`/`field_tys` desync is unspellable
    /// rather than merely rejected. Guessing a field type would be unsound:
    /// `Nil` is a prim `Con`, `is_heap` answers false for it, and Perceus then
    /// emits neither `Dup` nor `Drop` for what may be a heap field.
    /// [`Self::from_parts`] checks the widths the independent sources report.
    pub fn new(variant: VariantRef, fields: Vec<(StrId, RTy)>) -> CtorPat {
        let (labels, field_tys) = fields.into_iter().unzip();
        CtorPat {
            variant,
            labels,
            field_tys,
        }
    }

    /// [`Self::new`] from three widths the walk derived independently. A
    /// disagreement is a compiler bug and aborts — never a wildcard, which
    /// would silently match everything.
    pub fn from_parts(
        variant: VariantRef,
        declared_arity: Arity,
        labels: Vec<StrId>,
        field_tys: Vec<RTy>,
        span: Span,
    ) -> CtorPat {
        let arity = declared_arity.0 as usize;
        if labels.len() != arity || field_tys.len() != arity {
            elaborator_bug("constructor field-type desync", span)
        }
        CtorPat::new(variant, labels.into_iter().zip(field_tys).collect())
    }

    pub fn arity(&self) -> usize {
        self.field_tys.len()
    }

    /// Declared field labels, for hover and diagnostics.
    pub fn labels(&self) -> &[StrId] {
        &self.labels
    }

    /// The declared-fields description [`slot_labeled`] consumes: one labeled
    /// entry per field.
    pub fn slot_fields(&self) -> SmallVec<[Option<StrId>; 4]> {
        self.labels.iter().map(|&l| Some(l)).collect()
    }

    /// Field types in declared order. Exactly [`Self::arity`] long, so it zips
    /// against `slot_labeled`'s slots.
    pub fn field_tys(&self) -> &[RTy] {
        &self.field_tys
    }
}

/// The services pattern elaboration needs from the enclosing elaborator.
///
/// Deliberately not an `InferEngine`: every type crossing this seam is an
/// [`RTy`], so a `fresh_var()` cannot reach a `TypedPat`.
pub trait PatCtx {
    fn intern(&mut self, name: &str) -> StrId;

    /// Mint a fresh [`BindingId`] for `name` and bring it into scope for the
    /// rest of the pattern, its guard, and its body.
    fn bind(&mut self, name: StrId, ty: RTy) -> TypedBind;
    fn scope_mark(&mut self) -> usize;
    /// Drop every name bound since `mark`.
    fn scope_reset(&mut self, mark: usize);

    fn number_const(&mut self, lit: &ast::NumberLiteral) -> ConstId;
    fn string_const(&mut self, s: &str) -> ConstId;
    fn int_const(&mut self, i: i64) -> ConstId;
    fn binary_const(&mut self, bytes: Vec<u8>, bit_len: u64) -> ConstId;

    fn ty_int(&mut self) -> RTy;

    /// Resolve a constructor pattern head. Total: a diagnostics-clean module
    /// cannot name a constructor that does not resolve, so an implementation
    /// aborts rather than answering with a wildcard — that would match
    /// unconditionally and leave the body's names bound to nothing.
    fn resolve_ctor_pat(&mut self, name: &str, scrut: RTy) -> CtorPat;
    /// `io.NotFound(path)`, resolved through the module qualifier. Total for
    /// the same reason as the bare form.
    fn resolve_ctor_pat_qualified(&mut self, qual: &str, name: &str, scrut: RTy) -> CtorPat;

    /// `i`th element type of the tuple type `t`.
    fn tuple_elem_ty(&mut self, t: RTy, i: usize) -> RTy;
    /// Element type of the array type `t`.
    fn array_elem_ty(&mut self, t: RTy) -> RTy;

    /// Elaborate an expression: a `<<v:size(n)>>` width, a guard, an arm body.
    fn expr(&mut self, e: &ast::Expression) -> TypedExpr;
}

/// Elaborate the arms of a `match` whose scrutinee has type `scrut`.
///
/// Each arm opens its own name scope: the pattern's binds are visible to the
/// guard and the body and to nothing else.
pub fn elaborate_arms<C: PatCtx>(cx: &mut C, scrut: RTy, arms: &[ast::MatchArm]) -> Vec<TypedArm> {
    arms.iter()
        .map(|arm| {
            let mark = cx.scope_mark();
            let pat = elaborate_pattern(cx, &arm.pattern, scrut);
            let guard = arm.guard.as_ref().map(|g| cx.expr(g));
            let body = cx.expr(&arm.body);
            cx.scope_reset(mark);
            TypedArm { pat, guard, body }
        })
        .collect()
}

/// Elaborate one pattern against a scrutinee of type `scrut`. Binds it
/// introduces stay in scope; the caller decides when to drop them.
pub fn elaborate_pattern<C: PatCtx>(cx: &mut C, p: &ast::Pattern, scrut: RTy) -> TypedPat {
    PatElab {
        cx,
        bound: HashMap::new(),
        seen: HashSet::new(),
    }
    .pat(p, scrut)
}

/// Slot a constructor's pattern args into declared-field order, so a slot
/// index is a field index rather than a source-arg index.
///
/// The check walk already slotted these args the same way and reported any
/// error, so an error here means the two walks disagree. It aborts: dropping
/// the mis-slotted sub-pattern would leave its binds unminted and the arm
/// matching too much.
pub fn slot_pattern_args<'p, C: PatCtx>(
    cx: &mut C,
    fields: &[Option<StrId>],
    args: &'p [ast::PatternArg],
    span: Span,
) -> Slots<&'p ast::Pattern> {
    let supplied = args.iter().map(|a| match a {
        ast::PatternArg::Positional(p) => (None, p),
        ast::PatternArg::Labeled { label, pattern } => (Some(cx.intern(&label.name)), pattern),
    });
    let (by_pos, errors) = slot_labeled(fields, supplied);
    if !errors.is_empty() {
        elaborator_bug("constructor pattern arg desync", span)
    }
    by_pos
}

/// Every [`TypedBind`] `p` introduces, in left-to-right order, each listed
/// once however many or-alternatives mention it.
pub fn pat_binds(p: &TypedPat) -> Vec<TypedBind> {
    let mut out = Vec::new();
    collect_binds(p, &mut out);
    out
}

fn push_bind(out: &mut Vec<TypedBind>, b: TypedBind) {
    if !out.iter().any(|x| x.id == b.id) {
        out.push(b);
    }
}

fn collect_binds(p: &TypedPat, out: &mut Vec<TypedBind>) {
    match p {
        TypedPat::Wild { .. } | TypedPat::Lit { .. } | TypedPat::Range { .. } => {}
        TypedPat::Bind(b) => push_bind(out, *b),
        TypedPat::Ctor { fields, .. } => {
            for f in fields {
                collect_binds(f, out);
            }
        }
        TypedPat::Tuple { elems, .. } => {
            for e in elems {
                collect_binds(e, out);
            }
        }
        TypedPat::Or { alts, .. } => {
            for a in alts {
                collect_binds(a, out);
            }
        }
        TypedPat::Array { prefix, rest, .. } => {
            for e in prefix {
                collect_binds(e, out);
            }
            collect_rest(rest, out);
        }
        TypedPat::Bin { segs, rest, .. } => {
            for s in segs {
                match s {
                    TypedBinPatSeg::Utf8Literal { .. } => {}
                    TypedBinPatSeg::Utf8 { value }
                    | TypedBinPatSeg::Int { value, .. }
                    | TypedBinPatSeg::Binary { value, .. } => collect_binds(value, out),
                }
            }
            collect_rest(rest, out);
        }
    }
}

fn collect_rest(rest: &PatRest, out: &mut Vec<TypedBind>) {
    if let PatRest::Bind(b) = rest {
        push_bind(out, *b);
    }
}

struct PatElab<'c, C: PatCtx> {
    cx: &'c mut C,
    /// Names already bound in this pattern. A later or-alternative re-binds
    /// the same names onto the same [`BindingId`], which is the one-variable-
    /// per-name rule the single-body [`TypedArm`] needs.
    bound: HashMap<StrId, TypedBind>,
    /// Names bound in the *current* or-alternative. `bound` alone cannot tell
    /// a legitimate cross-alternative re-bind from `Pair(x, x)`.
    seen: HashSet<StrId>,
}

impl<C: PatCtx> PatElab<'_, C> {
    /// Bind `name`, or return the [`TypedBind`] an earlier or-alternative
    /// minted for it. A repeat within the current alternative is a compiler
    /// bug (the check walk rejects it) and aborts rather than aliasing two
    /// positions to one binder.
    fn bind(&mut self, name: &str, ty: RTy, span: Span) -> TypedBind {
        let sid = self.cx.intern(name);
        if !self.seen.insert(sid) {
            elaborator_bug("duplicate binding in pattern alternative", span)
        }
        match self.bound.get(&sid) {
            Some(b) => *b,
            None => {
                let b = self.cx.bind(sid, ty);
                self.bound.insert(sid, b);
                b
            }
        }
    }

    /// A `..name` rest binder. The parser leaves a `.._` rest as a binding
    /// named `_`, and the check walk never records `_`, so `_` must not reach
    /// [`Self::bind`]'s duplicate abort.
    fn rest_bind(&mut self, id: &ast::Identifier, ty: RTy) -> PatRest {
        if id.name == "_" {
            PatRest::Discard
        } else {
            PatRest::Bind(self.bind(&id.name, ty, id.span))
        }
    }

    fn pat(&mut self, p: &ast::Pattern, scrut: RTy) -> TypedPat {
        match p {
            ast::Pattern::Wildcard { .. } => TypedPat::Wild { ty: scrut },
            ast::Pattern::Var { name } => TypedPat::Bind(self.bind(&name.name, scrut, name.span)),
            ast::Pattern::Literal(ast::PatternLiteral::Number(n)) => {
                let value = self.cx.number_const(n);
                TypedPat::Lit { ty: scrut, value }
            }
            ast::Pattern::Literal(ast::PatternLiteral::String(s)) => TypedPat::Lit {
                ty: scrut,
                value: self.cx.string_const(&s.value),
            },
            ast::Pattern::Constructor {
                qualifier,
                name,
                args,
                span,
                ..
            } => self.ctor(qualifier.as_ref(), &name.name, args, scrut, *span),
            ast::Pattern::Tuple { elements, .. } => {
                let elems = elements
                    .iter()
                    .enumerate()
                    .map(|(i, e)| {
                        let ty = self.cx.tuple_elem_ty(scrut, i);
                        self.pat(e, ty)
                    })
                    .collect();
                TypedPat::Tuple { ty: scrut, elems }
            }
            ast::Pattern::Array { elements, span } => self.array(elements, scrut, *span),
            ast::Pattern::Binary { segments, rest, .. } => self.bin(segments, rest.as_ref(), scrut),
            ast::Pattern::Or { first, rest, .. } => {
                // `bound` is shared across alternatives (the bind-once rule),
                // but `seen` restarts from the enclosing names per
                // alternative, so only a repeat within one alternative trips
                // `bind`. What the alternatives bound folds back in after:
                // `(A(x) | B(x), x)` repeats `x` in its one alternative.
                let outer = std::mem::take(&mut self.seen);
                let mut after = outer.clone();
                let alts = std::iter::once(&**first)
                    .chain(rest.iter())
                    .map(|a| {
                        self.seen = outer.clone();
                        let alt = self.pat(a, scrut);
                        after.extend(self.seen.iter().copied());
                        alt
                    })
                    .collect();
                self.seen = after;
                TypedPat::Or { ty: scrut, alts }
            }
            ast::Pattern::Range { start, end, .. } => {
                let lo = self.cx.number_const(start);
                let hi = self.cx.number_const(end);
                TypedPat::Range { ty: scrut, lo, hi }
            }
        }
    }

    /// Args are slotted into declared-field order by the same `slot_labeled`
    /// the typechecker and exhaustiveness checker use. Every slot is filled —
    /// an unreached one becomes a `Wild` carrying that field's type — which is
    /// what gives `fields.len() == arity`.
    fn ctor(
        &mut self,
        qualifier: Option<&ast::Identifier>,
        name: &str,
        args: &[ast::PatternArg],
        scrut: RTy,
        span: Span,
    ) -> TypedPat {
        let info = match qualifier {
            Some(q) => self.cx.resolve_ctor_pat_qualified(&q.name, name, scrut),
            None => self.cx.resolve_ctor_pat(name, scrut),
        };
        let by_pos = slot_pattern_args(&mut *self.cx, &info.slot_fields(), args, span);

        let field_tys: SmallVec<[RTy; 4]> = info.field_tys().into();
        let mut fields = Vec::with_capacity(info.arity());
        for (slot, fty) in by_pos.into_iter().zip(field_tys) {
            fields.push(match slot {
                Some(sub) => self.pat(sub, fty),
                None => TypedPat::Wild { ty: fty },
            });
        }
        TypedPat::Ctor {
            ty: scrut,
            variant: info.variant,
            fields,
        }
    }

    /// The parser only accepts `..rest` as the final element. Anything after
    /// one is a desync and aborts: dropping it would match a shorter prefix
    /// than the source wrote.
    fn array(&mut self, elements: &[ast::ArrayPatternElement], scrut: RTy, span: Span) -> TypedPat {
        let elem_ty = self.cx.array_elem_ty(scrut);
        let pre = elements
            .iter()
            .position(|e| matches!(e, ast::ArrayPatternElement::Spread { .. }))
            .unwrap_or(elements.len());
        let (prefix_elems, rest_elems) = elements.split_at(pre);
        let mut prefix = Vec::with_capacity(prefix_elems.len());
        for e in prefix_elems {
            let ast::ArrayPatternElement::Pattern(ep) = e else {
                elaborator_bug("array spread not last", span)
            };
            prefix.push(self.pat(ep, elem_ty));
        }
        // The `..rest` binding takes the scrutinee's own array type: it is a
        // suffix of the same array, not an element.
        let rest = match rest_elems {
            [] => PatRest::None,
            [ast::ArrayPatternElement::Spread { binding: None, .. }] => PatRest::Discard,
            [
                ast::ArrayPatternElement::Spread {
                    binding: Some(id), ..
                },
            ] => self.rest_bind(id, scrut),
            _ => elaborator_bug("array spread not last", span),
        };
        // `lower` compares `ArrayLen(scrut)` against this and has no pool of
        // its own, so the constant is minted here.
        let len = self.cx.int_const(prefix.len() as i64);
        TypedPat::Array {
            ty: scrut,
            elem_ty,
            len,
            prefix,
            rest,
        }
    }

    fn bin(
        &mut self,
        segments: &[ast::BinSegmentPat],
        rest: Option<&ast::BinaryPatternRest>,
        scrut: RTy,
    ) -> TypedPat {
        let segs = segments.iter().map(|s| self.bin_seg(s, scrut)).collect();
        let rest = match rest {
            None => PatRest::None,
            Some(r) => match &r.binding {
                None => PatRest::Discard,
                Some(id) => self.rest_bind(id, scrut),
            },
        };
        // The bit cursor `lower` walks the segments with starts here.
        let zero = self.cx.int_const(0);
        TypedPat::Bin {
            ty: scrut,
            zero,
            segs,
            rest,
        }
    }

    fn bin_seg(&mut self, seg: &ast::BinSegmentPat, scrut: RTy) -> TypedBinPatSeg {
        // `<<'lit'>>`: match the literal's UTF-8 bytes as a prefix. Its width
        // is known now.
        if let Some(s) = seg.utf8_literal() {
            let width = (s.len() as i64) * 8;
            let bytes = self.cx.binary_const(s.as_bytes().to_vec(), width as u64);
            let bits = self.cx.int_const(width);
            return TypedBinPatSeg::Utf8Literal { bytes, bits };
        }
        // Width before value, matching the check walk's visit order.
        match seg_bits(&mut *self.cx, &seg.spec) {
            SpecWidth::Utf8 => {
                // `BinReadUtf8` yields the codepoint as an `Int`.
                let int_t = self.cx.ty_int();
                TypedBinPatSeg::Utf8 {
                    value: self.pat(&seg.value, int_t),
                }
            }
            SpecWidth::Bytes(bits) => {
                let value = self.pat(&seg.value, scrut);
                TypedBinPatSeg::Binary { bits, value }
            }
            SpecWidth::Int(bits) => {
                let int_t = self.cx.ty_int();
                let value = self.pat(&seg.value, int_t);
                TypedBinPatSeg::Int { bits, value }
            }
        }
    }
}

/// The width a `<<..>>` segment's spec prescribes. Carrying the kind means an
/// `Int` segment's width cannot be missing, so consumers need no "Int segment
/// with no width" guard.
#[derive(Debug)]
pub enum SpecWidth {
    /// `:N` / `:size(..)`, or the default 8 when sizeless.
    Int(TypedExpr),
    /// `:bytes(..)` scaled to bits; `None` is a sizeless `:binary`, which
    /// consumes the remainder.
    Bytes(Option<TypedExpr>),
    /// `:utf8` — the width is data-dependent, decoded at runtime.
    Utf8,
}

/// The bits-width rule for `<<..>>` segments, shared by expression and pattern
/// elaboration so the two cannot drift. `lower` reads each width as a plain
/// `Int` expression and never re-derives the default or the unit.
///
/// The `:bytes` scale uses the generic `Op::Mul`, not `MulInt`: this is a
/// synthesised node the checker never specialised.
pub fn seg_bits<C: PatCtx>(cx: &mut C, spec: &ast::BinSpec) -> SpecWidth {
    match spec {
        ast::BinSpec::Int { bits: Some(e) } => SpecWidth::Int(cx.expr(e)),
        ast::BinSpec::Int { bits: None } => {
            let ty = cx.ty_int();
            let value = cx.int_const(8);
            SpecWidth::Int(TypedExpr::Const { ty, value })
        }
        ast::BinSpec::Binary { bytes: Some(e) } => {
            let ty = cx.ty_int();
            let v = cx.expr(e);
            let eight = cx.int_const(8);
            SpecWidth::Bytes(Some(TypedExpr::Binary {
                ty,
                op: Op::Mul,
                lhs: Box::new(v),
                rhs: Box::new(TypedExpr::Const { ty, value: eight }),
            }))
        }
        ast::BinSpec::Binary { bytes: None } => SpecWidth::Bytes(None),
        ast::BinSpec::Utf8 => SpecWidth::Utf8,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::Value;
    use crate::span::Span;
    use crate::type_def::TypeId;
    use crate::typed_ir::BindingId;
    use crate::typed_ir::rty::ResolvedPool;
    use crate::types::PrimIds;

    const INT: TypeId = TypeId(1);
    const FLOAT: TypeId = TypeId(2);
    const STRING: TypeId = TypeId(3);
    const ARRAY: TypeId = TypeId(4);
    const BINARY: TypeId = TypeId(5);
    const USER: TypeId = TypeId(9);

    /// A `PatCtx` over a bare pool: constructors come from a table and
    /// expressions elaborate to a marker `Nil`.
    struct Ctx {
        pool: ResolvedPool,
        names: Vec<String>,
        consts: Vec<Value>,
        binds: Vec<TypedBind>,
        scope: Vec<StrId>,
        ctors: HashMap<String, (u16, Vec<&'static str>, Vec<RTy>)>,
        int: RTy,
        binary: RTy,
    }

    impl Ctx {
        fn new() -> Ctx {
            let mut pool = ResolvedPool::new(PrimIds {
                int: INT,
                float: FLOAT,
                string: STRING,
                array: ARRAY,
            });
            let int = pool.mk_con(INT, StrId(0), &[]);
            let binary = pool.mk_con(BINARY, StrId(0), &[]);
            Ctx {
                pool,
                names: Vec::new(),
                consts: Vec::new(),
                binds: Vec::new(),
                scope: Vec::new(),
                ctors: HashMap::new(),
                int,
                binary,
            }
        }

        fn arr(&mut self, elem: RTy) -> RTy {
            self.pool.mk_con(ARRAY, StrId(0), &[elem])
        }

        fn user(&mut self) -> RTy {
            self.pool.mk_con(USER, StrId(0), &[])
        }

        /// Declare `name` as variant `idx`.
        fn ctor(&mut self, name: &str, idx: u16, labels: &[&'static str], tys: &[RTy]) {
            self.ctors
                .insert(name.to_string(), (idx, labels.to_vec(), tys.to_vec()));
        }
    }

    impl PatCtx for Ctx {
        fn intern(&mut self, name: &str) -> StrId {
            match self.names.iter().position(|n| n == name) {
                Some(i) => StrId(i as u32),
                None => {
                    self.names.push(name.to_string());
                    StrId((self.names.len() - 1) as u32)
                }
            }
        }

        fn bind(&mut self, name: StrId, ty: RTy) -> TypedBind {
            let b = TypedBind {
                id: BindingId(self.binds.len() as u32),
                name,
                ty,
                global: None,
            };
            self.binds.push(b);
            self.scope.push(name);
            b
        }

        fn scope_mark(&mut self) -> usize {
            self.scope.len()
        }

        fn scope_reset(&mut self, mark: usize) {
            self.scope.truncate(mark);
        }

        fn number_const(&mut self, lit: &ast::NumberLiteral) -> ConstId {
            let v: i64 = lit.value.parse().unwrap_or(0);
            self.int_const(v)
        }

        fn string_const(&mut self, s: &str) -> ConstId {
            self.consts.push(Value::nil());
            let _ = s;
            ConstId((self.consts.len() - 1) as u32)
        }

        fn int_const(&mut self, i: i64) -> ConstId {
            self.consts.push(Value::small_int(i));
            ConstId((self.consts.len() - 1) as u32)
        }

        fn binary_const(&mut self, bytes: Vec<u8>, bit_len: u64) -> ConstId {
            let _ = (bytes, bit_len);
            self.consts.push(Value::nil());
            ConstId((self.consts.len() - 1) as u32)
        }

        fn ty_int(&mut self) -> RTy {
            self.int
        }

        /// The stub has no module table, so a qualifier resolves like a bare
        /// name.
        fn resolve_ctor_pat_qualified(&mut self, _q: &str, name: &str, scrut: RTy) -> CtorPat {
            self.resolve_ctor_pat(name, scrut)
        }

        fn resolve_ctor_pat(&mut self, name: &str, _scrut: RTy) -> CtorPat {
            let Some((idx, labels, tys)) = self.ctors.get(name).cloned() else {
                elaborator_bug("unresolved constructor pattern", Span::DUMMY)
            };
            let type_name = self.intern("T");
            let variant_name = self.intern(name);
            let arity = Arity::of(&labels);
            let labels = labels.iter().map(|l| self.intern(l)).collect();
            CtorPat::from_parts(
                VariantRef {
                    type_id: USER,
                    variant_idx: idx,
                    type_name,
                    variant_name,
                },
                arity,
                labels,
                tys,
                Span::DUMMY,
            )
        }

        fn tuple_elem_ty(&mut self, t: RTy, i: usize) -> RTy {
            self.pool.tuple_elem(t, i).unwrap_or(self.int)
        }

        fn array_elem_ty(&mut self, t: RTy) -> RTy {
            self.pool.con_arg(t, 0).unwrap_or(self.int)
        }

        fn expr(&mut self, _e: &ast::Expression) -> TypedExpr {
            TypedExpr::Nil { ty: self.int }
        }
    }

    fn ident(name: &str) -> ast::Identifier {
        ast::Identifier {
            name: name.to_string(),
            span: Span::DUMMY,
        }
    }

    fn var(name: &str) -> ast::Pattern {
        ast::Pattern::Var { name: ident(name) }
    }

    fn wild() -> ast::Pattern {
        ast::Pattern::Wildcard { span: Span::DUMMY }
    }

    fn num(v: i64) -> ast::NumberLiteral {
        ast::NumberLiteral {
            value: v.to_string(),
            span: Span::DUMMY,
        }
    }

    fn ctor_pat(name: &str, args: Vec<ast::PatternArg>, rest: bool) -> ast::Pattern {
        ast::Pattern::Constructor {
            qualifier: None,
            name: ident(name),
            args,
            rest,
            span: Span::DUMMY,
        }
    }

    fn pos(p: ast::Pattern) -> ast::PatternArg {
        ast::PatternArg::Positional(p)
    }

    fn lab(l: &str, p: ast::Pattern) -> ast::PatternArg {
        ast::PatternArg::Labeled {
            label: ident(l),
            pattern: p,
        }
    }

    #[test]
    fn a_labelled_subset_fills_every_slot_in_declared_order() {
        let mut cx = Ctx::new();
        let int = cx.int;
        let user = cx.user();
        let arr = cx.arr(int);
        cx.ctor("W", 3, &["a", "b", "c"], &[int, user, arr]);

        let p = elaborate_pattern(
            &mut cx,
            &ctor_pat("W", vec![lab("c", var("z"))], false),
            user,
        );
        let TypedPat::Ctor {
            variant, fields, ..
        } = p
        else {
            panic!("expected Ctor")
        };
        assert_eq!(variant.variant_idx, 3);
        assert_eq!(fields.len(), 3);
        assert_eq!(fields[0], TypedPat::Wild { ty: int });
        assert_eq!(fields[1], TypedPat::Wild { ty: user });
        match &fields[2] {
            TypedPat::Bind(b) => assert_eq!(b.ty, arr),
            other => panic!("expected Bind, got {other:?}"),
        }
    }

    #[test]
    fn a_rest_pattern_wilds_the_unfilled_slots_with_their_field_types() {
        let mut cx = Ctx::new();
        let int = cx.int;
        let user = cx.user();
        cx.ctor("Cons", 0, &["head", "tail"], &[int, user]);

        let p = elaborate_pattern(&mut cx, &ctor_pat("Cons", vec![pos(var("h"))], true), user);
        let TypedPat::Ctor { fields, .. } = p else {
            panic!("expected Ctor")
        };
        assert_eq!(fields.len(), 2);
        // The unfilled `tail` slot still carries the heap type Perceus needs.
        assert_eq!(fields[1], TypedPat::Wild { ty: user });
        assert!(cx.pool.is_heap(fields[1].ty()));
    }

    #[test]
    fn positional_args_of_arity_zero_ctors_produce_no_fields() {
        let mut cx = Ctx::new();
        let user = cx.user();
        cx.ctor("Nil", 1, &[], &[]);
        let p = elaborate_pattern(&mut cx, &ctor_pat("Nil", vec![], false), user);
        assert_eq!(
            p,
            TypedPat::Ctor {
                ty: user,
                variant: VariantRef {
                    type_id: USER,
                    variant_idx: 1,
                    type_name: StrId(0),
                    variant_name: StrId(1),
                },
                fields: vec![],
            }
        );
    }

    /// A pattern naming no constructor is a compiler bug, not a catch-all arm:
    /// wildcarding it would match everything and leave the arm body's names
    /// bound to nothing.
    #[test]
    #[should_panic(expected = "unresolved constructor pattern")]
    fn an_unresolved_constructor_aborts_rather_than_wildcarding() {
        let mut cx = Ctx::new();
        let user = cx.user();
        let _ = elaborate_pattern(&mut cx, &ctor_pat("Nope", vec![], false), user);
    }

    /// A `field_tys`/`labels` desync must abort. Guessing `Nil` loses
    /// Perceus's `Drop` for a heap field; guessing the scrutinee's type
    /// invents a `Dup`/`Drop` against an immediate.
    #[test]
    #[should_panic(expected = "constructor field-type desync")]
    fn a_field_type_desync_is_reported_not_guessed() {
        let mut cx = Ctx::new();
        let int = cx.int;
        let user = cx.user();
        // Two labels, one field type: the desync `CtorPat::from_parts` catches.
        cx.ctor("Bad", 0, &["a", "b"], &[int]);

        let _ = elaborate_pattern(
            &mut cx,
            &ctor_pat("Bad", vec![pos(var("x")), pos(var("y"))], false),
            user,
        );
    }

    /// An arg `slot_labeled` cannot place must abort, not be dropped:
    /// dropping it leaves its binds unminted and the arm matching more than
    /// the source wrote.
    #[test]
    #[should_panic(expected = "constructor pattern arg desync")]
    fn a_mis_slotted_pattern_arg_aborts_rather_than_dropping_it() {
        let mut cx = Ctx::new();
        let int = cx.int;
        let user = cx.user();
        cx.ctor("W", 0, &["a"], &[int]);
        let _ = elaborate_pattern(
            &mut cx,
            &ctor_pat("W", vec![lab("nope", var("x"))], false),
            user,
        );
    }

    /// So is an arg past the declared arity.
    #[test]
    #[should_panic(expected = "constructor pattern arg desync")]
    fn an_extra_positional_pattern_arg_aborts() {
        let mut cx = Ctx::new();
        let int = cx.int;
        let user = cx.user();
        cx.ctor("W", 0, &["a"], &[int]);
        let _ = elaborate_pattern(
            &mut cx,
            &ctor_pat("W", vec![pos(var("x")), pos(var("y"))], false),
            user,
        );
    }

    /// An element after `..rest` must abort, not be dropped: dropping it
    /// matches a shorter prefix than the source wrote.
    #[test]
    #[should_panic(expected = "array spread not last")]
    fn an_element_after_the_spread_aborts_rather_than_vanishing() {
        let mut cx = Ctx::new();
        let int = cx.int;
        let arr = cx.arr(int);
        let p = ast::Pattern::Array {
            elements: vec![
                ast::ArrayPatternElement::Pattern(var("x")),
                ast::ArrayPatternElement::Spread {
                    binding: Some(ident("rest")),
                    span: Span::DUMMY,
                },
                ast::ArrayPatternElement::Pattern(var("y")),
            ],
            span: Span::DUMMY,
        };
        let _ = elaborate_pattern(&mut cx, &p, arr);
    }

    /// `arity`, `labels` and `field_tys` all agree, or `from_parts` aborts.
    #[test]
    fn ctor_pat_pairs_one_field_type_with_each_label() {
        let mut cx = Ctx::new();
        let int = cx.int;
        let v = VariantRef {
            type_id: USER,
            variant_idx: 0,
            type_name: cx.intern("T"),
            variant_name: cx.intern("C"),
        };
        let a = cx.intern("a");
        let b = cx.intern("b");
        let ok = CtorPat::from_parts(v, Arity(2), vec![a, b], vec![int, int], Span::DUMMY);
        assert_eq!(ok.arity(), 2);
        assert_eq!(ok.field_tys().len(), ok.labels().len());
        assert_eq!(ok.labels(), [a, b]);

        let direct = CtorPat::new(v, vec![(a, int), (b, int)]);
        assert_eq!(direct, ok);
    }

    #[test]
    #[should_panic(expected = "constructor field-type desync")]
    fn ctor_pat_from_parts_aborts_on_a_short_field_type_list() {
        let mut cx = Ctx::new();
        let int = cx.int;
        let v = VariantRef {
            type_id: USER,
            variant_idx: 0,
            type_name: cx.intern("T"),
            variant_name: cx.intern("C"),
        };
        let a = cx.intern("a");
        let b = cx.intern("b");
        let _ = CtorPat::from_parts(v, Arity(2), vec![a, b], vec![int], Span::DUMMY);
    }

    #[test]
    #[should_panic(expected = "constructor field-type desync")]
    fn ctor_pat_from_parts_aborts_when_arity_disagrees() {
        let mut cx = Ctx::new();
        let int = cx.int;
        let v = VariantRef {
            type_id: USER,
            variant_idx: 0,
            type_name: cx.intern("T"),
            variant_name: cx.intern("C"),
        };
        let a = cx.intern("a");
        let b = cx.intern("b");
        let _ = CtorPat::from_parts(v, Arity(1), vec![a, b], vec![int, int], Span::DUMMY);
    }

    #[test]
    fn array_patterns_carry_their_element_type_and_rest() {
        let mut cx = Ctx::new();
        let int = cx.int;
        let user = cx.user();
        let arr = cx.arr(user);

        let ap = ast::Pattern::Array {
            elements: vec![
                ast::ArrayPatternElement::Pattern(var("x")),
                ast::ArrayPatternElement::Spread {
                    binding: Some(ident("rest")),
                    span: Span::DUMMY,
                },
            ],
            span: Span::DUMMY,
        };
        let p = elaborate_pattern(&mut cx, &ap, arr);
        let TypedPat::Array {
            ty,
            elem_ty,
            len,
            prefix,
            rest,
        } = p
        else {
            panic!("expected Array")
        };
        assert_eq!(ty, arr);
        assert_eq!(elem_ty, user);
        assert_ne!(elem_ty, int);
        assert_eq!(prefix.len(), 1);
        assert_eq!(cx.consts[len.0 as usize].as_int(), Some(1));
        assert_eq!(prefix[0].ty(), user);
        match rest {
            // The suffix is an array, not an element.
            PatRest::Bind(b) => assert_eq!(b.ty, arr),
            other => panic!("expected Bind, got {other:?}"),
        }
    }

    #[test]
    fn a_bare_spread_discards_and_no_spread_is_exact() {
        let mut cx = Ctx::new();
        let int = cx.int;
        let arr = cx.arr(int);
        let discard = ast::Pattern::Array {
            elements: vec![ast::ArrayPatternElement::Spread {
                binding: None,
                span: Span::DUMMY,
            }],
            span: Span::DUMMY,
        };
        let exact = ast::Pattern::Array {
            elements: vec![ast::ArrayPatternElement::Pattern(wild())],
            span: Span::DUMMY,
        };
        match elaborate_pattern(&mut cx, &discard, arr) {
            TypedPat::Array { rest, prefix, .. } => {
                assert_eq!(rest, PatRest::Discard);
                assert!(prefix.is_empty());
            }
            other => panic!("{other:?}"),
        }
        match elaborate_pattern(&mut cx, &exact, arr) {
            TypedPat::Array { rest, prefix, .. } => {
                assert_eq!(rest, PatRest::None);
                assert_eq!(prefix.len(), 1);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn literals_and_ranges_pool_const_ids() {
        let mut cx = Ctx::new();
        let int = cx.int;
        let lit = elaborate_pattern(
            &mut cx,
            &ast::Pattern::Literal(ast::PatternLiteral::Number(num(7))),
            int,
        );
        assert_eq!(
            lit,
            TypedPat::Lit {
                ty: int,
                value: ConstId(0)
            }
        );
        let rng = elaborate_pattern(
            &mut cx,
            &ast::Pattern::Range {
                start: num(1),
                end: num(9),
                span: Span::DUMMY,
            },
            int,
        );
        assert_eq!(
            rng,
            TypedPat::Range {
                ty: int,
                lo: ConstId(1),
                hi: ConstId(2)
            }
        );
        assert_eq!(cx.consts.len(), 3);
    }

    #[test]
    fn or_alternatives_bind_one_variable_per_name() {
        let mut cx = Ctx::new();
        let int = cx.int;
        let user = cx.user();
        cx.ctor("Cons", 0, &["head", "tail"], &[int, user]);
        cx.ctor("Wrap", 1, &["v"], &[int]);

        let or = ast::Pattern::Or {
            first: Box::new(ctor_pat("Cons", vec![pos(var("x")), pos(wild())], false)),
            rest: vec![ctor_pat("Wrap", vec![pos(var("x"))], false)],
            span: Span::DUMMY,
        };
        let p = elaborate_pattern(&mut cx, &or, user);
        let binds = pat_binds(&p);
        // One `x`, however many alternatives mention it.
        assert_eq!(binds.len(), 1);
        assert_eq!(binds[0].id, BindingId(0));
        assert_eq!(cx.binds.len(), 1);

        let TypedPat::Or { alts, .. } = &p else {
            panic!("expected Or")
        };
        let ids: Vec<_> = alts
            .iter()
            .map(|a| match a {
                TypedPat::Ctor { fields, .. } => match &fields[0] {
                    TypedPat::Bind(b) => b.id,
                    other => panic!("{other:?}"),
                },
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(ids, vec![BindingId(0), BindingId(0)]);
    }

    /// The check walk rejects `(x, x)`, so seeing one here is a compiler bug.
    /// Aliasing both positions to one binder would make `(x, x)` match any
    /// pair and bind `x` to an arbitrary side.
    #[test]
    #[should_panic(expected = "duplicate binding in pattern alternative")]
    fn a_repeated_name_within_one_alternative_aborts() {
        let mut cx = Ctx::new();
        let int = cx.int;
        let tup = cx.pool.mk_tuple(&[int, int]);
        let p = ast::Pattern::Tuple {
            elements: vec![var("x"), var("x")],
            span: Span::DUMMY,
        };
        let _ = elaborate_pattern(&mut cx, &p, tup);
    }

    /// Only the cross-alternative re-bind is sanctioned reuse; binding a name
    /// twice inside one alternative is still a repeat.
    #[test]
    #[should_panic(expected = "duplicate binding in pattern alternative")]
    fn a_repeat_inside_a_later_alternative_aborts() {
        let mut cx = Ctx::new();
        let int = cx.int;
        let user = cx.user();
        cx.ctor("Wrap", 0, &["v"], &[int]);
        cx.ctor("Pair", 1, &["a", "b"], &[int, int]);
        let or = ast::Pattern::Or {
            first: Box::new(ctor_pat("Wrap", vec![pos(var("x"))], false)),
            rest: vec![ctor_pat("Pair", vec![pos(var("x")), pos(var("x"))], false)],
            span: Span::DUMMY,
        };
        let _ = elaborate_pattern(&mut cx, &or, user);
    }

    /// `(A(x) | B(x), x)` repeats `x` within the one alternative the whole
    /// tuple is.
    #[test]
    #[should_panic(expected = "duplicate binding in pattern alternative")]
    fn a_name_bound_in_an_or_is_a_repeat_after_it() {
        let mut cx = Ctx::new();
        let int = cx.int;
        let user = cx.user();
        cx.ctor("A", 0, &["v"], &[int]);
        cx.ctor("B", 1, &["v"], &[int]);
        let tup_ty = cx.pool.mk_tuple(&[user, int]);
        let or = ast::Pattern::Or {
            first: Box::new(ctor_pat("A", vec![pos(var("x"))], false)),
            rest: vec![ctor_pat("B", vec![pos(var("x"))], false)],
            span: Span::DUMMY,
        };
        let p = ast::Pattern::Tuple {
            elements: vec![or, var("x")],
            span: Span::DUMMY,
        };
        let _ = elaborate_pattern(&mut cx, &p, tup_ty);
    }

    /// The check walk exempts `_` from the once-per-alternative rule, and the
    /// parser leaves a `.._` rest as a binding named `_`. Two of them in one
    /// alternative must elaborate as discards, not trip the duplicate abort.
    #[test]
    fn underscore_rests_are_discards_and_never_duplicates() {
        let mut cx = Ctx::new();
        let int = cx.int;
        let arr = cx.arr(int);
        let binary = cx.binary;
        let tup_ty = cx.pool.mk_tuple(&[arr, arr, binary]);
        let arr_rest = |bind: &str| ast::Pattern::Array {
            elements: vec![ast::ArrayPatternElement::Spread {
                binding: Some(ident(bind)),
                span: Span::DUMMY,
            }],
            span: Span::DUMMY,
        };
        let bin_rest = ast::Pattern::Binary {
            segments: vec![],
            rest: Some(ast::BinaryPatternRest {
                binding: Some(ident("_")),
                span: Span::DUMMY,
            }),
            span: Span::DUMMY,
        };
        let p = ast::Pattern::Tuple {
            elements: vec![arr_rest("_"), arr_rest("_"), bin_rest],
            span: Span::DUMMY,
        };
        let p = elaborate_pattern(&mut cx, &p, tup_ty);
        assert_eq!(cx.binds.len(), 0);
        let TypedPat::Tuple { elems, .. } = &p else {
            panic!("expected Tuple")
        };
        assert!(matches!(
            &elems[0],
            TypedPat::Array {
                rest: PatRest::Discard,
                ..
            }
        ));
        assert!(matches!(
            &elems[2],
            TypedPat::Bin {
                rest: PatRest::Discard,
                ..
            }
        ));
    }

    #[test]
    fn each_arm_opens_its_own_scope() {
        let mut cx = Ctx::new();
        let int = cx.int;
        let arms = vec![
            ast::MatchArm {
                pattern: var("a"),
                guard: None,
                body: ast::Expression::NumberLiteral(num(1)),
            },
            ast::MatchArm {
                pattern: var("a"),
                guard: Some(ast::Expression::NumberLiteral(num(2))),
                body: ast::Expression::NumberLiteral(num(3)),
            },
        ];
        let out = elaborate_arms(&mut cx, int, &arms);
        assert_eq!(out.len(), 2);
        assert!(out[0].guard.is_none());
        assert!(out[1].guard.is_some());
        // Distinct arms, distinct bindings: the reuse map is per-pattern.
        assert_eq!(cx.binds.len(), 2);
        assert_eq!(cx.binds[0].id, BindingId(0));
        assert_eq!(cx.binds[1].id, BindingId(1));
        assert!(cx.scope.is_empty());
    }

    fn seg(value: ast::Pattern, spec: ast::BinSpec) -> ast::BinSegmentPat {
        ast::BinSegmentPat {
            value,
            spec,
            span: Span::DUMMY,
        }
    }

    #[test]
    fn binary_segments_fold_width_and_unit_at_elaboration_time() {
        let mut cx = Ctx::new();
        let int = cx.int;
        let bin_t = cx.binary;

        let p = ast::Pattern::Binary {
            segments: vec![
                // `<<'hi'>>` — literal prefix, width known now.
                seg(
                    ast::Pattern::Literal(ast::PatternLiteral::String(ast::StringLiteral {
                        value: "hi".to_string(),
                        span: Span::DUMMY,
                    })),
                    ast::BinSpec::Utf8,
                ),
                // `n:8` — the Int default.
                seg(var("n"), ast::BinSpec::Int { bits: None }),
                // `m:bytes(2)` — the bytes-to-bits multiplier is folded in.
                seg(
                    var("m"),
                    ast::BinSpec::Binary {
                        bytes: Some(ast::Expression::NumberLiteral(num(2))),
                    },
                ),
                // `b:binary` — sizeless, consumes the remainder.
                seg(var("b"), ast::BinSpec::Binary { bytes: None }),
            ],
            rest: Some(ast::BinaryPatternRest {
                binding: Some(ident("tail")),
                span: Span::DUMMY,
            }),
            span: Span::DUMMY,
        };

        let TypedPat::Bin {
            ty,
            zero,
            segs,
            rest,
        } = elaborate_pattern(&mut cx, &p, bin_t)
        else {
            panic!("expected Bin")
        };
        assert_eq!(ty, bin_t);
        assert_eq!(segs.len(), 4);
        assert_eq!(cx.consts[zero.0 as usize].as_int(), Some(0));

        match &segs[0] {
            TypedBinPatSeg::Utf8Literal { bits, .. } => {
                assert_eq!(cx.consts[bits.0 as usize].as_int(), Some(16));
            }
            other => panic!("{other:?}"),
        }
        match &segs[1] {
            TypedBinPatSeg::Int { bits, value } => {
                let TypedExpr::Const {
                    ty: bits_ty,
                    value: c,
                } = bits
                else {
                    panic!("{bits:?}")
                };
                assert_eq!(*bits_ty, int);
                assert_eq!(cx.consts[c.0 as usize].as_int(), Some(8));
                assert!(matches!(value, TypedPat::Bind(_)));
            }
            other => panic!("{other:?}"),
        }
        match &segs[2] {
            TypedBinPatSeg::Binary {
                bits: Some(bits), ..
            } => match bits {
                TypedExpr::Binary { op, rhs, .. } => {
                    assert_eq!(*op, Op::Mul);
                    let TypedExpr::Const { value, .. } = rhs.as_ref() else {
                        panic!("{rhs:?}")
                    };
                    assert_eq!(cx.consts[value.0 as usize].as_int(), Some(8));
                }
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
        match &segs[3] {
            TypedBinPatSeg::Binary { bits, value } => {
                assert!(bits.is_none());
                assert_eq!(value.ty(), bin_t);
            }
            other => panic!("{other:?}"),
        }
        match rest {
            PatRest::Bind(b) => assert_eq!(b.ty, bin_t),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_utf8_codepoint_segment_matches_an_int() {
        let mut cx = Ctx::new();
        let int = cx.int;
        let bin_t = cx.binary;
        let p = ast::Pattern::Binary {
            segments: vec![seg(var("c"), ast::BinSpec::Utf8)],
            rest: None,
            span: Span::DUMMY,
        };
        let TypedPat::Bin { segs, rest, .. } = elaborate_pattern(&mut cx, &p, bin_t) else {
            panic!("expected Bin")
        };
        assert_eq!(rest, PatRest::None);
        match &segs[0] {
            TypedBinPatSeg::Utf8 { value } => assert_eq!(value.ty(), int),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn nested_tuple_fields_take_their_element_types() {
        let mut cx = Ctx::new();
        let int = cx.int;
        let user = cx.user();
        let tup = cx.pool.mk_tuple(&[int, user]);
        let p = ast::Pattern::Tuple {
            elements: vec![var("a"), var("b")],
            span: Span::DUMMY,
        };
        let TypedPat::Tuple { elems, .. } = elaborate_pattern(&mut cx, &p, tup) else {
            panic!("expected Tuple")
        };
        assert_eq!(elems[0].ty(), int);
        assert_eq!(elems[1].ty(), user);
        assert_eq!(pat_binds(&TypedPat::Tuple { ty: tup, elems }).len(), 2);
    }
}
