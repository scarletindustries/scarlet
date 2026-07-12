//! Core IR: typed A-Normal Form sitting between the typechecked AST and the
//! bytecode emitter. Every subexpression is a `Let`-bound `LocalId`, so
//! last-use / drop / reuse are computable by a linear backward scan (Perceus,
//! ICFP'22 frame-limited) instead of the forward-emit Nop-hole hedging that
//! regressed bench_typed. All optimisation passes in
//! `docs/type-directed-memory-design.md` are Core→Core; type erasure happens
//! exactly once at Core→bytecode. See `docs/core-ir-spec.md`.

pub mod emit;
pub mod lower;
pub mod perceus;

use std::fmt;

use crate::bytecode::{HeapTag, Op, Value};
use crate::newtype_index;
use crate::type_def::TypeId;
use crate::typed_ir::{GlobalSlot, RTy};
use crate::types::StrId;

// ── Operand spaces ─────────────────────────────────────────────────────────
// The `u32`-shaped index spaces a bytecode operand can hold, and which the
// compiler must never confuse: a `ConstId` may not subscript the local table,
// and only a `CodeAddr` names an instruction. Each is a `crate::tivec::Idx`, so
// the `TiVec` it indexes rejects the others outright — the mismatch is a type
// error rather than a wrong-but-plausible instruction.
//
// `GlobalSlot` (the entry-frame stack space) lives in `typed_ir`, where it is
// minted; the frame-local slot space is `bytecode::compiler`'s. `FuncIdx` is
// still an alias — see its doc.

newtype_index!(
    /// Dense per-function local index. Minted by `lower` in evaluation order so
    /// a backward scan over the `Let` spine sees ids monotonically decrease.
    /// Distinct from the AST's `StrId` bindings and from bytecode stack slots —
    /// mapping to slots is `emit`'s job.
    pub struct LocalId("%")
);

newtype_index!(
    /// Index into `CoreProgram.consts`. Literals are lowered to constants once
    /// at `lower` time so `Atom` stays operand-only.
    pub struct ConstId("c")
);

/// Index into `CoreProgram.fns` / `Program.functions`. Matches the `i32`
/// immediate `Op::CallKnown` carries so `emit` is a straight copy.
///
/// Still an alias, not a [`newtype_index!`]: it is the one operand space whose
/// producers (`typed_ir::eta`, `typed_ir::resolve`, `bytecode::compiler`'s
/// `global_to_func` / `closure_info`) sit outside this module.
pub type FuncIdx = i32;

newtype_index!(
    /// A **function-relative** instruction offset: an index into the block one
    /// `emit` call produces. Jump operands are exactly this (the VM adds the
    /// frame's `Function.code_start` back), which is why the emitter never
    /// spells an absolute `program.code` address. `emit::relocate` is the one
    /// place a block's offsets are shifted onto an absolute stream, and it
    /// takes the base as a plain `i32` because that base belongs to the
    /// program, not to any block.
    pub struct CodeAddr("@")
);

impl CodeAddr {
    /// The jump-operand encoding: `emit` writes this into `Instruction.operand`
    /// and the VM reads it back relative to `Function.code_start`.
    #[inline]
    pub fn to_operand(self) -> i32 {
        self.0 as i32
    }

    /// The address of the following instruction — the `SwitchTag` jump table
    /// sits at `switch.next()`. Arithmetic on addresses stays inside the type.
    #[inline]
    pub fn next(self) -> CodeAddr {
        CodeAddr(self.0 + 1)
    }
}

/// How many arguments a function type, constructor, or eta wrapper takes.
///
/// Its own type because it is compared against things that look just like it:
/// `eta_wrapper` asserts a constructor's *declared* field count equals the
/// arity of its *instantiated* function type, and a payload of the wrong width
/// is a silently corrupt heap value, not a crash.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Arity(pub u16);

impl Arity {
    /// The arity of a parameter/field list. The one constructor that is not a
    /// literal, so a `len()` cast lives here rather than at each call site.
    #[inline]
    pub fn of<T>(items: &[T]) -> Self {
        Arity(u16::try_from(items.len()).expect("constructor arity exceeds u16"))
    }
}

impl fmt::Display for Arity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A typed local binding. Carries a [`RTy`] — an index into the
/// `ResolvedPool` the elaborator built, in which an unsolved inference
/// variable is unrepresentable. Perceus therefore cannot be handed a type that
/// answers `is_heap` `false` merely because inference lost it.
#[derive(Debug, Clone)]
pub struct CoreBind {
    pub id: LocalId,
    pub ty: RTy,
    /// `Some(slot)` when this bind is a module-toplevel `fn`/`const`/`let`/
    /// destructured name and must land in that entry-frame slot: the fn bodies
    /// the check walk already emitted address it as `PushGlobal <slot>`, so the
    /// entry frame's `StoreLocal` has to agree.
    ///
    /// Copied verbatim from [`crate::typed_ir::TypedBind::global`]. It lives on
    /// the bind rather than in a `CoreProgram`-level `Vec<(LocalId, i32)>`
    /// because every later pass rewrites binds: a parallel `LocalId`-keyed
    /// table desyncs the moment one renumbers or drops a bind, whereas a field
    /// travels with the binding it describes. `emit_toplevel`'s `preassigned`
    /// argument is derived from the body it is handed
    /// ([`CoreExpr::toplevel_globals`]), so no caller can pass a pinning that
    /// disagrees with the IR.
    pub global: Option<GlobalSlot>,
}

impl CoreBind {
    /// Fresh bind, pinned to no slot. `lower` mints every bind through this
    /// and pins [`Self::global`] afterwards on the module-toplevel decls whose
    /// [`crate::typed_ir::TypedBind::global`] named one.
    pub fn new(id: LocalId, ty: RTy) -> Self {
        CoreBind {
            id,
            ty,
            global: None,
        }
    }
}

/// Resolved constructor identity captured at lowering time so `emit` need not
/// re-consult the `TypeEnv`. `type_name`/`variant_name` feed
/// `enum_name_prefix_hash`; `type_id`/`variant_idx` feed the packed header
/// constant. Shape equality (`type_id`, `variant_idx`, arity) is what Perceus
/// pairs drops against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VariantRef {
    pub type_id: TypeId,
    pub variant_idx: u16,
    pub type_name: StrId,
    pub variant_name: StrId,
}

/// Heap-cell shape for Perceus reuse pairing: a `Drop` of one shape may only
/// hand its cell to a `Ctor` of the *same* shape (same header tag, same
/// payload word count) so `reuse_or_alloc` can overwrite in place without a
/// resize. Captured on the `Drop` node so pairing is a pure IR walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReuseShape {
    pub tag: HeapTag,
    pub words: u16,
}

impl ReuseShape {
    pub fn enum_(arity: usize) -> Self {
        ReuseShape {
            tag: HeapTag::Enum,
            words: u16::try_from(arity).expect("constructor arity exceeds u16"),
        }
    }
}

/// Call target after resolution. `Known` and `Self_` lower to the fused
/// `CallKnown`/`CallSelf` opcodes; `Local` is the dynamic closure path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Callee {
    Known(FuncIdx),
    Self_,
    Local(LocalId),
}

/// Right-hand side of a `Let`, or the payload of a `Tail`. Every compound
/// operand is a `LocalId` — nested evaluation has already been linearised into
/// the enclosing `Let` spine.
#[derive(Debug, Clone)]
pub enum Atom {
    Local(LocalId),
    Const(ConstId),
    Ctor {
        variant: VariantRef,
        fields: Vec<LocalId>,
        /// Set by `perceus`: the local whose dropped cell this constructor
        /// overwrites in place. `emit` lowers `Some(_)` to `Op::Reuse slot`
        /// followed by `MakeEnumPayload a=1`; `None` allocates fresh.
        reuse: Option<LocalId>,
    },
    /// A primitive VM operation. `op` is the bytecode `Op` directly so
    /// `emit(Let{rhs=PrimOp})` is `PushLocal args; op{operand=imm}; StoreLocal`.
    /// `imm` is the instruction's immediate operand — an index (`TupleIndex`,
    /// `GetField`, `PushGlobal`, `PushCapture`, `ElemAt`), an argc (`MakeArray`,
    /// `MakeTuple`, `StrConcatN`, `BinConcatN`, `Prepend`, `Append`), or 0 for
    /// ops that carry none. `lower` sets it; `emit` passes it through verbatim.
    PrimOp {
        op: Op,
        args: Vec<LocalId>,
        imm: i32,
    },
    /// `PushLocal captures; MakeClosure func_idx`. `func_idx` and the capture
    /// set are resolved at lowering time from the fused walk's `Function`
    /// entry — the nested body itself has already been lowered/emitted
    /// recursively via `compile_fn_body`, so `emit` need only reference it.
    Closure {
        func_idx: FuncIdx,
        captures: Vec<LocalId>,
    },
    Call {
        callee: Callee,
        args: Vec<LocalId>,
    },
}

impl Atom {
    /// A `PrimOp` with no immediate (the common case for arithmetic/compare).
    pub fn prim(op: Op, args: Vec<LocalId>) -> Self {
        Atom::PrimOp { op, args, imm: 0 }
    }

    /// The locals this atom reads, in push order — a `Call` yields its
    /// arguments and then a dynamic callee. `Ctor`'s `reuse` is *not* an
    /// operand: it names the slot the constructor overwrites, not a value
    /// pushed onto the stack.
    pub fn operands(&self) -> impl Iterator<Item = LocalId> + '_ {
        let (pushed, trailing): (&[LocalId], Option<LocalId>) = match self {
            Atom::Local(x) => (&[], Some(*x)),
            Atom::Const(_) => (&[], None),
            Atom::Ctor { fields, .. } => (fields, None),
            Atom::PrimOp { args, .. } => (args, None),
            Atom::Closure { captures, .. } => (captures, None),
            Atom::Call { callee, args } => match callee {
                Callee::Local(id) => (args, Some(*id)),
                Callee::Known(_) | Callee::Self_ => (args, None),
            },
        };
        pushed.iter().copied().chain(trailing)
    }

    pub fn for_each_operand(&self, f: impl FnMut(LocalId)) {
        self.operands().for_each(f);
    }
}

/// Lowered match-arm pattern. Nesting is flattened by `lower` into successive
/// `Match` nodes, so a `Ctor` arm binds its fields as fresh locals and the
/// arm body handles any deeper structure.
#[derive(Debug, Clone)]
pub enum CorePat {
    Wild,
    Bind(CoreBind),
    Lit(ConstId),
    Ctor {
        variant: VariantRef,
        fields: Vec<CoreBind>,
    },
}

impl CorePat {
    /// The locals this pattern introduces, in binding order.
    pub fn binds(&self) -> std::slice::Iter<'_, CoreBind> {
        match self {
            CorePat::Wild | CorePat::Lit(_) => <&[CoreBind]>::default().iter(),
            CorePat::Bind(b) => std::slice::from_ref(b).iter(),
            CorePat::Ctor { fields, .. } => fields.iter(),
        }
    }

    pub fn binds_mut(&mut self) -> std::slice::IterMut<'_, CoreBind> {
        match self {
            CorePat::Wild | CorePat::Lit(_) => <&mut [CoreBind]>::default().iter_mut(),
            CorePat::Bind(b) => std::slice::from_mut(b).iter_mut(),
            CorePat::Ctor { fields, .. } => fields.iter_mut(),
        }
    }
}

/// ANF expression tree. The spine is a right-nested chain of `Let`s
/// terminating in `Tail`; `Match`/`If` are the only join points.
#[derive(Debug, Clone)]
pub enum CoreExpr {
    Let {
        bind: CoreBind,
        rhs: Atom,
        body: Box<CoreExpr>,
    },
    /// `let bind = <join>; body` where `<join>` is itself a control-flow tree
    /// (an `If`/`Match`/short-circuit in operand position). Every `Tail` inside
    /// `join` is a *value*, not a return: `emit` lowers each to leave-on-stack
    /// then jump-to-merge, then `StoreLocal bind`. Kept distinct from `Let` so
    /// `rhs: Atom` stays operand-only (Perceus's linear scan depends on that).
    LetJoin {
        bind: CoreBind,
        join: Box<CoreExpr>,
        body: Box<CoreExpr>,
    },
    /// Inserted by `perceus` at the last use of `local`: releases the frame's
    /// reference and — when the cell is uniquely owned — parks the hollowed
    /// allocation in `local`'s slot for a paired `Ctor{reuse}` to overwrite.
    /// `shape` is `Some` when the allocation size is statically known (rhs was
    /// a `Ctor`, or an arm pattern proved the variant) and thus reusable;
    /// `None` for heap locals of unknown arity (e.g. a `Call` result of a
    /// multi-variant enum), which drop for RC correctness but never pair.
    /// Lowers to `Op::Drop slot` regardless.
    Drop {
        local: LocalId,
        shape: Option<ReuseShape>,
        body: Box<CoreExpr>,
    },
    Match {
        scrut: LocalId,
        arms: Vec<(CorePat, CoreExpr)>,
        ty: RTy,
    },
    If {
        cond: LocalId,
        then: Box<CoreExpr>,
        els: Box<CoreExpr>,
        ty: RTy,
    },
    /// Tail position: return value or tail call.
    Tail(Atom),
}

impl CoreExpr {
    /// The `(bind, entry-frame slot)` pairs pinned on this expression's
    /// outermost `Let`/`LetJoin` spine, in binding order.
    ///
    /// Meaningful on a module toplevel, where `lower` copied
    /// [`crate::typed_ir::TypedBind::global`] onto each decl's [`CoreBind`];
    /// module decls are spine bindings by construction, so the walk steps
    /// through the `Drop`s Perceus interleaves and stops at the first join
    /// point. `emit_toplevel`'s `preassigned` argument is derived from the
    /// body it is handed via this walk, so no caller can pass a pinning that
    /// disagrees with the IR.
    pub fn toplevel_globals(&self) -> Vec<(LocalId, GlobalSlot)> {
        let mut out = Vec::new();
        let mut cur = self;
        loop {
            match cur {
                CoreExpr::Let { bind, body, .. } | CoreExpr::LetJoin { bind, body, .. } => {
                    if let Some(slot) = bind.global {
                        out.push((bind.id, slot));
                    }
                    cur = body;
                }
                CoreExpr::Drop { body, .. } => cur = body,
                CoreExpr::Match { .. } | CoreExpr::If { .. } | CoreExpr::Tail(_) => return out,
            }
        }
    }
}

/// One lowered function.
#[derive(Debug, Clone)]
pub struct CoreFn {
    pub name: StrId,
    pub params: Vec<CoreBind>,
    pub body: CoreExpr,
    pub ret_ty: RTy,
}

/// Whole-module lowering: every function plus the module toplevel as its own
/// expression (module init). `consts` is shared across all `ConstId`s.
#[derive(Debug, Clone)]
pub struct CoreProgram {
    pub fns: Vec<CoreFn>,
    pub consts: Vec<Value>,
    pub toplevel: CoreExpr,
}

impl Default for CoreProgram {
    /// The empty program: no functions, a toplevel that returns nil. The nil
    /// lives in `consts` so the invariant every consumer leans on — the
    /// toplevel only references `ConstId`s below `consts.len()` — holds for
    /// the default exactly as it does for every lowered program.
    fn default() -> Self {
        CoreProgram {
            fns: Vec::new(),
            consts: vec![Value::nil()],
            toplevel: CoreExpr::Tail(Atom::Const(ConstId(0))),
        }
    }
}

// ── Display ────────────────────────────────────────────────────────────────
// Golden-test printer (`crates/al/tests/core_ir.rs`). Indentation encodes the
// `Let` spine; ids print as `%n`, constants as `cN`, so snapshots are stable
// across string-interner churn. An `RTy` prints as `:N` — its raw index into
// the `ResolvedPool`, which this module cannot resolve structurally (it holds
// no pool). That index shifts whenever the elaborator allocates a different
// number of nodes ahead of it, so the golden harness renumbers `:N` → `:tK`
// per snapshot before comparing (`normalize_tys`); the raw index stays here
// for debug printing, where it is the thing you actually want to see. Hence
// `Display for RTy` prints the bare number: the `:` sigil is this printer's.

impl fmt::Display for CoreBind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.id, self.ty)?;
        match self.global {
            Some(GlobalSlot(slot)) => write!(f, "@g{slot}"),
            None => Ok(()),
        }
    }
}

impl fmt::Display for Callee {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Callee::Known(i) => write!(f, "fn#{i}"),
            Callee::Self_ => f.write_str("self"),
            Callee::Local(l) => write!(f, "{l}"),
        }
    }
}

fn write_locals(f: &mut fmt::Formatter<'_>, xs: &[LocalId]) -> fmt::Result {
    for (i, x) in xs.iter().enumerate() {
        if i > 0 {
            f.write_str(", ")?;
        }
        write!(f, "{x}")?;
    }
    Ok(())
}

impl fmt::Display for Atom {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Atom::Local(l) => write!(f, "{l}"),
            Atom::Const(c) => write!(f, "{c}"),
            Atom::Ctor {
                variant,
                fields,
                reuse,
            } => {
                write!(f, "ctor {}.{}(", variant.type_id.0, variant.variant_idx)?;
                write_locals(f, fields)?;
                f.write_str(")")?;
                if let Some(r) = reuse {
                    write!(f, " reuse {r}")?;
                }
                Ok(())
            }
            Atom::PrimOp { op, args, imm } => {
                write!(f, "{op:?}")?;
                if *imm != 0 {
                    write!(f, "#{imm}")?;
                }
                f.write_str("(")?;
                write_locals(f, args)?;
                f.write_str(")")
            }
            Atom::Closure { func_idx, captures } => {
                write!(f, "closure fn#{func_idx}(")?;
                write_locals(f, captures)?;
                f.write_str(")")
            }
            Atom::Call { callee, args } => {
                write!(f, "call {callee}(")?;
                write_locals(f, args)?;
                f.write_str(")")
            }
        }
    }
}

impl fmt::Display for CorePat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CorePat::Wild => f.write_str("_"),
            CorePat::Bind(b) => write!(f, "{}", b.id),
            CorePat::Lit(c) => write!(f, "{c}"),
            CorePat::Ctor { variant, fields } => {
                write!(f, "{}.{}(", variant.type_id.0, variant.variant_idx)?;
                for (i, b) in fields.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{}", b.id)?;
                }
                f.write_str(")")
            }
        }
    }
}

struct Indented<'a>(&'a CoreExpr, usize);

impl fmt::Display for Indented<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Indented(e, depth) = *self;
        let pad = |f: &mut fmt::Formatter<'_>, d: usize| {
            for _ in 0..d {
                f.write_str("  ")?;
            }
            Ok(())
        };
        // Walk the Let-spine iteratively so deep chains do not recurse on the
        // Rust stack (a lowered function body is one long spine).
        let mut cur = e;
        loop {
            pad(f, depth)?;
            match cur {
                CoreExpr::Let { bind, rhs, body } => {
                    writeln!(f, "let {bind} = {rhs}")?;
                    cur = body;
                }
                CoreExpr::LetJoin { bind, join, body } => {
                    writeln!(f, "letj {bind} =")?;
                    Indented(join, depth + 1).fmt(f)?;
                    cur = body;
                }
                CoreExpr::Drop { local, shape, body } => {
                    match shape {
                        Some(s) => writeln!(f, "drop {local} [{:?}:{}]", s.tag, s.words)?,
                        None => writeln!(f, "drop {local}")?,
                    }
                    cur = body;
                }
                CoreExpr::Tail(a) => {
                    return writeln!(f, "ret {a}");
                }
                CoreExpr::If {
                    cond,
                    then,
                    els,
                    ty,
                } => {
                    writeln!(f, "if {cond} :{ty}")?;
                    Indented(then, depth + 1).fmt(f)?;
                    pad(f, depth)?;
                    writeln!(f, "else")?;
                    return Indented(els, depth + 1).fmt(f);
                }
                CoreExpr::Match { scrut, arms, ty } => {
                    writeln!(f, "match {scrut} :{ty}")?;
                    for (pat, body) in arms {
                        pad(f, depth + 1)?;
                        writeln!(f, "| {pat} ->")?;
                        Indented(body, depth + 2).fmt(f)?;
                    }
                    return Ok(());
                }
            }
        }
    }
}

impl fmt::Display for CoreExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Indented(self, 0).fmt(f)
    }
}

impl fmt::Display for CoreFn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "fn s{}(", self.name)?;
        for (i, p) in self.params.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{p}")?;
        }
        writeln!(f, ") -> :{}", self.ret_ty)?;
        Indented(&self.body, 1).fmt(f)
    }
}

impl fmt::Display for CoreProgram {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for func in &self.fns {
            writeln!(f, "{func}")?;
        }
        writeln!(f, "toplevel:")?;
        Indented(&self.toplevel, 1).fmt(f)
    }
}

/// Fixture builders shared by the `core_ir` test modules (`emit`, `lower`,
/// `perceus`, and this module's own tests).
#[cfg(test)]
pub(crate) mod testkit {
    use super::*;
    use crate::core_ir::emit::EmitCtx;
    use crate::types::NO_STR;

    pub fn bind(id: u32, ty: RTy) -> CoreBind {
        CoreBind::new(LocalId(id), ty)
    }

    pub fn local(id: u32) -> LocalId {
        LocalId(id)
    }

    pub fn vref(tid: i32, idx: u16) -> VariantRef {
        VariantRef {
            type_id: TypeId(tid),
            variant_idx: idx,
            type_name: NO_STR,
            variant_name: NO_STR,
        }
    }

    /// The single anonymous variant most reuse/drop tests scrutinise.
    pub fn variant() -> VariantRef {
        vref(0, 0)
    }

    pub fn ctor(fields: &[u32]) -> Atom {
        Atom::Ctor {
            variant: variant(),
            fields: fields.iter().copied().map(LocalId).collect(),
            reuse: None,
        }
    }

    pub fn func(params: Vec<CoreBind>, body: CoreExpr, ret_ty: RTy) -> CoreFn {
        CoreFn {
            name: NO_STR,
            params,
            body,
            ret_ty,
        }
    }

    /// Minimal [`EmitCtx`] double: ints pool in push order, every name
    /// resolves to `"T"`, and `variant_count` decides `SwitchTag` vs the
    /// `MatchEnum` ladder.
    pub struct Ctx {
        pub consts: Vec<i64>,
        pub variant_count: Option<u8>,
    }

    pub fn ctx(variant_count: Option<u8>) -> Ctx {
        Ctx {
            consts: vec![],
            variant_count,
        }
    }

    impl EmitCtx for Ctx {
        fn resolve_str(&self, _id: StrId) -> &str {
            "T"
        }
        fn intern_int(&mut self, i: i64) -> i32 {
            self.consts.push(i);
            self.consts.len() as i32 - 1
        }
        fn intern_str(&mut self, _s: &str) -> i32 {
            0
        }
        fn intern_labels(&mut self, _t: TypeId, _v: u16) -> i32 {
            0
        }
        fn switch_variant_count(&self, _t: TypeId) -> Option<u8> {
            self.variant_count
        }
        fn bool_variant(&self, _t: TypeId, _v: u16) -> Option<bool> {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testkit::{bind, vref};
    use super::*;

    #[test]
    fn atom_forms() {
        assert_eq!(Atom::Local(LocalId(3)).to_string(), "%3");
        assert_eq!(Atom::Const(ConstId(7)).to_string(), "c7");
        assert_eq!(
            Atom::prim(Op::AddInt, vec![LocalId(0), LocalId(1)]).to_string(),
            "AddInt(%0, %1)"
        );
        assert_eq!(
            Atom::PrimOp {
                op: Op::TupleIndex,
                args: vec![LocalId(0)],
                imm: 2,
            }
            .to_string(),
            "TupleIndex#2(%0)"
        );
        assert_eq!(
            Atom::Closure {
                func_idx: 5,
                captures: vec![LocalId(1)],
            }
            .to_string(),
            "closure fn#5(%1)"
        );
        assert_eq!(
            Atom::Ctor {
                variant: vref(7, 1),
                fields: vec![LocalId(2)],
                reuse: None,
            }
            .to_string(),
            "ctor 7.1(%2)"
        );
        assert_eq!(
            Atom::Ctor {
                variant: vref(7, 1),
                fields: vec![LocalId(2), LocalId(3)],
                reuse: Some(LocalId(9)),
            }
            .to_string(),
            "ctor 7.1(%2, %3) reuse %9"
        );
        assert_eq!(
            Atom::Call {
                callee: Callee::Known(9),
                args: vec![LocalId(0)],
            }
            .to_string(),
            "call fn#9(%0)"
        );
        assert_eq!(
            Atom::Call {
                callee: Callee::Self_,
                args: vec![],
            }
            .to_string(),
            "call self()"
        );
        assert_eq!(
            Atom::Call {
                callee: Callee::Local(LocalId(4)),
                args: vec![LocalId(5), LocalId(6)],
            }
            .to_string(),
            "call %4(%5, %6)"
        );
    }

    #[test]
    fn pat_forms() {
        assert_eq!(CorePat::Wild.to_string(), "_");
        assert_eq!(CorePat::Lit(ConstId(2)).to_string(), "c2");
        assert_eq!(CorePat::Bind(bind(3, RTy(0))).to_string(), "%3");
        assert_eq!(
            CorePat::Ctor {
                variant: vref(4, 0),
                fields: vec![bind(5, RTy(2)), bind(6, RTy(2))],
            }
            .to_string(),
            "4.0(%5, %6)"
        );
    }

    #[test]
    fn let_drop_spine_is_flat() {
        let e = CoreExpr::Let {
            bind: bind(2, RTy(10)),
            rhs: Atom::prim(Op::AddInt, vec![LocalId(0), LocalId(1)]),
            body: Box::new(CoreExpr::Drop {
                local: LocalId(0),
                shape: Some(ReuseShape::enum_(3)),
                body: Box::new(CoreExpr::Let {
                    bind: bind(3, RTy(11)),
                    rhs: Atom::Ctor {
                        variant: vref(7, 1),
                        fields: vec![LocalId(2)],
                        reuse: Some(LocalId(0)),
                    },
                    body: Box::new(CoreExpr::Tail(Atom::Local(LocalId(3)))),
                }),
            }),
        };
        assert_eq!(
            e.to_string(),
            "\
let %2:10 = AddInt(%0, %1)
drop %0 [Enum:3]
let %3:11 = ctor 7.1(%2) reuse %0
ret %3
"
        );
    }

    #[test]
    fn if_and_match_indent() {
        let m = CoreExpr::Match {
            scrut: LocalId(0),
            arms: vec![
                (
                    CorePat::Ctor {
                        variant: vref(4, 0),
                        fields: vec![bind(5, RTy(2))],
                    },
                    CoreExpr::Tail(Atom::Call {
                        callee: Callee::Self_,
                        args: vec![LocalId(5)],
                    }),
                ),
                (CorePat::Wild, CoreExpr::Tail(Atom::Const(ConstId(0)))),
            ],
            ty: RTy(9),
        };
        let e = CoreExpr::If {
            cond: LocalId(1),
            then: Box::new(m),
            els: Box::new(CoreExpr::Tail(Atom::Local(LocalId(0)))),
            ty: RTy(9),
        };
        assert_eq!(
            e.to_string(),
            "\
if %1 :9
  match %0 :9
    | 4.0(%5) ->
      ret call self(%5)
    | _ ->
      ret c0
else
  ret %0
"
        );
    }

    #[test]
    fn core_fn_header_and_body() {
        let f = CoreFn {
            name: 42,
            params: vec![bind(0, RTy(1)), bind(1, RTy(1))],
            body: CoreExpr::Tail(Atom::prim(Op::LtInt, vec![LocalId(0), LocalId(1)])),
            ret_ty: RTy(3),
        };
        assert_eq!(
            f.to_string(),
            "\
fn s42(%0:1, %1:1) -> :3
  ret LtInt(%0, %1)
"
        );
    }
}
