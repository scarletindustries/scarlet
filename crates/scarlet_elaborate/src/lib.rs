//! Typed IR: the checker's output and `lower`'s only input.
//!
//! Every decision the checker made is a *field* on the node — the type, the
//! resolved field index, the resolved variant, the resolved call target — so
//! `lower` never re-resolves anything and needs no side table.
//!
//! What is absent matters as much: no `ErrorNode` (a `TypedProgram` is only
//! built for a diagnostics-clean module, so `lower` is total), no unsolved
//! type variable (see [`rty`]), and no `Span`.
//!
//! This is a tree, not a `Let`-spine: ANF is still `lower`'s job, and `Match`
//! patterns keep their nesting for `lower` to flatten into `CorePat` heads
//! (docs/core-ir-spec.md §IR).

pub mod binop;
pub mod checked;
pub mod elaborate;
pub mod elaborate_pat;
pub mod eta;
pub mod resolve;
pub mod rty;
pub use scarlet_types::slots;
pub mod zonk;

pub use checked::{BinderId, ClosureFact, NameAt, NameFact};
pub use elaborate::{
    ElabCtx, FieldAt, OrShape, PreludeTys, VariantLayout, elaborate_body, elaborate_toplevel,
    elaborator_bug,
};
pub use eta::FnTable;
pub use resolve::Denotation;
pub use rty::{Arity, RSlice, RTy, ResolvedNode, ResolvedPool};
pub use zonk::{Zonker, pool_for};

use scarlet_ir::core_ir::{ConstId, FuncIdx, PrimOp, VariantRef};
use scarlet_types::intrinsic::Intrinsic;
use scarlet_types::types::StrId;

/// A binding introduced by a `TypedFn`'s parameter list, a `let`, or a
/// pattern. Dense within its function: `BindingId(i)` for `i < TypedFn::binds`.
/// Distinct from `core_ir::LocalId` — `lower` mints those in ANF evaluation
/// order and maps this onto them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BindingId(pub u32);

impl std::fmt::Display for BindingId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "${}", self.0)
    }
}

pub use scarlet_ir::core_ir::{CaptureIdx, GlobalSlot};

/// A name bound to a value, with the type the checker gave it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TypedBind {
    pub id: BindingId,
    pub name: StrId,
    pub ty: RTy,
    /// `Some(slot)` when this binding is module-level and must land in that
    /// entry-frame slot, because fn bodies read it from there.
    pub global: Option<GlobalSlot>,
}

/// Where a value reference reads from at runtime. Name resolution is finished:
/// there is no name here, only a load.
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
    /// `v:size` — encode an integer into `bits` bits. The elaborator has
    /// already folded the `unit` multiplier and the default width into `bits`.
    Int { value: TypedExpr, bits: TypedExpr },
    /// `v:binary` — pass through, or take the leading `bits` bits.
    Binary {
        value: TypedExpr,
        bits: Option<TypedExpr>,
    },
    /// `v:utf8` — encode a string.
    Utf8 { value: TypedExpr },
}

/// The `..` tail of an array or binary pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatRest {
    /// No `..`: the length must match exactly.
    None,
    /// `..` with no binding: the length is a lower bound.
    Discard,
    /// `..rest`: the remainder is bound.
    Bind(TypedBind),
}

/// One `<<..>>` segment in *pattern* position.
#[derive(Debug, Clone, PartialEq)]
pub enum TypedBinPatSeg {
    /// `<<'lit'>>` — match the literal's UTF-8 bytes as a prefix. `bits` is its
    /// width, pooled at elaboration time because `lower` has no pool.
    Utf8Literal { bytes: ConstId, bits: ConstId },
    /// `v:utf8` — one variable-width codepoint.
    Utf8 { value: TypedPat },
    /// `v:size` — read `bits` bits as an integer.
    Int { bits: TypedExpr, value: TypedPat },
    /// `v:binary` — read `bits` bits as a binary, or the whole remainder when
    /// `bits` is `None`.
    Binary {
        bits: Option<TypedExpr>,
        value: TypedPat,
    },
}

/// A match pattern with every constructor and literal resolved. Nesting is
/// preserved; flattening it is `lower`'s job.
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
        /// The `Int` `0`, pooled: `lower`'s `<<>>` walk starts its bit cursor
        /// there.
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
    pub fn ty(&self) -> RTy {
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

/// One arm of a `match`. Exhaustiveness has already been proven.
#[derive(Debug, Clone, PartialEq)]
pub struct TypedArm {
    pub pat: TypedPat,
    pub guard: Option<TypedExpr>,
    pub body: TypedExpr,
}

/// A typechecked expression. Every node carries its resolved type.
///
/// There is deliberately no `ErrorNode`, no `Or`-expression (the checker knows
/// the `Option`/`Result` shape, so it emits the `Match`), and no unresolved
/// name.
#[derive(Debug, Clone, PartialEq)]
pub enum TypedExpr {
    /// A pooled literal: number, string, or binary.
    Const {
        ty: RTy,
        value: ConstId,
    },
    /// `Nil` — a block that ended in a statement, or an empty one.
    Nil {
        ty: RTy,
    },
    Var {
        ty: RTy,
        place: ValueRef,
    },
    /// `bind = init; body`. Blocks are right-nested `Let`s, so `ty` (always
    /// `body`'s) is stored rather than recursed for: [`TypedExpr::ty`] must be
    /// O(1) on a spine thousands of bindings long.
    Let {
        ty: RTy,
        bind: TypedBind,
        init: Box<TypedExpr>,
        body: Box<TypedExpr>,
    },
    /// `effect; body` — a non-tail expression evaluated for its effect.
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
    /// `!x`, `-x` — specialised the same way (`IntNeg`/`FloatNeg`/`Neg`).
    Unary {
        ty: RTy,
        op: PrimOp,
        operand: Box<TypedExpr>,
    },
    /// `a && b`. Control flow, not an operator: `b` is not evaluated when `a`
    /// is false.
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
    /// `recv[index]` — yields `Option(elem)`, which is what lets `xs[i] or d`
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
    /// `<<..>>`. Never empty — an empty binary literal elaborates to a
    /// [`TypedExpr::Const`].
    BinLit {
        ty: RTy,
        segs: Vec<TypedBinSeg>,
    },
    /// `MakeClosure func_idx` over the captured values. An eta-expanded
    /// constructor or builtin (`map(Some, xs)`) is this node with no captures.
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
    pub fn ty(&self) -> RTy {
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
/// Nested closures and eta-wrappers are ordinary entries in
/// [`TypedProgram::fns`]; a `TypedFn` never contains another.
#[derive(Debug, Clone, PartialEq)]
pub struct TypedFn {
    pub name: StrId,
    pub params: Vec<TypedBind>,
    pub ret: RTy,
    pub body: TypedExpr,
    /// Number of [`BindingId`]s minted in this function, params included, so a
    /// consumer can size a dense `BindingId → LocalId` map with a `Vec`.
    ///
    /// The counter is per function, not global: a nested closure is a separate
    /// [`TypedFn`] whose ids restart at zero and which reaches an enclosing
    /// binding through [`ValueRef::Capture`].
    pub binds: u32,
}

/// A whole module, typechecked. The only input `lower` takes.
///
/// It **owns** its type pool and constant pool rather than borrowing the
/// compiler's, so the elaborator can hand the finished program over by value
/// and `lower` can take `&self`.
///
/// [`Self::pool`] dies at the end of the compile but the `CoreProgram` it typed
/// does not, so an `RTy` on a surviving `CoreFn::ret_ty` names a pool that no
/// longer exists. Nothing may re-interpret a lowered function's types after
/// emit, and `Compiler::reset_to` must `clear()` `core.fns` outright rather
/// than truncate against a pool it does not have.
#[derive(Debug, Clone)]
pub struct TypedProgram {
    /// Top-level functions, nested closures, and eta-wrappers, indexed by the
    /// [`FuncIdx`] every [`TypedCallee::Known`] and [`TypedExpr::Closure`]
    /// carries.
    pub fns: Vec<TypedFn>,
    /// The module's initialiser — top-level declarations in dependency order
    /// followed by its statements. `params` is empty.
    pub toplevel: TypedFn,
    /// The arena every [`RTy`] in the program indexes. Append-only during
    /// elaboration, immutable afterwards. `lower` never reads it; `perceus`
    /// reads it for `is_heap`, and `emit` erases types entirely.
    pub pool: ResolvedPool,
    /// Types for the locals `lower` mints that no source expression names.
    pub temps: TempTys,
}

/// The handful of [`RTy`]s `lower` needs for the temporaries it introduces —
/// an array pattern's length check, a `<<>>` walk's bit cursor, an
/// interpolation's stringified parts.
///
/// Those locals have no [`TypedExpr`] to read a type off and the pool is
/// immutable by the time `lower` runs, so the elaborator interns them up front.
/// They must be the *real* prelude nodes: `perceus` asks `is_heap` about them,
/// and `Bool` is a nominal non-primitive and so heap-shaped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TempTys {
    pub int: RTy,
    pub bool: RTy,
    pub string: RTy,
    pub binary: RTy,
    /// `(Int, Int)` — `Op::BinReadUtf8`'s `(codepoint, bits)` result.
    pub int_pair: RTy,
}

impl TempTys {
    /// Intern the five nodes into `pool`, once per compile, before any body is
    /// elaborated. Each comes from the real prelude type through
    /// [`PreludeTys::resolve_rty`], never a synthetic stand-in, so `Bool` and
    /// `Binary` stay outside `PrimIds` and [`ResolvedPool::is_heap`] answers
    /// `true` for them. `int_pair` has no prelude `Ty` — the source language
    /// never spells `Op::BinReadUtf8`'s result — so it is built structurally.
    pub fn intern<C: PreludeTys>(ctx: &mut C, pool: &mut ResolvedPool) -> TempTys {
        let t = ctx.ty_int();
        let int = ctx.resolve_rty(pool, t);
        let t = ctx.ty_bool();
        let boolean = ctx.resolve_rty(pool, t);
        let t = ctx.ty_string();
        let string = ctx.resolve_rty(pool, t);
        let t = ctx.ty_binary();
        let binary = ctx.resolve_rty(pool, t);
        let int_pair = pool.mk_tuple(&[int, int]);
        TempTys {
            int,
            bool: boolean,
            string,
            binary,
            int_pair,
        }
    }
}
