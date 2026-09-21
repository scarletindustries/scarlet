//! Core → CLIF: the native (Cranelift) backend beside [`emit`](super::emit).
//!
//! [`plan`] + [`compile`] lower the same post-perceus [`CoreFn`] that `emit`
//! turns into bytecode, producing a
//! [`NativeEntry`](crate::bytecode::NativeEntry) (`extern "C" fn(vmx) ->
//! NativeStatus`). The interpreter's frame layout is kept, so per-function
//! fallback, preemption and migration work unchanged.
//!
//! # Two phases
//!
//! An `RTy` indexes a per-body [`ResolvedPool`] that dies as soon as the body
//! is lowered, so everything type-directed must happen inside the compiler's
//! [`NativeHook`](crate::bytecode::NativeHook), where both `CoreFn` and the
//! pool are alive. Constants, the JIT module and finalize only exist after
//! the compile finishes.
//!
//! - [`plan`] (hook time): coverage gate plus the pool-derived facts, saved
//!   with a clone of the body into a pool-independent [`NativePlan`].
//! - [`compile`] (post-compile): resolves constants, re-derives the frame
//!   layout, builds the CLIF, defines it into the caller's [`Module`]. The
//!   caller finalizes and publishes into the program's `NativeTable`.
//!
//! # The frame layout is emit's
//!
//! A suspension resumes the frame *interpreted* at its stored bytecode ip, so
//! every live slot must hold what the interpreter's bytecode would hold at
//! that point. [`compile`] recovers emit's slot numbering and resume ips by
//! re-running `emit` with [`LayoutCtx`]. That run agrees with the real
//! emission instruction for instruction because the gate rejects every
//! construct whose emitted shape depends on `EmitCtx` beyond `bool_variant`
//! and `switch_variant_count`, both of which the plan captured. The recorded
//! [`FrameLayout::call_resume_ips`] line up for the same reason: both
//! backends meet call atoms in one order (spine order, `LetJoin` join before
//! body, `LetCont` body before cont, arms in order).
//!
//! # Coverage (stage A0)
//!
//! All or nothing: one uncovered node makes the whole function interpret. The
//! covered set is [`Gate`]'s arms.
//!
//! # Value representation and RC parity
//!
//! A local's canonical home is its frame slot, written where the
//! interpreter's `StoreLocal` runs. In registers it also keeps the boxed
//! word, plus a raw `i64` view where an Int op consumes it; proven-`Int`
//! results stay raw between ops and NaN-box only at slot writes and call
//! boundaries, spilling past the 47-bit range exactly as the interpreter
//! does. A slot owns one reference, so a consuming use of a slotted local
//! takes its own via an inline dup. A slotless local is a single-use operand
//! temp, so its one owned word transfers to its one consumer.
//!
//! RC sequences are type-directed through each local's [`Repr`]. Elision
//! never changes a count the interpreter would touch: a dynamic gate on a
//! proven immediate is dead code, and a strengthened gate reads the same rc
//! slot, so `FREED_OBJECTS` accounting sees identical traffic.
//!
//! A reusable `Drop` (`shape: Some`) mirrors `Op::Drop` exactly. A call
//! argument's last-use `Drop` is peeled to between the operand copies and the
//! call (emit's `peel_call_arg_drops`, mirrored) so the callee is sole owner
//! and reuse propagates down a recursive chain. One deliberate divergence,
//! observable only in allocation timing: a `shape: None` `Drop` releases a
//! uniquely-owned cell immediately where the interpreter hollows it and frees
//! at `Ret`. Sound because no `Reuse` ever pairs with a shapeless drop.
//!
//! # Runtime symbols
//!
//! Generated code reaches the runtime by NAME (`Linkage::Import`). The names
//! Runtime symbols are declared from the runtime's own signature table
//! (`scarlet_vm::vm::jit::RT_SIGS`), so no C signature is written in this
//! crate.
//! imports `vm::jit` declares.

use std::collections::{HashMap, HashSet};

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{
    self, AbiParam, InstBuilder, JumpTableData, MemFlagsData, StackSlotData, StackSlotKind, types,
};
use cranelift_codegen::isa::CallConv;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_module::{FuncId, Linkage, Module, ModuleError};

use super::emit::FrameLayout;
use super::native_frame::{
    self, FrameSlots, NATIVE_INT_BOX_SYMBOL, ValueBits, box_bool, box_int, unbox_int, value_bits,
};
use super::native_rc::{self, RcGate, emit_drop, emit_dup};
use super::{
    Atom, Callee, CoreBind, CoreExpr, CoreFn, CorePat, FuncIdx, Imm, JoinId, LocalId, SwitchCounts,
    VariantRef,
};
use crate::bytecode::value::{
    ENUM_FIELDS_WORD, NATIVE_HOLLOW_FOR_REUSE_SYMBOL, NATIVE_MORTAL_HEAP_BITS, NATIVE_PTR_MASK,
    NATIVE_RC_BYTE_OFFSET, NATIVE_RELEASE_AT_ZERO_SYMBOL, TUPLE_ELEMS_WORD,
};
use crate::bytecode::{
    CtorRef, NativeCtx, NativeStatus, Op, PreludeBindings, TypeRef, Value, is_native_bridge_op,
    is_native_park_op, is_native_try_op,
};
use crate::tivec::{Idx, TiVec};
use crate::type_def::TypeId;
use crate::typed_ir::{RTy, ResolvedNode, ResolvedPool};
use crate::types::Prim;

const SYM_RT_PREPARE_CALL: &str = "al_rt_prepare_call";
const SYM_RT_PREPARE_CALL_VALUE: &str = "al_rt_prepare_call_value";
const SYM_RT_PREPARE_TAIL: &str = "al_rt_prepare_tail";
const SYM_RT_PREPARE_TAIL_VALUE: &str = "al_rt_prepare_tail_value";
const SYM_RT_RET_TRANSFER: &str = "al_rt_ret_transfer";
const SYM_RT_CHECKPOINT: &str = "al_rt_checkpoint";
const SYM_RT_FRAME_BASE: &str = "al_rt_frame_base";
const SYM_RT_MAKE_CLOSURE: &str = "al_rt_make_closure";
const SYM_DIV_INT: &str = "al_shim_div_int";
const SYM_MOD_INT: &str = "al_shim_mod_int";
const SYM_SHIM_OP: &str = "al_shim_op";
const SYM_PARK_OP: &str = "al_shim_park_op";
const SYM_TRY_OP: &str = "al_shim_try_op";
const SYM_ENUM_ALLOC: &str = "al_shim_enum_alloc";
const SYM_MAKE_ARRAY: &str = "al_shim_make_array";
const SYM_MAKE_TUPLE: &str = "al_shim_make_tuple";
const SYM_SEQ_LEN: &str = "al_shim_seq_len";
const SYM_SEQ_APPEND: &str = "al_shim_seq_append";
const SYM_SEQ_PREPEND: &str = "al_shim_seq_prepend";
const SYM_BIN_BYTE_SIZE: &str = "al_shim_bin_byte_size";
const SYM_HTTP_PARSE_HEAD: &str = "al_shim_http_parse_head";
const SYM_HTTP_HEADERS_VALID: &str = "al_shim_http_headers_valid";
const SYM_HTTP_HEADER_HAS: &str = "al_shim_http_header_has";
const SYM_HTTP_SERIALIZE_HEAD: &str = "al_shim_http_serialize_head";
const SYM_HTTP_FRAMING: &str = "al_shim_http_framing";
const SYM_PUSH_GLOBAL: &str = "al_shim_push_global";
const SYM_PUSH_CAPTURE: &str = "al_shim_push_capture";
const SYM_PUSH_SELF: &str = "al_shim_push_self";

/// A gated body, captured pool-independently at hook time. Everything only
/// the per-body [`ResolvedPool`] could answer is resolved here.
pub struct NativePlan {
    pub func_idx: FuncIdx,
    fun: CoreFn,
    bools: BoolCtors,
    /// Per-local facts, captured from each binding's `RTy` while the pool is
    /// alive.
    locals: TiVec<LocalId, LocalPlan>,
    /// `EmitCtx::switch_variant_count`'s answers for every type this body
    /// matches over by constructor, captured through the [`SwitchCounts`]
    /// oracle while the type table was alive. Both the layout re-emission
    /// and codegen answer the `SwitchTag`-vs-ladder question from this map,
    /// so they cannot disagree with each other — or, since the oracle is the
    /// bytecode emitter's own rule, with the interpreter.
    switch_counts: HashMap<TypeId, u8>,
}

/// The representation class a local's `RTy` proves, deciding its unboxed
/// register views and how much RC machinery its dup/drop sites can skip.
/// Every arm is backed by a `value.rs` invariant; anything the types cannot
/// prove stays [`Repr::Dyn`], which is always correct.
/// The nominal facts a dedicated shim needs in order to skip a runtime check.
///
/// [`Repr`] cannot carry these: it classifies *representation* (Array, Binary
/// and Tuple are all `Heap`), while these name the *type*. Recorded per local
/// at plan time for the same reason `reprs` is — the `ResolvedPool` is gone by
/// the time the emitter runs.
///
/// A local with no proof still compiles: the emitter lowers that site through
/// the bridge, which re-checks the operand, instead of the unchecked shim.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
enum TyProof {
    /// Nothing proven; the site must use a checking lowering.
    #[default]
    None,
    /// A persistent array or a lazy range — the two shapes the seq shims are
    /// total over.
    Array,
    /// A `Binary` (nominal).
    Binary,
    /// A tuple of exactly this many elements.
    Tuple(u16),
}

/// What a [`NativePlan`] knows about one local.
#[derive(Clone, Copy, Default)]
struct LocalPlan {
    /// Its representation class.
    repr: Repr,
    /// The nominal proof the emitter uses to pick the unchecked lowering.
    proof: TyProof,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Repr {
    /// Proven `Int`: keeps a raw unboxed `i64` view between ops. RC stays
    /// dynamic, since a value past the 47-bit range spills to a heap
    /// `BigInt`.
    Int,
    /// Proven immediate: `Float` and `Bool`, never heap. Dup and drop compile
    /// to nothing. `Nil` deliberately does NOT qualify: a `Nil()` constructor
    /// allocates a mortal heap enum like any other nullary ctor (only the
    /// frozen prelude constant is an immediate), so a Nil-typed local can own
    /// a live refcount and eliding its gates double-frees.
    Immediate,
    /// Proven heap cell: strings, arrays, binaries, tuples, `fn` values.
    /// `SIGN|QNAN` are statically set, so RC gates reduce to the
    /// `VALUE_IMMORTAL` bit-0 test.
    Heap,
    /// No static proof: a rigid type variable, or a nominal type whose values
    /// may still be immediates (`Socket` and user enums are both
    /// `ResolvedNode::Con`). Full dynamic mortal-heap gate.
    #[default]
    Dyn,
}

/// The prelude identities [`classify`] needs beyond the pool's prims. `Bool`
/// and `Binary` are ordinary `Con`s in an `RTy`, so the pool cannot reveal
/// that one is an immediate and the other a heap cell.
#[derive(Clone, Copy)]
struct ReprTys {
    bool: TypeRef,
    binary: TypeRef,
}

impl ReprTys {
    fn of(prelude: &PreludeBindings) -> ReprTys {
        ReprTys {
            bool: prelude.bool(),
            binary: prelude.binary(),
        }
    }
}

/// The single place type facts become codegen decisions.
/// The [`TyProof`] for `t`, if any.
fn prove(pool: &ResolvedPool, tys: ReprTys, t: RTy) -> TyProof {
    match pool.node(t) {
        ResolvedNode::Con { id, .. } if id == pool.prims().array => TyProof::Array,
        ResolvedNode::Con { id, .. } if tys.binary.is(id) => TyProof::Binary,
        ResolvedNode::Tuple { .. } => {
            let n = pool.tuple_elems(t).len();
            u16::try_from(n).map_or(TyProof::None, TyProof::Tuple)
        }
        // Named, not wildcarded: a new node kind must decide what it proves.
        ResolvedNode::Con { .. } | ResolvedNode::Bound(_) | ResolvedNode::Fun { .. } => {
            TyProof::None
        }
    }
}

fn classify(pool: &ResolvedPool, tys: ReprTys, t: RTy) -> Repr {
    match pool.node(t) {
        ResolvedNode::Con { id, .. } => match pool.prims().prim_of(id) {
            Some(Prim::Int) => Repr::Int,
            Some(Prim::Float) => Repr::Immediate,
            Some(Prim::String) => Repr::Heap,
            None if tys.bool.is(id) => Repr::Immediate,
            None if id == pool.prims().array || tys.binary.is(id) => Repr::Heap,
            None => Repr::Dyn,
        },
        ResolvedNode::Tuple { .. } | ResolvedNode::Fun { .. } => Repr::Heap,
        ResolvedNode::Bound(_) => Repr::Dyn,
    }
}

/// `Bool`'s nominal identity, captured from the prelude at plan time. The
/// pool alone cannot tell `Bool` from a heap enum, but its heads are VM
/// immediates, so the gate, the layout re-emission and codegen all answer
/// polarity exactly as `EmitCtx::bool_variant` does.
#[derive(Clone, Copy)]
struct BoolCtors {
    bool: TypeRef,
    true_: CtorRef,
}

impl BoolCtors {
    fn of(prelude: &PreludeBindings) -> BoolCtors {
        BoolCtors {
            bool: prelude.bool(),
            true_: prelude.true_(),
        }
    }

    /// `EmitCtx::bool_variant`, verbatim (bridges.rs).
    fn bool_variant(&self, tid: TypeId, variant_idx: u16) -> Option<bool> {
        if !self.bool.is(tid) {
            return None;
        }
        Some(self.true_.is(tid, variant_idx))
    }

    fn polarity(&self, v: &VariantRef) -> Option<bool> {
        self.bool_variant(v.type_id, v.variant_idx)
    }
}

/// Gate `f` for native coverage and capture its type-directed facts. `None`
/// means the body interprets. The decision is total: one uncovered node
/// rejects the whole function.
pub fn plan(
    func_idx: FuncIdx,
    f: &CoreFn,
    pool: &ResolvedPool,
    prelude: &PreludeBindings,
    counts: SwitchCounts<'_>,
) -> NativePlan {
    let bools = BoolCtors::of(prelude);
    let mut walk = PlanWalk {
        pool,
        counts,
        bools,
        tys: ReprTys::of(prelude),
        locals: TiVec::new(),
        switch_counts: HashMap::new(),
    };
    for p in &f.params {
        walk.record(p);
    }
    walk.walk(&f.body);
    NativePlan {
        func_idx,
        fun: f.clone(),
        bools,
        locals: walk.locals,
        switch_counts: walk.switch_counts,
    }
}

impl NativePlan {
    fn local(&self, id: LocalId) -> LocalPlan {
        self.locals.get(id).copied().unwrap_or_default()
    }

    fn repr_of(&self, id: LocalId) -> Repr {
        self.local(id).repr
    }

    fn proof(&self, id: LocalId) -> TyProof {
        self.local(id).proof
    }

    fn is_int(&self, id: LocalId) -> bool {
        self.repr_of(id) == Repr::Int
    }

    /// Whether the unchecked seq shims may take `id` as a sequence.
    fn is_array(&self, id: LocalId) -> bool {
        self.proof(id) == TyProof::Array
    }

    /// Whether the unchecked binary shims may take `id` as a binary.
    fn is_binary(&self, id: LocalId) -> bool {
        self.proof(id) == TyProof::Binary
    }

    /// Whether `id` is a tuple with more than `i` elements, so the payload
    /// word can be read without a bounds check.
    fn tuple_has(&self, id: LocalId, i: usize) -> bool {
        matches!(self.proof(id), TyProof::Tuple(n) if usize::from(n) > i)
    }

    /// The RC gate a dup/drop of `id`'s current value needs. `None` means
    /// the type proves an immediate and the gate is elided entirely.
    fn rc_gate(&self, id: LocalId) -> Option<RcGate> {
        match self.repr_of(id) {
            Repr::Immediate => None,
            Repr::Heap => Some(RcGate::ProvenHeap),
            Repr::Int | Repr::Dyn => Some(RcGate::Dynamic),
        }
    }
}

/// What a covered `PrimOp` lowers to. The typed `*Int` opcodes carry their
/// own type proof; the polymorphic comparisons need the pool's, which the
/// gate demands.
#[derive(Clone, Copy, PartialEq, Eq)]
enum NOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Neg,
    Cmp(IntCC),
    Not,
}

/// The covered op set and its lowering, shared by the gate, the use scan and
/// codegen so they cannot disagree. The bool marks the polymorphic
/// comparisons, whose operands the gate must prove `Int`.
fn nop_of(op: Op) -> Option<(NOp, bool)> {
    let typed = |n| Some((n, false));
    let poly = |n| Some((n, true));
    match op {
        Op::AddInt => typed(NOp::Add),
        Op::SubInt => typed(NOp::Sub),
        Op::MulInt => typed(NOp::Mul),
        Op::DivInt => typed(NOp::Div),
        Op::ModInt => typed(NOp::Mod),
        Op::NegInt => typed(NOp::Neg),
        Op::LtInt => typed(NOp::Cmp(IntCC::SignedLessThan)),
        Op::GtInt => typed(NOp::Cmp(IntCC::SignedGreaterThan)),
        Op::LteInt => typed(NOp::Cmp(IntCC::SignedLessThanOrEqual)),
        Op::GteInt => typed(NOp::Cmp(IntCC::SignedGreaterThanOrEqual)),
        Op::EqInt => typed(NOp::Cmp(IntCC::Equal)),
        Op::NeqInt => typed(NOp::Cmp(IntCC::NotEqual)),
        Op::Lt => poly(NOp::Cmp(IntCC::SignedLessThan)),
        Op::Gt => poly(NOp::Cmp(IntCC::SignedGreaterThan)),
        Op::Lte => poly(NOp::Cmp(IntCC::SignedLessThanOrEqual)),
        Op::Gte => poly(NOp::Cmp(IntCC::SignedGreaterThanOrEqual)),
        Op::Eq => poly(NOp::Cmp(IntCC::Equal)),
        Op::Neq => poly(NOp::Cmp(IntCC::NotEqual)),
        Op::Not => typed(NOp::Not),
        _ => None,
    }
}

struct PlanWalk<'a> {
    pool: &'a ResolvedPool,
    counts: SwitchCounts<'a>,
    bools: BoolCtors,
    tys: ReprTys,
    /// Facts that outlive the pool; see [`TyProof`] for the proofs.
    locals: TiVec<LocalId, LocalPlan>,
    switch_counts: HashMap<TypeId, u8>,
}

impl PlanWalk<'_> {
    fn record(&mut self, b: &CoreBind) {
        self.locals.resize_at_least(b.id, LocalPlan::default());
        self.locals[b.id] = LocalPlan {
            repr: classify(self.pool, self.tys, b.ty),
            proof: prove(self.pool, self.tys, b.ty),
        };
    }

    /// The plan-time half of emit's `switch_plan`: does this match compile to
    /// a `SwitchTag`, and over how many variants?
    ///
    /// The type a match dispatches on by constructor, if every arm is a
    /// constructor of one (non-`Bool`) type — the precondition `emit`'s
    /// `switch_plan` shares — so its variant count is worth asking for.
    /// Whether the match then switches or ladders is decided later, by both
    /// backends alike, from the arm count against that answer.
    fn switched_type(&self, arms: &[(CorePat, CoreExpr)]) -> Option<TypeId> {
        let mut tid: Option<TypeId> = None;
        for (pat, _) in arms {
            let CorePat::Ctor { variant, .. } = pat else {
                return None;
            };
            if self.bools.polarity(variant).is_some() {
                return None;
            }
            match tid {
                None => tid = Some(variant.type_id),
                Some(t) if t != variant.type_id => return None,
                Some(_) => {}
            }
        }
        tid
    }

    /// Walk the body recording the per-local facts the emitter needs after the
    /// pool is gone. Infallible: every Core node has a lowering, so there is
    /// nothing to admit or refuse — an unproven operand picks the checking
    /// lowering rather than sending the whole body back to the interpreter.
    fn walk(&mut self, mut e: &CoreExpr) {
        loop {
            match e {
                CoreExpr::Let { bind, body, .. } => {
                    self.record(bind);
                    e = body;
                }
                CoreExpr::LetJoin { bind, join, body } => {
                    self.walk(join);
                    self.record(bind);
                    e = body;
                }
                CoreExpr::LetCont { cont, body, .. } => {
                    self.walk(cont);
                    e = body;
                }
                CoreExpr::Drop { body, .. } => e = body,
                CoreExpr::If { then, els, .. } => {
                    self.walk(then);
                    e = els;
                }
                CoreExpr::Match { arms, .. } => {
                    if let Some(tid) = self.switched_type(arms)
                        && let Some(count) = (self.counts)(tid)
                    {
                        self.switch_counts.insert(tid, count);
                    }
                    for (pat, body) in arms {
                        match pat {
                            CorePat::Wild | CorePat::Lit(_) => {}
                            CorePat::Bind(b) => self.record(b),
                            CorePat::Ctor { variant, fields } => {
                                if self.bools.polarity(variant).is_some() {
                                    if !fields.is_empty() {
                                        unsupported_node("Bool pattern with payload fields");
                                    }
                                } else {
                                    for f in fields {
                                        self.record(f);
                                    }
                                }
                            }
                        }
                        self.walk(body);
                    }
                    return;
                }
                CoreExpr::Tail(_) | CoreExpr::Goto(_) => return,
            }
        }
    }
}

/// One compiled body, defined but not yet finalized into the caller's module.
/// The caller finalizes and publishes `get_finalized_function(func_id)` into
/// the program's `NativeTable`.
pub struct CompiledBody {
    pub func_idx: FuncIdx,
    pub func_id: FuncId,
    pub clif: String,
    pub code_size: u32,
}

/// A resolved constant: stable bits plus the decoded views codegen bakes into
/// instruction immediates. Only non-heap constants resolve, since a frozen
/// heap constant's word is an address.
struct ConstVal {
    bits: u64,
    int: Option<i64>,
    boolean: Option<bool>,
}

/// Keyed by `ConstId`'s raw index, the canonical identity of a pooled
/// constant within one program.
type ConstMap = std::collections::HashMap<u32, ConstVal>;

fn resolve_const(consts: &[Value], c: super::ConstId, map: &mut ConstMap) -> Option<()> {
    if map.contains_key(&c.0) {
        return Some(());
    }
    let v = consts.get(c.0 as usize)?;
    // A heap constant is baked as its pointer bits, which is only sound if the
    // cell outlives every compiled body. The compiler interns all heap
    // constants into the program's frozen area, so they are immortal: the
    // pointer stays valid for the program's life, and the retain/release gates
    // skip them because the mortality bit is clear. A mortal heap value in the
    // pool would be a dangling bake, so it still interprets.
    if v.is_heap() && !v.is_immortal() {
        return None;
    }
    map.insert(
        c.0,
        ConstVal {
            bits: v.to_bits(),
            int: v.as_int(),
            boolean: v.as_bool(),
        },
    );
    Some(())
}

/// Collect and resolve every constant the body references. `None` rejects the
/// body: a heap constant, or an id outside the pool.
fn resolve_consts(fun: &CoreFn, consts: &[Value]) -> Option<ConstMap> {
    let mut map = ConstMap::new();
    let mut stack = vec![&fun.body];
    while let Some(mut e) = stack.pop() {
        loop {
            let atom_consts = |a: &Atom, map: &mut ConstMap| -> Option<()> {
                if let Atom::Const(c) = a {
                    resolve_const(consts, *c, map)?;
                }
                Some(())
            };
            match e {
                CoreExpr::Let { rhs, body, .. } => {
                    atom_consts(rhs, &mut map)?;
                    e = body;
                }
                CoreExpr::LetJoin { join, body, .. }
                | CoreExpr::LetCont {
                    cont: join, body, ..
                } => {
                    stack.push(join);
                    e = body;
                }
                CoreExpr::Drop { body, .. } => e = body,
                CoreExpr::If { then, els, .. } => {
                    stack.push(then);
                    e = els;
                }
                CoreExpr::Match { arms, .. } => {
                    for (pat, body) in arms {
                        if let CorePat::Lit(c) = pat {
                            resolve_const(consts, *c, &mut map)?;
                        }
                        stack.push(body);
                    }
                    break;
                }
                CoreExpr::Tail(a) => {
                    atom_consts(a, &mut map)?;
                    break;
                }
                CoreExpr::Goto(_) => break,
            }
        }
    }
    Some(map)
}

/// One non-Bool `Atom::Ctor` site, in emit's walk order: the header words
/// codegen bakes as instruction immediates. `packed` is
/// `type_id | variant_idx << 32`; the names are the same frozen `Str` words
/// the interpreter's `PushConst`es use. `labels` is a frozen `Tuple` of the
/// field-label `Str`s, equal in contents to what the interpreter's per-VM
/// `label_cache` builds. Nothing compares labels by pointer.
#[derive(Clone, Copy)]
struct EnumCtorSite {
    packed: u64,
    enum_name: u64,
    variant_name: u64,
    labels: u64,
    /// Whether the real emission put an `Op::Reuse` before the make. Read
    /// back rather than re-derived so the two backends cannot disagree.
    reuse: bool,
}

/// Collect the body's `Ctor` atoms in emit's walk order, which is also
/// codegen's. That shared order is what lets [`BodyGen`]'s ctor cursor pair
/// sites with atoms.
fn collect_ctor_atoms<'a>(e: &'a CoreExpr, out: &mut Vec<(&'a VariantRef, usize)>) {
    let mut e = e;
    let atom = |a: &'a Atom, out: &mut Vec<(&'a VariantRef, usize)>| {
        if let Atom::Ctor {
            variant, fields, ..
        } = a
        {
            out.push((variant, fields.len()));
        }
    };
    loop {
        match e {
            CoreExpr::Let { rhs, body, .. } => {
                atom(rhs, out);
                e = body;
            }
            CoreExpr::LetJoin { join, body, .. } => {
                collect_ctor_atoms(join, out);
                e = body;
            }
            CoreExpr::LetCont { cont, body, .. } => {
                collect_ctor_atoms(body, out);
                e = cont;
            }
            CoreExpr::Drop { body, .. } => e = body,
            CoreExpr::If { then, els, .. } => {
                collect_ctor_atoms(then, out);
                e = els;
            }
            CoreExpr::Match { arms, .. } => {
                for (_, b) in arms {
                    collect_ctor_atoms(b, out);
                }
                return;
            }
            CoreExpr::Tail(a) => {
                atom(a, out);
                return;
            }
            CoreExpr::Goto(_) => return,
        }
    }
}

/// Recover every non-Bool ctor site's header constants from the emitted
/// bytecode, the one place they exist after the hook. The k-th
/// `MakeEnumPayload` in code order is the k-th non-Bool ctor atom in walk
/// order, because both orders are emit's own walk. The peephole pass cannot
/// disturb this: it rewrites only `PushLocal; PushConst; IntOp` windows, and
/// a ctor header is always followed by `Reuse`/`MakeEnumPayload`.
///
/// Label tuples are frozen into the *program's* area, so the words baked into
/// compiled code live as long as the program.
///
/// `None` is never expected — every clause checks an emit invariant — and
/// falls back to interpreting the body.
///
/// `freeze` controls the label-tuple side effect. Without a builder the array
/// is only validated and `labels` stays zero. [`native_set`] runs the gate
/// pass that way so the frozen area is written exactly once, by [`compile`],
/// instead of orphaning a second immortal tuple per site.
fn enum_ctor_sites(
    plan: &NativePlan,
    program: &crate::bytecode::Program,
    layout: &FrameLayout,
    freeze: Option<&mut crate::frozen::FrozenBuilder>,
) -> Option<Vec<EnumCtorSite>> {
    let mut atoms = Vec::new();
    collect_ctor_atoms(&plan.fun.body, &mut atoms);
    atoms.retain(|(v, _)| plan.bools.polarity(v).is_none());
    if atoms.is_empty() {
        return Some(Vec::new());
    }
    // One recorded header per non-Bool ctor, in the same walk order as
    // `collect_ctor_atoms`. A length mismatch means emit and this walk
    // disagree about the body, so refuse to pair them up wrong.
    if layout.ctor_headers.len() != atoms.len() {
        return None;
    }
    let consts: &[Value] = &program.constants;
    let mut freeze = freeze;
    let mut sites = Vec::with_capacity(atoms.len());
    for ((variant, _arity), site) in atoms.iter().zip(&layout.ctor_headers) {
        let at = |i: i32| consts.get(usize::try_from(i).ok()?);
        let packed = at(site.packed)?.as_int()? as u64;
        // The packed word must name the atom's own variant, or the pairing
        // has drifted.
        let expect =
            crate::bytecode::value::pack_variant(variant.type_id, variant.variant_idx) as u64;
        if packed != expect {
            return None;
        }
        let en = at(site.enum_name)?;
        let vn = at(site.variant_name)?;
        if en.as_str().is_none() || vn.as_str().is_none() {
            return None;
        }
        let arr = at(site.labels)?.as_array()?;
        let labels = match freeze.as_deref_mut() {
            Some(fb) => {
                let mut items = Vec::with_capacity(arr.len());
                for i in 0..arr.len() {
                    items.push(fb.str(arr.get(i)?.as_str()?));
                }
                fb.tuple(items).into_value().to_bits()
            }
            None => {
                for i in 0..arr.len() {
                    arr.get(i)?.as_str()?;
                }
                0
            }
        };
        sites.push(EnumCtorSite {
            packed,
            enum_name: en.to_bits(),
            variant_name: vn.to_bits(),
            labels,
            reuse: site.reuse,
        });
    }
    Some(sites)
}

/// Per-local use facts gathered before codegen: which locals need a raw `i64`
/// view and which are used as value words. A slotless local with only Int-op
/// uses skips boxing entirely.
struct Uses {
    int: TiVec<LocalId, bool>,
    word: TiVec<LocalId, u32>,
    /// `Ctor { reuse: Some(x) }` targets. Emit's `Scan::reuse_claimed` twin,
    /// guarding [`peel_call_arg_drops`] so both backends peel identically.
    reuse_claimed: TiVec<LocalId, bool>,
}

impl Uses {
    fn scan(fun: &CoreFn, plan: &NativePlan, cmap: &ConstMap) -> Option<Uses> {
        let mut u = Uses {
            int: TiVec::new(),
            word: TiVec::new(),
            reuse_claimed: TiVec::new(),
        };
        u.expr(&fun.body, plan, cmap)?;
        Some(u)
    }

    fn need_int(&mut self, id: LocalId) {
        self.int.resize_at_least(id, false);
        self.int[id] = true;
    }

    fn need_word(&mut self, id: LocalId) {
        self.word.resize_at_least(id, 0);
        self.word[id] += 1;
    }

    fn int_demand(&self, id: LocalId) -> bool {
        self.int.get(id).copied().unwrap_or(false)
    }

    fn word_uses(&self, id: LocalId) -> u32 {
        self.word.get(id).copied().unwrap_or(0)
    }

    /// Whether some `Ctor { reuse: Some(id) }` in the body claims `id`'s cell.
    fn reuse_claimed(&self, id: LocalId) -> bool {
        self.reuse_claimed.get(id).copied().unwrap_or(false)
    }

    fn atom(&mut self, a: &Atom, plan: &NativePlan) {
        match a {
            Atom::Local(x) => self.need_word(*x),
            Atom::Const(_) => {}
            Atom::PrimOp { op, args, .. } => match op {
                // One owned word per operand.
                Op::TupleIndex
                | Op::GetFieldUnchecked
                | Op::MakeArray
                | Op::MakeTuple
                | Op::Append
                | Op::Prepend
                | Op::ArrayLen
                | Op::BinByteSize
                | Op::HttpHeadersValid
                | Op::HttpFraming
                | Op::HttpHeaderHas => {
                    for &x in args {
                        self.need_word(x);
                    }
                }
                // Mixed views: the Int operand is handed to the shim raw.
                Op::HttpParseHead => {
                    if let [buf, off] = args.as_slice() {
                        self.need_word(*buf);
                        self.need_int(*off);
                    }
                }
                Op::HttpSerializeHead => {
                    if let [code, reason, headers] = args.as_slice() {
                        self.need_int(*code);
                        self.need_word(*reason);
                        self.need_word(*headers);
                    }
                }
                // `PushGlobal` reads the entry frame, not a local; the Bool
                // heads are nullary constants. Neither has an operand.
                Op::PushGlobal
                | Op::PushCapture
                | Op::PushSelf
                | Op::PushNil
                | Op::PushTrue
                | Op::PushFalse => {}
                // The bridges hand every operand to their shim as an owned word.
                _ if is_native_bridge_op(*op)
                    || is_native_park_op(*op)
                    || is_native_try_op(*op) =>
                {
                    for &x in args {
                        self.need_word(x);
                    }
                }
                _ => match nop_of(*op) {
                    Some((NOp::Not, _)) => {
                        for &x in args {
                            self.need_word(x);
                        }
                    }
                    // A poly compare the plan cannot prove Int-only takes the
                    // bridge, which wants owned words.
                    Some((_, true)) if !args.iter().all(|&x| plan.is_int(x)) => {
                        for &x in args {
                            self.need_word(x);
                        }
                    }
                    Some(_) => {
                        for &x in args {
                            self.need_int(x);
                        }
                    }
                    None => unsupported_node(
                        "primop: `op_coverage` classifies this opcode NotAPrimOp, so lowering should never meet it as one",
                    ),
                },
            },
            Atom::Call { callee, args } => {
                if let Callee::Local(x) = callee {
                    self.need_word(*x);
                }
                for &x in args {
                    self.need_word(x);
                }
            }
            Atom::Closure { captures, .. } => {
                for &x in captures {
                    self.need_word(x);
                }
            }
            // One owned word per field. The reuse slot is not an operand:
            // codegen reads it through the frame slot directly.
            Atom::Ctor { fields, reuse, .. } => {
                for &x in fields {
                    self.need_word(x);
                }
                if let Some(r) = reuse {
                    self.reuse_claimed.resize_at_least(*r, false);
                    self.reuse_claimed[*r] = true;
                }
            }
        }
    }

    fn expr(&mut self, mut e: &CoreExpr, plan: &NativePlan, cmap: &ConstMap) -> Option<()> {
        loop {
            match e {
                CoreExpr::Let { rhs, body, .. } => {
                    self.atom(rhs, plan);
                    e = body;
                }
                CoreExpr::LetJoin { join, body, .. }
                | CoreExpr::LetCont {
                    cont: join, body, ..
                } => {
                    self.expr(join, plan, cmap)?;
                    e = body;
                }
                CoreExpr::Drop { body, .. } => e = body,
                CoreExpr::If {
                    cond, then, els, ..
                } => {
                    self.need_word(*cond);
                    self.expr(then, plan, cmap)?;
                    e = els;
                }
                CoreExpr::Match { scrut, arms, .. } => {
                    self.need_word(*scrut);
                    for (pat, body) in arms {
                        if let CorePat::Lit(c) = pat {
                            let cv = cmap.get(&c.0)?;
                            // Bool words compare as bits. An Int literal
                            // decodes the scrutinee, but only where the pool
                            // proved it Int — a spilled BigInt still decodes,
                            // anything else must not. Every other literal
                            // compares structurally through the bridge and
                            // needs no extra view.
                            if cv.boolean.is_none() && cv.int.is_some() && plan.is_int(*scrut) {
                                self.need_int(*scrut);
                            }
                        }
                        self.expr(body, plan, cmap)?;
                    }
                    return Some(());
                }
                CoreExpr::Tail(a) => {
                    self.atom(a, plan);
                    return Some(());
                }
                CoreExpr::Goto(_) => return Some(()),
            }
        }
    }
}

/// Codegen reached a node the gate should have rejected, or asked for a view
/// the use scan should have provided. A backend bug.
#[allow(clippy::panic)]
#[cold]
#[inline(never)]
fn unsupported_node(what: &str) -> ! {
    panic!(
        "internal compiler error: native backend reached unsupported {what}. \
         Report this as a compiler bug."
    )
}

/// The declared runtime imports, one `FuncRef` per symbol. Signatures must
/// match the shims in `crates/al` and its `vm::jit` declarations. Cranelift
/// rejects conflicting redeclarations, so drift fails at declare time.
struct RtRefs {
    release: ir::FuncRef,
    hollow: ir::FuncRef,
    enum_alloc: ir::FuncRef,
    make_array: ir::FuncRef,
    make_tuple: ir::FuncRef,
    seq_len: ir::FuncRef,
    seq_append: ir::FuncRef,
    seq_prepend: ir::FuncRef,
    bin_byte_size: ir::FuncRef,
    http_parse_head: ir::FuncRef,
    http_headers_valid: ir::FuncRef,
    http_header_has: ir::FuncRef,
    http_serialize_head: ir::FuncRef,
    http_framing: ir::FuncRef,
    push_global: ir::FuncRef,
    push_capture: ir::FuncRef,
    push_self: ir::FuncRef,
    int_box: ir::FuncRef,
    div_int: ir::FuncRef,
    mod_int: ir::FuncRef,
    shim_op: ir::FuncRef,
    park_op: ir::FuncRef,
    try_op: ir::FuncRef,
    prepare_call: ir::FuncRef,
    prepare_call_value: ir::FuncRef,
    prepare_tail: ir::FuncRef,
    prepare_tail_value: ir::FuncRef,
    ret_transfer: ir::FuncRef,
    rt_cont: ir::FuncRef,
    make_closure: ir::FuncRef,
    rt_checkpoint: ir::FuncRef,
    rt_frame_base: ir::FuncRef,
}

fn declare_imports<M: Module>(
    module: &mut M,
    func: &mut ir::Function,
) -> Result<RtRefs, Box<ModuleError>> {
    // Every import comes from the runtime's own signature table
    // (`RT_SIGS`), so no signature is written on this side of the seam — a
    // hand-copied one here could drift from the `extern "C"` definition
    // silently, which is ABI corruption. A name missing from the table
    // panics loudly at first compile.
    let ids = scarlet_vm::vm::jit::declare_runtime_imports(module)?;
    let mut r = |name: &str| module.declare_func_in_func(ids[name], func);
    Ok(RtRefs {
        release: r(NATIVE_RELEASE_AT_ZERO_SYMBOL),
        hollow: r(NATIVE_HOLLOW_FOR_REUSE_SYMBOL),
        enum_alloc: r(SYM_ENUM_ALLOC),
        make_array: r(SYM_MAKE_ARRAY),
        make_tuple: r(SYM_MAKE_TUPLE),
        seq_len: r(SYM_SEQ_LEN),
        seq_append: r(SYM_SEQ_APPEND),
        seq_prepend: r(SYM_SEQ_PREPEND),
        bin_byte_size: r(SYM_BIN_BYTE_SIZE),
        http_parse_head: r(SYM_HTTP_PARSE_HEAD),
        http_headers_valid: r(SYM_HTTP_HEADERS_VALID),
        http_header_has: r(SYM_HTTP_HEADER_HAS),
        http_serialize_head: r(SYM_HTTP_SERIALIZE_HEAD),
        http_framing: r(SYM_HTTP_FRAMING),
        push_global: r(SYM_PUSH_GLOBAL),
        push_capture: r(SYM_PUSH_CAPTURE),
        push_self: r(SYM_PUSH_SELF),
        int_box: r(NATIVE_INT_BOX_SYMBOL),
        div_int: r(SYM_DIV_INT),
        mod_int: r(SYM_MOD_INT),
        shim_op: r(SYM_SHIM_OP),
        park_op: r(SYM_PARK_OP),
        try_op: r(SYM_TRY_OP),
        prepare_call: r(SYM_RT_PREPARE_CALL),
        prepare_call_value: r(SYM_RT_PREPARE_CALL_VALUE),
        prepare_tail: r(SYM_RT_PREPARE_TAIL),
        prepare_tail_value: r(SYM_RT_PREPARE_TAIL_VALUE),
        ret_transfer: r(SYM_RT_RET_TRANSFER),
        rt_cont: r("al_rt_cont"),
        make_closure: r(SYM_RT_MAKE_CLOSURE),
        rt_checkpoint: r(SYM_RT_CHECKPOINT),
        rt_frame_base: r(SYM_RT_FRAME_BASE),
    })
}

/// Encode everything the backend needs to compile one body after the
/// `ResolvedPool` is gone: the lowered `CoreFn`, the plan's pool-derived
/// facts, and the frame layout emit fixed. One blob per body; the static
/// stdlib ships these so startup never re-lowers the stdlib.
pub(crate) fn encode_plan_bundle(plan: &NativePlan, layout: &FrameLayout) -> Vec<u8> {
    use super::codec::Enc;
    let mut e = Enc::new();
    let fn_bytes = super::codec::encode_fn(&plan.fun);
    e.usize(fn_bytes.len());
    e.buf.extend_from_slice(&fn_bytes);
    // Every repr, then every proof: the layout the shipped bundles have.
    e.usize(plan.locals.len());
    for local in &plan.locals {
        e.u8(match local.repr {
            Repr::Int => 0,
            Repr::Immediate => 1,
            Repr::Heap => 2,
            Repr::Dyn => 3,
        });
    }
    e.usize(plan.locals.len());
    for local in &plan.locals {
        match local.proof {
            TyProof::None => e.u8(0),
            TyProof::Array => e.u8(1),
            TyProof::Binary => e.u8(2),
            TyProof::Tuple(n) => {
                e.u8(3);
                e.u32(u32::from(n));
            }
        }
    }
    // Sorted, so the blob is deterministic run to run.
    let mut counts: Vec<(TypeId, u8)> = plan.switch_counts.iter().map(|(t, c)| (*t, *c)).collect();
    counts.sort_by_key(|(t, _)| t.0);
    e.usize(counts.len());
    for (tid, count) in counts {
        e.i32(tid.0);
        e.u8(count);
    }
    let layout_bytes = super::codec::encode_layout(layout);
    e.usize(layout_bytes.len());
    e.buf.extend_from_slice(&layout_bytes);
    e.buf
}

/// Decode one [`encode_plan_bundle`] image back into the plan and layout for
/// `func_idx`. `prelude` supplies what was never encoded because the runtime
/// already has it.
pub fn decode_plan_bundle(
    func_idx: FuncIdx,
    bytes: &[u8],
    prelude: &PreludeBindings,
) -> Result<(NativePlan, FrameLayout), super::codec::DecodeError> {
    use super::codec::{Dec, DecodeError};
    let mut d = Dec::new(bytes);
    let n = d.usize()?;
    let at = bytes.len() - d.remaining();
    let fun = super::codec::decode_fn(bytes.get(at..at + n).ok_or(DecodeError::Truncated)?)?;
    d.skip(n)?;
    let n = d.usize()?;
    let mut reprs = Vec::with_capacity(n);
    for _ in 0..n {
        reprs.push(match d.u8()? {
            0 => Repr::Int,
            1 => Repr::Immediate,
            2 => Repr::Heap,
            3 => Repr::Dyn,
            b => return Err(DecodeError::BadTag("Repr", b)),
        });
    }
    let n = d.usize()?;
    let mut proofs = Vec::with_capacity(n);
    for _ in 0..n {
        proofs.push(match d.u8()? {
            0 => TyProof::None,
            1 => TyProof::Array,
            2 => TyProof::Binary,
            3 => TyProof::Tuple(d.u32()? as u16),
            b => return Err(DecodeError::BadTag("TyProof", b)),
        });
    }
    let n = d.usize()?;
    let mut switch_counts = HashMap::with_capacity(n);
    for _ in 0..n {
        let tid = TypeId(d.i32()?);
        switch_counts.insert(tid, d.u8()?);
    }
    let n = d.usize()?;
    let at = bytes.len() - d.remaining();
    let layout = super::codec::decode_layout(bytes.get(at..at + n).ok_or(DecodeError::Truncated)?)?;
    d.skip(n)?;
    d.finish()?;
    // A local past the end of either list has that fact's default.
    let mut locals = TiVec::new();
    for i in 0..reprs.len().max(proofs.len()) {
        locals.push(LocalPlan {
            repr: reprs.get(i).copied().unwrap_or_default(),
            proof: proofs.get(i).copied().unwrap_or_default(),
        });
    }
    let plan = NativePlan {
        func_idx,
        fun,
        bools: BoolCtors::of(prelude),
        locals,
        switch_counts,
    };
    Ok((plan, layout))
}

/// The `FuncIdx`s whose [`compile`] will actually define a body: plans that
/// also pass the compile-time-only gates. Direct native→native call sites
/// must consult this. Naming a body outside it as a peer leaves its
/// `al_fn_{idx}` symbol undefined and `finalize_definitions` fails.
#[cfg(test)]
pub(crate) fn native_set(
    plans: &[NativePlan],
    program: &crate::bytecode::Program,
    layouts: &std::collections::HashMap<FuncIdx, FrameLayout>,
) -> HashSet<FuncIdx> {
    let consts: &[Value] = &program.constants;
    plans
        .iter()
        .filter(|p| {
            let Some(cmap) = resolve_consts(&p.fun, consts) else {
                return false;
            };
            if Uses::scan(&p.fun, p, &cmap).is_none() {
                return false;
            }
            let Some(layout) = layouts.get(&p.func_idx) else {
                return false;
            };
            enum_ctor_sites(p, program, layout, None).is_some()
        })
        .map(|p| p.func_idx)
        .collect()
}

/// Lower one planned body to CLIF and define it into `module` under the
/// [`NativeEntry`](crate::bytecode::NativeEntry) signature.
///
/// `program` must be the finished program: the plan's `ConstId`s index its
/// constant pool, and each ctor site's header constants are read back out of
/// its emitted bytecode. `Ok(None)` means a compile-time-only gate clause
/// failed and the body stays interpreted. `Err` is the module's own failure.
pub fn compile<M: Module>(
    module: &mut M,
    plan: &NativePlan,
    program: &crate::bytecode::Program,
    layout: &FrameLayout,
) -> Result<CompiledBody, Box<ModuleError>> {
    let consts: &[Value] = &program.constants;
    let Some(cmap) = resolve_consts(&plan.fun, consts) else {
        // Every heap constant is interned into the program's frozen area, so
        // a mortal one here means the constant pool was built wrong.
        unsupported_node("constant pool holding a mortal heap value")
    };
    let Some(uses) = Uses::scan(&plan.fun, plan, &cmap) else {
        unsupported_node("use scan failing on a body every opcode has a lowering for")
    };
    let mut frozen = program.frozen.builder();
    let Some(ctor_sites) = enum_ctor_sites(plan, program, layout, Some(&mut frozen)) else {
        // The recorded header positions no longer line up with the emitted
        // code — an emission or peephole change drifted. There is no slower
        // mode to fall back to any more, and a wrong constant would miscompile
        // the constructor, so this has to be loud.
        unsupported_node("ctor-site headers disagreeing with the emitted code")
    };

    let ptr_ty = module.target_config().pointer_type();
    let mut sig = module.make_signature();
    // Tail calling convention: what admits `return_call` between Scarlet bodies.
    // Matches `scarlet_vm::vm::jit::native_entry_signature`; Rust reaches these
    // only through the module's entry trampoline.
    sig.call_conv = CallConv::Tail;
    sig.params.push(AbiParam::new(ptr_ty));
    // The resume ordinal: 0 enters at the head, k at continuation k.
    sig.params.push(AbiParam::new(types::I64));
    sig.returns.push(AbiParam::new(types::I64));
    let name = format!("al_fn_{}", plan.func_idx.index());
    let func_id = module.declare_function(&name, Linkage::Export, &sig)?;

    let mut ctx = module.make_context();
    ctx.func.signature = sig;
    let mut fbc = FunctionBuilderContext::new();
    {
        let b = FunctionBuilder::new(&mut ctx.func, &mut fbc);
        let fns = declare_imports(module, b.func)?;
        let live = cont_live_sets(&plan.fun.body);
        // The word `Op::PushNil` pushes: the prelude's `Nil` constructor,
        // pre-built in the frozen area, so compiled code bakes it as an
        // immediate exactly as it does the Bool heads. Every real program
        // binds it (the prelude is always loaded); only a test program can
        // leave it `None`, and lowering a `PushNil` then is loud.
        let unit = program
            .abi_nullary(scarlet_vm::abi::AbiSlot::Unit)
            .map(Value::to_bits);
        let g = BodyGen::prologue(
            b,
            plan,
            live,
            layout.clone(),
            uses,
            cmap,
            ctor_sites,
            unit,
            fns,
            ptr_ty,
        );
        g.run();
    }
    let clif = ctx.func.display().to_string();
    module.define_function(func_id, &mut ctx)?;
    let code_size = ctx
        .compiled_code()
        .map(|c| c.code_info().total_size)
        .unwrap_or(0);
    Ok(CompiledBody {
        func_idx: plan.func_idx,
        func_id,
        clif,
        code_size,
    })
}

/// For each non-tail call (continuation), the locals read strictly after it
/// in emission order — its live-in set. `reload_cached_views` restores exactly
/// these at the continuation's top; a local dead here is never reloaded, so a
/// freed slot is never dereferenced. Indexed by continuation ordinal, matching
/// [`count_call_conts`] / [`BodyGen::next_cont`].
fn cont_live_sets(body: &CoreExpr) -> Vec<HashSet<LocalId>> {
    enum Ev {
        Read(LocalId),
        Boundary,
    }
    /// A parking op resumes at one of two ordinals (retry / continue), so it
    /// contributes two boundaries here — matching the emitter's two
    /// `next_cont` calls.
    fn park_boundaries(a: &Atom, ev: &mut Vec<Ev>) -> bool {
        let park = matches!(a, Atom::PrimOp { op, .. } if is_native_park_op(*op));
        if park {
            ev.push(Ev::Boundary);
            ev.push(Ev::Boundary);
        }
        park
    }
    fn collect(e: &CoreExpr, in_join: bool, ev: &mut Vec<Ev>) {
        let mut e = e;
        loop {
            match e {
                CoreExpr::Let { rhs, body, .. } => {
                    rhs.for_each_operand(|x| ev.push(Ev::Read(x)));
                    if matches!(rhs, Atom::Call { .. }) {
                        ev.push(Ev::Boundary);
                    }
                    park_boundaries(rhs, ev);
                    e = body;
                }
                CoreExpr::LetJoin { join, body, .. } => {
                    collect(join, true, ev);
                    e = body;
                }
                CoreExpr::LetCont { cont, body, .. } => {
                    collect(body, in_join, ev);
                    e = cont;
                }
                CoreExpr::Drop { body, .. } => e = body,
                CoreExpr::If {
                    cond, then, els, ..
                } => {
                    ev.push(Ev::Read(*cond));
                    collect(then, in_join, ev);
                    e = els;
                }
                CoreExpr::Match { scrut, arms, .. } => {
                    ev.push(Ev::Read(*scrut));
                    for (_, b) in arms {
                        collect(b, in_join, ev);
                    }
                    return;
                }
                CoreExpr::Tail(a) => {
                    a.for_each_operand(|x| ev.push(Ev::Read(x)));
                    if in_join && matches!(a, Atom::Call { .. }) {
                        ev.push(Ev::Boundary);
                    }
                    // Unconditional, unlike a tail call: a parked op resumes
                    // into this body whatever position it sits in.
                    park_boundaries(a, ev);
                    return;
                }
                CoreExpr::Goto(_) => return,
            }
        }
    }
    let mut ev = Vec::new();
    collect(body, false, &mut ev);
    let mut seen = HashSet::new();
    let mut rev = Vec::new();
    for e in ev.iter().rev() {
        match e {
            Ev::Read(x) => {
                seen.insert(*x);
            }
            Ev::Boundary => rev.push(seen.clone()),
        }
    }
    rev.reverse();
    rev
}

/// Where an expression delivers its value: function-tail position, or a jump
/// to a `LetJoin` merge block with the boxed word.
#[derive(Clone, Copy)]
enum Dest {
    Ret,
    Merge(ir::Block),
}

/// A produced atom value: the boxed word (owned, when requested) and/or the
/// raw `i64` view.
struct AtomVal {
    word: Option<ir::Value>,
    int: Option<ir::Value>,
}

/// Which of an [`AtomVal`]'s views a caller of `eval_pure` needs.
#[derive(Clone, Copy)]
struct Want {
    word: bool,
    int: bool,
}

struct BodyGen<'a> {
    b: FunctionBuilder<'a>,
    plan: &'a NativePlan,
    /// Direct-call targets for `Callee::Known` sites whose callee is in this
    /// JIT round's native set. A `Known` callee absent here falls back to the
    /// `al_rt_*` trampoline.
    /// Continuation blocks, one per non-tail call in walk order; the entry
    /// dispatch table's targets 1..=N.
    cont_blocks: Vec<ir::Block>,
    cont_cursor: usize,
    /// Live-in set per continuation (locals read after it); reload restores
    /// only these, so a dead local's freed slot is never dereferenced.
    cont_live: Vec<HashSet<LocalId>>,
    /// This body's own (tail) signature, imported for `return_call_indirect`
    /// transfers.
    sig_ref: ir::SigRef,
    /// The entry's resume-ordinal parameter, read by the dispatch table.
    resume_param: ir::Value,
    /// Locals bound to a constant `(bits, int)` — slotless, so a continuation
    /// rematerializes them instead of reloading a frame slot.
    const_locals: TiVec<LocalId, Option<(u64, Option<i64>)>>,
    layout: FrameLayout,
    uses: Uses,
    cmap: ConstMap,
    facts: ValueBits,
    /// The frozen `Nil` constructor's bits, for `Op::PushNil`. See [`compile`].
    unit: Option<u64>,
    fns: RtRefs,
    ptr_ty: ir::Type,
    /// The frame-base pointer (`&stack[frame.base_slot]`), re-fetched after
    /// every runtime call that can grow the value stack.
    base: Variable,
    words: TiVec<LocalId, Option<Variable>>,
    ints: TiVec<LocalId, Option<Variable>>,
    joins: TiVec<JoinId, Option<ir::Block>>,
    loop_head: ir::Block,
    /// Non-Bool constructor sites in walk order, paired with `Atom::Ctor`s by
    /// [`Self::next_ctor_site`]'s cursor.
    ctor_sites: Vec<EnumCtorSite>,
    ctor_cursor: usize,
    /// Locals whose consuming read in the current bind's rhs is their last
    /// use (the bind is followed by their `Drop`): `owned_word` moves the
    /// slot's own reference out instead of dup-now-drop-later, which is what
    /// lets a callee see refcount 1 and edit in place.
    move_args: Vec<LocalId>,
    /// Moves `owned_word` actually performed; the matching `Drop` nodes are
    /// skipped, their release having happened as the consumer's.
    consumed_drops: Vec<LocalId>,
}

impl<'a> BodyGen<'a> {
    /// Entry-block prologue, ending with the self-tail loop header open.
    #[allow(clippy::too_many_arguments)]
    fn prologue(
        mut b: FunctionBuilder<'a>,
        plan: &'a NativePlan,
        cont_live: Vec<HashSet<LocalId>>,
        layout: FrameLayout,
        uses: Uses,
        cmap: ConstMap,
        ctor_sites: Vec<EnumCtorSite>,
        unit: Option<u64>,
        fns: RtRefs,
        ptr_ty: ir::Type,
    ) -> BodyGen<'a> {
        let entry = b.create_block();
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        b.seal_block(entry);
        // The `NativeCtx` argument goes into the pinned register and every
        // use re-reads it from there. It must never live in the frame as a
        // spillable value: a parked frame resumes on whatever scheduler wakes
        // it, and a stale scheduler pointer means cross-thread mutation of
        // one VM.
        let ctx = b.block_params(entry)[0];
        // Captured in the entry block; the dispatch at the end of the
        // prologue may sit blocks later (param unboxing branches), and entry
        // dominates it either way.
        let resume_param = b.block_params(entry)[1];
        b.ins().set_pinned_reg(ctx);

        // `base` is defined only in the head and each continuation — never in
        // the entry block — so no value is live across the dispatch br_table.
        let base = b.declare_var(ptr_ty);

        let loop_head = b.create_block();
        let cont_blocks: Vec<ir::Block> = (0..cont_live.len()).map(|_| b.create_block()).collect();
        let sig_ref = {
            let own = b.func.signature.clone();
            b.import_signature(own)
        };

        let mut g = BodyGen {
            b,
            plan,
            layout,
            uses,
            cmap,
            facts: value_bits(),
            unit,
            fns,
            ptr_ty,
            base,
            cont_blocks,
            cont_cursor: 0,
            cont_live,
            sig_ref,
            resume_param,
            const_locals: TiVec::new(),
            words: TiVec::new(),
            ints: TiVec::new(),
            joins: TiVec::new(),
            loop_head,
            ctor_sites,
            ctor_cursor: 0,
            move_args: Vec::new(),
            consumed_drops: Vec::new(),
        };
        g.init_params();
        g
    }

    /// The context pointer, re-read from the pinned register at every use.
    /// Deliberately not a cached `ir::Value`: regalloc may spill one into the
    /// frame, and this word must never survive into a parked frame.
    fn ctx(&mut self) -> ir::Value {
        self.b.ins().get_pinned_reg(self.ptr_ty)
    }

    /// Fetch the current frame's slot-0 address through `rt_frame_base`.
    fn frame_base_now(&mut self) -> ir::Value {
        let vmx = self.vmx();
        let call = self.b.ins().call(self.fns.rt_frame_base, &[vmx]);
        self.b.inst_results(call)[0]
    }

    /// The scheduler's VM for shim calls. Loaded immediately before each use
    /// and never earlier, so a resumed frame reads the resuming scheduler's
    /// VM instead of a stale one.
    fn vmx(&mut self) -> ir::Value {
        let ctx = self.ctx();
        self.b.ins().load(
            self.ptr_ty,
            MemFlagsData::trusted(),
            ctx,
            NativeCtx::VM_OFFSET,
        )
    }

    fn init_params(&mut self) {
        let g = self;
        // Entry dispatch, emitted in the entry block BEFORE any Variable is
        // defined: resume 0 is the head, k is continuation k. Because no
        // value flows into a Variable that is live across this br_table, the
        // FunctionBuilder inserts no implicit block params on head/conts — so
        // `self_tail`'s arg-less back-edge jump to the head stays consistent.
        let head = g.loop_head;
        let resume = g.resume_param;
        let r32 = g.b.ins().ireduce(types::I32, resume);
        let default = g.b.func.dfg.block_call(head, &[]);
        let mut targets = vec![default];
        for &cb in &g.cont_blocks {
            targets.push(g.b.func.dfg.block_call(cb, &[]));
        }
        let jt =
            g.b.create_jump_table(ir::JumpTableData::new(default, &targets));
        g.b.ins().br_table(r32, jt);

        // The head loads params into their Variables, so every resume enters
        // the head with clean register state and continuations reload from
        // slots. `loop_head` is the self-tail back-edge target too.
        g.b.switch_to_block(head);
        let nb = g.frame_base_now();
        g.b.def_var(g.base, nb);
        for p in &g.plan.fun.params {
            let slot = g.slot_of(p.id);
            let w = g.load_slot(slot);
            // Registers only: the slot already owns this word, so re-storing
            // would release its reference.
            g.def_regs(
                p.id,
                AtomVal {
                    word: Some(w),
                    int: None,
                },
            );
        }
    }

    fn run(mut self) {
        // Split borrow: the walk takes `&mut self`, so the body pointer must
        // not run through `self`.
        let body: &CoreExpr = &self.plan.fun.body;
        self.expr(body, Dest::Ret);
        if self.cont_cursor != self.cont_blocks.len() {
            resume_walk_mismatch();
        }
        if self.ctor_cursor != self.ctor_sites.len() {
            ctor_walk_mismatch();
        }
        self.b.seal_all_blocks();
        self.b.finalize();
    }

    fn slot_of(&self, id: LocalId) -> i32 {
        match self.layout.slot(id) {
            Some(s) => s,
            None => unsupported_node("slotless frame access"),
        }
    }

    fn load_slot(&mut self, slot: i32) -> ir::Value {
        let base = self.b.use_var(self.base);
        FrameSlots::new(base).load_slot(&mut self.b, slot)
    }

    /// `StoreLocal` parity: release the old word, store the new. `bits` must
    /// carry an owned reference; the slot takes it.
    fn store_slot(&mut self, slot: i32, bits: ir::Value) {
        let base = self.b.use_var(self.base);
        FrameSlots::new(base).store_slot(&mut self.b, slot, bits, self.fns.release);
    }

    /// [`Self::store_slot`] for `id`'s own slot, eliding the old-word release
    /// where `id`'s type proves an immediate. Slots are per-local, so the old
    /// word is the frame fill, a `Drop` zero, or an earlier value of `id`.
    /// A [`Repr::Heap`] local must still keep the *dynamic* gate: the fill and
    /// `Drop` zeros are immediates, which a proven-heap gate would
    /// dereference.
    fn store_local_slot(&mut self, id: LocalId, slot: i32, bits: ir::Value) {
        if self.plan.repr_of(id) == Repr::Immediate {
            let base = self.b.use_var(self.base);
            FrameSlots::new(base).store_slot_no_release(&mut self.b, slot, bits);
        } else {
            self.store_slot(slot, bits);
        }
    }

    fn store_slot_zero(&mut self, slot: i32) {
        let zero = self
            .b
            .ins()
            .iconst(types::I64, self.facts.int_header as i64);
        let base = self.b.use_var(self.base);
        FrameSlots::new(base).store_slot_no_release(&mut self.b, slot, zero);
    }

    /// The word view of `id`, always. A cached view is the register copy; a
    /// miss re-materialises from the local's home — its frame slot, or its
    /// constant bits.
    ///
    /// A miss is normal, not a failure: a continuation drops every register
    /// copy, and `reload_cached_views` only restores the locals it can prove
    /// live. Reaching a use *is* the proof that this local is live, so its
    /// slot still holds its value and reloading it here is sound. The reload
    /// is deliberately a bare `ir::Value` rather than a `def_var`: defining
    /// the Variable inside one branch would not dominate a sibling branch's
    /// use.
    fn word_of(&mut self, id: LocalId) -> ir::Value {
        if let Some(v) = self.words.get(id).copied().flatten() {
            return self.b.use_var(v);
        }
        self.rematerialize_word(id)
    }

    /// [`Self::word_of`]'s miss path: the home read, split out so `int_of`
    /// shares it.
    fn rematerialize_word(&mut self, id: LocalId) -> ir::Value {
        if let Some(slot) = self.layout.slot(id) {
            return self.load_slot(slot);
        }
        if let Some((bits, _)) = self.const_locals.get(id).copied().flatten() {
            return self.b.ins().iconst(types::I64, bits as i64);
        }
        // Slotless and non-constant: a single-use temp, whose one use is in
        // the region that defined it, so its register copy is always present
        // above. Landing here means the frame plan dropped a slot a live
        // local needed.
        unsupported_node("word view: local with no slot and no constant home")
    }

    /// The raw `i64` view of `id`, always. Mirrors [`Self::word_of`]: a miss
    /// re-derives the word from the local's home and unboxes it.
    fn int_of(&mut self, id: LocalId) -> ir::Value {
        if let Some(v) = self.ints.get(id).copied().flatten() {
            return self.b.use_var(v);
        }
        if let Some((_, Some(i))) = self.const_locals.get(id).copied().flatten() {
            return self.b.ins().iconst(types::I64, i);
        }
        let w = self.rematerialize_word(id);
        unbox_int(&mut self.b, &self.facts, w)
    }

    /// An owned word for a consuming use. A slotted local normally keeps its
    /// slot's reference and hands out a retained copy — but when this read is
    /// the local's last use (`move_args`), the slot's own reference transfers
    /// to the consumer and the slot is cleared, so no count traffic happens
    /// and the consumer can observe a unique value. A slotless local is a
    /// single-use temp whose one owned word transfers.
    fn owned_word(&mut self, id: LocalId) -> ir::Value {
        let w = self.word_of(id);
        if self.layout.slot(id).is_some()
            && let Some(gate) = self.plan.rc_gate(id)
        {
            if let Some(i) = self.move_args.iter().position(|&x| x == id) {
                self.move_args.swap_remove(i);
                self.consumed_drops.push(id);
                let slot = self.slot_of(id);
                self.store_slot_zero(slot);
            } else {
                emit_dup(&mut self.b, w, gate);
            }
        }
        w
    }

    /// The owned word a whole-op shim returned. The int view goes through the
    /// dynamic unbox: a shim result carries no static Int proof.
    fn opaque_result(&mut self, call: ir::Inst, want_int: bool) -> AtomVal {
        let r = self.b.inst_results(call)[0];
        let int = want_int.then(|| unbox_int(&mut self.b, &self.facts, r));
        AtomVal { word: Some(r), int }
    }

    /// Call the generic bridge shim and return its owned result word.
    ///
    /// The shim pushes its operands onto the value stack, and a push can grow
    /// the `Vec` — which moves it, leaving every cached frame-base pointer
    /// dangling. The shim therefore returns `(base, result)`, and this is the
    /// only place that may call it: the fresh base is re-established here
    /// before any caller can touch a slot again.
    fn shim_op_call(
        &mut self,
        opc: ir::Value,
        opv: ir::Value,
        buf: ir::Value,
        n: ir::Value,
    ) -> ir::Value {
        let vmx = self.vmx();
        let call = self
            .b
            .ins()
            .call(self.fns.shim_op, &[vmx, opc, opv, buf, n]);
        let nb = self.b.inst_results(call)[0];
        let r = self.b.inst_results(call)[1];
        self.b.def_var(self.base, nb);
        r
    }

    /// [`Self::shim_op_call`] wrapped as an [`AtomVal`], the shim-op analogue
    /// of [`Self::opaque_result`].
    fn shim_op_result(
        &mut self,
        opc: ir::Value,
        opv: ir::Value,
        buf: ir::Value,
        n: ir::Value,
        want_int: bool,
    ) -> AtomVal {
        let r = self.shim_op_call(opc, opv, buf, n);
        let int = want_int.then(|| unbox_int(&mut self.b, &self.facts, r));
        AtomVal { word: Some(r), int }
    }

    /// Record a binding's produced value: write it to its frame slot, which
    /// takes the owned reference, and define the register views uses demand.
    fn def_local(&mut self, id: LocalId, val: AtomVal) {
        if let Some(slot) = self.layout.slot(id) {
            let Some(w) = val.word else {
                unsupported_node("slotted binding without a word")
            };
            self.store_local_slot(id, slot, w);
        }
        self.def_regs(id, val);
    }

    /// Define a local's register views without touching its frame slot, for
    /// values the slot already owns. [`Self::def_local`] would re-store the
    /// word, releasing the very reference the slot holds.
    ///
    /// One Variable per view for the body's whole lifetime. A redefinition
    /// (the self-tail back-edge rebinding parameters) must `def_var` the
    /// existing Variable so uses already emitted at the loop head resolve to
    /// the back-edge values. A fresh Variable would leave the loop reading the
    /// entry values forever.
    fn def_regs(&mut self, id: LocalId, val: AtomVal) {
        if let Some(w) = val.word {
            self.words.resize_at_least(id, None);
            let var = match self.words[id] {
                Some(var) => var,
                None => {
                    let var = self.b.declare_var(types::I64);
                    self.words[id] = Some(var);
                    var
                }
            };
            self.b.def_var(var, w);
        }
        if self.uses.int_demand(id) {
            let iv = match val.int {
                Some(v) => v,
                None => {
                    let Some(w) = val.word else {
                        unsupported_node("int view without a word")
                    };
                    unbox_int(&mut self.b, &self.facts, w)
                }
            };
            self.ints.resize_at_least(id, None);
            let var = match self.ints[id] {
                Some(var) => var,
                None => {
                    let var = self.b.declare_var(types::I64);
                    self.ints[id] = Some(var);
                    var
                }
            };
            self.b.def_var(var, iv);
        }
    }

    /// Structural equality of two already-materialised words, via the same
    /// `values_equal` the interpreter uses. Returns the Bool word.
    fn eq_words(&mut self, a: ir::Value, b: ir::Value) -> ir::Value {
        let slot =
            self.b
                .create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 16, 3));
        self.b.ins().stack_store(a, slot, 0);
        self.b.ins().stack_store(b, slot, 8);
        let buf = self.b.ins().stack_addr(self.ptr_ty, slot, 0);
        let opc = self.b.ins().iconst(types::I64, i64::from(Op::Eq as u8));
        let zero = self.b.ins().iconst(types::I64, 0);
        let n = self.b.ins().iconst(types::I64, 2);
        self.shim_op_call(opc, zero, buf, n)
    }

    /// Lower `a` through the generic bridge, which re-checks its operands.
    /// Used wherever the fast path needs a type proof the plan does not have,
    /// so an unproven site still compiles instead of falling back to bytecode.
    fn bridge_call(&mut self, op: Op, args: &[LocalId], imm: Imm, want_int: bool) -> AtomVal {
        let operand = super::emit::imm_operand(op, imm);
        let buf = self.arg_buffer(args);
        let opc = self.b.ins().iconst(types::I64, i64::from(op as u8));
        let opv = self.b.ins().iconst(types::I64, i64::from(operand));
        let n = self.b.ins().iconst(types::I64, args.len() as i64);
        self.shim_op_result(opc, opv, buf, n, want_int)
    }

    /// Evaluate a non-call atom. `want.word` asks for an owned boxed word,
    /// `want.int` for the raw `i64` view.
    fn eval_pure(&mut self, a: &Atom, want: Want) -> AtomVal {
        let Want {
            word: want_word,
            int: want_int,
        } = want;
        match a {
            Atom::Local(src) => {
                let word = want_word.then(|| self.owned_word(*src));
                let int = want_int.then(|| match self.ints.get(*src).copied().flatten() {
                    Some(v) => self.b.use_var(v),
                    None => {
                        let w = self.word_of(*src);
                        unbox_int(&mut self.b, &self.facts, w)
                    }
                });
                AtomVal { word, int }
            }
            Atom::Const(c) => {
                let Some(cv) = self.cmap.get(&c.0) else {
                    unsupported_node("unresolved constant")
                };
                let (bits, int) = (cv.bits, cv.int);
                let word = want_word.then(|| self.b.ins().iconst(types::I64, bits as i64));
                let int = want_int.then(|| {
                    let Some(i) = int else {
                        unsupported_node("non-Int constant in Int position")
                    };
                    self.b.ins().iconst(types::I64, i)
                });
                AtomVal { word, int }
            }
            Atom::PrimOp {
                op: Op::TupleIndex,
                args,
                imm: Imm::Index(i),
            } => {
                let [recv] = args.as_slice() else {
                    unsupported_node("TupleIndex arity")
                };
                if !self.plan.tuple_has(*recv, *i as usize) {
                    return self.bridge_call(Op::TupleIndex, args, Imm::Index(*i), want_int);
                }
                // The gate proved a wide-enough Tuple, so the word is a heap
                // cell `[count][elements…]` and element `i` is payload word
                // `1 + i`. The interpreter's bounds/type errors are
                // unreachable under that proof.
                let w = self.word_of(*recv);
                let obj = self.b.ins().band_imm(w, NATIVE_PTR_MASK as i64);
                let f = self.load_payload_word(obj, TUPLE_ELEMS_WORD + *i as usize);
                self.field_result(f, want_word, want_int)
            }
            // The shim hands back the global area's word borrowed;
            // `field_result` retains only where this use keeps it, matching
            // the interpreter's `globals[slot].clone()`.
            Atom::PrimOp {
                op: Op::PushGlobal,
                imm: Imm::Index(slot),
                ..
            } => {
                let n = self.b.ins().iconst(types::I64, i64::from(*slot));
                let vmx = self.vmx();
                let call = self.b.ins().call(self.fns.push_global, &[vmx, n]);
                let w = self.b.inst_results(call)[0];
                self.field_result(w, want_word, want_int)
            }
            // The shim hands back a borrowed word from the running closure;
            // `field_result` retains only where this use keeps it, matching the
            // interpreter's `clone()`.
            Atom::PrimOp {
                op: Op::PushCapture,
                imm: Imm::Index(idx),
                ..
            } => {
                let n = self.b.ins().iconst(types::I64, i64::from(*idx));
                let vmx = self.vmx();
                let call = self.b.ins().call(self.fns.push_capture, &[vmx, n]);
                let w = self.b.inst_results(call)[0];
                self.field_result(w, want_word, want_int)
            }
            Atom::PrimOp {
                op: Op::PushSelf, ..
            } => {
                let vmx = self.vmx();
                let call = self.b.ins().call(self.fns.push_self, &[vmx]);
                let w = self.b.inst_results(call)[0];
                self.field_result(w, want_word, want_int)
            }
            // One iconst of the frozen constructor's bits: immortal, so no
            // retain is owed, the same as the interpreter's clone of it.
            Atom::PrimOp {
                op: Op::PushNil, ..
            } => {
                if want_int {
                    unsupported_node("int view of Nil");
                }
                let Some(bits) = self.unit else {
                    unsupported_node("PushNil in a program whose Unit ABI slot is unbound")
                };
                AtomVal {
                    word: want_word.then(|| self.b.ins().iconst(types::I64, bits as i64)),
                    int: None,
                }
            }
            // One iconst of the immediate's bits.
            Atom::PrimOp {
                op: op @ (Op::PushTrue | Op::PushFalse),
                ..
            } => {
                if want_int {
                    unsupported_node("int view of a Bool");
                }
                let bits = if matches!(op, Op::PushTrue) {
                    self.facts.bool_true
                } else {
                    self.facts.bool_false
                };
                AtomVal {
                    word: want_word.then(|| self.b.ins().iconst(types::I64, bits as i64)),
                    int: None,
                }
            }
            // Owned operands in, owned result out.
            Atom::PrimOp {
                op: op @ (Op::Append | Op::Prepend),
                args,
                imm: imm @ Imm::Argc(_),
            } => {
                if want_int {
                    unsupported_node("int view of a sequence");
                }
                let seq = if matches!(op, Op::Append) {
                    args[0]
                } else {
                    args[args.len() - 1]
                };
                if !self.plan.is_array(seq) {
                    return self.bridge_call(*op, args, *imm, want_int);
                }
                let buf = self.arg_buffer(args);
                let n = self.b.ins().iconst(types::I64, args.len() as i64);
                let f = if matches!(op, Op::Append) {
                    self.fns.seq_append
                } else {
                    self.fns.seq_prepend
                };
                let vmx = self.vmx();
                let call = self.b.ins().call(f, &[vmx, buf, n]);
                AtomVal {
                    word: Some(self.b.inst_results(call)[0]),
                    int: None,
                }
            }
            // Owned operands in, Int views raw, one shim call.
            Atom::PrimOp {
                op: Op::HttpParseHead,
                args,
                imm: Imm::None,
            } => {
                let [buf, off] = args.as_slice() else {
                    unsupported_node("HttpParseHead arity")
                };
                if !(self.plan.is_binary(*buf) && self.plan.is_int(*off)) {
                    return self.bridge_call(Op::HttpParseHead, args, Imm::None, want_int);
                }
                let b = self.owned_word(*buf);
                let o = self.int_of(*off);
                let vmx = self.vmx();
                let call = self.b.ins().call(self.fns.http_parse_head, &[vmx, b, o]);
                self.opaque_result(call, want_int)
            }
            Atom::PrimOp {
                op: op @ (Op::HttpHeadersValid | Op::HttpFraming),
                args,
                imm: Imm::None,
            } => {
                let [headers] = args.as_slice() else {
                    unsupported_node("http headers arity")
                };
                if !self.plan.is_array(*headers) {
                    return self.bridge_call(*op, args, Imm::None, want_int);
                }
                let h = self.owned_word(*headers);
                let call = if matches!(op, Op::HttpHeadersValid) {
                    self.b.ins().call(self.fns.http_headers_valid, &[h])
                } else {
                    let vmx = self.vmx();
                    self.b.ins().call(self.fns.http_framing, &[vmx, h])
                };
                self.opaque_result(call, want_int)
            }
            Atom::PrimOp {
                op: Op::HttpHeaderHas,
                args,
                imm: Imm::None,
            } => {
                let [headers, name] = args.as_slice() else {
                    unsupported_node("HttpHeaderHas arity")
                };
                if !(self.plan.is_array(*headers) && self.plan.is_binary(*name)) {
                    return self.bridge_call(Op::HttpHeaderHas, args, Imm::None, want_int);
                }
                let h = self.owned_word(*headers);
                let n = self.owned_word(*name);
                let call = self.b.ins().call(self.fns.http_header_has, &[h, n]);
                self.opaque_result(call, want_int)
            }
            Atom::PrimOp {
                op: Op::HttpSerializeHead,
                args,
                imm: Imm::None,
            } => {
                let [code, reason, headers] = args.as_slice() else {
                    unsupported_node("HttpSerializeHead arity")
                };
                if !(self.plan.is_int(*code)
                    && self.plan.is_binary(*reason)
                    && self.plan.is_array(*headers))
                {
                    return self.bridge_call(Op::HttpSerializeHead, args, Imm::None, want_int);
                }
                let c = self.int_of(*code);
                let r = self.owned_word(*reason);
                let h = self.owned_word(*headers);
                let vmx = self.vmx();
                let call = self
                    .b
                    .ins()
                    .call(self.fns.http_serialize_head, &[vmx, c, r, h]);
                self.opaque_result(call, want_int)
            }
            Atom::PrimOp {
                op: op @ (Op::ArrayLen | Op::BinByteSize),
                args,
                imm: Imm::None,
            } => {
                let [recv] = args.as_slice() else {
                    unsupported_node("length arity")
                };
                let proven = if matches!(op, Op::ArrayLen) {
                    self.plan.is_array(*recv)
                } else {
                    self.plan.is_binary(*recv)
                };
                if !proven {
                    return self.bridge_call(*op, args, Imm::None, want_int);
                }
                let w = self.owned_word(*recv);
                let f = if matches!(op, Op::ArrayLen) {
                    self.fns.seq_len
                } else {
                    self.fns.bin_byte_size
                };
                let vmx = self.vmx();
                let call = self.b.ins().call(f, &[vmx, w]);
                let r = self.b.inst_results(call)[0];
                let int = want_int.then(|| unbox_int(&mut self.b, &self.facts, r));
                AtomVal { word: Some(r), int }
            }
            // Spill the owned element words; the shim builds the aggregate in
            // the process heap and releases the transferred references.
            Atom::PrimOp {
                op: op @ (Op::MakeArray | Op::MakeTuple),
                args,
                imm: Imm::Argc(_),
            } => {
                if want_int {
                    unsupported_node("int view of an aggregate");
                }
                let buf = self.arg_buffer(args);
                let n = self.b.ins().iconst(types::I64, args.len() as i64);
                let f = if matches!(op, Op::MakeArray) {
                    self.fns.make_array
                } else {
                    self.fns.make_tuple
                };
                let vmx = self.vmx();
                let call = self.b.ins().call(f, &[vmx, buf, n]);
                AtomVal {
                    word: Some(self.b.inst_results(call)[0]),
                    int: None,
                }
            }
            Atom::PrimOp {
                op: Op::GetFieldUnchecked,
                args,
                imm: Imm::Index(i),
            } => {
                let [recv] = args.as_slice() else {
                    unsupported_node("GetFieldUnchecked arity")
                };
                // The checker proved the field exists at payload word
                // `6 + i`, but `enum_field_typed` still gates on the value
                // being a heap cell and answers nil otherwise. Mirror that
                // gate so a non-heap word is never dereferenced.
                let w = self.word_of(*recv);
                let heap_b = self.b.create_block();
                let merge = self.b.create_block();
                self.b.append_block_param(merge, types::I64);
                let g = self.b.ins().band_imm(w, NATIVE_MORTAL_HEAP_BITS as i64);
                let is_heap =
                    self.b
                        .ins()
                        .icmp_imm(IntCC::Equal, g, NATIVE_MORTAL_HEAP_BITS as i64);
                let nil = self.b.ins().iconst(types::I64, self.facts.nil as i64);
                self.b
                    .ins()
                    .brif(is_heap, heap_b, &[], merge, &[nil.into()]);
                self.b.seal_block(heap_b);
                self.b.switch_to_block(heap_b);
                let obj = self.b.ins().band_imm(w, NATIVE_PTR_MASK as i64);
                let f = self.load_payload_word(obj, ENUM_FIELDS_WORD + *i as usize);
                self.b.ins().jump(merge, &[f.into()]);
                self.b.seal_block(merge);
                self.b.switch_to_block(merge);
                let f = self.b.block_params(merge)[0];
                self.field_result(f, want_word, want_int)
            }
            // The pure single-result ops: spill the owned operands, call the
            // generic bridge (which runs the interpreter's op method over the
            // value stack), take the owned result. `operand` is the flattened
            // immediate the method reads; ops without one pass 0. An Int result
            // (StrLen, BinBitSize, MapSize, BinByteAt) is a boxed word
            // `opaque_result` unboxes on demand.
            // A fallible op: one call, then unwind on anything but `Done`.
            // No resume ordinal — an error never comes back here.
            Atom::PrimOp { op, args, imm } if is_native_try_op(*op) => {
                let operand = super::emit::imm_operand(*op, *imm);
                let buf = self.arg_buffer(args);
                let opc = self.b.ins().iconst(types::I64, i64::from(*op as u8));
                let opv = self.b.ins().iconst(types::I64, i64::from(operand));
                let n = self.b.ins().iconst(types::I64, args.len() as i64);
                let vmx = self.vmx();
                let call = self.b.ins().call(self.fns.try_op, &[vmx, opc, opv, buf, n]);
                let status = self.b.inst_results(call)[0];
                let ok = self.b.create_block();
                let bail = self.b.create_block();
                self.b.set_cold_block(bail);
                let done =
                    self.b
                        .ins()
                        .icmp_imm(IntCC::Equal, status, NativeStatus::Done as u64 as i64);
                self.b.ins().brif(done, ok, &[], bail, &[]);
                self.b.seal_block(ok);
                self.b.seal_block(bail);
                self.b.switch_to_block(bail);
                self.b.ins().return_(&[status]);
                self.b.switch_to_block(ok);
                let vmx = self.vmx();
                let entry = self.b.ins().call(self.fns.rt_cont, &[vmx]);
                // The shim's operand pushes can have grown (moved) the value
                // stack; adopt the base `rt_cont` fetched after them.
                let nb = self.b.inst_results(entry)[0];
                let w = self.b.inst_results(entry)[1];
                self.b.def_var(self.base, nb);
                let int = want_int.then(|| unbox_int(&mut self.b, &self.facts, w));
                AtomVal { word: Some(w), int }
            }
            // A parking op owns two resume ordinals.
            //
            // The first attempt runs here, where the operands are still in
            // registers, and hands them to the shim. If the op parks with
            // `Resume::Retry` it has pushed its operands back onto the value
            // stack, so the retry attempt passes `argc == 0` and the shim
            // consumes those words instead — compiled code could not re-supply
            // them, since a parking op's operands are usually slotless temps
            // with no home once the machine frame is gone.
            //
            // `cont_blk` is both the completion path and the resume target for
            // the ops whose waker leaves the result on the stack. Every block
            // an entry dispatch can land on re-establishes the frame base and
            // its register views before touching a slot.
            Atom::PrimOp { op, args, .. } if is_native_park_op(*op) => {
                let (retry_ord, retry_blk) = self.next_cont();
                let (cont_ord, cont_blk) = self.next_cont();
                let opc = i64::from(*op as u8);

                let attempt = |g: &mut Self, buf: ir::Value, argc: i64| {
                    let o = g.b.ins().iconst(types::I64, opc);
                    let n = g.b.ins().iconst(types::I64, argc);
                    let r = g.b.ins().iconst(types::I64, retry_ord);
                    let c = g.b.ins().iconst(types::I64, cont_ord);
                    let vmx = g.vmx();
                    let call = g.b.ins().call(g.fns.park_op, &[vmx, o, buf, n, r, c]);
                    let status = g.b.inst_results(call)[0];
                    let bail = g.b.create_block();
                    g.b.set_cold_block(bail);
                    let done =
                        g.b.ins()
                            .icmp_imm(IntCC::Equal, status, NativeStatus::Done as u64 as i64);
                    g.b.ins().brif(done, cont_blk, &[], bail, &[]);
                    g.b.seal_block(bail);
                    g.b.switch_to_block(bail);
                    g.b.ins().return_(&[status]);
                };

                // First attempt: operands are live here.
                let buf = self.arg_buffer(args);
                attempt(self, buf, args.len() as i64);

                // Retry: re-entered by the dispatch with the operands already
                // on the value stack.
                self.b.switch_to_block(retry_blk);
                let nb = self.frame_base_now();
                self.b.def_var(self.base, nb);
                let live = self.cont_live[(retry_ord - 1) as usize].clone();
                self.reload_cached_views(&live);
                let empty = self.arg_buffer(&[]);
                attempt(self, empty, 0);

                self.b.switch_to_block(cont_blk);
                let vmx = self.vmx();
                let entry = self.b.ins().call(self.fns.rt_cont, &[vmx]);
                let nb = self.b.inst_results(entry)[0];
                let w = self.b.inst_results(entry)[1];
                self.b.def_var(self.base, nb);
                let live = self.cont_live[(cont_ord - 1) as usize].clone();
                self.reload_cached_views(&live);
                let int = want_int.then(|| unbox_int(&mut self.b, &self.facts, w));
                AtomVal { word: Some(w), int }
            }
            Atom::PrimOp { op, args, imm } if is_native_bridge_op(*op) => {
                let operand = super::emit::imm_operand(*op, *imm);
                let buf = self.arg_buffer(args);
                let opc = self.b.ins().iconst(types::I64, i64::from(*op as u8));
                let opv = self.b.ins().iconst(types::I64, i64::from(operand));
                let n = self.b.ins().iconst(types::I64, args.len() as i64);
                self.shim_op_result(opc, opv, buf, n, want_int)
            }
            // A polymorphic compare whose operands are not both proven Int
            // cannot use the Int fast path; run the interpreter's own
            // comparison through the bridge instead.
            Atom::PrimOp { op, args, imm }
                if matches!(nop_of(*op), Some((_, true)))
                    && !args.iter().all(|&x| self.plan.is_int(x)) =>
            {
                let operand = super::emit::imm_operand(*op, *imm);
                let buf = self.arg_buffer(args);
                let opc = self.b.ins().iconst(types::I64, i64::from(*op as u8));
                let opv = self.b.ins().iconst(types::I64, i64::from(operand));
                let n = self.b.ins().iconst(types::I64, args.len() as i64);
                self.shim_op_result(opc, opv, buf, n, want_int)
            }
            Atom::PrimOp { op, args, .. } => {
                let Some((nop, _)) = nop_of(*op) else {
                    unsupported_node(
                        "primop: `op_coverage` classifies this opcode NotAPrimOp, so lowering should never meet it as one",
                    )
                };
                match nop {
                    NOp::Not => {
                        let [x] = args.as_slice() else {
                            unsupported_node("Not arity")
                        };
                        // Only two Bool words exist and they differ in bit 0,
                        // so negation is a one-bit flip.
                        let w = self.word_of(*x);
                        let flipped = self.b.ins().bxor_imm(w, 1);
                        if want_int {
                            unsupported_node("int view of a Bool");
                        }
                        AtomVal {
                            word: Some(flipped),
                            int: None,
                        }
                    }
                    NOp::Cmp(cc) => {
                        let [x, y] = args.as_slice() else {
                            unsupported_node("compare arity")
                        };
                        let a = self.int_of(*x);
                        let bv = self.int_of(*y);
                        let flag = self.b.ins().icmp(cc, a, bv);
                        if want_int {
                            unsupported_node("int view of a Bool");
                        }
                        AtomVal {
                            word: Some(box_bool(&mut self.b, &self.facts, flag)),
                            int: None,
                        }
                    }
                    NOp::Neg => {
                        let [x] = args.as_slice() else {
                            unsupported_node("negate arity")
                        };
                        let a = self.int_of(*x);
                        let r = self.b.ins().ineg(a);
                        self.int_result(r, want_word)
                    }
                    NOp::Add | NOp::Sub | NOp::Mul | NOp::Div | NOp::Mod => {
                        let [x, y] = args.as_slice() else {
                            unsupported_node("arithmetic arity")
                        };
                        let a = self.int_of(*x);
                        let bv = self.int_of(*y);
                        // Wrapping two's-complement. `/` and `%` route
                        // through shims for the x/0 = 0, x%0 = x rules.
                        let r = match nop {
                            NOp::Add => self.b.ins().iadd(a, bv),
                            NOp::Sub => self.b.ins().isub(a, bv),
                            NOp::Mul => self.b.ins().imul(a, bv),
                            NOp::Div => {
                                let call = self.b.ins().call(self.fns.div_int, &[a, bv]);
                                self.b.inst_results(call)[0]
                            }
                            NOp::Mod => {
                                let call = self.b.ins().call(self.fns.mod_int, &[a, bv]);
                                self.b.inst_results(call)[0]
                            }
                            // A deliberate assertion: only arithmetic ops
                            // reach this table, and a new NOp that does is a
                            // routing bug this panic names.
                            #[allow(unknown_lints, wildcard_over_own_enum)]
                            _ => unsupported_node("arithmetic op"),
                        };
                        self.int_result(r, want_word)
                    }
                }
            }
            Atom::Closure { func_idx, captures } => {
                // One owned reference per capture transfers into the shim,
                // which copies them into the fresh cell and releases the
                // transferred words. The result owns the cell's one reference.
                let buf = self.arg_buffer(captures);
                let fi = self
                    .b
                    .ins()
                    .iconst(types::I64, i64::from(func_idx.to_operand()));
                let n = self.b.ins().iconst(types::I64, captures.len() as i64);
                let vmx = self.vmx();
                let call = self.b.ins().call(self.fns.make_closure, &[vmx, fi, buf, n]);
                let w = self.b.inst_results(call)[0];
                if want_int {
                    unsupported_node("int view of a closure");
                }
                AtomVal {
                    word: Some(w),
                    int: None,
                }
            }
            Atom::Ctor {
                variant,
                fields,
                reuse,
            } => {
                if want_int {
                    unsupported_node("int view of a constructor");
                }
                match self.plan.bools.polarity(variant) {
                    // No allocation, and a perceus reuse pairing is ignored
                    // exactly as `emit_ctor`'s Bool path ignores it.
                    Some(polarity) => {
                        let bits = if polarity {
                            self.facts.bool_true
                        } else {
                            self.facts.bool_false
                        };
                        AtomVal {
                            word: want_word.then(|| self.b.ins().iconst(types::I64, bits as i64)),
                            int: None,
                        }
                    }
                    // Runs regardless of demand: it allocates or consumes the
                    // parked cell, and the result carries the cell's one owned
                    // reference.
                    None => {
                        let site = self.next_ctor_site();
                        let w = self.emit_enum_ctor(site, fields, *reuse);
                        AtomVal {
                            word: Some(w),
                            int: None,
                        }
                    }
                }
            }
            Atom::Call { .. } => unsupported_node("atom in pure position"),
        }
    }

    /// The raw value always, the boxed word only when a use wants it.
    fn int_result(&mut self, raw: ir::Value, want_word: bool) -> AtomVal {
        let word = want_word.then(|| {
            let vmx = self.vmx();
            box_int(&mut self.b, &self.facts, vmx, raw, self.fns.int_box)
        });
        AtomVal {
            word,
            int: Some(raw),
        }
    }

    /// Payload word `i` of the object at `obj`. Word 0 is the header.
    fn load_payload_word(&mut self, obj: ir::Value, word: usize) -> ir::Value {
        self.b.ins().load(
            types::I64,
            MemFlagsData::trusted(),
            obj,
            (8 * (1 + word)) as i32,
        )
    }

    /// Package a field word read out of a heap cell. Retained where an owned
    /// word is wanted, since the cell keeps its own reference.
    fn field_result(&mut self, f: ir::Value, want_word: bool, want_int: bool) -> AtomVal {
        if want_word {
            native_frame::emit_retain(&mut self.b, f);
        }
        let int = want_int.then(|| unbox_int(&mut self.b, &self.facts, f));
        AtomVal {
            word: want_word.then_some(f),
            int,
        }
    }

    /// An owned word for a tail/merge atom, whatever its shape.
    fn owned_atom_word(&mut self, a: &Atom) -> ir::Value {
        match self
            .eval_pure(
                a,
                Want {
                    word: true,
                    int: false,
                },
            )
            .word
        {
            Some(w) => w,
            None => unsupported_node("valueless atom"),
        }
    }

    fn next_ctor_site(&mut self) -> EnumCtorSite {
        let Some(&site) = self.ctor_sites.get(self.ctor_cursor) else {
            ctor_walk_mismatch()
        };
        self.ctor_cursor += 1;
        site
    }

    /// `Op::MakeEnumPayload`'s allocation path: owned field words spill into a
    /// buffer and `al_shim_enum_alloc` builds the cell, releasing the
    /// transferred field references.
    fn enum_alloc(&mut self, site: &EnumCtorSite, fields: &[LocalId]) -> ir::Value {
        let buf = self.arg_buffer(fields);
        let packed = self.b.ins().iconst(types::I64, site.packed as i64);
        let en = self.b.ins().iconst(types::I64, site.enum_name as i64);
        let vn = self.b.ins().iconst(types::I64, site.variant_name as i64);
        let lb = self.b.ins().iconst(types::I64, site.labels as i64);
        let n = self.b.ins().iconst(types::I64, fields.len() as i64);
        let vmx = self.vmx();
        let call = self
            .b
            .ins()
            .call(self.fns.enum_alloc, &[vmx, packed, en, vn, lb, buf, n]);
        self.b.inst_results(call)[0]
    }

    /// A non-Bool `Atom::Ctor`, mirroring `Op::Reuse` + `Op::MakeEnumPayload`.
    /// `site.reuse` reads the emitted `MakeEnumPayload.a`, so pinning agrees
    /// with emit by construction. With a reuse, the candidate transfers out of
    /// its slot; a uniquely-owned mortal cell is overwritten in place and
    /// anything else falls back to a fresh allocation.
    fn emit_enum_ctor(
        &mut self,
        site: EnumCtorSite,
        fields: &[LocalId],
        reuse: Option<LocalId>,
    ) -> ir::Value {
        if !site.reuse {
            return self.enum_alloc(&site, fields);
        }
        let Some(r) = reuse else {
            unsupported_node("reuse site without a reuse local")
        };
        let slot = self.slot_of(r);
        let w = self.load_slot(slot);
        self.store_slot_zero(slot);

        let merge = self.b.create_block();
        self.b.append_block_param(merge, types::I64);
        let rc_block = self.b.create_block();
        let inplace = self.b.create_block();
        let miss = self.b.create_block();

        // The parked candidate is either a hollowed mortal cell or the zero
        // a shared `Drop` left, so the mortal gate stays dynamic.
        native_rc::emit_mortal_gate(&mut self.b, w, RcGate::Dynamic, rc_block, miss);

        self.b.switch_to_block(rc_block);
        let obj = self.b.ins().band_imm(w, NATIVE_PTR_MASK as i64);
        let rc = self.b.ins().load(
            types::I64,
            MemFlagsData::trusted(),
            obj,
            NATIVE_RC_BYTE_OFFSET,
        );
        let unique = self.b.ins().icmp_imm(IntCC::Equal, rc, 1);
        self.b.ins().brif(unique, inplace, &[], miss, &[]);
        self.b.seal_block(inplace);
        self.b.seal_block(miss);

        // In place over a hollowed same-shape cell. The header is
        // byte-identical by the shape-pairing invariant and left alone; every
        // other word is rewritten, hash 0 included, so the old cached hash
        // cannot leak into the new value. The name/label/payload slots hold
        // `hollow_for_reuse`'s sentinels, so plain stores suffice.
        self.b.switch_to_block(inplace);
        let packed = self.b.ins().iconst(types::I64, site.packed as i64);
        self.b.ins().store(MemFlagsData::trusted(), packed, obj, 8);
        let zero = self.b.ins().iconst(types::I64, 0);
        self.b.ins().store(MemFlagsData::trusted(), zero, obj, 16);
        let en = self.b.ins().iconst(types::I64, site.enum_name as i64);
        self.b.ins().store(MemFlagsData::trusted(), en, obj, 24);
        let vn = self.b.ins().iconst(types::I64, site.variant_name as i64);
        self.b.ins().store(MemFlagsData::trusted(), vn, obj, 32);
        let lb = self.b.ins().iconst(types::I64, site.labels as i64);
        self.b.ins().store(MemFlagsData::trusted(), lb, obj, 40);
        let n = self.b.ins().iconst(types::I64, fields.len() as i64);
        self.b.ins().store(MemFlagsData::trusted(), n, obj, 48);
        for (i, &fld) in fields.iter().enumerate() {
            // The cell takes its own reference: a slotted field is retained,
            // a slotless one transfers.
            let fw = self.owned_word(fld);
            self.b
                .ins()
                .store(MemFlagsData::trusted(), fw, obj, 56 + 8 * i as i32);
        }
        self.b.ins().jump(merge, &[w.into()]);

        // Miss: drop the non-reusable candidate and allocate fresh.
        self.b.switch_to_block(miss);
        emit_drop(&mut self.b, w, self.fns.release, RcGate::Dynamic);
        let fresh = self.enum_alloc(&site, fields);
        self.b.ins().jump(merge, &[fresh.into()]);

        self.b.seal_block(merge);
        self.b.switch_to_block(merge);
        self.b.block_params(merge)[0]
    }

    /// The next continuation ordinal (1-based; 0 is the head) and its block.
    fn next_cont(&mut self) -> (i64, ir::Block) {
        let Some(&block) = self.cont_blocks.get(self.cont_cursor) else {
            resume_walk_mismatch()
        };
        self.cont_cursor += 1;
        (self.cont_cursor as i64, block)
    }

    /// Act on a `PreparedCall`: null entry returns `aux` as the status;
    /// otherwise control transfers to `entry` at resume ordinal `aux` via a
    /// machine tail call — the caller's native frame is gone before the
    /// target runs.
    fn transfer(&mut self, entry: ir::Value, aux: ir::Value) {
        let go = self.b.create_block();
        let bail = self.b.create_block();
        self.b.set_cold_block(bail);
        let is_null = self.b.ins().icmp_imm(IntCC::Equal, entry, 0);
        self.b.ins().brif(is_null, bail, &[], go, &[]);
        self.b.seal_block(bail);
        self.b.seal_block(go);
        self.b.switch_to_block(bail);
        self.b.ins().return_(&[aux]);
        self.b.switch_to_block(go);
        let ctx = self.ctx();
        self.b
            .ins()
            .return_call_indirect(self.sig_ref, entry, &[ctx, aux]);
    }

    /// Spill owned argument words into a native stack buffer for a runtime
    /// call shim, returning its address.
    fn arg_buffer(&mut self, args: &[LocalId]) -> ir::Value {
        let size = (args.len().max(1) * 8) as u32;
        let ss = self.b.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            size,
            3,
        ));
        for (i, &a) in args.iter().enumerate() {
            let w = self.owned_word(a);
            self.b.ins().stack_store(w, ss, (i * 8) as i32);
        }
        self.b.ins().stack_addr(self.ptr_ty, ss, 0)
    }

    /// Return `status` unless it is `Done`, which keeps every native frame
    /// droppable at a suspension. Leaves the builder in the continue block.
    /// A non-tail call, as a transfer plus a continuation. The prepare shim
    /// pushes the callee frame and stamps this frame's `ip` with the
    /// continuation ordinal; control tail-transfers to a native callee or
    /// returns `Done` for the trampoline to dispatch an interpreted one.
    /// Either way this native frame is gone while the callee runs, and the
    /// continuation block — entered only through the entry dispatch — picks
    /// the result off the value stack.
    ///
    /// `moved` are [`peel_call_arg_drops`]'s peeled last-use drops. They must
    /// run after the operand copies but *before* the transfer, so the callee
    /// is sole owner and its Perceus `Drop` sees rc==1.
    fn call_value(
        &mut self,
        callee: Callee,
        args: &[LocalId],
        moved: &[(LocalId, bool)],
    ) -> ir::Value {
        let (resume, cont) = self.next_cont();
        let buf = self.arg_buffer(args);
        // Emit's operand order: args, then a dynamic callee, then the peeled
        // drops. The dispatch word must be resolved before the drops. `Self_`
        // is an immediate like `Known`, since covered bodies never read
        // captures. A dynamic callee is an owned closure word that becomes
        // the callee frame's `captures` handle.
        let dispatch = match callee {
            Callee::Known(f) => Ok(self.b.ins().iconst(types::I64, i64::from(f.to_operand()))),
            Callee::Self_ => {
                let fi = self.plan.func_idx;
                Ok(self.b.ins().iconst(types::I64, i64::from(fi.to_operand())))
            }
            Callee::Local(x) => Err(self.owned_word(x)),
        };
        for &(m, reusable) in moved {
            self.drop_local(m, reusable);
        }
        let r = self.b.ins().iconst(types::I64, resume);
        let n = self.b.ins().iconst(types::I64, args.len() as i64);
        let vmx = self.vmx();
        let prepared = match dispatch {
            Ok(t) => self
                .b
                .ins()
                .call(self.fns.prepare_call, &[vmx, t, r, buf, n]),
            Err(cw) => self
                .b
                .ins()
                .call(self.fns.prepare_call_value, &[vmx, cw, r, buf, n]),
        };
        let entry = self.b.inst_results(prepared)[0];
        let aux = self.b.inst_results(prepared)[1];
        self.transfer(entry, aux);

        // The continuation: a fresh entry into this function. Nothing from
        // before the call survives in machine registers — every cached view
        // is redefined from its frame slot (which Perceus already keeps
        // authoritative at every call); slotless single-use temps are dead
        // here by construction and their cache entries drop.
        self.b.switch_to_block(cont);
        let vmx = self.vmx();
        // One crossing for the whole prologue: refetched base + the result.
        let entry = self.b.ins().call(self.fns.rt_cont, &[vmx]);
        let nb = self.b.inst_results(entry)[0];
        let result = self.b.inst_results(entry)[1];
        self.b.def_var(self.base, nb);
        // A continuation is a fresh machine entry: nothing computed before the
        // transfer survives in a register. Redefine the live cached views HERE,
        // at the continuation's top, which dominates all downstream blocks — a
        // lazy reload inside a later if-arm would define a Variable the sibling
        // arm does not see. Only locals live after this continuation are
        // reloaded, so a dead local's freed slot is never dereferenced.
        let live = self.cont_live[(resume - 1) as usize].clone();
        self.reload_cached_views(&live);
        result
    }

    /// Reload each live cached local from its frame slot (or constant) into its
    /// existing Variable, at a continuation's dominating top. A local absent
    /// from `live` is dead here — dropping its cache entry keeps a stray later
    /// use as a loud build error rather than reloading (and unboxing) a freed
    /// slot. Int views are re-derived from the reloaded word.
    fn reload_cached_views(&mut self, live: &HashSet<LocalId>) {
        let n = self.words.len().max(self.ints.len());
        let ids: Vec<LocalId> = (0..n)
            .map(LocalId::from_usize)
            .filter(|&id| {
                self.words.get(id).copied().flatten().is_some()
                    || self.ints.get(id).copied().flatten().is_some()
            })
            .collect();
        for id in ids {
            if !live.contains(&id) {
                // Dead here: forget it so a later use is a loud miss, never a
                // dereference of a stale slot.
                if self.words.get(id).copied().flatten().is_some() {
                    self.words[id] = None;
                }
                if self.ints.get(id).copied().flatten().is_some() {
                    self.ints[id] = None;
                }
                continue;
            }
            let word_var = self.words.get(id).copied().flatten();
            let int_var = self.ints.get(id).copied().flatten();
            if let Some(slot) = self.layout.slot(id) {
                let w = self.load_slot(slot);
                if let Some(wv) = word_var {
                    self.b.def_var(wv, w);
                }
                if let Some(iv) = int_var {
                    let d = unbox_int(&mut self.b, &self.facts, w);
                    self.b.def_var(iv, d);
                }
            } else if let Some((bits, int)) = self.const_locals.get(id).copied().flatten() {
                if let Some(wv) = word_var {
                    let w = self.b.ins().iconst(types::I64, bits as i64);
                    self.b.def_var(wv, w);
                }
                if let Some(iv) = int_var {
                    let Some(i) = int else {
                        unsupported_node("non-Int constant in Int position")
                    };
                    let d = self.b.ins().iconst(types::I64, i);
                    self.b.def_var(iv, d);
                }
            } else {
                // Consumed temp: forget it.
                if word_var.is_some() {
                    self.words[id] = None;
                }
                if int_var.is_some() {
                    self.ints[id] = None;
                }
            }
        }
    }

    /// A cross-function tail call, as a transfer: collapse the frame in
    /// place (interpreter surgery, `ip = 0`), then tail-transfer to a native
    /// target or return `Done` for the trampoline. Machine tail chains cost
    /// one `return_call_indirect` per hop and never grow any stack.
    fn tail_call_known(&mut self, target: FuncIdx, args: &[LocalId]) {
        let buf = self.arg_buffer(args);
        let t = self
            .b
            .ins()
            .iconst(types::I64, i64::from(target.to_operand()));
        let n = self.b.ins().iconst(types::I64, args.len() as i64);
        let vmx = self.vmx();
        let prepared = self.b.ins().call(self.fns.prepare_tail, &[vmx, t, buf, n]);
        let entry = self.b.inst_results(prepared)[0];
        let aux = self.b.inst_results(prepared)[1];
        self.transfer(entry, aux);
    }

    /// [`Self::tail_call_known`] with an owned closure word instead of an
    /// immediate target. The collapsed frame takes it as its `captures`
    /// handle.
    fn tail_call_value(&mut self, callee: LocalId, args: &[LocalId]) {
        let buf = self.arg_buffer(args);
        let cw = self.owned_word(callee);
        let n = self.b.ins().iconst(types::I64, args.len() as i64);
        let vmx = self.vmx();
        let prepared = self
            .b
            .ins()
            .call(self.fns.prepare_tail_value, &[vmx, cw, buf, n]);
        let entry = self.b.inst_results(prepared)[0];
        let aux = self.b.inst_results(prepared)[1];
        self.transfer(entry, aux);
    }

    /// The native loop back-edge, in the interpreter's `TailCallSelf` order:
    /// copy the argument words, run the parked argument-slot drops, swap the
    /// copies into the parameter slots, checkpoint. Anything but `Done`
    /// unwinds with the frame resumable at ip 0.
    fn self_tail(&mut self, args: &[LocalId], moved_drops: &[(LocalId, bool)]) {
        let params: Vec<LocalId> = self.plan.fun.params.iter().map(|p| p.id).collect();
        if args.len() != params.len() {
            unsupported_node("self-tail arity");
        }
        let words: Vec<ir::Value> = args.iter().map(|&a| self.owned_word(a)).collect();
        let ints: Vec<Option<ir::Value>> = args
            .iter()
            .map(|&a| {
                self.ints
                    .get(a)
                    .copied()
                    .flatten()
                    .map(|v| self.b.use_var(v))
            })
            .collect();
        for &(d, reusable) in moved_drops {
            self.drop_local(d, reusable);
        }
        for (i, &p) in params.iter().enumerate() {
            let slot = self.slot_of(p);
            self.store_local_slot(p, slot, words[i]);
        }
        for (i, &p) in params.iter().enumerate() {
            // Registers only: the store above handed the slot its owned word.
            self.def_regs(
                p,
                AtomVal {
                    word: Some(words[i]),
                    int: ints[i],
                },
            );
        }
        let vmx = self.vmx();
        let call = self.b.ins().call(self.fns.rt_checkpoint, &[vmx]);
        let status = self.b.inst_results(call)[0];
        // `base` points into the scheduler's `VM::stack` Vec, and the
        // checkpoint is a suspension point. Refetch unconditionally so the
        // loop-carried value is scheduler-clean by construction.
        let nb = self.frame_base_now();
        self.b.def_var(self.base, nb);
        let bail = self.b.create_block();
        self.b.set_cold_block(bail);
        let ok = self
            .b
            .ins()
            .icmp_imm(IntCC::Equal, status, NativeStatus::Done as u64 as i64);
        self.b.ins().brif(ok, self.loop_head, &[], bail, &[]);
        self.b.seal_block(bail);
        self.b.switch_to_block(bail);
        self.b.ins().return_(&[status]);
    }

    /// `Op::Drop` for a covered body. `reusable` means the IR node carried a
    /// `ReuseShape`.
    ///
    /// A non-reusable drop never pairs with a `Reuse`, so it releases through
    /// the mortal gate and zeroes the slot. Freeing now instead of at `Ret`
    /// like the interpreter is observationally identical.
    ///
    /// A reusable drop hollows a uniquely-owned cell and parks it *in its
    /// frame slot* for a paired `Ctor { reuse }`. Hollowing at the drop point
    /// is what lets reuse propagate down a recursive chain. A shared value
    /// releases and zeroes instead, so the paired reuse allocates fresh. The
    /// slot survives a self-tail back-edge untouched, matching
    /// `collapse_tail_frame_self`.
    fn drop_local(&mut self, id: LocalId, reusable: bool) {
        let slot = self.slot_of(id);
        // The slot holds `id`'s own value here: a local is dropped at most
        // once per path, after its binding store.
        let Some(gate) = self.plan.rc_gate(id) else {
            // Proven immediate: nothing to release or park. Immediates are
            // never unique, so the interpreter also just clears the slot.
            self.store_slot_zero(slot);
            return;
        };
        let w = self.load_slot(slot);
        if !reusable {
            emit_drop(&mut self.b, w, self.fns.release, gate);
            self.store_slot_zero(slot);
            return;
        }

        let rc_block = self.b.create_block();
        let hollow_block = self.b.create_block();
        let shared_block = self.b.create_block();
        let dec_block = self.b.create_block();
        let clear_block = self.b.create_block();
        let done_block = self.b.create_block();

        // Pure bit math; never reads the object.
        native_rc::emit_mortal_gate(&mut self.b, w, gate, rc_block, clear_block);

        // Uniqueness: rc == 1 means the frame is the sole owner.
        self.b.switch_to_block(rc_block);
        let obj = self.b.ins().band_imm(w, NATIVE_PTR_MASK as i64);
        let rc = self.b.ins().load(
            types::I64,
            MemFlagsData::trusted(),
            obj,
            NATIVE_RC_BYTE_OFFSET,
        );
        let unique = self.b.ins().icmp_imm(IntCC::Equal, rc, 1);
        self.b
            .ins()
            .brif(unique, hollow_block, &[], shared_block, &[]);
        self.b.seal_block(hollow_block);
        self.b.seal_block(shared_block);

        // Unique: release the children now and park the hollowed cell. The
        // slot already holds it.
        self.b.switch_to_block(hollow_block);
        self.b.ins().call(self.fns.hollow, &[obj]);
        self.b.ins().jump(done_block, &[]);

        // Shared: rc >= 2 here, since rc == 1 took the hollow path, so the
        // decrement can never reach zero and no free-at-zero call is needed.
        // A saturated count is permanently live and never decrements.
        self.b.switch_to_block(shared_block);
        let saturated = self.b.ins().icmp_imm(IntCC::Equal, rc, -1);
        self.b
            .ins()
            .brif(saturated, clear_block, &[], dec_block, &[]);
        self.b.seal_block(dec_block);

        self.b.switch_to_block(dec_block);
        let dec = self.b.ins().iadd_imm(rc, -1);
        self.b
            .ins()
            .store(MemFlagsData::trusted(), dec, obj, NATIVE_RC_BYTE_OFFSET);
        self.b.ins().jump(clear_block, &[]);
        self.b.seal_block(clear_block);

        self.b.switch_to_block(clear_block);
        self.store_slot_zero(slot);
        self.b.ins().jump(done_block, &[]);
        self.b.seal_block(done_block);

        self.b.switch_to_block(done_block);
    }

    fn expr(&mut self, mut e: &CoreExpr, dst: Dest) {
        loop {
            match e {
                CoreExpr::Let { bind, rhs, body } => {
                    if let Atom::Call { callee, args } = rhs {
                        // Peel the operands' last-use drops out of `body`
                        // into the call so the callee sees rc==1.
                        let dyn_callee = match callee {
                            Callee::Local(x) => Some(*x),
                            Callee::Known(_) | Callee::Self_ => None,
                        };
                        let (moved, rest) = peel_call_arg_drops(body, args, dyn_callee, &self.uses);
                        let w = self.call_value(*callee, args, &moved);
                        self.def_local(
                            bind.id,
                            AtomVal {
                                word: Some(w),
                                int: None,
                            },
                        );
                        e = rest;
                        continue;
                    } else {
                        // A `Closure` rhs must run regardless of demand: it
                        // allocates, and its result carries an owned
                        // reference the slot must take.
                        let want_word = self.layout.slot(bind.id).is_some()
                            || self.uses.word_uses(bind.id) > 0
                            || matches!(rhs, Atom::Closure { .. });
                        let want_int = self.uses.int_demand(bind.id);
                        if let Atom::Const(c) = rhs
                            && let Some(cv) = self.cmap.get(&c.0)
                        {
                            self.const_locals.resize_at_least(bind.id, None);
                            self.const_locals[bind.id] = Some((cv.bits, cv.int));
                        }
                        // A rhs operand whose `Drop` directly follows this
                        // bind is at its last use: mark it so `owned_word`
                        // moves the slot reference instead of dup+drop —
                        // which is what lets `map.set`/`map.delete` see a
                        // unique map and edit it in place. Currently limited
                        // to the map ops: they are pure, consume every
                        // operand exactly once, and have the in-place
                        // runtime path that pays for it. (Widening this to
                        // every pure bridge op crashed the http goldens —
                        // some op re-reads a vacated slot; find it before
                        // generalizing.) Only reads that actually go through
                        // `owned_word` fuse; leftovers are cleared and their
                        // drops emit as written.
                        if let Atom::PrimOp { op, args, .. } = rhs
                            && (is_native_bridge_op(*op)
                                // Inline-lowered, but they consume every
                                // operand through the same owned-word spill
                                // the bridge shims use — and moving the
                                // sequence in is what lets a sole owner's
                                // array push edit the tree in place.
                                || matches!(*op, Op::Append | Op::Prepend))
                        {
                            // One exception to "a Drop marks the last use":
                            // `drop x; tail self(..x..)` is legal IR — the
                            // tail's operand copy runs first and the drop is
                            // sunk past it (`release_self_tail_args`). A
                            // local the terminal tail call still reads must
                            // keep its slot reference.
                            let mut after = body.as_ref();
                            let mut chain = Vec::new();
                            while let CoreExpr::Drop {
                                local,
                                shape: None,
                                body: b,
                            } = after
                            {
                                chain.push(*local);
                                after = b;
                            }
                            if let CoreExpr::Tail(Atom::Call {
                                callee: Callee::Self_,
                                args: targs,
                            }) = after
                            {
                                chain.retain(|x| !targs.contains(x));
                            }
                            for local in chain {
                                if args.iter().filter(|&&a| a == local).count() == 1
                                    && self.layout.slot(local).is_some()
                                    && self.plan.rc_gate(local).is_some()
                                {
                                    self.move_args.push(local);
                                }
                            }
                        }
                        let want = Want {
                            word: want_word,
                            int: want_int,
                        };
                        let v = self.eval_pure(rhs, want);
                        self.move_args.clear();
                        self.def_local(bind.id, v);
                    }
                    e = body;
                }
                CoreExpr::LetJoin { bind, join, body } => {
                    let mb = self.b.create_block();
                    self.b.append_block_param(mb, types::I64);
                    self.expr(join, Dest::Merge(mb));
                    self.b.switch_to_block(mb);
                    let w = self.b.block_params(mb)[0];
                    self.def_local(
                        bind.id,
                        AtomVal {
                            word: Some(w),
                            int: None,
                        },
                    );
                    e = body;
                }
                CoreExpr::LetCont { id, cont, body } => {
                    let cb = self.b.create_block();
                    self.joins.resize_at_least(*id, None);
                    self.joins[*id] = Some(cb);
                    // Body first, cont after: emit's order, which the
                    // resume-ip cursor depends on.
                    self.expr(body, dst);
                    self.b.switch_to_block(cb);
                    e = cont;
                }
                CoreExpr::Drop { .. } => {
                    // `drop x…; tail self(args)` releases argument slots
                    // between the operand copies and the frame swap. Mirror
                    // emit's split so a dropped argument survives into its
                    // copy.
                    if matches!(dst, Dest::Ret)
                        && let Some((now, moved, args)) = split_self_tail(e)
                    {
                        for (x, reusable) in now {
                            // A drop already fused into an op's operand move
                            // has nothing left to release; its slot is clear.
                            if let Some(i) = self.consumed_drops.iter().position(|&c| c == x) {
                                self.consumed_drops.swap_remove(i);
                            } else {
                                self.drop_local(x, reusable);
                            }
                        }
                        self.self_tail(args, &moved);
                        return;
                    }
                    let CoreExpr::Drop { local, shape, body } = e else {
                        unsupported_node("drop shape")
                    };
                    if let Some(i) = self.consumed_drops.iter().position(|&x| x == *local) {
                        // The reference already moved into the op that read
                        // it; the slot is cleared and there is nothing left
                        // to release.
                        self.consumed_drops.swap_remove(i);
                    } else {
                        self.drop_local(*local, shape.is_some());
                    }
                    e = body;
                }
                CoreExpr::If {
                    cond, then, els, ..
                } => {
                    let w = self.word_of(*cond);
                    // The condition is proven Bool and only two Bool words
                    // exist, so truthiness is one comparison.
                    let t = self
                        .b
                        .ins()
                        .icmp_imm(IntCC::Equal, w, self.facts.bool_true as i64);
                    let tb = self.b.create_block();
                    let eb = self.b.create_block();
                    self.b.ins().brif(t, tb, &[], eb, &[]);
                    self.b.seal_block(tb);
                    self.b.seal_block(eb);
                    self.b.switch_to_block(tb);
                    self.expr(then, dst);
                    self.b.switch_to_block(eb);
                    e = els;
                }
                CoreExpr::Match { scrut, arms, .. } => {
                    if let Some(tags) = self.switch_tags(arms) {
                        self.match_switch(*scrut, arms, &tags, dst);
                    } else {
                        self.match_ladder(*scrut, arms, dst);
                    }
                    return;
                }
                CoreExpr::Tail(a) => {
                    self.tail_atom(a, dst);
                    return;
                }
                CoreExpr::Goto(id) => {
                    let Some(cb) = self.joins.get(*id).copied().flatten() else {
                        unsupported_node("goto to undeclared join")
                    };
                    self.b.ins().jump(cb, &[]);
                    return;
                }
            }
        }
    }

    fn tail_atom(&mut self, a: &Atom, dst: Dest) {
        match (a, dst) {
            (
                Atom::Call {
                    callee: Callee::Self_,
                    args,
                },
                Dest::Ret,
            ) => self.self_tail(args, &[]),
            (
                Atom::Call {
                    callee: Callee::Known(f),
                    args,
                },
                Dest::Ret,
            ) => self.tail_call_known(*f, args),
            (
                Atom::Call {
                    callee: Callee::Local(x),
                    args,
                },
                Dest::Ret,
            ) => self.tail_call_value(*x, args),
            (Atom::Call { callee, args }, Dest::Merge(mb)) => {
                // A call in operand position is an ordinary (non-tail) call
                // whose result feeds the merge. Nothing follows a `Tail` in
                // its spine, so there are no arg drops to peel.
                let w = self.call_value(*callee, args, &[]);
                self.b.ins().jump(mb, &[w.into()]);
            }
            (a, Dest::Ret) => {
                let w = self.owned_atom_word(a);
                let vmx = self.vmx();
                let prepared = self.b.ins().call(self.fns.ret_transfer, &[vmx, w]);
                let entry = self.b.inst_results(prepared)[0];
                let aux = self.b.inst_results(prepared)[1];
                self.transfer(entry, aux);
            }
            (a, Dest::Merge(mb)) => {
                let w = self.owned_atom_word(a);
                self.b.ins().jump(mb, &[w.into()]);
            }
        }
    }

    /// A `CorePat::Bind` binder takes the scrutinee's value: an owned word,
    /// plus the raw int view where one exists.
    fn bind_scrutinee(&mut self, scrut: LocalId, binder: LocalId) {
        let w = self.owned_word(scrut);
        let int = self
            .ints
            .get(scrut)
            .copied()
            .flatten()
            .map(|v| self.b.use_var(v));
        self.def_local(binder, AtomVal { word: Some(w), int });
    }

    /// emit's `switch_plan`, answered from the plan's captured variant counts
    /// so codegen takes the `SwitchTag` table exactly when the emitted
    /// bytecode did. Mirrors `switch_plan` clause for clause.
    fn switch_tags(&self, arms: &[(CorePat, CoreExpr)]) -> Option<Vec<u16>> {
        if arms.is_empty() {
            return None;
        }
        let mut tid: Option<TypeId> = None;
        let mut tags = Vec::with_capacity(arms.len());
        for (pat, _) in arms {
            let CorePat::Ctor { variant, .. } = pat else {
                return None;
            };
            match tid {
                None => tid = Some(variant.type_id),
                Some(t) if t != variant.type_id => return None,
                _ => {}
            }
            tags.push(variant.variant_idx);
        }
        let count = self.plan.switch_counts.get(&tid?).copied()?;
        if arms.len() != count as usize {
            return None;
        }
        let mut seen = vec![false; count as usize];
        for &t in &tags {
            let i = t as usize;
            if i >= count as usize || seen[i] {
                return None;
            }
            seen[i] = true;
        }
        Some(tags)
    }

    /// The `SwitchTag` fast path: one indexed jump through a table of arm
    /// blocks. The eligible shape proves the scrutinee an enum cell, so a
    /// non-heap word or an out-of-table tag takes the outlined cold trap and
    /// surfaces `NativeStatus::Error`.
    fn match_switch(
        &mut self,
        scrut: LocalId,
        arms: &[(CorePat, CoreExpr)],
        tags: &[u16],
        dst: Dest,
    ) {
        let sw = self.word_of(scrut);
        let trap = self.b.create_block();
        self.b.set_cold_block(trap);
        let dispatch = self.b.create_block();
        let g = self.b.ins().band_imm(sw, NATIVE_MORTAL_HEAP_BITS as i64);
        let is_heap = self
            .b
            .ins()
            .icmp_imm(IntCC::Equal, g, NATIVE_MORTAL_HEAP_BITS as i64);
        self.b.ins().brif(is_heap, dispatch, &[], trap, &[]);
        self.b.seal_block(dispatch);
        self.b.switch_to_block(dispatch);
        let obj = self.b.ins().band_imm(sw, NATIVE_PTR_MASK as i64);
        // Enum payload word 0 is `type_id | variant_idx << 32`.
        let w0 = self.load_payload_word(obj, 0);
        let hi = self.b.ins().ushr_imm(w0, 32);
        let tag64 = self.b.ins().band_imm(hi, 0xFFFF);
        // `br_table` dispatches on an i32 index.
        let tag = self.b.ins().ireduce(types::I32, tag64);
        let arm_blocks: Vec<ir::Block> = arms.iter().map(|_| self.b.create_block()).collect();
        let mut by_tag = vec![trap; tags.len()];
        for (i, &t) in tags.iter().enumerate() {
            by_tag[t as usize] = arm_blocks[i];
        }
        let default_call = self.b.func.dfg.block_call(trap, &[]);
        let table: Vec<ir::BlockCall> = by_tag
            .iter()
            .map(|&bb| self.b.func.dfg.block_call(bb, &[]))
            .collect();
        let jt = self
            .b
            .create_jump_table(JumpTableData::new(default_call, &table));
        self.b.ins().br_table(tag, jt);
        self.b.seal_block(trap);
        for &bb in &arm_blocks {
            self.b.seal_block(bb);
        }
        self.b.switch_to_block(trap);
        let err = self
            .b
            .ins()
            .iconst(types::I64, NativeStatus::Error as u64 as i64);
        self.b.ins().return_(&[err]);
        for ((pat, body), &bb) in arms.iter().zip(&arm_blocks) {
            self.b.switch_to_block(bb);
            if let CorePat::Ctor { fields, .. } = pat {
                self.bind_payload(obj, fields);
            }
            self.expr(body, dst);
        }
    }

    /// Spill the matched cell's payload into the pattern's field binds.
    /// Payload fields start at payload word 6. Each word is retained at its
    /// bind's gate strength and the bind's slot takes the reference.
    fn bind_payload(&mut self, obj: ir::Value, fields: &[CoreBind]) {
        for (i, bind) in fields.iter().enumerate() {
            let f = self.load_payload_word(obj, ENUM_FIELDS_WORD + i);
            if let Some(gate) = self.plan.rc_gate(bind.id) {
                emit_dup(&mut self.b, f, gate);
            }
            self.def_local(
                bind.id,
                AtomVal {
                    word: Some(f),
                    int: None,
                },
            );
        }
    }

    /// The compare ladder: arms in order, each refutable head a branch.
    /// Arms behind an irrefutable one are dead but still walked, in detached
    /// blocks, so the resume-ip cursor stays aligned with emit.
    fn match_ladder(&mut self, scrut: LocalId, arms: &[(CorePat, CoreExpr)], dst: Dest) {
        let sw = self.word_of(scrut);
        let mut matched = false;
        for (pat, body) in arms {
            if matched {
                let dead = self.b.create_block();
                self.b.switch_to_block(dead);
                // A dead arm's binders must still be defined, or a binder
                // reference in the dead body aborts compilation.
                if let CorePat::Bind(bind) = pat {
                    self.bind_scrutinee(scrut, bind.id);
                }
                if let CorePat::Ctor { variant, fields } = pat
                    && self.plan.bools.polarity(variant).is_none()
                {
                    let obj = self.b.ins().band_imm(sw, NATIVE_PTR_MASK as i64);
                    self.bind_payload(obj, fields);
                }
                self.expr(body, dst);
                continue;
            }
            match pat {
                CorePat::Lit(c) => {
                    let Some(cv) = self.cmap.get(&c.0) else {
                        unsupported_node("unresolved literal arm")
                    };
                    let (bits, boolean, int) = (cv.bits, cv.boolean, cv.int);
                    let int_bits = int.filter(|_| self.plan.is_int(scrut));
                    let flag = if boolean.is_some() {
                        self.b.ins().icmp_imm(IntCC::Equal, sw, bits as i64)
                    } else if let Some(k) = int_bits {
                        // Compare decoded values, so a spilled BigInt
                        // scrutinee still matches its literal.
                        let si = self.int_of(scrut);
                        self.b.ins().icmp_imm(IntCC::Equal, si, k)
                    } else {
                        // A String literal arm, or an Int literal against a
                        // scrutinee the pool could not prove Int: equality is
                        // structural, not a bit compare. Run the interpreter's
                        // own `values_equal` over the two words.
                        // The bridge consumes both operands, and later arms
                        // still read the scrutinee, so hand it a retained copy
                        // regardless of whether it owns a slot. The literal is
                        // a frozen immortal: its release is a no-op.
                        let lit = self.b.ins().iconst(types::I64, bits as i64);
                        let w = self.word_of(scrut);
                        if let Some(gate) = self.plan.rc_gate(scrut) {
                            emit_dup(&mut self.b, w, gate);
                        }
                        let eq = self.eq_words(w, lit);
                        self.b
                            .ins()
                            .icmp_imm(IntCC::Equal, eq, self.facts.bool_true as i64)
                    };
                    let arm_b = self.b.create_block();
                    let next_b = self.b.create_block();
                    self.b.ins().brif(flag, arm_b, &[], next_b, &[]);
                    self.b.seal_block(arm_b);
                    self.b.seal_block(next_b);
                    self.b.switch_to_block(arm_b);
                    self.expr(body, dst);
                    self.b.switch_to_block(next_b);
                }
                CorePat::Bind(bind) => {
                    self.bind_scrutinee(scrut, bind.id);
                    self.expr(body, dst);
                    matched = true;
                }
                CorePat::Wild => {
                    self.expr(body, dst);
                    matched = true;
                }
                CorePat::Ctor { variant, fields } => {
                    if let Some(polarity) = self.plan.bools.polarity(variant) {
                        // The scrutinee is one of the two Bool immediates, so
                        // the test is a bit compare. Bool heads are nullary,
                        // so there are no payload binds.
                        let bits = if polarity {
                            self.facts.bool_true
                        } else {
                            self.facts.bool_false
                        };
                        let flag = self.b.ins().icmp_imm(IntCC::Equal, sw, bits as i64);
                        let arm_b = self.b.create_block();
                        let next_b = self.b.create_block();
                        self.b.ins().brif(flag, arm_b, &[], next_b, &[]);
                        self.b.seal_block(arm_b);
                        self.b.seal_block(next_b);
                        self.b.switch_to_block(arm_b);
                        self.expr(body, dst);
                        self.b.switch_to_block(next_b);
                    } else {
                        // One packed compare against payload word 0
                        // (`type_id | variant_idx << 32`), which every enum
                        // constructor writes, so the name words are never
                        // touched. The heap gate mirrors `as_enum`'s
                        // `None` meaning no match.
                        let arm_b = self.b.create_block();
                        let next_b = self.b.create_block();
                        let tag_b = self.b.create_block();
                        let g = self.b.ins().band_imm(sw, NATIVE_MORTAL_HEAP_BITS as i64);
                        let is_heap =
                            self.b
                                .ins()
                                .icmp_imm(IntCC::Equal, g, NATIVE_MORTAL_HEAP_BITS as i64);
                        self.b.ins().brif(is_heap, tag_b, &[], next_b, &[]);
                        self.b.seal_block(tag_b);
                        self.b.switch_to_block(tag_b);
                        let obj = self.b.ins().band_imm(sw, NATIVE_PTR_MASK as i64);
                        let w0 = self.load_payload_word(obj, 0);
                        let packed = crate::bytecode::value::pack_variant(
                            variant.type_id,
                            variant.variant_idx,
                        );
                        let hit = self.b.ins().icmp_imm(IntCC::Equal, w0, packed);
                        self.b.ins().brif(hit, arm_b, &[], next_b, &[]);
                        self.b.seal_block(arm_b);
                        self.b.seal_block(next_b);
                        self.b.switch_to_block(arm_b);
                        self.bind_payload(obj, fields);
                        self.expr(body, dst);
                        self.b.switch_to_block(next_b);
                    }
                }
            }
        }
        if !matched {
            // Fell through: an exhaustiveness bug, where the interpreter
            // halts. Surface an error status so the VM reports it instead of
            // executing garbage.
            let err = self
                .b
                .ins()
                .iconst(types::I64, NativeStatus::Error as u64 as i64);
            self.b.ins().return_(&[err]);
        }
    }
}

/// The two backends walked the same IR differently. A backend bug.
#[allow(clippy::panic)]
#[cold]
#[inline(never)]
fn resume_walk_mismatch() -> ! {
    panic!(
        "internal compiler error: native backend call-site walk diverged from emit's \
         recorded resume ips. Report this as a compiler bug."
    )
}

/// [`resume_walk_mismatch`] for constructor sites.
#[allow(clippy::panic)]
#[cold]
#[inline(never)]
fn ctor_walk_mismatch() -> ! {
    panic!(
        "internal compiler error: native backend constructor-site walk diverged from \
         the emitted construct headers. Report this as a compiler bug."
    )
}

/// Peeled `Drop`s, each local paired with its reuse eligibility.
type DropRun = Vec<(LocalId, bool)>;

/// Emit's `peel_call_arg_drops`, mirrored so both backends transfer ownership
/// into the callee at the same sites. For
/// `let r = call f(args); drop a; …body` with `a` in `args`, the drops move
/// to between the operand copies and the call.
///
/// Two exceptions, both emit's. A drop whose local some `Ctor{reuse}` claims
/// is not peeled: the cell is this frame's reuse token. A drop whose local a
/// directly-following `tail self(..)` passes again is not peeled either;
/// peeling would release the slot's reference before the back-edge re-reads
/// the local.
fn peel_call_arg_drops<'a>(
    body: &'a CoreExpr,
    args: &[LocalId],
    callee: Option<LocalId>,
    uses: &Uses,
) -> (DropRun, &'a CoreExpr) {
    let self_tail_args: &[LocalId] = {
        let mut e = body;
        while let CoreExpr::Drop { body: b, .. } = e {
            e = b;
        }
        match e {
            CoreExpr::Tail(Atom::Call {
                callee: Callee::Self_,
                args,
            }) => args,
            _ => &[],
        }
    };
    let mut moved = Vec::new();
    let mut body = body;
    while let CoreExpr::Drop {
        local,
        shape,
        body: b,
    } = body
    {
        if !args.contains(local) && callee != Some(*local) {
            break;
        }
        if uses.reuse_claimed(*local) {
            break;
        }
        if self_tail_args.contains(local) {
            break;
        }
        moved.push((*local, shape.is_some()));
        body = b;
    }
    (moved, body)
}

/// Emit's `split_self_tail_drops`: partition `drop a; …; tail self(args)`
/// into drops that run before the operand copies and drops of the call's own
/// arguments, which must run after. That order is what keeps a dropped
/// argument's value alive into its copy.
fn split_self_tail(mut e: &CoreExpr) -> Option<(DropRun, DropRun, &[LocalId])> {
    let mut run: DropRun = Vec::new();
    while let CoreExpr::Drop { local, shape, body } = e {
        run.push((*local, shape.is_some()));
        e = body;
    }
    let CoreExpr::Tail(Atom::Call {
        callee: Callee::Self_,
        args,
    }) = e
    else {
        return None;
    };
    let (moved, now): (DropRun, DropRun) = run.into_iter().partition(|&(x, _)| args.contains(&x));
    if moved.is_empty() {
        return None;
    }
    Some((now, moved, args))
}

#[cfg(test)]
#[allow(unsafe_code)] // drives the JIT'd code and the C-ABI mock runtime
mod tests {
    use std::mem::ManuallyDrop;

    use crate::core_ir::emit::{self, EmitCtx};
    use crate::types::StrId;

    use cranelift_jit::JITModule;

    use super::super::ConstId;
    use super::super::testkit;
    use super::*;
    use crate::bytecode::NativeEntry;
    use crate::bytecode::value::{
        native_hollow_for_reuse, native_release_at_zero, take_freed_objects,
    };
    use crate::heap::ProcHeap;
    use crate::typed_ir::RTy;
    use crate::types::PrimIds;

    fn int_pool() -> (ResolvedPool, RTy) {
        let mut pool = ResolvedPool::new(PrimIds {
            int: TypeId(1),
            float: TypeId(2),
            string: TypeId(3),
            array: TypeId(4),
        });
        let int = pool.mk_con(TypeId(1), StrId(0), &[]);
        (pool, int)
    }

    /// Distinct from the `int_pool` prim ids and `testkit::variant()`'s
    /// `TypeId(0)`.
    const BOOL_TID: TypeId = TypeId(5);

    /// The Binary nominal id the test prelude assigns.
    const BIN_TID: TypeId = TypeId(6);

    /// A captured-prelude stand-in. `True` at variant 0 and `False` at 1 is
    /// the real prelude's order; every other binding stays pending so nothing
    /// else falsely matches.
    fn test_prelude() -> PreludeBindings {
        PreludeBindings::test_bool_binary(BOOL_TID, BIN_TID)
    }

    /// The type table's stand-in for [`SwitchCounts`]: the one enum the
    /// tests match over by constructor has two variants; nothing else
    /// switches.
    fn test_counts(tid: TypeId) -> Option<u8> {
        (tid == ENUM_TID).then_some(2)
    }

    fn local(i: u32) -> LocalId {
        LocalId(i)
    }

    struct FnMeta {
        arity: usize,
        locals: usize,
    }

    struct TestVm {
        stack: Vec<Value>,
        frames: Vec<TestFrame>,
        funcs: Vec<FnMeta>,
        trampoline: *const u8,
        entries: Vec<Option<NativeEntry>>,
        heap: ProcHeap,
        reds: i64,
        budget: i64,
        yields: usize,
        /// Re-published per invocation, as `VM::call_native` does.
        ctx: NativeCtx,
    }

    struct TestFrame {
        func: usize,
        base: usize,
        ip: i32,
        /// A sentinel immediate for known calls. Dropped with the frame,
        /// releasing its reference like the real `CallFrame`.
        captures: Value,
    }

    fn vm_of(vmx: *mut core::ffi::c_void) -> &'static mut TestVm {
        // SAFETY: every entry invocation in these tests passes a live
        // `&mut TestVm`, exclusively borrowed for the call's duration.
        unsafe { &mut *(vmx as *mut TestVm) }
    }

    impl TestVm {
        fn push_frame(&mut self, func: usize, argc: usize) {
            self.push_frame_with(func, argc, Value::small_int(0));
        }

        fn push_frame_with(&mut self, func: usize, argc: usize, captures: Value) {
            let base = self.stack.len() - argc;
            let locals = self.funcs[func].locals;
            self.frames.push(TestFrame {
                func,
                base,
                ip: 0,
                captures,
            });
            for _ in argc..locals {
                self.stack.push(Value::small_int(0));
            }
        }

        /// `run_slice`'s trampoline mock: dispatch the top frame at its
        /// stored resume ordinal until the slice ends. Every test fn is
        /// native, so `Done` with frames left means a transfer landed on an
        /// interpreted parent — impossible here — and `Done` with none means
        /// the slice result is on the stack.
        fn drive(&mut self) -> u64 {
            loop {
                let Some(f) = self.frames.last() else {
                    return NativeStatus::Done as u64;
                };
                let (func, resume) = (f.func, i64::from(f.ip));
                let entry = self.entries[func].expect("mock runtime only drives native fns");
                let status = Self::call_entry(self, entry, resume) as u64;
                if status == NativeStatus::Done as u64 {
                    if self.frames.is_empty() {
                        return status;
                    }
                    continue;
                }
                return status;
            }
        }

        /// Mirrors `VM::call_native`, including the pinned-register bracket
        /// and the SystemV->tail trampoline. `enable_pinned_reg` drops that
        /// register from Cranelift's callee-save set, so a compiled entry
        /// clobbers it while this caller's ABI says it survives.
        /// `#[inline(never)]` is a barrier only; it is not what makes the
        /// register safe.
        #[inline(never)]
        fn call_entry(vm: &mut TestVm, entry: NativeEntry, resume: i64) -> NativeStatus {
            vm.ctx.vm = (vm as *mut TestVm).cast();
            let tramp = vm.trampoline;
            // SAFETY: a finalized trampoline + entry from this harness's
            // module, and this TestVm's live ctx.
            unsafe {
                crate::bytecode::native::call_entry_preserving_pinned(
                    (&raw mut vm.ctx).cast(),
                    tramp,
                    entry,
                    resume,
                )
            }
        }
    }

    extern "C" fn t_frame_base(vmx: *mut core::ffi::c_void) -> *mut u64 {
        let vm = vm_of(vmx);
        let base = vm.frames.last().unwrap().base;
        // SAFETY: `Value` is repr(transparent) over u64 and
        // `base < stack.len()` for a live frame.
        unsafe { vm.stack.as_mut_ptr().add(base).cast::<u64>() }
    }

    #[repr(C)]
    struct TPrepared {
        entry: *const u8,
        aux: u64,
    }

    impl TPrepared {
        fn status(s: NativeStatus) -> TPrepared {
            TPrepared {
                entry: std::ptr::null(),
                aux: s as u64,
            }
        }
    }

    /// The transfer decision after a mock frame push/collapse: every test fn
    /// is native, so this hands back its entry unless the budget yielded.
    fn t_prepared(vm: &mut TestVm) -> TPrepared {
        vm.reds -= 1;
        if vm.reds <= 0 {
            return TPrepared::status(NativeStatus::Yield);
        }
        let func = vm.frames.last().unwrap().func;
        match vm.entries[func] {
            Some(entry) => TPrepared { entry, aux: 0 },
            None => TPrepared::status(NativeStatus::Done),
        }
    }

    unsafe extern "C" fn t_prepare_call(
        vmx: *mut core::ffi::c_void,
        target: i64,
        resume: i64,
        args: *const u64,
        argc: i64,
    ) -> TPrepared {
        let vm = vm_of(vmx);
        vm.frames.last_mut().unwrap().ip = resume as i32;
        for i in 0..argc as usize {
            // SAFETY: owned words per the shim contract.
            vm.stack
                .push(unsafe { Value::from_bits(args.add(i).read()) });
        }
        vm.push_frame(target as usize, argc as usize);
        t_prepared(vm)
    }

    unsafe extern "C" fn t_prepare_call_value(
        vmx: *mut core::ffi::c_void,
        callee: u64,
        resume: i64,
        args: *const u64,
        argc: i64,
    ) -> TPrepared {
        let vm = vm_of(vmx);
        vm.frames.last_mut().unwrap().ip = resume as i32;
        // SAFETY: `callee` is an owned closure word per the shim contract.
        let callee = unsafe { Value::from_bits(callee) };
        let target = callee
            .as_closure()
            .expect("dynamic call target must be a closure")
            .func_idx() as usize;
        for i in 0..argc as usize {
            // SAFETY: owned words per the shim contract.
            vm.stack
                .push(unsafe { Value::from_bits(args.add(i).read()) });
        }
        vm.push_frame_with(target, argc as usize, callee);
        t_prepared(vm)
    }

    unsafe extern "C" fn t_prepare_tail(
        vmx: *mut core::ffi::c_void,
        target: i64,
        args: *const u64,
        argc: i64,
    ) -> TPrepared {
        let vm = vm_of(vmx);
        for i in 0..argc as usize {
            // SAFETY: owned words per the shim contract.
            vm.stack
                .push(unsafe { Value::from_bits(args.add(i).read()) });
        }
        let args_start = vm.stack.len() - argc as usize;
        let base = vm.frames.last().unwrap().base;
        vm.stack.drain(base..args_start);
        let locals = vm.funcs[target as usize].locals;
        let f = vm.frames.last_mut().unwrap();
        f.func = target as usize;
        f.ip = 0;
        f.captures = Value::small_int(0);
        for _ in argc as usize..locals {
            vm.stack.push(Value::small_int(0));
        }
        t_prepared(vm)
    }

    unsafe extern "C" fn t_prepare_tail_value(
        vmx: *mut core::ffi::c_void,
        callee: u64,
        args: *const u64,
        argc: i64,
    ) -> TPrepared {
        let vm = vm_of(vmx);
        // SAFETY: `callee` is an owned closure word per the shim contract.
        let callee = unsafe { Value::from_bits(callee) };
        let target = callee
            .as_closure()
            .expect("dynamic tail-call target must be a closure")
            .func_idx() as usize;
        for i in 0..argc as usize {
            // SAFETY: owned words per the shim contract.
            vm.stack
                .push(unsafe { Value::from_bits(args.add(i).read()) });
        }
        let args_start = vm.stack.len() - argc as usize;
        let base = vm.frames.last().unwrap().base;
        vm.stack.drain(base..args_start);
        let locals = vm.funcs[target].locals;
        let f = vm.frames.last_mut().unwrap();
        f.func = target;
        f.ip = 0;
        f.captures = callee;
        for _ in argc as usize..locals {
            vm.stack.push(Value::small_int(0));
        }
        t_prepared(vm)
    }

    unsafe extern "C" fn t_ret_transfer(vmx: *mut core::ffi::c_void, result: u64) -> TPrepared {
        let vm = vm_of(vmx);
        let frame = vm.frames.pop().unwrap();
        vm.stack.truncate(frame.base);
        // SAFETY: `result` is an owned value word per the shim contract.
        vm.stack.push(unsafe { Value::from_bits(result) });
        let Some(parent) = vm.frames.last() else {
            return TPrepared::status(NativeStatus::Done);
        };
        match vm.entries[parent.func] {
            Some(entry) => TPrepared {
                entry,
                aux: parent.ip as u64,
            },
            None => TPrepared::status(NativeStatus::Done),
        }
    }

    /// [`t_frame_base`] + [`t_pop`] in one crossing, mirroring `al_rt_cont`.
    #[repr(C)]
    struct TCont {
        base: *mut u64,
        result: u64,
    }
    extern "C" fn t_cont(vmx: *mut core::ffi::c_void) -> TCont {
        let vm = vm_of(vmx);
        let result = ManuallyDrop::new(vm.stack.pop().expect("continuation result")).to_bits();
        let base = vm.frames.last().unwrap().base;
        // SAFETY: `Value` is repr(transparent) over u64 and
        // `base < stack.len()` for a live frame.
        let base = unsafe { vm.stack.as_mut_ptr().add(base).cast::<u64>() };
        TCont { base, result }
    }

    unsafe extern "C" fn t_make_closure(
        vmx: *mut core::ffi::c_void,
        func_idx: i64,
        caps: *const u64,
        count: i64,
    ) -> u64 {
        let vm = vm_of(vmx);
        let n = count as usize;
        // SAFETY: `Value` is repr(transparent) over its u64 bits; borrowed
        // view of the caller's transferred words for the copy.
        let borrowed: &[Value] = if n == 0 {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(caps.cast::<Value>(), n) }
        };
        let v = Value::closure_in(&mut vm.heap, func_idx as i32, borrowed);
        for i in 0..n {
            // SAFETY: releases the one reference each word transferred in.
            drop(unsafe { Value::from_bits(caps.add(i).read()) });
        }
        ManuallyDrop::new(v).to_bits()
    }

    extern "C" fn t_rt_checkpoint(vmx: *mut core::ffi::c_void) -> u64 {
        let vm = vm_of(vmx);
        vm.reds -= 1;
        if vm.reds <= 0 {
            vm.frames.last_mut().unwrap().ip = 0;
            return NativeStatus::Yield as u64;
        }
        NativeStatus::Done as u64
    }

    extern "C" fn t_int_box(vmx: *mut core::ffi::c_void, i: i64) -> u64 {
        let vm = vm_of(vmx);
        ManuallyDrop::new(Value::int_in(&mut vm.heap, i)).to_bits()
    }

    extern "C" fn t_div_int(a: i64, b: i64) -> i64 {
        if b == 0 { 0 } else { a.wrapping_div(b) }
    }

    /// `al_shim_enum_alloc`'s mock, over the test VM's heap.
    unsafe extern "C" fn t_enum_alloc(
        vmx: *mut core::ffi::c_void,
        packed: u64,
        enum_name: u64,
        variant_name: u64,
        labels: u64,
        payload: *const u64,
        len: i64,
    ) -> u64 {
        let vm = vm_of(vmx);
        let n = len as usize;
        // SAFETY: frozen immortal name/label constants and `n` owned
        // payload words per the shim contract.
        unsafe {
            let (en, vn, lb) = (
                Value::from_bits(enum_name),
                Value::from_bits(variant_name),
                Value::from_bits(labels),
            );
            let fields: &[Value] = if n == 0 {
                &[]
            } else {
                std::slice::from_raw_parts(payload.cast::<Value>(), n)
            };
            let v = Value::enum_reuse_in(
                &mut vm.heap,
                crate::bytecode::value::ReuseAddr::none(),
                TypeId(packed as i32),
                (packed >> 32) as u16,
                0,
                en,
                vn,
                lb,
                fields,
            );
            for i in 0..n {
                drop(Value::from_bits(payload.add(i).read()));
            }
            ManuallyDrop::new(v).to_bits()
        }
    }

    extern "C" fn t_mod_int(a: i64, b: i64) -> i64 {
        if b == 0 { a } else { a.wrapping_rem(b) }
    }

    /// `al_shim_make_array`'s mock: build from the borrowed element words,
    /// then release the transferred references — the shim contract.
    unsafe extern "C" fn t_make_array(
        vmx: *mut core::ffi::c_void,
        elems: *const u64,
        len: i64,
    ) -> u64 {
        let vm = vm_of(vmx);
        let n = len as usize;
        // SAFETY: `n` owned element words per the shim contract.
        unsafe {
            let vals: &[Value] = if n == 0 {
                &[]
            } else {
                std::slice::from_raw_parts(elems.cast::<Value>(), n)
            };
            let v = Value::array_in(&mut vm.heap, vals);
            for i in 0..n {
                drop(Value::from_bits(elems.add(i).read()));
            }
            ManuallyDrop::new(v).to_bits()
        }
    }

    /// `al_shim_seq_len`'s mock: length of an owned Array/Range/Tuple word,
    /// boxed into the test heap; the transferred reference released.
    unsafe extern "C" fn t_seq_len(vmx: *mut core::ffi::c_void, seq: u64) -> u64 {
        use crate::bytecode::ValueView;
        let vm = vm_of(vmx);
        // SAFETY: owned word per the shim contract.
        let v = unsafe { Value::from_bits(seq) };
        let n = match v.kind() {
            ValueView::Array(a) => a.len() as i64,
            ValueView::Range(s, e) => crate::bytecode::value::range_len(s, e),
            ValueView::Tuple(t) => t.len() as i64,
            _ => panic!("t_seq_len on a non-sequence"),
        };
        drop(v);
        ManuallyDrop::new(Value::int_in(&mut vm.heap, n)).to_bits()
    }

    /// `al_shim_bin_byte_size`'s mock.
    unsafe extern "C" fn t_bin_byte_size(vmx: *mut core::ffi::c_void, bin: u64) -> u64 {
        let vm = vm_of(vmx);
        // SAFETY: owned word per the shim contract.
        let v = unsafe { Value::from_bits(bin) };
        let n = match v.kind() {
            crate::bytecode::ValueView::Binary(b) => b.bit_len().div_ceil(8) as i64,
            _ => panic!("t_bin_byte_size on a non-binary"),
        };
        drop(v);
        ManuallyDrop::new(Value::int_in(&mut vm.heap, n)).to_bits()
    }

    /// `al_shim_seq_append`'s mock: `buf[0]` the sequence, the rest pushed
    /// elements; transferred references released at the end.
    unsafe extern "C" fn t_seq_append(
        vmx: *mut core::ffi::c_void,
        buf: *const u64,
        len: i64,
    ) -> u64 {
        use crate::bytecode::seq;
        let vm = vm_of(vmx);
        let n = len as usize;
        // SAFETY: `n` owned value words per the shim contract.
        unsafe {
            let words: &[Value] = std::slice::from_raw_parts(buf.cast::<Value>(), n);
            let mut root = words[0].clone();
            for e in &words[1..] {
                root = seq::push_back(&mut vm.heap, root, e.clone());
            }
            for i in 0..n {
                drop(Value::from_bits(buf.add(i).read()));
            }
            ManuallyDrop::new(root).to_bits()
        }
    }

    /// `al_shim_seq_prepend`'s mock: elements in source order, sequence last.
    unsafe extern "C" fn t_seq_prepend(
        vmx: *mut core::ffi::c_void,
        buf: *const u64,
        len: i64,
    ) -> u64 {
        use crate::bytecode::seq;
        let vm = vm_of(vmx);
        let n = len as usize;
        // SAFETY: `n` owned value words per the shim contract.
        unsafe {
            let words: &[Value] = std::slice::from_raw_parts(buf.cast::<Value>(), n);
            let mut root = words[n - 1].clone();
            for e in words[..n - 1].iter().rev() {
                root = seq::push_front(&mut vm.heap, root, e.clone());
            }
            for i in 0..n {
                drop(Value::from_bits(buf.add(i).read()));
            }
            ManuallyDrop::new(root).to_bits()
        }
    }

    /// `al_shim_make_tuple`'s mock — see [`t_make_array`].
    unsafe extern "C" fn t_make_tuple(
        vmx: *mut core::ffi::c_void,
        elems: *const u64,
        len: i64,
    ) -> u64 {
        let vm = vm_of(vmx);
        let n = len as usize;
        // SAFETY: `n` owned element words per the shim contract.
        unsafe {
            let vals: &[Value] = if n == 0 {
                &[]
            } else {
                std::slice::from_raw_parts(elems.cast::<Value>(), n)
            };
            let v = Value::tuple_in(&mut vm.heap, vals);
            for i in 0..n {
                drop(Value::from_bits(elems.add(i).read()));
            }
            ManuallyDrop::new(v).to_bits()
        }
    }

    /// Stand-in for the HTTP shims. No clif unit test drives them, but every
    /// compiled body declares all imports, so finalize needs an address per
    /// symbol.
    extern "C" fn t_http_unused() -> u64 {
        panic!("http shim called from a clif unit test")
    }

    struct Jit {
        // Keeps the executable mapping alive for the entries' lifetime.
        _module: JITModule,
        // Keeps the frozen constants whose addresses are baked into the
        // code alive (the program owns its frozen area).
        _program: crate::bytecode::Program,
        /// The module's SystemV->tail bridge; entries are called through it.
        trampoline: *const u8,
        entries: Vec<Option<NativeEntry>>,
        metas: Vec<FnMeta>,
        clifs: Vec<String>,
    }

    /// Plan + compile every function against the mock runtime symbols and
    /// finalize. Panics if a function the test expects to cover is rejected.
    /// Per-body frame layouts, keyed the way `compile` looks them up.
    type LayoutMap = std::collections::HashMap<FuncIdx, FrameLayout>;

    fn jit(fns: &[CoreFn], pool: &ResolvedPool, consts: &[Value]) -> Jit {
        // No enum ctors here, so the bodies need no bytecode — but they still
        // need the layout emit fixes, and any constant emit interns.
        let area = std::sync::Arc::new(crate::frozen::FrozenArea::new());
        let mut ctx = PoolingCtx {
            fb: area.builder(),
            consts: consts.to_vec(),
            bools: BoolCtors::of(&test_prelude()),
        };
        let mut layouts = LayoutMap::new();
        let mut functions = Vec::with_capacity(fns.len());
        for (i, f) in fns.iter().enumerate() {
            let out = emit::emit(f, &mut ctx);
            layouts.insert(FuncIdx::from_usize(i), out.layout);
            // No code: these bodies never consult it. The slot count does
            // matter — it is the frame size the harness allocates.
            functions.push(crate::bytecode::Function {
                name: format!("f{i}").into(),
                arity: f.params.len() as i32,
                locals: out.locals.max(f.params.len() as i32),
                capture_count: 0,
                code_start: 0,
                code_len: 0,
            });
        }
        // Bind the Unit slot the way `bind_abi` does, so a body whose value
        // is a statement's implicit `Nil` (`Op::PushNil`) compiles here too.
        let prelude = test_prelude();
        let unit =
            scarlet_vm::EnumTemplate::build(&mut ctx.fb, prelude.nil().id, 0, "Nil", "Nil", &[]);
        let mut templates = crate::tivec::TiVec::new();
        let mut abi = scarlet_vm::AbiTable::default();
        abi.bind(scarlet_vm::abi::AbiSlot::Unit, templates.push(unit));
        drop(ctx.fb);
        let program = crate::bytecode::Program {
            constants: ctx.consts,
            frozen: area,
            functions,
            templates,
            abi,
            ..Default::default()
        };
        jit_with(fns, pool, program, &layouts)
    }

    /// [`jit`] against a full test `Program`, which the enum-ctor tests need:
    /// `compile` reads header constants back from the emitted bytecode.
    fn jit_with(
        fns: &[CoreFn],
        pool: &ResolvedPool,
        program: crate::bytecode::Program,
        layouts: &LayoutMap,
    ) -> Jit {
        // The production flag set, so these tests compile exactly what the
        // real module would; only the runtime symbols are substituted.
        let mut jb = scarlet_vm::vm::jit::jit_builder().unwrap();
        jb.symbol(
            NATIVE_RELEASE_AT_ZERO_SYMBOL,
            native_release_at_zero as *const u8,
        );
        jb.symbol(
            NATIVE_HOLLOW_FOR_REUSE_SYMBOL,
            native_hollow_for_reuse as *const u8,
        );
        jb.symbol(NATIVE_INT_BOX_SYMBOL, t_int_box as *const u8);
        jb.symbol(SYM_DIV_INT, t_div_int as *const u8);
        jb.symbol(SYM_MOD_INT, t_mod_int as *const u8);
        jb.symbol(SYM_ENUM_ALLOC, t_enum_alloc as *const u8);
        jb.symbol(SYM_MAKE_ARRAY, t_make_array as *const u8);
        jb.symbol(SYM_MAKE_TUPLE, t_make_tuple as *const u8);
        jb.symbol(SYM_SEQ_LEN, t_seq_len as *const u8);
        jb.symbol(SYM_SEQ_APPEND, t_seq_append as *const u8);
        jb.symbol(SYM_SEQ_PREPEND, t_seq_prepend as *const u8);
        jb.symbol(SYM_BIN_BYTE_SIZE, t_bin_byte_size as *const u8);
        jb.symbol(SYM_HTTP_PARSE_HEAD, t_http_unused as *const u8);
        jb.symbol(SYM_HTTP_HEADERS_VALID, t_http_unused as *const u8);
        jb.symbol(SYM_HTTP_HEADER_HAS, t_http_unused as *const u8);
        jb.symbol(SYM_HTTP_SERIALIZE_HEAD, t_http_unused as *const u8);
        jb.symbol(SYM_HTTP_FRAMING, t_http_unused as *const u8);
        jb.symbol(SYM_RT_PREPARE_CALL, t_prepare_call as *const u8);
        jb.symbol(SYM_RT_PREPARE_CALL_VALUE, t_prepare_call_value as *const u8);
        jb.symbol(SYM_RT_PREPARE_TAIL, t_prepare_tail as *const u8);
        jb.symbol(SYM_RT_PREPARE_TAIL_VALUE, t_prepare_tail_value as *const u8);
        jb.symbol("al_rt_cont", t_cont as *const u8);
        jb.symbol(SYM_RT_MAKE_CLOSURE, t_make_closure as *const u8);
        jb.symbol(SYM_RT_CHECKPOINT, t_rt_checkpoint as *const u8);
        jb.symbol(SYM_RT_FRAME_BASE, t_frame_base as *const u8);
        jb.symbol(SYM_RT_RET_TRANSFER, t_ret_transfer as *const u8);
        let mut module = JITModule::new(jb);

        let mut ids = Vec::new();
        let mut metas = Vec::new();
        let mut clifs = Vec::new();
        let prelude = test_prelude();
        for (i, f) in fns.iter().enumerate() {
            let p = plan(FuncIdx::from_usize(i), f, pool, &prelude, &test_counts);
            // The layout comes from the emission that built `program`, the
            // way the real pipeline hands emit's output to the backend.
            let layout = layouts
                .get(&FuncIdx::from_usize(i))
                .expect("a layout per body");
            let body = compile(&mut module, &p, &program, layout).expect("module error");
            assert!(!body.clif.is_empty());
            clifs.push(body.clif);
            let locals = program
                .functions
                .get(i)
                .map(|fun| fun.locals)
                .unwrap_or(0)
                .max(f.params.len() as i32);
            metas.push(FnMeta {
                arity: f.params.len(),
                locals: locals as usize,
            });
            ids.push(body.func_id);
        }
        let tramp_id = scarlet_vm::vm::jit::entry_trampoline(&mut module).unwrap();
        module.finalize_definitions().unwrap();
        let trampoline = module.get_finalized_function(tramp_id);
        let entries = ids
            .iter()
            .map(|&id| Some(module.get_finalized_function(id) as NativeEntry))
            .collect();
        Jit {
            _module: module,
            _program: program,
            trampoline,
            entries,
            metas,
            clifs,
        }
    }

    /// Run `func(args…)` on the mock VM, re-entering on yields (the frame is
    /// resumable at ip 0 by the convention). Returns the result value.
    fn run(jit: &Jit, func: usize, args: &[Value], budget: i64) -> (Value, usize) {
        let mut vm = TestVm {
            ctx: NativeCtx::new(),
            stack: Vec::new(),
            frames: Vec::new(),
            funcs: jit
                .metas
                .iter()
                .map(|m| FnMeta {
                    arity: m.arity,
                    locals: m.locals,
                })
                .collect(),
            trampoline: jit.trampoline,
            entries: jit.entries.clone(),
            heap: ProcHeap::new(),
            reds: budget,
            budget,
            yields: 0,
        };
        assert_eq!(args.len(), jit.metas[func].arity);
        for a in args {
            vm.stack.push(a.clone());
        }
        vm.push_frame(func, args.len());
        loop {
            let status = vm.drive();
            if status == NativeStatus::Done as u64 {
                break;
            }
            assert_eq!(status, NativeStatus::Yield as u64, "unexpected status");
            // Every A0 yield leaves the top frame resumable from the top.
            assert_eq!(vm.frames.last().unwrap().ip, 0);
            vm.yields += 1;
            vm.reds = vm.budget;
        }
        assert!(vm.frames.is_empty(), "return protocol must pop the frame");
        assert_eq!(vm.stack.len(), 1, "result is the one remaining word");
        let yields = vm.yields;
        (vm.stack.pop().unwrap(), yields)
    }

    /// An `EmitCtx` that pools constants for real, so the emitted bytecode
    /// carries genuine ctor header constants for [`enum_ctor_sites`].
    struct PoolingCtx {
        fb: crate::frozen::FrozenBuilder,
        consts: Vec<Value>,
        bools: BoolCtors,
    }

    impl PoolingCtx {
        fn pool(&mut self, v: Value) -> i32 {
            if let Some(i) = self.consts.iter().position(|c| c.to_bits() == v.to_bits()) {
                return i as i32;
            }
            self.consts.push(v);
            self.consts.len() as i32 - 1
        }
    }

    impl EmitCtx for PoolingCtx {
        fn resolve_str(&self, _id: StrId) -> &str {
            "T"
        }
        fn intern_int(&mut self, i: i64) -> i32 {
            let v = self.fb.int(i).into_value();
            self.pool(v)
        }
        fn intern_str(&mut self, s: &str) -> i32 {
            let v = self.fb.str(s).into_value();
            self.pool(v)
        }
        fn intern_labels(&mut self, _tid: TypeId, _variant_idx: u16) -> i32 {
            // Test variants are label-less; the pooled constant is the
            // empty label array, like the compiler's `intern_labels`.
            let v = self.fb.str_array(&[]).into_value();
            self.pool(v)
        }
        fn variant_name(&self, _tid: TypeId, _variant_idx: u16) -> &str {
            "T"
        }
        fn switch_variant_count(&self, _tid: TypeId) -> Option<u8> {
            None
        }
        fn bool_variant(&self, tid: TypeId, variant_idx: u16) -> Option<bool> {
            self.bools.bool_variant(tid, variant_idx)
        }
    }

    /// Emit `fns` for real into a test `Program`. The frozen area is shared
    /// with the program, so `compile`'s label tuples outlive the baked code.
    fn ctor_program(
        fns: &[CoreFn],
        base_consts: &[Value],
    ) -> (crate::bytecode::Program, LayoutMap) {
        use std::sync::Arc;
        let area = Arc::new(crate::frozen::FrozenArea::new());
        let mut ctx = PoolingCtx {
            fb: area.builder(),
            consts: base_consts.to_vec(),
            bools: BoolCtors::of(&test_prelude()),
        };
        let mut program = crate::bytecode::Program {
            frozen: area,
            ..Default::default()
        };
        let mut layouts: std::collections::HashMap<FuncIdx, FrameLayout> =
            std::collections::HashMap::new();
        for (i, f) in fns.iter().enumerate() {
            let start = program.code.len() as i32;
            let out = emit::emit(f, &mut ctx);
            layouts.insert(FuncIdx::from_usize(i), out.layout.clone());
            program.code.extend(out.code);
            program.functions.push(crate::bytecode::Function {
                name: format!("f{i}").into(),
                arity: f.params.len() as i32,
                locals: out.locals.max(f.params.len() as i32),
                capture_count: 0,
                code_start: start,
                code_len: program.code.len() as i32 - start,
            });
        }
        program.constants = ctx.consts;
        (program, layouts)
    }

    fn let_(id: u32, ty: RTy, rhs: Atom, body: CoreExpr) -> CoreExpr {
        CoreExpr::Let {
            bind: testkit::bind(id, ty),
            rhs,
            body: Box::new(body),
        }
    }

    /// `fib(n) = if n < 2 { n } else { fib(n-1) + fib(n-2) }`, self-recursive
    /// through `CallKnown(0)` — exercises the whole call convention.
    fn fib_fn(int: RTy) -> CoreFn {
        // consts: c0 = 2, c1 = 1
        let els = let_(
            3,
            int,
            Atom::Const(ConstId(1)),
            let_(
                4,
                int,
                Atom::prim(Op::SubInt, vec![local(0), local(3)]),
                let_(
                    5,
                    int,
                    Atom::Call {
                        callee: Callee::Known(FuncIdx(0)),
                        args: vec![local(4)],
                    },
                    let_(
                        6,
                        int,
                        Atom::Const(ConstId(0)),
                        let_(
                            7,
                            int,
                            Atom::prim(Op::SubInt, vec![local(0), local(6)]),
                            let_(
                                8,
                                int,
                                Atom::Call {
                                    callee: Callee::Known(FuncIdx(0)),
                                    args: vec![local(7)],
                                },
                                CoreExpr::Tail(Atom::prim(Op::AddInt, vec![local(5), local(8)])),
                            ),
                        ),
                    ),
                ),
            ),
        );
        let body = let_(
            1,
            int,
            Atom::Const(ConstId(0)),
            let_(
                2,
                int,
                Atom::prim(Op::LtInt, vec![local(0), local(1)]),
                CoreExpr::If {
                    cond: local(2),
                    then: Box::new(CoreExpr::Tail(Atom::Local(local(0)))),
                    els: Box::new(els),
                    ty: int,
                },
            ),
        );
        testkit::func(vec![testkit::bind(0, int)], body, int)
    }

    /// `count(n, acc) = if n == 0 { acc } else { count(n - 1, acc + 1) }`
    /// with the recursive call in self-tail position — the native loop.
    fn count_fn(int: RTy) -> CoreFn {
        let els = let_(
            4,
            int,
            Atom::prim(Op::SubInt, vec![local(0), local(3)]),
            let_(
                5,
                int,
                Atom::prim(Op::AddInt, vec![local(1), local(3)]),
                CoreExpr::Tail(Atom::Call {
                    callee: Callee::Self_,
                    args: vec![local(4), local(5)],
                }),
            ),
        );
        let body = let_(
            2,
            int,
            Atom::prim(Op::EqInt, vec![local(0), local(6)]),
            CoreExpr::If {
                cond: local(2),
                then: Box::new(CoreExpr::Tail(Atom::Local(local(1)))),
                els: Box::new(let_(3, int, Atom::Const(ConstId(1)), els)),
                ty: int,
            },
        );
        let body = let_(6, int, Atom::Const(ConstId(2)), body);
        testkit::func(
            vec![testkit::bind(0, int), testkit::bind(1, int)],
            body,
            int,
        )
    }

    fn test_consts() -> Vec<Value> {
        vec![
            Value::small_int(2),
            Value::small_int(1),
            Value::small_int(0),
        ]
    }

    #[test]
    fn gate_accepts_the_a0_shapes_and_rejects_the_rest() {
        let (pool, int) = int_pool();
        let pre = test_prelude();
        let _ = plan(FuncIdx(0), &fib_fn(int), &pool, &pre, &test_counts);
        let _ = plan(FuncIdx(0), &count_fn(int), &pool, &pre, &test_counts);

        // A non-Bool constructor allocates through the enum-ctor path:
        // covered.
        let f = testkit::func(vec![], CoreExpr::Tail(testkit::ctor(&[])), int);
        let _ = plan(FuncIdx(0), &f, &pool, &pre, &test_counts);

        // Bool's heads are immediates: covered.
        let f = testkit::func(
            vec![],
            CoreExpr::Tail(Atom::Ctor {
                variant: testkit::vref(BOOL_TID.0, 0),
                fields: vec![],
                reuse: None,
            }),
            int,
        );
        let _ = plan(FuncIdx(0), &f, &pool, &pre, &test_counts);

        // A closure allocates through the MakeClosure shim: covered.
        let f = testkit::func(
            vec![],
            CoreExpr::Tail(Atom::Closure {
                func_idx: FuncIdx(1),
                captures: vec![],
            }),
            int,
        );
        let _ = plan(FuncIdx(0), &f, &pool, &pre, &test_counts);

        // A dynamic call through a closure-valued local: covered.
        let f = testkit::func(
            vec![testkit::bind(0, int)],
            CoreExpr::Tail(Atom::Call {
                callee: Callee::Local(local(0)),
                args: vec![],
            }),
            int,
        );
        let _ = plan(FuncIdx(0), &f, &pool, &pre, &test_counts);

        // A capture *read* stays uncovered: the body needs the frame's
        // `PushCapture` reads the running closure through a shim, so a body
        // using it compiles like any other.
        let f = testkit::func(
            vec![],
            CoreExpr::Tail(Atom::prim(Op::PushCapture, vec![])),
            int,
        );
        let _ = plan(FuncIdx(0), &f, &pool, &pre, &test_counts);

        // A polymorphic comparison needs the pool's Int proof.
        let mut pool2 = ResolvedPool::new(PrimIds {
            int: TypeId(1),
            float: TypeId(2),
            string: TypeId(3),
            array: TypeId(4),
        });
        let bound = pool2.mk_bound(0);
        let intt = pool2.mk_con(TypeId(1), StrId(0), &[]);
        let poly_eq = |ty| {
            testkit::func(
                vec![testkit::bind(0, ty), testkit::bind(1, ty)],
                CoreExpr::Tail(Atom::prim(Op::Eq, vec![local(0), local(1)])),
                ty,
            )
        };
        // A polymorphic compare is covered either way: proven-Int operands
        // lower to an inline `icmp`, anything else runs the interpreter's own
        // comparison through the bridge.
        let _ = plan(FuncIdx(0), &poly_eq(bound), &pool2, &pre, &test_counts);
        let _ = plan(FuncIdx(0), &poly_eq(intt), &pool2, &pre, &test_counts);
    }

    /// A `Program` whose constant pool holds a *mortal* heap value is
    /// malformed — the compiler interns every heap constant into the frozen
    /// area. There is no slower mode to retreat to, so this aborts.
    #[test]
    #[should_panic(expected = "mortal heap value")]
    fn compile_aborts_on_a_mortal_heap_constant() {
        let (pool, int) = int_pool();
        let f = testkit::func(vec![], CoreExpr::Tail(Atom::Const(ConstId(0))), int);
        let p = plan(FuncIdx(0), &f, &pool, &test_prelude(), &test_counts);
        let mut module = test_module();

        let mut heap = ProcHeap::new();
        let heap_const = Value::str_in(&mut heap, "not an immediate");
        let heap_program = crate::bytecode::Program {
            constants: vec![heap_const],
            ..Default::default()
        };
        let layout = test_layout(&f);
        let _ = compile(&mut module, &p, &heap_program, &layout);
    }

    #[test]
    fn compile_embeds_an_immediate_constant() {
        let (pool, int) = int_pool();
        let f = testkit::func(vec![], CoreExpr::Tail(Atom::Const(ConstId(0))), int);
        let p = plan(FuncIdx(0), &f, &pool, &test_prelude(), &test_counts);
        let mut module = test_module();
        let ok_program = crate::bytecode::Program {
            constants: vec![Value::small_int(7)],
            ..Default::default()
        };
        let layout = test_layout(&f);
        compile(&mut module, &p, &ok_program, &layout).expect("immediate constants compile");
    }

    /// The layout `emit` fixes for `f`, as the real pipeline would hand it over.
    fn test_layout(f: &CoreFn) -> FrameLayout {
        let area = std::sync::Arc::new(crate::frozen::FrozenArea::new());
        let mut ctx = PoolingCtx {
            fb: area.builder(),
            consts: Vec::new(),
            bools: BoolCtors::of(&test_prelude()),
        };
        emit::emit(f, &mut ctx).layout
    }

    /// A bare JIT module for the compile-level tests: production flags, no
    /// runtime symbols (nothing here is ever run).
    fn test_module() -> JITModule {
        JITModule::new(scarlet_vm::vm::jit::jit_builder().unwrap())
    }

    #[test]
    fn straight_line_return_of_a_constant() {
        let (pool, int) = int_pool();
        let f = testkit::func(vec![], CoreExpr::Tail(Atom::Const(ConstId(0))), int);
        let j = jit(&[f], &pool, &test_consts());
        let (v, yields) = run(&j, 0, &[], 1 << 40);
        assert_eq!(v.to_bits(), Value::small_int(2).to_bits());
        assert_eq!(yields, 0);
    }

    /// A Nil-typed value must keep dynamic RC gates. `Nil()` heap-allocates
    /// like any nullary ctor, so classifying it immediate elides the dup on a
    /// consuming use and double-frees. Among nominals only `Bool` may claim
    /// `Repr::Immediate`.
    #[test]
    fn nil_typed_values_are_not_immediates() {
        let mut pool = ResolvedPool::new(PrimIds {
            int: TypeId(1),
            float: TypeId(2),
            string: TypeId(3),
            array: TypeId(4),
        });
        let pre = test_prelude();
        let tys = ReprTys::of(&pre);
        let nil = pool.mk_con(pre.nil().id, StrId(0), &[]);
        assert_eq!(classify(&pool, tys, nil), Repr::Dyn);
        let boolean = pool.mk_con(BOOL_TID, StrId(0), &[]);
        assert_eq!(classify(&pool, tys, boolean), Repr::Immediate);
    }

    /// Aggregate literals through the allocator shims, checking contents and
    /// refcounts through the returned value.
    #[test]
    fn make_array_and_tuple_literals() {
        let (pool, int) = int_pool();
        let body = |op| {
            testkit::func(
                vec![testkit::bind(0, int)],
                CoreExpr::Let {
                    bind: testkit::bind(1, int),
                    rhs: Atom::Const(ConstId(0)),
                    body: Box::new(CoreExpr::Tail(Atom::PrimOp {
                        op,
                        args: vec![local(1), local(0)],
                        imm: Imm::Argc(2),
                    })),
                },
                int,
            )
        };
        let j = jit(
            &[body(Op::MakeArray), body(Op::MakeTuple)],
            &pool,
            &test_consts(),
        );
        let (v, _) = run(&j, 0, &[Value::small_int(9)], 1 << 40);
        let arr = v.as_array().expect("MakeArray builds an array");
        let got: Vec<i64> = arr.iter().filter_map(|e| e.as_int()).collect();
        assert_eq!(got, vec![2, 9]);
        drop(v);
        let (v, _) = run(&j, 1, &[Value::small_int(9)], 1 << 40);
        let t = v.as_tuple().expect("MakeTuple builds a tuple");
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].as_int(), Some(2));
        assert_eq!(t[1].as_int(), Some(9));
    }

    /// The seq/binary whole-op shims. The seq binds are Array-typed so the
    /// gate's proofs hold.
    #[test]
    fn seq_extension_and_length_shims() {
        let (mut pool, int) = int_pool();
        let arr = pool.mk_con(TypeId(4), StrId(0), &[]);
        // fn f(x Int) Int { a = [c, x]; b = append(a, x); c2 = prepend(x, b); len(c2) }
        let f = testkit::func(
            vec![testkit::bind(0, int)],
            CoreExpr::Let {
                bind: testkit::bind(1, arr),
                rhs: Atom::PrimOp {
                    op: Op::MakeArray,
                    args: vec![local(0), local(0)],
                    imm: Imm::Argc(2),
                },
                body: Box::new(CoreExpr::Let {
                    bind: testkit::bind(2, arr),
                    rhs: Atom::PrimOp {
                        op: Op::Append,
                        args: vec![local(1), local(0)],
                        imm: Imm::Argc(1),
                    },
                    body: Box::new(CoreExpr::Let {
                        bind: testkit::bind(3, arr),
                        rhs: Atom::PrimOp {
                            op: Op::Prepend,
                            args: vec![local(0), local(2)],
                            imm: Imm::Argc(1),
                        },
                        body: Box::new(CoreExpr::Tail(Atom::PrimOp {
                            op: Op::ArrayLen,
                            args: vec![local(3)],
                            imm: Imm::None,
                        })),
                    }),
                }),
            },
            int,
        );
        let j = jit(&[f], &pool, &test_consts());
        let (v, _) = run(&j, 0, &[Value::small_int(7)], 1 << 40);
        assert_eq!(v.as_int(), Some(4), "[7,7] + append + prepend has 4 elems");
    }

    /// `BinByteSize` through its shim, on a proven Binary-typed param.
    #[test]
    fn bin_byte_size_shim() {
        let (mut pool, int) = int_pool();
        let pre = test_prelude();
        let bin = pool.mk_con(pre.binary().id, StrId(0), &[]);
        let f = testkit::func(
            vec![testkit::bind(0, bin)],
            CoreExpr::Tail(Atom::PrimOp {
                op: Op::BinByteSize,
                args: vec![local(0)],
                imm: Imm::None,
            }),
            int,
        );
        let j = jit(&[f], &pool, &test_consts());
        let mut heap = ProcHeap::new();
        let b = Value::binary_in(&mut heap, vec![1, 2, 3, 4, 5]);
        let (v, _) = run(&j, 0, &[b], 1 << 40);
        assert_eq!(v.as_int(), Some(5));
    }

    /// The nullary Bool heads `&&`/`||` lowering materializes
    /// (`Op::PushTrue`/`Op::PushFalse`): tail position and via a `Let` bind.
    #[test]
    fn bool_heads_from_short_circuit_lowering() {
        let (pool, int) = int_pool();
        let t = testkit::func(
            vec![],
            CoreExpr::Tail(Atom::prim(Op::PushTrue, vec![])),
            int,
        );
        let f = testkit::func(
            vec![],
            CoreExpr::Let {
                bind: testkit::bind(0, int),
                rhs: Atom::prim(Op::PushFalse, vec![]),
                body: Box::new(CoreExpr::Tail(Atom::Local(local(0)))),
            },
            int,
        );
        let j = jit(&[t, f], &pool, &test_consts());
        let (v, _) = run(&j, 0, &[], 1 << 40);
        assert_eq!(v.to_bits(), Value::bool(true).to_bits());
        let (v, _) = run(&j, 1, &[], 1 << 40);
        assert_eq!(v.to_bits(), Value::bool(false).to_bits());
    }

    /// A block that ends in a statement has the prelude's `Nil` as its value
    /// (`Op::PushNil`): it must lower to the very word the interpreter pushes,
    /// in tail position and through a `Let`, without any runtime call.
    #[test]
    fn statement_blocks_yield_the_frozen_nil_constructor() {
        let (pool, int) = int_pool();
        let t = testkit::func(vec![], CoreExpr::Tail(Atom::prim(Op::PushNil, vec![])), int);
        let f = testkit::func(
            vec![],
            CoreExpr::Let {
                bind: testkit::bind(0, int),
                rhs: Atom::prim(Op::PushNil, vec![]),
                body: Box::new(CoreExpr::Tail(Atom::Local(local(0)))),
            },
            int,
        );
        let j = jit(&[t, f], &pool, &test_consts());
        let expected = j
            ._program
            .abi_nullary(scarlet_vm::abi::AbiSlot::Unit)
            .expect("the harness binds Unit")
            .to_bits();
        let baked = format!("iconst.i64 {}", expected as i64);
        for (i, clif) in j.clifs.iter().enumerate() {
            assert!(
                clif.contains(&baked),
                "PushNil must bake the constructor word as an immediate; body {i} was:\n{clif}"
            );
        }
        let (v, _) = run(&j, 0, &[], 1 << 40);
        assert_eq!(v.to_bits(), expected);
        let (v, _) = run(&j, 1, &[], 1 << 40);
        assert_eq!(v.to_bits(), expected);
    }

    #[test]
    fn fib_matches_the_interpreter_result() {
        let (pool, int) = int_pool();
        let j = jit(&[fib_fn(int)], &pool, &test_consts());
        for (n, expect) in [(0, 0), (1, 1), (2, 1), (10, 55), (20, 6765)] {
            let (v, _) = run(&j, 0, &[Value::small_int(n)], i64::MAX / 2);
            assert_eq!(v.as_int(), Some(expect), "fib({n})");
        }
    }

    /// A known native callee compiles to a direct call, not the trampoline.
    /// Same-module functions are `colocated` in the CLIF ext-func table, so a
    /// colocated entry is the direct-call proof; recompiling with the callee
    /// outside the native set leaves none.
    #[test]
    fn known_calls_transfer_and_dispatch_through_the_resume_table() {
        let (pool, int) = int_pool();
        let j = jit(&[fib_fn(int)], &pool, &test_consts());
        let clif = &j.clifs[0];
        // Every call site is a transfer: prepare shim, then a machine tail
        // call to the target's entry — never a stack-growing `call`.
        assert!(
            clif.contains("return_call_indirect"),
            "call sites must tail-transfer to the callee entry; CLIF was:\n{clif}"
        );
        // The entry dispatch: fib has two non-tail call sites, so the resume
        // table routes 0 (head) plus two continuations.
        assert!(
            clif.contains("br_table"),
            "the prologue must dispatch on the resume ordinal; CLIF was:\n{clif}"
        );
        let (v, _) = run(&j, 0, &[Value::small_int(15)], i64::MAX / 2);
        assert_eq!(v.as_int(), Some(610));
    }

    #[test]
    fn self_tail_loop_counts_and_yields_like_the_interpreter() {
        let (pool, int) = int_pool();
        let j = jit(&[count_fn(int)], &pool, &test_consts());
        let n = 50_000i64;
        let (v, yields) = run(&j, 0, &[Value::small_int(n), Value::small_int(0)], 1000);
        assert_eq!(v.as_int(), Some(n));
        // One reduction per back-edge against a 1000-budget: the loop must
        // have been preempted many times and still finish correctly.
        assert!(yields >= 40, "expected preemption, got {yields} yields");
    }

    #[test]
    fn int_overflow_spills_to_a_bigint_box_exactly_like_push_int() {
        let (pool, int) = int_pool();
        // add2(a, b) = a + b
        let f = testkit::func(
            vec![testkit::bind(0, int), testkit::bind(1, int)],
            CoreExpr::Tail(Atom::prim(Op::AddInt, vec![local(0), local(1)])),
            int,
        );
        let j = jit(&[f], &pool, &test_consts());

        let max_small = (1i64 << 47) - 1;
        let (v, _) = run(
            &j,
            0,
            &[Value::small_int(max_small), Value::small_int(1)],
            1 << 40,
        );
        assert_eq!(v.as_int(), Some(1 << 47));
        assert!(v.is_heap() && !v.is_immortal(), "past the range must spill");

        // And a spilled *argument* decodes through the BigInt path.
        let mut heap = ProcHeap::new();
        let big = Value::int_in(&mut heap, 1 << 47);
        let (v, _) = run(&j, 0, &[big, Value::small_int(-1)], 1 << 40);
        assert_eq!(v.as_int(), Some(max_small));
        assert!(!v.is_heap(), "back in range must re-box small");
    }

    #[test]
    fn division_and_modulo_keep_the_interpreter_totality() {
        let (pool, int) = int_pool();
        let div = testkit::func(
            vec![testkit::bind(0, int), testkit::bind(1, int)],
            CoreExpr::Tail(Atom::prim(Op::DivInt, vec![local(0), local(1)])),
            int,
        );
        let modu = testkit::func(
            vec![testkit::bind(0, int), testkit::bind(1, int)],
            CoreExpr::Tail(Atom::prim(Op::ModInt, vec![local(0), local(1)])),
            int,
        );
        let j = jit(&[div, modu], &pool, &test_consts());
        let go = |f: usize, a: i64, b: i64| {
            let (v, _) = run(&j, f, &[Value::small_int(a), Value::small_int(b)], 1 << 40);
            v.as_int().unwrap()
        };
        assert_eq!(go(0, 7, 2), 3);
        assert_eq!(go(0, 7, 0), 0); // x / 0 = 0
        assert_eq!(go(1, 7, 2), 1);
        assert_eq!(go(1, 7, 0), 7); // x % 0 = x
    }

    #[test]
    fn match_ladder_over_int_literals() {
        let (pool, int) = int_pool();
        // pick(n) = match n { 0 -> 2, 1 -> 1, other -> other }
        let arms = vec![
            (
                CorePat::Lit(ConstId(2)),
                CoreExpr::Tail(Atom::Const(ConstId(0))),
            ),
            (
                CorePat::Lit(ConstId(1)),
                CoreExpr::Tail(Atom::Const(ConstId(1))),
            ),
            (
                CorePat::Bind(testkit::bind(1, int)),
                CoreExpr::Tail(Atom::Local(local(1))),
            ),
        ];
        let f = testkit::func(
            vec![testkit::bind(0, int)],
            CoreExpr::Match {
                scrut: local(0),
                arms,
                ty: int,
            },
            int,
        );
        let j = jit(&[f], &pool, &test_consts());
        let go = |n: i64| {
            let (v, _) = run(&j, 0, &[Value::small_int(n)], 1 << 40);
            v.as_int().unwrap()
        };
        assert_eq!(go(0), 2);
        assert_eq!(go(1), 1);
        assert_eq!(go(42), 42);
    }

    fn bool_head(variant_idx: u16) -> Atom {
        Atom::Ctor {
            variant: testkit::vref(BOOL_TID.0, variant_idx),
            fields: vec![],
            reuse: None,
        }
    }

    #[test]
    fn bool_ctor_heads_construct_immediates() {
        let (mut pool, int) = int_pool();
        let boolt = pool.mk_con(BOOL_TID, StrId(0), &[]);
        // is_even(n) = let r = n % 2; let b = r == 0; if b { True } else { False }
        let body = let_(
            1,
            int,
            Atom::Const(ConstId(0)),
            let_(
                2,
                int,
                Atom::prim(Op::ModInt, vec![local(0), local(1)]),
                let_(
                    3,
                    int,
                    Atom::Const(ConstId(2)),
                    let_(
                        4,
                        boolt,
                        Atom::prim(Op::EqInt, vec![local(2), local(3)]),
                        CoreExpr::If {
                            cond: local(4),
                            then: Box::new(CoreExpr::Tail(bool_head(0))),
                            els: Box::new(CoreExpr::Tail(bool_head(1))),
                            ty: boolt,
                        },
                    ),
                ),
            ),
        );
        let f = testkit::func(vec![testkit::bind(0, int)], body, boolt);
        let j = jit(&[f], &pool, &test_consts());
        let go = |n: i64| run(&j, 0, &[Value::small_int(n)], 1 << 40).0.to_bits();
        assert_eq!(go(4), Value::bool(true).to_bits());
        assert_eq!(go(7), Value::bool(false).to_bits());
    }

    #[test]
    fn match_ladder_over_bool_ctor_heads() {
        let (mut pool, int) = int_pool();
        let boolt = pool.mk_con(BOOL_TID, StrId(0), &[]);
        // pick(b) = match b { True -> 2, False -> 1 }
        let arms = vec![
            (
                CorePat::Ctor {
                    variant: testkit::vref(BOOL_TID.0, 0),
                    fields: vec![],
                },
                CoreExpr::Tail(Atom::Const(ConstId(0))),
            ),
            (
                CorePat::Ctor {
                    variant: testkit::vref(BOOL_TID.0, 1),
                    fields: vec![],
                },
                CoreExpr::Tail(Atom::Const(ConstId(1))),
            ),
        ];
        let f = testkit::func(
            vec![testkit::bind(0, boolt)],
            CoreExpr::Match {
                scrut: local(0),
                arms,
                ty: int,
            },
            int,
        );
        let j = jit(&[f], &pool, &test_consts());
        let go = |b: bool| {
            let (v, _) = run(&j, 0, &[Value::bool(b)], 1 << 40);
            v.as_int().unwrap()
        };
        assert_eq!(go(true), 2);
        assert_eq!(go(false), 1);
    }

    /// A two-variant `Option`-shaped test enum, distinct from the prelude
    /// ids and `testkit::variant()`'s `TypeId(0)`.
    const ENUM_TID: TypeId = TypeId(10);

    fn enum_val(heap: &mut ProcHeap, variant_idx: u16, payload: &[Value]) -> Value {
        let (vn, labels): (&str, &[&str]) = if variant_idx == 0 {
            ("Some", &["0"])
        } else {
            ("None", &[])
        };
        Value::enum_with_names_in(heap, ENUM_TID, variant_idx, "Opt", vn, labels, payload)
    }

    /// `unwrap_or_zero(o) = match o { Some(v) -> v, None -> 0 }` — an
    /// exhaustive all-constructor match, the `SwitchTag` shape.
    fn unwrap_fn(enum_ty: RTy, int: RTy) -> CoreFn {
        let arms = vec![
            (
                CorePat::Ctor {
                    variant: testkit::vref(ENUM_TID.0, 0),
                    fields: vec![testkit::bind(1, int)],
                },
                CoreExpr::Tail(Atom::Local(local(1))),
            ),
            (
                CorePat::Ctor {
                    variant: testkit::vref(ENUM_TID.0, 1),
                    fields: vec![],
                },
                CoreExpr::Tail(Atom::Const(ConstId(2))),
            ),
        ];
        testkit::func(
            vec![testkit::bind(0, enum_ty)],
            CoreExpr::Match {
                scrut: local(0),
                arms,
                ty: int,
            },
            int,
        )
    }

    #[test]
    fn match_switch_over_enum_variants() {
        let (mut pool, int) = int_pool();
        let et = pool.mk_con(ENUM_TID, StrId(0), &[]);
        let f = unwrap_fn(et, int);
        // The gate recovered the variant count from the exhaustive
        // all-constructor shape.
        let p = plan(FuncIdx(0), &f, &pool, &test_prelude(), &test_counts);
        assert_eq!(p.switch_counts.get(&ENUM_TID), Some(&2));

        let j = jit(&[f], &pool, &test_consts());
        let mut h = ProcHeap::new();
        take_freed_objects();
        let some = enum_val(&mut h, 0, &[Value::small_int(41)]);
        let none = enum_val(&mut h, 1, &[]);
        assert_eq!(
            run(&j, 0, std::slice::from_ref(&some), 1 << 40).0.as_int(),
            Some(41)
        );
        assert_eq!(
            run(&j, 0, std::slice::from_ref(&none), 1 << 40).0.as_int(),
            Some(0)
        );
        // Retains balanced: the runs freed nothing, and our handles still
        // free their cells.
        assert_eq!(take_freed_objects(), 0);
        drop(some);
        drop(none);
        assert!(take_freed_objects() > 0);
    }

    /// The payload bind takes its own reference: a returned heap payload must
    /// outlive the cell it was read from.
    #[test]
    fn match_payload_bind_retains_the_field() {
        let (mut pool, int) = int_pool();
        let et = pool.mk_con(ENUM_TID, StrId(0), &[]);
        let f = unwrap_fn(et, int);
        let j = jit(&[f], &pool, &test_consts());
        let mut h = ProcHeap::new();
        take_freed_objects();
        let big = Value::int_in(&mut h, i64::MAX); // mortal BigInt payload
        let some = enum_val(&mut h, 0, std::slice::from_ref(&big));
        drop(big); // the cell holds its own reference (`store_child` retained)
        let (v, _) = run(&j, 0, std::slice::from_ref(&some), 1 << 40);
        drop(some); // frees the cell; the returned payload keeps its own ref
        assert_eq!(v.as_int(), Some(i64::MAX));
        drop(v);
        assert!(take_freed_objects() > 0);
    }

    #[test]
    fn match_ladder_over_enum_heads_and_wildcard() {
        let (mut pool, int) = int_pool();
        let et = pool.mk_con(ENUM_TID, StrId(0), &[]);
        // f(o) = match o { Some(v) -> v, _ -> 1 } — not all-constructor, so
        // the `MatchEnum`-shaped tag-compare ladder.
        let arms = vec![
            (
                CorePat::Ctor {
                    variant: testkit::vref(ENUM_TID.0, 0),
                    fields: vec![testkit::bind(1, int)],
                },
                CoreExpr::Tail(Atom::Local(local(1))),
            ),
            (CorePat::Wild, CoreExpr::Tail(Atom::Const(ConstId(1)))),
        ];
        let f = testkit::func(
            vec![testkit::bind(0, et)],
            CoreExpr::Match {
                scrut: local(0),
                arms,
                ty: int,
            },
            int,
        );
        let p = plan(FuncIdx(0), &f, &pool, &test_prelude(), &test_counts);
        assert!(p.switch_counts.is_empty());

        let j = jit(&[f], &pool, &test_consts());
        let mut h = ProcHeap::new();
        let some = enum_val(&mut h, 0, &[Value::small_int(7)]);
        let none = enum_val(&mut h, 1, &[]);
        assert_eq!(run(&j, 0, &[some], 1 << 40).0.as_int(), Some(7));
        assert_eq!(run(&j, 0, &[none], 1 << 40).0.as_int(), Some(1));
    }

    #[test]
    fn tuple_index_reads_elements() {
        let (mut pool, int) = int_pool();
        let tup = pool.mk_tuple(&[int, int]);
        // f(t) = t.0 + t.1
        let idx = |i: u16| Atom::PrimOp {
            op: Op::TupleIndex,
            args: vec![local(0)],
            imm: Imm::Index(i),
        };
        let body = let_(
            1,
            int,
            idx(0),
            let_(
                2,
                int,
                idx(1),
                CoreExpr::Tail(Atom::prim(Op::AddInt, vec![local(1), local(2)])),
            ),
        );
        let f = testkit::func(vec![testkit::bind(0, tup)], body, int);
        let j = jit(&[f], &pool, &test_consts());
        let mut h = ProcHeap::new();
        let t = Value::tuple_in(&mut h, &[Value::small_int(40), Value::small_int(2)]);
        assert_eq!(run(&j, 0, &[t], 1 << 40).0.as_int(), Some(42));
    }

    #[test]
    fn tuple_index_gate_needs_the_width_proof() {
        let (mut pool, int) = int_pool();
        let tup = pool.mk_tuple(&[int, int]);
        let out_of_range = Atom::PrimOp {
            op: Op::TupleIndex,
            args: vec![local(0)],
            imm: Imm::Index(2),
        };
        let f = testkit::func(
            vec![testkit::bind(0, tup)],
            let_(1, int, out_of_range, CoreExpr::Tail(Atom::Local(local(1)))),
            int,
        );
        // An out-of-range index is malformed IR the checker cannot produce.
        // Planning no longer screens it: the site loses its tuple proof and
        // lowers through the checking bridge, which reports the bad index.
        let _ = plan(FuncIdx(0), &f, &pool, &test_prelude(), &test_counts);
    }

    #[test]
    fn get_field_unchecked_reads_enum_fields() {
        let (mut pool, int) = int_pool();
        let et = pool.mk_con(ENUM_TID, StrId(0), &[]);
        // f(p) = p#0 + p#1 — the checker-proven record-field reads.
        let fld = |i: u16| Atom::PrimOp {
            op: Op::GetFieldUnchecked,
            args: vec![local(0)],
            imm: Imm::Index(i),
        };
        let body = let_(
            1,
            int,
            fld(0),
            let_(
                2,
                int,
                fld(1),
                CoreExpr::Tail(Atom::prim(Op::AddInt, vec![local(1), local(2)])),
            ),
        );
        let f = testkit::func(vec![testkit::bind(0, et)], body, int);
        let j = jit(&[f], &pool, &test_consts());
        let mut h = ProcHeap::new();
        let p = Value::enum_with_names_in(
            &mut h,
            ENUM_TID,
            0,
            "Point",
            "Point",
            &["x", "y"],
            &[Value::small_int(30), Value::small_int(12)],
        );
        assert_eq!(run(&j, 0, &[p], 1 << 40).0.as_int(), Some(42));
    }

    #[test]
    fn letcont_goto_shares_one_failure_continuation() {
        let (pool, int) = int_pool();
        // f(n) = letc j = ret 2 in if n == 0 { goto j } else { ret 1 }
        let body = let_(
            1,
            int,
            Atom::Const(ConstId(2)),
            let_(
                2,
                int,
                Atom::prim(Op::EqInt, vec![local(0), local(1)]),
                CoreExpr::LetCont {
                    id: JoinId(0),
                    cont: Box::new(CoreExpr::Tail(Atom::Const(ConstId(0)))),
                    body: Box::new(CoreExpr::If {
                        cond: local(2),
                        then: Box::new(CoreExpr::Goto(JoinId(0))),
                        els: Box::new(CoreExpr::Tail(Atom::Const(ConstId(1)))),
                        ty: int,
                    }),
                },
            ),
        );
        let f = testkit::func(vec![testkit::bind(0, int)], body, int);
        let j = jit(&[f], &pool, &test_consts());
        let go = |n: i64| run(&j, 0, &[Value::small_int(n)], 1 << 40).0.as_int();
        assert_eq!(go(0), Some(2));
        assert_eq!(go(5), Some(1));
    }

    #[test]
    fn letjoin_merges_a_value_from_both_arms() {
        let (pool, int) = int_pool();
        // f(n) = let m = (if n == 0 { 1 } else { n + n }); m + 1
        let join = CoreExpr::If {
            cond: local(2),
            then: Box::new(CoreExpr::Tail(Atom::Const(ConstId(1)))),
            els: Box::new(CoreExpr::Tail(Atom::prim(
                Op::AddInt,
                vec![local(0), local(0)],
            ))),
            ty: int,
        };
        let body = let_(
            1,
            int,
            Atom::Const(ConstId(2)),
            let_(
                2,
                int,
                Atom::prim(Op::EqInt, vec![local(0), local(1)]),
                CoreExpr::LetJoin {
                    bind: testkit::bind(3, int),
                    join: Box::new(join),
                    body: Box::new(let_(
                        4,
                        int,
                        Atom::Const(ConstId(1)),
                        CoreExpr::Tail(Atom::prim(Op::AddInt, vec![local(3), local(4)])),
                    )),
                },
            ),
        );
        let f = testkit::func(vec![testkit::bind(0, int)], body, int);
        let j = jit(&[f], &pool, &test_consts());
        let go = |n: i64| run(&j, 0, &[Value::small_int(n)], 1 << 40).0.as_int();
        assert_eq!(go(0), Some(2));
        assert_eq!(go(21), Some(43));
    }

    #[test]
    fn drop_on_a_spilled_int_frees_through_release_at_zero() {
        let (pool, int) = int_pool();
        // f(n) = drop n; ret 1  — with a BigInt argument the drop must free.
        let body = CoreExpr::Drop {
            local: local(0),
            shape: None,
            body: Box::new(CoreExpr::Tail(Atom::Const(ConstId(1)))),
        };
        let f = testkit::func(vec![testkit::bind(0, int)], body, int);
        let j = jit(&[f], &pool, &test_consts());

        let mut heap = ProcHeap::new();
        let big = Value::int_in(&mut heap, i64::MAX);
        take_freed_objects();
        let (v, _) = run(&j, 0, std::slice::from_ref(&big), 1 << 40);
        drop(big); // release the test's own handle: rc 2 -> 1 -> the drop's 0
        assert_eq!(take_freed_objects(), 1, "the argument box must be freed");
        assert_eq!(v.as_int(), Some(1));
    }

    #[test]
    fn cross_function_tail_call_goes_through_the_trampoline() {
        let (pool, int) = int_pool();
        // f0(n) = tail f1(n + 1);   f1(n) = n + n
        let f0 = testkit::func(
            vec![testkit::bind(0, int)],
            let_(
                1,
                int,
                Atom::Const(ConstId(1)),
                let_(
                    2,
                    int,
                    Atom::prim(Op::AddInt, vec![local(0), local(1)]),
                    CoreExpr::Tail(Atom::Call {
                        callee: Callee::Known(FuncIdx(1)),
                        args: vec![local(2)],
                    }),
                ),
            ),
            int,
        );
        let f1 = testkit::func(
            vec![testkit::bind(0, int)],
            CoreExpr::Tail(Atom::prim(Op::AddInt, vec![local(0), local(0)])),
            int,
        );
        let j = jit(&[f0, f1], &pool, &test_consts());
        let (v, _) = run(&j, 0, &[Value::small_int(20)], 1 << 40);
        assert_eq!(v.as_int(), Some(42));
    }

    #[test]
    fn closure_alloc_and_dynamic_call_roundtrip() {
        let (mut pool, int) = int_pool();
        let clo = pool.mk_bound(0);
        // f0(n) = let c = closure(f1); let r = c(n); drop c; r
        // f1(m) = m + m
        let f0 = testkit::func(
            vec![testkit::bind(0, int)],
            let_(
                1,
                clo,
                Atom::Closure {
                    func_idx: FuncIdx(1),
                    captures: vec![],
                },
                let_(
                    2,
                    int,
                    Atom::Call {
                        callee: Callee::Local(local(1)),
                        args: vec![local(0)],
                    },
                    CoreExpr::Drop {
                        local: local(1),
                        shape: None,
                        body: Box::new(CoreExpr::Tail(Atom::Local(local(2)))),
                    },
                ),
            ),
            int,
        );
        let f1 = testkit::func(
            vec![testkit::bind(0, int)],
            CoreExpr::Tail(Atom::prim(Op::AddInt, vec![local(0), local(0)])),
            int,
        );
        let j = jit(&[f0, f1], &pool, &test_consts());
        take_freed_objects();
        let (v, _) = run(&j, 0, &[Value::small_int(21)], 1 << 40);
        assert_eq!(v.as_int(), Some(42));
        // One mortal cell was allocated (the closure) and its two references
        // (slot + callee-frame handle) are both released by the end.
        assert_eq!(take_freed_objects(), 1, "the closure cell must be freed");
    }

    #[test]
    fn closure_captures_are_retained_and_released() {
        let (mut pool, int) = int_pool();
        let clo = pool.mk_bound(0);
        // f0(x) = let c = closure(f1, [x]); drop x; drop c; 1
        let f0 = testkit::func(
            vec![testkit::bind(0, int)],
            let_(
                1,
                clo,
                Atom::Closure {
                    func_idx: FuncIdx(1),
                    captures: vec![local(0)],
                },
                CoreExpr::Drop {
                    local: local(0),
                    shape: None,
                    body: Box::new(CoreExpr::Drop {
                        local: local(1),
                        shape: None,
                        body: Box::new(CoreExpr::Tail(Atom::Const(ConstId(1)))),
                    }),
                },
            ),
            int,
        );
        let f1 = testkit::func(
            vec![testkit::bind(0, int)],
            CoreExpr::Tail(Atom::Local(local(0))),
            int,
        );
        let j = jit(&[f0, f1], &pool, &test_consts());

        let mut heap = ProcHeap::new();
        let big = Value::int_in(&mut heap, i64::MAX);
        take_freed_objects();
        let (v, _) = run(&j, 0, std::slice::from_ref(&big), 1 << 40);
        assert_eq!(v.as_int(), Some(1));
        // `drop x` released the slot's reference while the closure kept the
        // capture alive; `drop c` freed the closure and released the capture.
        assert_eq!(take_freed_objects(), 1, "only the closure is freed");
        drop(big); // the capture's release left rc 1: the test's own handle
        assert_eq!(take_freed_objects(), 1, "the captured box frees last");
    }

    #[test]
    fn dynamic_tail_call_collapses_through_the_trampoline() {
        let (mut pool, int) = int_pool();
        let clo = pool.mk_bound(0);
        // f0(n) = let c = closure(f1); tail c(n);   f1(m) = m + m
        let f0 = testkit::func(
            vec![testkit::bind(0, int)],
            let_(
                1,
                clo,
                Atom::Closure {
                    func_idx: FuncIdx(1),
                    captures: vec![],
                },
                CoreExpr::Tail(Atom::Call {
                    callee: Callee::Local(local(1)),
                    args: vec![local(0)],
                }),
            ),
            int,
        );
        let f1 = testkit::func(
            vec![testkit::bind(0, int)],
            CoreExpr::Tail(Atom::prim(Op::AddInt, vec![local(0), local(0)])),
            int,
        );
        let j = jit(&[f0, f1], &pool, &test_consts());
        take_freed_objects();
        let (v, _) = run(&j, 0, &[Value::small_int(20)], 1 << 40);
        assert_eq!(v.as_int(), Some(40));
        // The collapse released the caller's slots; the collapsed frame's
        // handle was the last reference, dropped when the callee returned.
        assert_eq!(take_freed_objects(), 1, "the closure cell must be freed");
    }

    #[test]
    fn enum_ctor_allocates_through_the_shim() {
        let (mut pool, int) = int_pool();
        let et = pool.mk_con(TypeId(7), StrId(0), &[]);
        // mk(x) = ctor 7.1(x)
        let f = testkit::func(
            vec![testkit::bind(0, int)],
            CoreExpr::Tail(Atom::Ctor {
                variant: testkit::vref(7, 1),
                fields: vec![local(0)],
                reuse: None,
            }),
            et,
        );
        let (program, layouts) = ctor_program(std::slice::from_ref(&f), &test_consts());
        let j = jit_with(&[f], &pool, program, &layouts);
        let (v, _) = run(&j, 0, &[Value::small_int(41)], 1 << 40);
        let e = v.as_enum().expect("the shim must build an Enum cell");
        assert_eq!(e.type_id(), TypeId(7));
        assert_eq!(e.variant_idx(), 1);
        assert_eq!(e.enum_name(), "T");
        assert_eq!(e.variant_name(), "T");
        assert_eq!(e.payload().len(), 1);
        assert_eq!(e.payload()[0].as_int(), Some(41));
        // Lazy hash: the word was written 0, computed on first use.
        assert_ne!(v.as_enum().unwrap().hash(), 0);
    }

    #[test]
    fn enum_ctor_reuses_a_hollowed_cell_in_place() {
        let (mut pool, int) = int_pool();
        let et = pool.mk_con(TypeId(7), StrId(0), &[]);
        // re(c, x) = ctor 7.1(x) reuse c — the candidate arrives parked in
        // the parameter slot, the shape `Op::Drop` leaves behind.
        let f = testkit::func(
            vec![testkit::bind(0, et), testkit::bind(1, int)],
            CoreExpr::Tail(Atom::Ctor {
                variant: testkit::vref(7, 1),
                fields: vec![local(1)],
                reuse: Some(local(0)),
            }),
            et,
        );
        let (program, layouts) = ctor_program(std::slice::from_ref(&f), &test_consts());
        let j = jit_with(&[f], &pool, program, &layouts);

        // Drive by hand: the parked cell must sit in the slot at rc == 1,
        // which `run`'s argument clone would break.
        let mut vm = TestVm {
            ctx: NativeCtx::new(),
            stack: Vec::new(),
            frames: Vec::new(),
            trampoline: j.trampoline,
            funcs: j
                .metas
                .iter()
                .map(|m| FnMeta {
                    arity: m.arity,
                    locals: m.locals,
                })
                .collect(),
            entries: j.entries.clone(),
            heap: ProcHeap::new(),
            reds: 1 << 40,
            budget: 1 << 40,
            yields: 0,
        };
        let mut cell = Value::enum_with_names_in(
            &mut vm.heap,
            TypeId(7),
            0,
            "T",
            "T",
            &[],
            &[Value::small_int(9)],
        );
        let addr = cell.object_addr().expect("a mortal enum cell");
        cell.hollow_for_reuse();
        vm.stack.push(cell); // rc == 1, exactly a parked candidate
        vm.stack.push(Value::small_int(41));
        vm.push_frame(0, 2);

        take_freed_objects();
        let status = vm.drive();
        assert_eq!(status, NativeStatus::Done as u64);
        // In place: nothing was freed (the candidate was consumed, not
        // dropped) and the result IS the old cell — no fresh allocation.
        assert_eq!(take_freed_objects(), 0);
        let v = vm.stack.pop().unwrap();
        assert_eq!(v.object_addr(), Some(addr));
        let e = v.as_enum().expect("an Enum cell");
        assert_eq!(e.type_id(), TypeId(7));
        assert_eq!(e.variant_idx(), 1, "the variant word must be rewritten");
        assert_eq!(e.payload().len(), 1);
        assert_eq!(e.payload()[0].as_int(), Some(41));
        // The hash word was rewritten to 0 (lazy), not left as the old
        // cell's cached hash.
        assert_ne!(e.hash(), 0);
    }

    #[test]
    fn enum_ctor_reuse_falls_back_to_alloc_on_a_cleared_slot() {
        let (mut pool, int) = int_pool();
        let et = pool.mk_con(TypeId(7), StrId(0), &[]);
        let f = testkit::func(
            vec![testkit::bind(0, et), testkit::bind(1, int)],
            CoreExpr::Tail(Atom::Ctor {
                variant: testkit::vref(7, 1),
                fields: vec![local(1)],
                reuse: Some(local(0)),
            }),
            et,
        );
        let (program, layouts) = ctor_program(std::slice::from_ref(&f), &test_consts());
        let j = jit_with(&[f], &pool, program, &layouts);
        // A shared `Drop` clears the slot to `small_int(0)`; the ctor must
        // fall through to a fresh allocation.
        let (v, _) = run(&j, 0, &[Value::small_int(0), Value::small_int(41)], 1 << 40);
        let e = v.as_enum().expect("the fallback must allocate an Enum");
        assert_eq!(e.variant_idx(), 1);
        assert_eq!(e.payload()[0].as_int(), Some(41));
    }

    /// `re(c, x) = drop c (shape); ctor 7.1(x) reuse c` — the producer side
    /// of reuse pairing.
    fn reuse_pair_fn(pool: &mut ResolvedPool, int: crate::typed_ir::RTy) -> CoreFn {
        let et = pool.mk_con(TypeId(7), StrId(0), &[]);
        testkit::func(
            vec![testkit::bind(0, et), testkit::bind(1, int)],
            CoreExpr::Drop {
                local: local(0),
                shape: Some(super::super::ReuseShape::enum_(1)),
                body: Box::new(CoreExpr::Tail(Atom::Ctor {
                    variant: testkit::vref(7, 1),
                    fields: vec![local(1)],
                    reuse: Some(local(0)),
                })),
            },
            et,
        )
    }

    fn hand_vm(j: &Jit, budget: i64) -> TestVm {
        TestVm {
            ctx: NativeCtx::new(),
            stack: Vec::new(),
            frames: Vec::new(),
            trampoline: j.trampoline,
            funcs: j
                .metas
                .iter()
                .map(|m| FnMeta {
                    arity: m.arity,
                    locals: m.locals,
                })
                .collect(),
            entries: j.entries.clone(),
            heap: ProcHeap::new(),
            reds: budget,
            budget,
            yields: 0,
        }
    }

    /// A `shape: Some` drop of a uniquely-owned cell hollows it, releasing
    /// children at the drop point, and parks it in its slot so the paired
    /// ctor rewrites it in place.
    #[test]
    fn reusable_drop_hollows_a_unique_cell_and_parks_it_for_the_ctor() {
        let (mut pool, int) = int_pool();
        let f = reuse_pair_fn(&mut pool, int);
        let (program, layouts) = ctor_program(std::slice::from_ref(&f), &test_consts());
        let j = jit_with(&[f], &pool, program, &layouts);
        let mut vm = hand_vm(&j, 1 << 40);

        // A mortal heap child proves the hollow released it: the outside
        // reference returns to rc == 1 only if the walk ran. The name/label
        // words are frozen, so the walk over them is a no-op.
        let child = Value::int_in(&mut vm.heap, i64::MAX);
        let area = std::sync::Arc::new(crate::frozen::FrozenArea::new());
        let mut fb = area.builder();
        let en = fb.str("T").into_value();
        let vn = fb.str("T").into_value();
        let labels = fb.tuple(Vec::new()).into_value();
        let cell = Value::enum_in(
            &mut vm.heap,
            TypeId(7),
            0,
            0,
            en,
            vn,
            labels,
            std::slice::from_ref(&child),
        );
        assert!(!child.is_unique(), "the cell holds a second reference");
        let addr = cell.object_addr().expect("a mortal enum cell");
        vm.stack.push(cell); // rc == 1: uniquely owned by the frame
        vm.stack.push(Value::small_int(41));
        vm.push_frame(0, 2);

        take_freed_objects();
        let status = vm.drive();
        assert_eq!(status, NativeStatus::Done as u64);
        assert!(child.is_unique(), "the hollow released the cell's child");
        // The drop parked the cell rather than freeing it, and the paired
        // ctor consumed it in place: nothing was reclaimed on the way.
        assert_eq!(
            take_freed_objects(),
            0,
            "the drop must park the cell for the paired ctor, not free it"
        );
        let v = vm.stack.pop().unwrap();
        assert_eq!(v.object_addr(), Some(addr), "the result IS the old cell");
        let e = v.as_enum().expect("an Enum cell");
        assert_eq!(e.variant_idx(), 1, "rewritten in place");
        assert_eq!(e.payload()[0].as_int(), Some(41));
    }

    /// A `shape: Some` drop of a shared cell must not hollow: the other owner
    /// still sees the children. It releases the frame's reference and clears
    /// the slot, so the paired ctor allocates fresh.
    #[test]
    fn reusable_drop_of_a_shared_cell_releases_and_clears_the_slot() {
        let (mut pool, int) = int_pool();
        let f = reuse_pair_fn(&mut pool, int);
        let (program, layouts) = ctor_program(std::slice::from_ref(&f), &test_consts());
        let j = jit_with(&[f], &pool, program, &layouts);
        let mut vm = hand_vm(&j, 1 << 40);

        let cell = Value::enum_with_names_in(
            &mut vm.heap,
            TypeId(7),
            0,
            "T",
            "T",
            &[],
            &[Value::small_int(9)],
        );
        let keep = cell.clone(); // rc == 2: shared
        vm.stack.push(cell);
        vm.stack.push(Value::small_int(41));
        vm.push_frame(0, 2);

        take_freed_objects();
        let status = vm.drive();
        assert_eq!(status, NativeStatus::Done as u64);
        assert_eq!(take_freed_objects(), 0, "a shared drop frees nothing");
        assert!(
            keep.is_unique(),
            "the frame's reference was released exactly once"
        );
        let v = vm.stack.pop().unwrap();
        assert_ne!(
            v.object_addr(),
            keep.object_addr(),
            "no candidate was parked: the ctor allocated fresh"
        );
        let e = keep.as_enum().expect("the survivor is untouched");
        assert_eq!(e.variant_idx(), 0);
        assert_eq!(e.payload()[0].as_int(), Some(9));
        let r = v.as_enum().expect("a fresh Enum cell");
        assert_eq!(r.variant_idx(), 1);
        assert_eq!(r.payload()[0].as_int(), Some(41));
    }

    /// A slotted field is retained for the cell, and dropping the cell
    /// releases exactly the references it took.
    #[test]
    fn enum_ctor_field_references_balance() {
        let (mut pool, int) = int_pool();
        let et = pool.mk_con(TypeId(7), StrId(0), &[]);
        // mk(x) = let e = ctor 7.0(x); drop x; e
        let f = testkit::func(
            vec![testkit::bind(0, int)],
            let_(
                1,
                et,
                Atom::Ctor {
                    variant: testkit::vref(7, 0),
                    fields: vec![local(0)],
                    reuse: None,
                },
                CoreExpr::Drop {
                    local: local(0),
                    shape: None,
                    body: Box::new(CoreExpr::Tail(Atom::Local(local(1)))),
                },
            ),
            et,
        );
        let (program, layouts) = ctor_program(std::slice::from_ref(&f), &test_consts());
        let j = jit_with(&[f], &pool, program, &layouts);

        let mut heap = ProcHeap::new();
        let big = Value::int_in(&mut heap, i64::MAX);
        take_freed_objects();
        let (v, _) = run(&j, 0, std::slice::from_ref(&big), 1 << 40);
        // The cell kept the field alive across `drop x`.
        assert_eq!(take_freed_objects(), 0);
        assert_eq!(v.as_enum().unwrap().payload()[0].as_int(), Some(i64::MAX));
        drop(v); // frees the cell and releases its field reference
        assert_eq!(take_freed_objects(), 1, "only the cell frees");
        drop(big); // the test's own handle was the last reference
        assert_eq!(take_freed_objects(), 1, "the boxed field frees last");
    }
}
