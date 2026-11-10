//! Typed IR is what lowering builds from the checker's typed tree, and what
//! `lower` turns into Core IR.
//!
//! Every decision the checker made is stored as a field on the node: the type,
//! the resolved field index, the resolved variant and the resolved call target.
//! So `lower` never resolves anything again and needs no side table.
//!
//! What's missing matters just as much. There is no `ErrorNode`, because a
//! `TypedProgram` is only built for a module with no diagnostics, so `lower`
//! never has to fail. There is no unsolved type variable (see [`rty`]) and no
//! `Span`.
//!
//! This is a tree, not a spine of `Let`s. Turning it into ANF (A-normal form,
//! where every intermediate value gets a name) is still `lower`'s job, and
//! `Match` patterns keep their nesting for `lower` to flatten into `CorePat`
//! heads (docs/core-ir-spec.md §IR).

pub use scarlet_ir::rty::{RSlice, RTy, ResolvedNode, ResolvedPool};

use scarlet_ir::core_ir::{ConstId, FuncIdx, PrimOp, VariantRef};
use scarlet_ir::intrinsic::Intrinsic;

/// A binding introduced by a `TypedFn`'s parameter list, a `let`, or a
/// pattern. The ids in a function are dense: `BindingId(i)` exists for every
/// `i < TypedFn::binds`. It is not a `core_ir::LocalId`: `lower` mints those in
/// ANF evaluation order and maps each `BindingId` onto one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BindingId(pub(crate) u32);

impl std::fmt::Display for BindingId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "${}", self.0)
    }
}

pub use scarlet_ir::core_ir::{CaptureIdx, GlobalSlot};

/// A name bound to a value, with the type the checker gave it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TypedBind {
    pub(crate) id: BindingId,
    /// Whether the source names it, as opposed to a temporary lowering made.
    /// A named `let` on a function's outermost spine keeps a local of its own.
    pub(crate) named: bool,
    pub(crate) ty: RTy,
    /// `Some(slot)` when this binding is module-level and must land in that
    /// entry-frame slot, because fn bodies read it from there.
    pub(crate) global: Option<GlobalSlot>,
}

/// Where a value reference reads from at runtime. Names are already resolved,
/// so this holds no name, only where to load from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueRef {
    /// A [`TypedBind`] in the current function.
    Local(BindingId),
    /// An entry-frame slot. A top-level `fn` referenced as a *value* loads this
    /// way even inside itself; self-*calls* are [`TypedCallee::SelfRec`].
    Global(GlobalSlot),
    /// A value captured from the enclosing frame.
    Capture(CaptureIdx),
    /// The current closure itself.
    SelfClosure,
}

/// A resolved call target.
#[derive(Debug, Clone, PartialEq)]
pub enum TypedCallee {
    /// A top-level function, by index into [`TypedProgram::fns`].
    Known(FuncIdx),
    /// The function currently being lowered.
    SelfRec,
    /// A `@vm` builtin: the call *is* the intrinsic.
    Builtin { intrinsic: Intrinsic },
    /// A closure value computed at runtime.
    Dynamic(Box<TypedExpr>),
}

/// One element of an array literal.
#[derive(Debug, Clone, PartialEq)]
pub enum TypedArrayElem {
    Elem(TypedExpr),
    Spread(TypedExpr),
}

/// One piece of a `"a${b}c"` interpolation. Literal pieces are already pooled.
#[derive(Debug, Clone, PartialEq)]
pub enum TypedInterpPart {
    Str(ConstId),
    Expr(TypedExpr),
}

/// One `<<..>>` segment in *value* position. Each yields a `Binary`.
#[derive(Debug, Clone, PartialEq)]
pub enum TypedBinSeg {
    /// `v:size` encodes an integer into `bits` bits. The elaborator has
    /// already folded the `unit` multiplier and the default width into `bits`.
    Int { value: TypedExpr, bits: TypedExpr },
    /// `v:binary` passes the value through, or takes its leading `bits` bits.
    Binary {
        value: TypedExpr,
        bits: Option<TypedExpr>,
    },
    /// `v:utf8` encodes a string.
    Utf8 { value: TypedExpr },
}

/// The `..` tail of an array or binary pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatRest {
    /// With no `..`, the length must match exactly.
    None,
    /// A `..` with no binding makes the length a lower bound.
    Discard,
    /// `..rest` binds the remainder.
    Bind(TypedBind),
}

/// One `<<..>>` segment in *pattern* position.
#[derive(Debug, Clone, PartialEq)]
pub enum TypedBinPatSeg {
    /// `<<'lit'>>` matches the literal's UTF-8 bytes as a prefix. `bits` is its
    /// width, pooled at elaboration time because `lower` has no pool.
    Utf8Literal { bytes: ConstId, bits: ConstId },
    /// `v:utf8` reads one variable-width codepoint.
    Utf8 { value: TypedPat },
    /// `v:size` reads `bits` bits as an integer.
    Int { bits: TypedExpr, value: TypedPat },
    /// `v:binary` reads `bits` bits as a binary, or the whole remainder when
    /// `bits` is `None`.
    Binary {
        bits: Option<TypedExpr>,
        value: TypedPat,
    },
}

/// A match pattern with every constructor and literal resolved. It keeps its
/// nesting, and `lower` flattens it.
#[derive(Debug, Clone, PartialEq)]
pub enum TypedPat {
    Wild {
        ty: RTy,
    },
    Bind(TypedBind),
    /// A number or string literal, already pooled.
    Lit {
        ty: RTy,
        value: ConstId,
    },
    /// `fields` has exactly the constructor's arity, in declared-field order.
    /// A slot no source argument filled is a [`TypedPat::Wild`].
    Ctor {
        ty: RTy,
        variant: VariantRef,
        fields: Vec<TypedPat>,
    },
    Tuple {
        ty: RTy,
        elems: Vec<TypedPat>,
    },
    Array {
        ty: RTy,
        /// The element type, projected out of `ty` by the elaborator so
        /// `lower` never asks the pool for `Array(T)`'s `T`.
        elem_ty: RTy,
        /// `prefix.len()`, pooled.
        len: ConstId,
        prefix: Vec<TypedPat>,
        rest: PatRest,
    },
    Bin {
        ty: RTy,
        /// The pooled `Int` `0`, where `lower`'s `<<>>` walk starts its bit
        /// cursor.
        zero: ConstId,
        segs: Vec<TypedBinPatSeg>,
        rest: PatRest,
    },
    Or {
        ty: RTy,
        alts: Vec<TypedPat>,
    },
    /// `lo..hi`, both already pooled as `Int` constants.
    Range {
        ty: RTy,
        lo: ConstId,
        hi: ConstId,
    },
}

impl TypedPat {
    pub(crate) fn ty(&self) -> RTy {
        match self {
            TypedPat::Wild { ty }
            | TypedPat::Lit { ty, .. }
            | TypedPat::Ctor { ty, .. }
            | TypedPat::Tuple { ty, .. }
            | TypedPat::Array { ty, .. }
            | TypedPat::Bin { ty, .. }
            | TypedPat::Or { ty, .. }
            | TypedPat::Range { ty, .. } => *ty,
            TypedPat::Bind(b) => b.ty,
        }
    }
}

/// One arm of a `match`. The checker has already proven the match exhaustive.
#[derive(Debug, Clone, PartialEq)]
pub struct TypedArm {
    pub(crate) pat: TypedPat,
    pub(crate) guard: Option<TypedExpr>,
    pub(crate) body: TypedExpr,
}

/// A typechecked expression. Every node carries its resolved type.
///
/// There is deliberately no `ErrorNode` and no unresolved name. There is no
/// `Or` expression either: the elaborator knows the `Option`/`Result` shape, so
/// it builds the `Match` instead.
#[derive(Debug, Clone, PartialEq)]
pub enum TypedExpr {
    /// A pooled literal: number, string, or binary.
    Const {
        ty: RTy,
        value: ConstId,
    },
    /// `Nil`, the value of an empty block or of one that ends in a statement.
    Nil {
        ty: RTy,
    },
    Var {
        ty: RTy,
        place: ValueRef,
    },
    /// `bind = init; body`. Blocks are right-nested `Let`s, so `ty` (always
    /// `body`'s type) is stored instead of found by recursion. That keeps
    /// [`TypedExpr::ty`] O(1) on a spine thousands of bindings long.
    Let {
        ty: RTy,
        bind: TypedBind,
        init: Box<TypedExpr>,
        body: Box<TypedExpr>,
    },
    /// `effect; body`, where `effect` is a non-tail expression run for its
    /// side effect.
    Seq {
        ty: RTy,
        effect: Box<TypedExpr>,
        body: Box<TypedExpr>,
    },
    /// A binary operator, already specialised against the operand's resolved
    /// primitive (`IntAdd` rather than `Add`). Never `&&`/`||`.
    Binary {
        ty: RTy,
        op: PrimOp,
        lhs: Box<TypedExpr>,
        rhs: Box<TypedExpr>,
    },
    /// `!x` or `-x`, specialised the same way (`IntNeg`/`FloatNeg`/`Neg`).
    Unary {
        ty: RTy,
        op: PrimOp,
        operand: Box<TypedExpr>,
    },
    /// `a && b`. This is control flow rather than an operator, because `b` is
    /// not evaluated when `a` is false.
    And {
        ty: RTy,
        lhs: Box<TypedExpr>,
        rhs: Box<TypedExpr>,
    },
    /// `a || b`.
    Or {
        ty: RTy,
        lhs: Box<TypedExpr>,
        rhs: Box<TypedExpr>,
    },
    Tuple {
        ty: RTy,
        elems: Vec<TypedExpr>,
    },
    TupleIndex {
        ty: RTy,
        recv: Box<TypedExpr>,
        idx: u32,
    },
    /// Field `idx` of `recv`, where every variant `recv` could be holds it.
    /// A `.field` whose variants hold it in different slots is a
    /// [`TypedExpr::Match`] instead, which reads each variant's own.
    Field {
        ty: RTy,
        recv: Box<TypedExpr>,
        idx: u32,
    },
    /// `args` is exactly the variant's arity, in declared-field order. The
    /// elaborator has reordered labels and expanded `..base` spreads into
    /// [`TypedExpr::Field`] projections.
    Ctor {
        ty: RTy,
        variant: VariantRef,
        args: Vec<TypedExpr>,
    },
    Array {
        ty: RTy,
        elems: Vec<TypedArrayElem>,
    },
    /// `recv[index]`. It yields `Option(elem)`, which is what lets `xs[i] or d`
    /// resolve.
    Index {
        ty: RTy,
        recv: Box<TypedExpr>,
        index: Box<TypedExpr>,
    },
    /// `recv[start..end]`.
    Slice {
        ty: RTy,
        recv: Box<TypedExpr>,
        start: Box<TypedExpr>,
        end: Box<TypedExpr>,
    },
    /// `start..end` as a value.
    Range {
        ty: RTy,
        start: Box<TypedExpr>,
        end: Box<TypedExpr>,
    },
    Interp {
        ty: RTy,
        parts: Vec<TypedInterpPart>,
    },
    /// `<<..>>`. It is never empty, because an empty binary literal elaborates
    /// to a [`TypedExpr::Const`].
    BinLit {
        ty: RTy,
        segs: Vec<TypedBinSeg>,
    },
    /// A closure over `func_idx` and the captured values. A constructor or
    /// builtin wrapped by eta expansion, as in `map(Some, xs)`, is this node
    /// with no captures.
    Closure {
        ty: RTy,
        func_idx: FuncIdx,
        captures: Vec<TypedExpr>,
    },
    Call {
        ty: RTy,
        callee: TypedCallee,
        args: Vec<TypedExpr>,
    },
    If {
        ty: RTy,
        cond: Box<TypedExpr>,
        then: Box<TypedExpr>,
        els: Box<TypedExpr>,
    },
    Match {
        ty: RTy,
        scrut: Box<TypedExpr>,
        arms: Vec<TypedArm>,
    },
}

impl TypedExpr {
    /// The resolved type of this expression. Every arm is a field read, so this
    /// never recurses and never walks a `Let` spine.
    pub(crate) fn ty(&self) -> RTy {
        match self {
            TypedExpr::Const { ty, .. }
            | TypedExpr::Nil { ty }
            | TypedExpr::Var { ty, .. }
            | TypedExpr::Let { ty, .. }
            | TypedExpr::Seq { ty, .. }
            | TypedExpr::Binary { ty, .. }
            | TypedExpr::Unary { ty, .. }
            | TypedExpr::And { ty, .. }
            | TypedExpr::Or { ty, .. }
            | TypedExpr::Tuple { ty, .. }
            | TypedExpr::TupleIndex { ty, .. }
            | TypedExpr::Field { ty, .. }
            | TypedExpr::Ctor { ty, .. }
            | TypedExpr::Array { ty, .. }
            | TypedExpr::Index { ty, .. }
            | TypedExpr::Slice { ty, .. }
            | TypedExpr::Range { ty, .. }
            | TypedExpr::Interp { ty, .. }
            | TypedExpr::BinLit { ty, .. }
            | TypedExpr::Closure { ty, .. }
            | TypedExpr::Call { ty, .. }
            | TypedExpr::If { ty, .. }
            | TypedExpr::Match { ty, .. } => *ty,
        }
    }
}

/// One typechecked function.
///
/// Nested closures and eta wrappers are ordinary entries in
/// [`TypedProgram::fns`], so a `TypedFn` never contains another.
#[derive(Debug, Clone, PartialEq)]
pub struct TypedFn {
    pub(crate) params: Vec<TypedBind>,
    pub(crate) ret: RTy,
    pub(crate) body: TypedExpr,
    /// How many [`BindingId`]s this function minted, params included, so a
    /// consumer can hold a dense `BindingId → LocalId` map in a `Vec`.
    ///
    /// Each function has its own counter. A nested closure is a separate
    /// [`TypedFn`] whose ids start again at zero, and it reaches an enclosing
    /// binding through [`ValueRef::Capture`].
    pub(crate) binds: u32,
}

/// The few [`RTy`]s `lower` needs for the temporaries it introduces, such as an
/// array pattern's length check, a `<<>>` walk's bit cursor, or an
/// interpolation's stringified parts.
///
/// Those locals have no [`TypedExpr`] to read a type from, and the pool can't
/// change by the time `lower` runs, so the elaborator interns them up front.
/// They must be the *real* prelude nodes, because `perceus` asks `is_heap`
/// about them, and `Bool` is a nominal type rather than a primitive, so it
/// counts as a heap type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TempTys {
    pub(crate) int: RTy,
    pub(crate) bool: RTy,
    pub(crate) string: RTy,
    pub(crate) binary: RTy,
    /// `(Int, Int)`, the `(codepoint, bits)` result of `PrimOp::BinaryReadUtf8`.
    pub(crate) int_pair: RTy,
}

/// Every [`TypedBind`] `p` introduces, in left-to-right order, each listed
/// once however many or-alternatives mention it.
pub(crate) fn pat_binds(p: &TypedPat) -> Vec<TypedBind> {
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
        TypedPat::Ctor { fields: ps, .. }
        | TypedPat::Tuple { elems: ps, .. }
        | TypedPat::Or { alts: ps, .. } => ps.iter().for_each(|q| collect_binds(q, out)),
        TypedPat::Array { prefix, rest, .. } => {
            prefix.iter().for_each(|q| collect_binds(q, out));
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
