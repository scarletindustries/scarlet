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

pub mod elaborate;
pub mod elaborate_pat;
pub mod eta;
pub mod resolve;
pub mod rty;
pub(crate) use al_types::slots;
pub mod zonk;

pub use elaborate::{
    Elab, ElabCtx, OrShape, PreludeTys, WalkStep, elaborate_body, elaborate_toplevel,
    elaborator_bug,
};
pub use elaborate_pat::{CtorPat, PatCtx, elaborate_arms, elaborate_pattern};
pub use eta::{FnRTy, FnTable, eta_wrapper};
pub use resolve::{CallForm, Denotation, EtaTarget, ValueForm};
pub use rty::{Arity, RSlice, RTy, ResolvedNode, ResolvedPool};
pub use zonk::{Zonker, pool_for};

use crate::bytecode::{Op, Value};
use crate::core_ir::{ConstId, FuncIdx, VariantRef};
use crate::types::StrId;

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

/// A slot in the entry (module) frame: what `PushGlobal` addresses. A
/// module-scope name may be bound more than once (an import shadowed by a later
/// `let`), so each [`TypedBind`] carries the slot its own binding lands in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GlobalSlot(pub i32);

/// A slot in the *current* frame: what `PushLocal` addresses. A different index
/// space from [`GlobalSlot`] and [`CaptureIdx`], kept distinct so the three
/// cannot be swapped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FrameSlot(pub i32);

/// An index into the current closure's capture array: what `PushCapture`
/// addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CaptureIdx(pub i32);

/// A name bound to a value, with the type the checker gave it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TypedBind {
    pub id: BindingId,
    pub name: StrId,
    pub ty: RTy,
    /// `Some(slot)` when this binding is module-level and must land in that
    /// entry-frame slot, because fn bodies address it by `PushGlobal slot`.
    pub global: Option<GlobalSlot>,
}

/// Where a value reference reads from at runtime. Name resolution is finished:
/// there is no name here, only a load.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueRef {
    /// A [`TypedBind`] in the current function.
    Local(BindingId),
    /// ESCAPE HATCH: `PushLocal slot` for a raw frame slot the module walk
    /// assigned (a selective `import mod.{x}` binding). Anchored to no
    /// [`BindingId`], because import bindings are materialised by the module
    /// walk rather than by the elaborator.
    Slot(FrameSlot),
    /// `PushGlobal slot`. A top-level `fn` referenced as a *value* loads this
    /// way even inside itself; self-*calls* are [`TypedCallee::SelfRec`].
    Global(GlobalSlot),
    /// `PushCapture idx` — a value captured from the enclosing frame.
    Capture(CaptureIdx),
    /// `PushSelf` — the current closure itself.
    SelfClosure,
}

/// A resolved call target.
#[derive(Debug, Clone, PartialEq)]
pub enum TypedCallee {
    /// A top-level function, by index into [`TypedProgram::fns`].
    Known(FuncIdx),
    /// The function currently being lowered.
    SelfRec,
    /// A `@vm` builtin: the call *is* the opcode.
    Builtin(Op),
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
    /// `PushNil` — a block that ended in a statement, or an empty one.
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
    /// primitive (`AddInt` rather than `Add`). Never `&&`/`||`.
    Binary {
        ty: RTy,
        op: Op,
        lhs: Box<TypedExpr>,
        rhs: Box<TypedExpr>,
    },
    /// `!x`, `-x` — specialised the same way (`NegInt`/`NegFloat`/`Neg`).
    Unary {
        ty: RTy,
        op: Op,
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
    /// `recv.field`, with the field index the checker resolved.
    ///
    /// `checked` selects `GetField` over `GetFieldUnchecked`: a projection out
    /// of a `..base` spread must verify the tag, whereas a field access whose
    /// receiver type admits the field across every variant, and a destructure
    /// whose tag exhaustiveness has proven, need not.
    Field {
        ty: RTy,
        recv: Box<TypedExpr>,
        idx: u32,
        checked: bool,
    },
    /// `args` is exactly the variant's arity, in declared-field order. Labels
    /// are reordered and `..base` spreads expanded into [`TypedExpr::Field`]
    /// projections by the elaborator.
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
    /// constructor or builtin (`map(Some, xs)`) is this node with no captures,
    /// pointing at a [`TypedFn`] the elaborator already put in
    /// [`TypedProgram::fns`].
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
    /// The resolved type of this expression. Total by construction — this
    /// exhaustive match is the guard: a new arm cannot be added without one.
    /// Every arm is a field read, so this never recurses and never walks a
    /// `Let` spine.
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
    /// Each parameter carries the [`BindingId`] the elaborator minted for it,
    /// so a body reference names the id actually stored here rather than
    /// trusting a numbering convention.
    pub params: Vec<TypedBind>,
    pub ret: RTy,
    pub body: TypedExpr,
    /// Number of [`BindingId`]s minted in this function, params included, so a
    /// consumer can size a dense `BindingId → LocalId` map with a `Vec`.
    ///
    /// Minted **per function, by the elaborator**, and never by anything
    /// downstream: [`elaborate::Elab`] holds the counter, hands out
    /// `BindingId(0..n)` in the order the body binds them (params first), and
    /// writes the final `n` here.
    ///
    /// The counter is not global because nothing wants it to be: a `BindingId`
    /// is only ever read against the `TypedFn` that minted it (`lower` builds
    /// one `Vec<LocalId>` of length `binds` per function), and a per-function
    /// dense range is what makes that `Vec` an index rather than a map. A
    /// nested closure is a *separate* [`TypedFn`] in [`TypedProgram::fns`] with
    /// its own range starting at zero; it reaches an enclosing binding through
    /// [`ValueRef::Capture`], never through the enclosing function's ids.
    pub binds: u32,
}

/// A whole module, typechecked. The only input `lower` takes.
///
/// `lower(p: &TypedProgram) -> CoreProgram` needs nothing else: no engine to
/// resolve a type against, no `Program` to append to, no side table to miss.
///
/// # Ownership
///
/// A `TypedProgram` **owns** its two arenas — the type pool and the constant
/// pool — rather than borrowing the compiler's. Both are compile-local:
///
/// * [`Self::pool`] is created empty per compile and dies at the end of it.
///   The `CoreProgram` it typed does **not** — that lives on `Compiler::core`
///   and outlives the arena. So the `RTy`s a surviving `CoreFn::ret_ty` or
///   `CoreBind::ty` still carries are opaque handles into a pool that no
///   longer exists: nothing may re-interpret a lowered function's types after
///   emit, and there is no pool on the `Compiler` for a `Watermark` entry to
///   truncate against — which is why `Compiler::reset_to` must `clear()`
///   `core.fns` outright rather than truncate it.
/// * [`Self::consts`] is the compiler's constant pool, moved in. See its doc:
///   `ConstId`s are stable through `lower`, so it moves back out again.
///
/// Neither is borrowed, because the borrow would have to be `&mut`: the
/// elaborator interns into both while it walks, and it also holds `&mut` on the
/// inference engine it is reading types out of. Owning them lets the elaborator
/// hand the finished program over by value and lets `lower` take `&self`.
#[derive(Debug, Clone)]
pub struct TypedProgram {
    /// Top-level functions, nested closures, and eta-wrappers, indexed by the
    /// [`FuncIdx`] every [`TypedCallee::Known`] and [`TypedExpr::Closure`]
    /// carries.
    pub fns: Vec<TypedFn>,
    /// The module's initialiser — top-level declarations in dependency order
    /// followed by its statements. `params` is empty.
    pub toplevel: TypedFn,
    /// The program's constant pool: **the compiler's own pool**, not a copy of
    /// it and not a second pool merged at emit.
    ///
    /// A [`ConstId`] is a `PushConst` operand. There is exactly one numbering,
    /// established when the elaborator pools a literal
    /// ([`elaborate::ElabCtx::add_const`] and friends, which are the compiler's
    /// `add_constant`), and it survives untouched all the way to the VM:
    ///
    /// 1. the elaborator pools into `Program::constants` and moves it here;
    /// 2. `lower` copies it **verbatim** — it neither appends nor reorders, so
    ///    `CoreProgram::consts` is this vector;
    /// 3. the compiler adopts `CoreProgram::consts` back into
    ///    `Program::constants` wholesale — an assignment, not a merge, and the
    ///    `Value::to_bits` dedup map stays valid because nothing moved.
    ///
    /// Step 2 is what makes `lower` `&mut`-free: interning is a mutation, so
    /// every constant a lowered body pushes must already be a [`ConstId`] on
    /// the node being lowered. The ones with no source literal behind them are
    /// pooled by the elaborator all the same and carried on the node that needs
    /// them — `TypedPat::Array`'s `len`, `TypedPat::Bin`'s `zero`, and
    /// `TypedBinPatSeg::Utf8Literal`'s `bits`.
    ///
    /// A separate pool merged at emit would have to renumber, and `ConstId`s
    /// are already baked into `TypedExpr::Const`, `TypedPat::Lit`,
    /// `TypedBinPatSeg::Utf8Literal` and the eta-wrappers' construct headers,
    /// whose pool order `emit` observes. `typed_program_consts_are_stable_ids`
    /// pins the identity that makes step 3 sound.
    pub consts: Vec<Value>,
    /// The arena every [`RTy`] in the program indexes. Owned, append-only
    /// during elaboration, immutable afterwards.
    ///
    /// `lower` never reads it — it moves `RTy`s around opaquely and gets the
    /// handful it must *name* from [`Self::temps`], which is why `lower_fn`
    /// takes no pool. `perceus` reads it (`is_heap`) by shared
    /// borrow of `program.pool`, and `emit` erases types entirely.
    pub pool: ResolvedPool,
    /// Types for the locals `lower` mints that no source expression names.
    pub temps: TempTys,
}

/// The handful of [`RTy`]s `lower` needs for the temporaries it introduces —
/// an array pattern's length check, a `<<>>` walk's bit cursor, an
/// interpolation's stringified parts.
///
/// Those locals have no [`TypedExpr`] to read a type off, and the pool is
/// immutable by the time `lower` runs, so the elaborator interns them once and
/// hands them over. This is what lets `lower` take `&TypedProgram` rather than
/// `&mut ResolvedPool`: it never needs to name a type the elaborator did not
/// already resolve.
///
/// Only `perceus` reads these (via `is_heap`); `emit` erases types.
/// They must therefore be the *real* prelude nodes — `Bool` is a nominal
/// non-primitive and so is heap-shaped, exactly as the inference engine
/// reported it before this arena existed.
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
    /// elaborated.
    ///
    /// Each comes from the enclosing compilation's *real* prelude type through
    /// the same [`PreludeTys::resolve_rty`] bridge every other node crosses —
    /// nothing here is a synthetic node invented to stand in for one. That
    /// matters because `perceus` reads these: `Bool` and `Binary` are nominal
    /// non-primitives (not in `PrimIds`), so [`ResolvedPool::is_heap`] answers
    /// `true` for them, exactly as `InferEngine`-era `is_heap` did.
    ///
    /// `int_pair` has no prelude `Ty` to resolve — it is `Op::BinReadUtf8`'s
    /// result, a shape the VM has but the source language never spells — so it
    /// is built structurally from the `Int` node just resolved.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core_ir::lower;
    use crate::type_def::TypeId;
    use crate::types::{InferEngine, NullaryPrim, Prim, PrimIds, StrId, Ty, new_engine};

    /// The prelude registers its type heads with 1-based ids and records only
    /// `Int`/`Float`/`String`/`Array` as [`PrimIds`]; `Bool` and `Binary` are
    /// ordinary nominal types whose ids fall outside that set.
    const BOOL_ID: TypeId = TypeId(5);
    const BINARY_ID: TypeId = TypeId(6);

    /// A [`PreludeTys`] over a real [`InferEngine`], seeded the way the
    /// prelude seeds one, so [`TempTys::intern`] can be driven for what it
    /// actually produces rather than for what a hand-built struct literal
    /// restates. `PreludeTys` is exactly the seam `intern` needs, so nothing
    /// here stubs a method it cannot reach.
    struct PreludeCtx {
        eng: InferEngine,
    }

    impl PreludeCtx {
        fn new() -> PreludeCtx {
            let mut eng = new_engine();
            eng.set_prim_ids(PrimIds {
                int: TypeId(1),
                float: TypeId(2),
                string: TypeId(3),
                array: TypeId(4),
            });
            PreludeCtx { eng }
        }
    }

    impl PreludeTys for PreludeCtx {
        fn resolve_rty(&mut self, pool: &mut ResolvedPool, t: Ty) -> RTy {
            Zonker::new(&self.eng).zonk_or_opaque(pool, t).0
        }
        fn ty_int(&mut self) -> Ty {
            let id = self.eng.prim_ids().int;
            self.eng.nullary_con(NullaryPrim::Int, id, "Int")
        }
        fn ty_string(&mut self) -> Ty {
            let id = self.eng.prim_ids().string;
            self.eng.nullary_con(NullaryPrim::String, id, "String")
        }
        fn ty_bool(&mut self) -> Ty {
            self.eng.nullary_con(NullaryPrim::Bool, BOOL_ID, "Bool")
        }
        fn ty_binary(&mut self) -> Ty {
            self.eng
                .nullary_con(NullaryPrim::Binary, BINARY_ID, "Binary")
        }
    }

    /// [`TempTys::intern`]'s contract, behaviourally: the nodes it hands
    /// `perceus` are the *real* prelude ones, so `Bool` and `Binary` — nominal
    /// types outside `PrimIds` — are heap-shaped and keep the `Drop` every
    /// `Op::Eq` result temp got before this arena existed. `int_pair` is the
    /// structural `(Int, Int)` `Op::BinReadUtf8` returns.
    ///
    /// Driven through the constructor rather than restated as a struct
    /// literal: a prelude that gave `Bool` a `PrimIds` id would silently stop
    /// `perceus` emitting those `Drop`s, and this is the assertion that fails.
    #[test]
    fn interned_temps_are_the_shapes_perceus_expects() {
        let mut ctx = PreludeCtx::new();
        let mut pool = ResolvedPool::new(ctx.eng.prim_ids());
        let t = TempTys::intern(&mut ctx, &mut pool);

        assert!(
            pool.is_heap(t.bool),
            "Bool is nominal: perceus must Drop it"
        );
        assert!(
            pool.is_heap(t.binary),
            "Binary is nominal: perceus must Drop it"
        );
        assert!(pool.is_heap(t.int_pair), "a tuple is always a heap cell");
        assert!(!pool.is_heap(t.int));
        assert!(!pool.is_heap(t.string));

        assert_eq!(pool.prim_of(t.int), Some(Prim::Int));
        assert_eq!(pool.prim_of(t.string), Some(Prim::String));
        assert!(matches!(pool.node(t.int_pair), ResolvedNode::Tuple { elems } if elems.len == 2));
    }

    /// A pool holding the five [`TempTys`] nodes with the shapes
    /// [`TempTys::intern`] gives them — `Bool`/`Binary` nominal, so outside
    /// `PrimIds` — but reachable without an `ElabCtx` to borrow. That the
    /// constructor really produces these shapes is pinned separately, by
    /// [`interned_temps_are_the_shapes_perceus_expects`].
    fn pool_and_temps() -> (ResolvedPool, TempTys) {
        let mut p = ResolvedPool::new(PrimIds::default());
        let prims = p.prims();
        let int = p.mk_con(prims.int, StrId::NONE, &[]);
        let boolean = p.mk_con(TypeId(100), StrId::NONE, &[]);
        let string = p.mk_con(prims.string, StrId::NONE, &[]);
        let binary = p.mk_con(TypeId(101), StrId::NONE, &[]);
        let int_pair = p.mk_tuple(&[int, int]);
        (
            p,
            TempTys {
                int,
                bool: boolean,
                string,
                binary,
                int_pair,
            },
        )
    }

    fn nullary(name: StrId, ret: RTy, body: TypedExpr) -> TypedFn {
        TypedFn {
            name,
            params: Vec::new(),
            ret,
            body,
            binds: 0,
        }
    }

    fn bits(vs: &[Value]) -> Vec<u64> {
        vs.iter().map(|v| v.to_bits()).collect()
    }

    /// DECISION (2), pinned behaviourally: `TypedProgram::consts` is not a
    /// second pool that emit merges — it is *the* pool, and lowering preserves
    /// every `ConstId` in it.
    #[test]
    fn typed_program_consts_are_stable_ids() {
        let (pool, temps) = pool_and_temps();
        let int = temps.int;
        let consts = vec![Value::small_int(7), Value::small_int(9)];
        let p = TypedProgram {
            fns: vec![nullary(
                StrId::NONE,
                int,
                TypedExpr::Const {
                    ty: int,
                    value: ConstId(1),
                },
            )],
            toplevel: nullary(
                StrId::NONE,
                int,
                TypedExpr::Const {
                    ty: int,
                    value: ConstId(0),
                },
            ),
            consts: consts.clone(),
            pool,
            temps,
        };

        let out = lower::lower(&p);
        assert_eq!(
            out.consts.len(),
            consts.len(),
            "lower has no pool to intern into: it hands back what it was given"
        );
        assert_eq!(
            bits(&out.consts),
            bits(&consts),
            "every ConstId the elaborator minted must name the same Value after \
             lowering, or PushConst operands baked into TypedExpr::Const would \
             have to be renumbered"
        );
        assert_eq!(
            p.consts.get(ConstId(1).0 as usize).map(Value::to_bits),
            Some(out.consts[1].to_bits())
        );
    }

    /// The constants `lower` needs but no source literal spells (here an array
    /// pattern's length check) are pooled by the elaborator and carried on the
    /// node — `TypedPat::Array`'s `len`. So the pool cannot grow: `lower` has
    /// nothing to intern with, which is what lets it take `&TypedProgram`.
    #[test]
    fn lower_cannot_mint_a_constant_the_elaborator_did_not_pool() {
        let (pool, temps) = pool_and_temps();
        let int = temps.int;
        let array = {
            let mut pool = pool;
            let prims = pool.prims();
            let a = pool.mk_con(prims.array, StrId::NONE, &[int]);
            (pool, a)
        };
        let (pool, arr_ty) = array;

        // `match xs { [_, _] -> 1, _ -> 0 }` — the array arm needs the length
        // constant 2, which the elaborator pooled as `ConstId(0)`; the arm
        // bodies are `ConstId(1)` and `ConstId(2)`.
        let scrut = TypedExpr::Var {
            ty: arr_ty,
            place: ValueRef::Slot(FrameSlot(0)),
        };
        let arms = vec![
            TypedArm {
                pat: TypedPat::Array {
                    ty: arr_ty,
                    elem_ty: int,
                    len: ConstId(0),
                    prefix: vec![TypedPat::Wild { ty: int }, TypedPat::Wild { ty: int }],
                    rest: PatRest::None,
                },
                guard: None,
                body: TypedExpr::Const {
                    ty: int,
                    value: ConstId(1),
                },
            },
            TypedArm {
                pat: TypedPat::Wild { ty: arr_ty },
                guard: None,
                body: TypedExpr::Const {
                    ty: int,
                    value: ConstId(2),
                },
            },
        ];
        let consts = vec![
            Value::small_int(2),
            Value::small_int(1),
            Value::small_int(0),
        ];
        let p = TypedProgram {
            fns: Vec::new(),
            toplevel: nullary(
                StrId::NONE,
                int,
                TypedExpr::Match {
                    ty: int,
                    scrut: Box::new(scrut),
                    arms,
                },
            ),
            consts: consts.clone(),
            pool,
            temps,
        };

        let out = lower::lower(&p);
        assert_eq!(
            bits(&out.consts),
            bits(&consts),
            "the array arm's length check reads TypedPat::Array's pooled `len`; \
             lower neither appends to nor reorders the pool it was handed"
        );
    }
}
