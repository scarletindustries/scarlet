//! Pattern elaboration turns a [`CPat`] into a [`TypedPat`] and a [`CArm`]
//! into a [`TypedArm`].
//!
//! The check walk has already resolved every pattern. Constructors point at
//! their variants, with one sub-pattern per declared field, and names point at
//! their binders. What's left is the work that needs the module's final types,
//! and the interning `lower` can't do:
//!
//! * Every node's type is resolved into the pool, so `lower` never sees an
//!   unsolved type variable.
//! * Every `ConstId` `lower` emits is minted here, including ones with no
//!   source literal, like an array pattern's length or the starting bit
//!   cursor of a `<<>>` walk.
//!
//! ## Or-patterns bind once
//!
//! `Cons(x, _) | Wrap(x)` binds one `x`. The arm's guard and body are one
//! tree, so which [`BindingId`] a name means must not depend on which
//! alternative matched. The check walk gave the name one binder across the
//! alternatives, and [`PatCtx::bind`] gives a binder one binding.

use scarlet_ir::core_ir::ConstId;
use scarlet_ir::core_ir::PrimOp;
use scarlet_syntax::ast;
use scarlet_types::types::{StrId, Ty};

use super::checked::{BinderId, CArm, CBinPatSeg, CExpr, CPat, CPatKind, CRest, CWidth};
use super::rty::RTy;
use super::{PatRest, TypedArm, TypedBinPatSeg, TypedBind, TypedExpr, TypedPat, elaborator_bug};

/// The services pattern elaboration needs from the enclosing elaborator.
///
/// It is deliberately not an `InferEngine`. Every type that crosses here is an
/// [`RTy`], so a `fresh_var()` can't reach a `TypedPat`.
pub trait PatCtx {
    /// The bind for `binder`. It gets a new [`BindingId`] the first time, and
    /// the same one again when a later or-alternative mentions it.
    fn bind(&mut self, binder: BinderId, name: StrId, ty: RTy) -> TypedBind;

    fn number_const(&mut self, lit: &ast::NumberLiteral) -> ConstId;
    fn string_const(&mut self, s: &str) -> ConstId;
    fn int_const(&mut self, i: i64) -> ConstId;
    fn binary_const(&mut self, bytes: Vec<u8>, bit_len: u64) -> ConstId;

    fn ty_int(&mut self) -> RTy;

    /// Put a type the check walk inferred, which is now final, into the pool.
    fn resolve(&mut self, t: Ty) -> RTy;

    /// Elaborate a checked expression, such as a `<<v:size(n)>>` width, a
    /// guard or an arm body.
    fn expr(&mut self, e: &CExpr) -> TypedExpr;
}

/// Elaborate the arms of a `match`.
pub(crate) fn elaborate_arms<C: PatCtx>(cx: &mut C, arms: &[CArm]) -> Vec<TypedArm> {
    arms.iter()
        .map(|arm| {
            let pat = elaborate_pattern(cx, &arm.pattern);
            let guard = arm.guard.as_ref().map(|g| cx.expr(g));
            let body = cx.expr(&arm.body);
            TypedArm { pat, guard, body }
        })
        .collect()
}

/// Elaborate one pattern. A segment's size is elaborated where the segment
/// is, which is where the check walk checked it.
fn elaborate_pattern<C: PatCtx>(cx: &mut C, p: &CPat) -> TypedPat {
    let ty = cx.resolve(p.ty);
    match &p.kind {
        CPatKind::Wild => TypedPat::Wild { ty },
        CPatKind::Bind { name, binder } => TypedPat::Bind(cx.bind(*binder, *name, ty)),
        CPatKind::Number(n) => TypedPat::Lit {
            ty,
            value: cx.number_const(n),
        },
        CPatKind::String(s) => TypedPat::Lit {
            ty,
            value: cx.string_const(s),
        },
        CPatKind::Range { start, end } => {
            let lo = cx.number_const(start);
            let hi = cx.number_const(end);
            TypedPat::Range { ty, lo, hi }
        }
        CPatKind::Ctor { variant, fields } => TypedPat::Ctor {
            ty,
            variant: *variant,
            fields: fields.iter().map(|f| elaborate_pattern(cx, f)).collect(),
        },
        CPatKind::Tuple(elems) => TypedPat::Tuple {
            ty,
            elems: elems.iter().map(|e| elaborate_pattern(cx, e)).collect(),
        },
        CPatKind::Array { elem, prefix, rest } => {
            let elem_ty = cx.resolve(*elem);
            let prefix: Vec<TypedPat> = prefix.iter().map(|e| elaborate_pattern(cx, e)).collect();
            // The `..rest` binding takes the scrutinee's own array type,
            // because it is a suffix of the same array and not an element.
            let rest = rest_of(cx, rest, ty);
            // `lower` compares `ArrayLen(scrut)` against this and has no pool
            // of its own, so the constant is minted here.
            let len = cx.int_const(prefix.len() as i64);
            TypedPat::Array {
                ty,
                elem_ty,
                len,
                prefix,
                rest,
            }
        }
        CPatKind::Binary { segs, rest } => {
            let segs = segs.iter().map(|s| bin_seg(cx, s)).collect();
            let rest = rest_of(cx, rest, ty);
            // The bit cursor `lower` walks the segments with starts here.
            let zero = cx.int_const(0);
            TypedPat::Bin {
                ty,
                zero,
                segs,
                rest,
            }
        }
        CPatKind::Or(alts) => TypedPat::Or {
            ty,
            alts: alts.iter().map(|a| elaborate_pattern(cx, a)).collect(),
        },
        CPatKind::Error => elaborator_bug("a pattern the check walk rejected", p.span),
    }
}

fn rest_of<C: PatCtx>(cx: &mut C, rest: &CRest, ty: RTy) -> PatRest {
    match rest {
        CRest::None => PatRest::None,
        CRest::Discard => PatRest::Discard,
        CRest::Bind { name, binder } => PatRest::Bind(cx.bind(*binder, *name, ty)),
    }
}

fn bin_seg<C: PatCtx>(cx: &mut C, seg: &CBinPatSeg) -> TypedBinPatSeg {
    match seg {
        // `<<'lit'>>` matches the literal's UTF-8 bytes as a prefix, and its
        // width is already known.
        CBinPatSeg::Literal(s) => {
            let width = (s.len() as i64) * 8;
            let bytes = cx.binary_const(s.as_bytes().to_vec(), width as u64);
            let bits = cx.int_const(width);
            TypedBinPatSeg::Utf8Literal { bytes, bits }
        }
        CBinPatSeg::Value { width, value } => {
            let width = seg_bits(cx, width);
            let value = elaborate_pattern(cx, value);
            match width {
                // `BinReadUtf8` yields the codepoint as an `Int`.
                SpecWidth::Utf8 => TypedBinPatSeg::Utf8 { value },
                SpecWidth::Bytes(bits) => TypedBinPatSeg::Binary { bits, value },
                SpecWidth::Int(bits) => TypedBinPatSeg::Int { bits, value },
            }
        }
    }
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

/// The width a `<<..>>` segment's spec sets. Because this carries the kind, an
/// `Int` segment's width can't be missing, so consumers don't need to guard
/// against an `Int` segment with no width.
#[derive(Debug)]
pub enum SpecWidth {
    /// `:N` or `:size(..)`, or the default of 8 when no size is given.
    Int(TypedExpr),
    /// `:bytes(..)` scaled to bits. `None` is a `:binary` with no size, which
    /// takes the remainder.
    Bytes(Option<TypedExpr>),
    /// `:utf8`, whose width depends on the data and is decoded at runtime.
    Utf8,
}

/// The rule for a `<<..>>` segment's width in bits. Expression and pattern
/// elaboration share it so the two can't drift apart. `lower` reads each width
/// as a plain `Int` expression and never works out the default or the unit
/// again.
///
/// The `:bytes` scale multiplies with `PrimOp::IntMul`. Both operands are
/// always `Int`, because the width expression is checked as `Int` and the
/// scale is a generated `Int` constant. So the typed op is right even though
/// the checker never saw the node.
pub(crate) fn seg_bits<C: PatCtx>(cx: &mut C, width: &CWidth) -> SpecWidth {
    match width {
        CWidth::Int(Some(e)) => SpecWidth::Int(cx.expr(e)),
        CWidth::Int(None) => {
            let ty = cx.ty_int();
            let value = cx.int_const(8);
            SpecWidth::Int(TypedExpr::Const { ty, value })
        }
        CWidth::Bytes(Some(e)) => {
            let ty = cx.ty_int();
            let v = cx.expr(e);
            let eight = cx.int_const(8);
            SpecWidth::Bytes(Some(TypedExpr::Binary {
                ty,
                op: PrimOp::IntMul,
                lhs: Box::new(v),
                rhs: Box::new(TypedExpr::Const { ty, value: eight }),
            }))
        }
        CWidth::Bytes(None) => SpecWidth::Bytes(None),
        CWidth::Utf8 => SpecWidth::Utf8,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BindingId;
    use crate::checked::CKind;
    use crate::rty::ResolvedPool;
    use scarlet_ir::core_ir::{Const, VariantRef};
    use scarlet_syntax::span::Span;
    use scarlet_types::type_def::TypeId;
    use scarlet_types::types::PrimIds;
    use std::collections::HashMap;

    const INT: TypeId = TypeId(1);
    const FLOAT: TypeId = TypeId(2);
    const STRING: TypeId = TypeId(3);
    const ARRAY: TypeId = TypeId(4);
    const BINARY: TypeId = TypeId(5);
    const USER: TypeId = TypeId(9);

    /// A `PatCtx` over a bare pool. A test's checked types are stand-ins that
    /// each name one pool type, and every expression elaborates to a marker
    /// `Nil`.
    struct Ctx {
        pool: ResolvedPool,
        tys: Vec<RTy>,
        consts: Vec<Const>,
        binds: Vec<TypedBind>,
        bound: HashMap<BinderId, TypedBind>,
        next_binder: u32,
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
            let int = pool.mk_con(INT, &[]);
            let binary = pool.mk_con(BINARY, &[]);
            Ctx {
                pool,
                tys: Vec::new(),
                consts: Vec::new(),
                binds: Vec::new(),
                bound: HashMap::new(),
                next_binder: 0,
                int,
                binary,
            }
        }

        /// The checked type that resolves to `r`.
        fn ty(&mut self, r: RTy) -> Ty {
            self.tys.push(r);
            Ty((self.tys.len() - 1) as u32)
        }

        fn arr(&mut self, elem: RTy) -> RTy {
            self.pool.mk_con(ARRAY, &[elem])
        }

        fn user(&mut self) -> RTy {
            self.pool.mk_con(USER, &[])
        }

        fn pat(&mut self, ty: RTy, kind: CPatKind) -> CPat {
            CPat {
                ty: self.ty(ty),
                span: Span::DUMMY,
                kind,
            }
        }

        fn binder(&mut self) -> BinderId {
            self.next_binder += 1;
            BinderId(self.next_binder - 1)
        }

        fn var(&mut self, name: u32, ty: RTy) -> CPat {
            let binder = self.binder();
            self.pat(
                ty,
                CPatKind::Bind {
                    name: StrId(name),
                    binder,
                },
            )
        }
    }

    impl PatCtx for Ctx {
        fn bind(&mut self, binder: BinderId, name: StrId, ty: RTy) -> TypedBind {
            if let Some(b) = self.bound.get(&binder) {
                return *b;
            }
            let b = TypedBind {
                id: BindingId(self.binds.len() as u32),
                name,
                ty,
                global: None,
            };
            self.binds.push(b);
            self.bound.insert(binder, b);
            b
        }

        fn number_const(&mut self, lit: &ast::NumberLiteral) -> ConstId {
            let v: i64 = lit.value.parse().unwrap_or(0);
            self.int_const(v)
        }

        fn string_const(&mut self, s: &str) -> ConstId {
            self.consts.push(Const::String(s.to_string()));
            ConstId((self.consts.len() - 1) as u32)
        }

        fn int_const(&mut self, i: i64) -> ConstId {
            self.consts.push(Const::Int(i));
            ConstId((self.consts.len() - 1) as u32)
        }

        fn binary_const(&mut self, bytes: Vec<u8>, bit_len: u64) -> ConstId {
            self.consts.push(Const::Binary { bytes, bit_len });
            ConstId((self.consts.len() - 1) as u32)
        }

        fn ty_int(&mut self) -> RTy {
            self.int
        }

        fn resolve(&mut self, t: Ty) -> RTy {
            self.tys[t.0 as usize]
        }

        fn expr(&mut self, _e: &CExpr) -> TypedExpr {
            TypedExpr::Nil { ty: self.int }
        }
    }

    /// A checked number literal. The test context elaborates every expression
    /// to `Nil`, so its type is never read.
    fn checked_num(v: i64) -> CExpr {
        CExpr {
            ty: Ty(0),
            span: Span::DUMMY,
            kind: CKind::Number(num(v)),
        }
    }

    fn num(v: i64) -> ast::NumberLiteral {
        ast::NumberLiteral {
            value: v.to_string(),
            span: Span::DUMMY,
        }
    }

    fn variant(idx: u16) -> VariantRef {
        VariantRef {
            type_id: USER,
            variant_idx: idx,
        }
    }

    #[test]
    fn array_patterns_carry_their_element_type_and_rest() {
        let mut cx = Ctx::new();
        let int = cx.int;
        let user = cx.user();
        let arr = cx.arr(user);

        let x = cx.var(0, user);
        let binder = cx.binder();
        let elem = cx.ty(user);
        let ap = cx.pat(
            arr,
            CPatKind::Array {
                elem,
                prefix: vec![x],
                rest: CRest::Bind {
                    name: StrId(1),
                    binder,
                },
            },
        );
        let TypedPat::Array {
            ty,
            elem_ty,
            len,
            prefix,
            rest,
        } = elaborate_pattern(&mut cx, &ap)
        else {
            panic!("expected Array")
        };
        assert_eq!(ty, arr);
        assert_eq!(elem_ty, user);
        assert_ne!(elem_ty, int);
        assert_eq!(prefix.len(), 1);
        assert_eq!(cx.consts[len.0 as usize], Const::Int(1));
        assert_eq!(prefix[0].ty(), user);
        match rest {
            // The suffix is an array, not an element.
            PatRest::Bind(b) => assert_eq!(b.ty, arr),
            other => panic!("expected Bind, got {other:?}"),
        }
    }

    #[test]
    fn literals_and_ranges_pool_const_ids() {
        let mut cx = Ctx::new();
        let int = cx.int;
        let lit = cx.pat(int, CPatKind::Number(num(7)));
        assert_eq!(
            elaborate_pattern(&mut cx, &lit),
            TypedPat::Lit {
                ty: int,
                value: ConstId(0)
            }
        );
        let rng = cx.pat(
            int,
            CPatKind::Range {
                start: num(1),
                end: num(9),
            },
        );
        assert_eq!(
            elaborate_pattern(&mut cx, &rng),
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

        // `Cons(x, _) | Wrap(x)`: one binder for both `x`s.
        let x1 = cx.var(0, int);
        let shared = match x1.kind {
            CPatKind::Bind { binder, .. } => binder,
            _ => unreachable!(),
        };
        let x2 = cx.pat(
            int,
            CPatKind::Bind {
                name: StrId(0),
                binder: shared,
            },
        );
        let tail = cx.pat(user, CPatKind::Wild);
        let cons = cx.pat(
            user,
            CPatKind::Ctor {
                variant: variant(0),
                fields: vec![x1, tail],
            },
        );
        let wrap = cx.pat(
            user,
            CPatKind::Ctor {
                variant: variant(1),
                fields: vec![x2],
            },
        );
        let or = cx.pat(user, CPatKind::Or(vec![cons, wrap]));
        let p = elaborate_pattern(&mut cx, &or);
        let binds = pat_binds(&p);
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

    #[test]
    fn each_arm_binds_its_own_names() {
        let mut cx = Ctx::new();
        let int = cx.int;
        let a1 = cx.var(0, int);
        let a2 = cx.var(0, int);
        let arms = vec![
            CArm {
                pattern: a1,
                guard: None,
                body: checked_num(1),
            },
            CArm {
                pattern: a2,
                guard: Some(checked_num(2)),
                body: checked_num(3),
            },
        ];
        let out = elaborate_arms(&mut cx, &arms);
        assert_eq!(out.len(), 2);
        assert!(out[0].guard.is_none());
        assert!(out[1].guard.is_some());
        // Different arms have different binders, so they get different bindings.
        assert_eq!(cx.binds.len(), 2);
        assert_eq!(cx.binds[0].id, BindingId(0));
        assert_eq!(cx.binds[1].id, BindingId(1));
    }

    #[test]
    fn binary_segments_fold_width_and_unit_at_elaboration_time() {
        let mut cx = Ctx::new();
        let int = cx.int;
        let bin_t = cx.binary;

        let n = cx.var(0, int);
        let m = cx.var(1, bin_t);
        let b = cx.var(2, bin_t);
        let tail = cx.binder();
        let seg = |width, value| CBinPatSeg::Value {
            width,
            value: Box::new(value),
        };
        let p = cx.pat(
            bin_t,
            CPatKind::Binary {
                segs: vec![
                    // `<<'hi'>>` is a literal prefix whose width is already known.
                    CBinPatSeg::Literal("hi".to_string()),
                    // `n:8` is the Int default.
                    seg(CWidth::Int(None), n),
                    // `m:bytes(2)` has the bytes-to-bits multiplier folded in.
                    seg(CWidth::Bytes(Some(checked_num(2))), m),
                    // `b:binary` has no size, so it takes the remainder.
                    seg(CWidth::Bytes(None), b),
                ],
                rest: CRest::Bind {
                    name: StrId(3),
                    binder: tail,
                },
            },
        );

        let TypedPat::Bin {
            ty,
            zero,
            segs,
            rest,
        } = elaborate_pattern(&mut cx, &p)
        else {
            panic!("expected Bin")
        };
        assert_eq!(ty, bin_t);
        assert_eq!(segs.len(), 4);
        assert_eq!(cx.consts[zero.0 as usize], Const::Int(0));

        match &segs[0] {
            TypedBinPatSeg::Utf8Literal { bits, .. } => {
                assert_eq!(cx.consts[bits.0 as usize], Const::Int(16));
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
                assert_eq!(cx.consts[c.0 as usize], Const::Int(8));
                assert!(matches!(value, TypedPat::Bind(_)));
            }
            other => panic!("{other:?}"),
        }
        match &segs[2] {
            TypedBinPatSeg::Binary {
                bits: Some(bits), ..
            } => match bits {
                TypedExpr::Binary { op, rhs, .. } => {
                    assert_eq!(*op, PrimOp::IntMul);
                    let TypedExpr::Const { value, .. } = rhs.as_ref() else {
                        panic!("{rhs:?}")
                    };
                    assert_eq!(cx.consts[value.0 as usize], Const::Int(8));
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
    fn nested_tuple_fields_take_their_element_types() {
        let mut cx = Ctx::new();
        let int = cx.int;
        let user = cx.user();
        let tup = cx.pool.mk_tuple(&[int, user]);
        let a = cx.var(0, int);
        let b = cx.var(1, user);
        let p = cx.pat(tup, CPatKind::Tuple(vec![a, b]));
        let TypedPat::Tuple { elems, .. } = elaborate_pattern(&mut cx, &p) else {
            panic!("expected Tuple")
        };
        assert_eq!(elems[0].ty(), int);
        assert_eq!(elems[1].ty(), user);
        assert_eq!(pat_binds(&TypedPat::Tuple { ty: tup, elems }).len(), 2);
    }
}
