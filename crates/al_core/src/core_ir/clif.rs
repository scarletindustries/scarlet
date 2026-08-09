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
//! and signatures below must match the shim tables in `crates/al`
//! (`vm::native::rt_symbols`, `vm::native_shims::shim_symbols`) and the
//! imports `vm::jit` declares.

use std::collections::{HashMap, HashSet};

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{
    self, AbiParam, InstBuilder, JumpTableData, MemFlagsData, StackSlotData, StackSlotKind, types,
};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_module::{FuncId, Linkage, Module, ModuleError};

use super::emit::{self, EmitCtx, FrameLayout};
use super::native_frame::{
    self, FrameSlots, NATIVE_INT_BOX_SYMBOL, ValueBits, box_bool, box_int, unbox_int, value_bits,
};
use super::native_rc::{self, RcGate, emit_drop, emit_dup};
use super::{
    Atom, Callee, CoreBind, CoreExpr, CoreFn, CorePat, FuncIdx, Imm, JoinId, LocalId, VariantRef,
};
use crate::bytecode::value::{
    NATIVE_HOLLOW_FOR_REUSE_SYMBOL, NATIVE_MORTAL_HEAP_BITS, NATIVE_PTR_MASK,
    NATIVE_RC_BYTE_OFFSET, NATIVE_RELEASE_AT_ZERO_SYMBOL,
};
use crate::bytecode::{CtorRef, NativeCtx, NativeStatus, Op, PreludeBindings, TypeRef, Value};
use crate::tivec::{Idx, TiVec};
use crate::type_def::TypeId;
use crate::typed_ir::{RTy, ResolvedNode, ResolvedPool};
use crate::types::{Prim, StrId};

const SYM_RT_CALL: &str = "al_rt_call";
const SYM_RT_TAIL_CALL: &str = "al_rt_tail_call";
const SYM_RT_PUSH_FRAME: &str = "al_rt_push_frame";
const SYM_RT_DIRECT_RESULT: &str = "al_rt_direct_result";
const SYM_RT_CHECKPOINT: &str = "al_rt_checkpoint";
const SYM_RT_FRAME_BASE: &str = "al_rt_frame_base";
const SYM_RT_RET: &str = "al_rt_ret";
const SYM_RT_MAKE_CLOSURE: &str = "al_rt_make_closure";
const SYM_RT_CALL_VALUE: &str = "al_rt_call_value";
const SYM_RT_TAIL_CALL_VALUE: &str = "al_rt_tail_call_value";
const SYM_DIV_INT: &str = "al_shim_div_int";
const SYM_MOD_INT: &str = "al_shim_mod_int";
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

/// A gated body, captured pool-independently at hook time. Everything only
/// the per-body [`ResolvedPool`] could answer is resolved here.
pub struct NativePlan {
    pub func_idx: FuncIdx,
    fun: CoreFn,
    bools: BoolCtors,
    /// Per-local representation class, captured from each binding's `RTy`
    /// while the pool is alive.
    reprs: TiVec<LocalId, Repr>,
    /// The plan-time stand-in for `EmitCtx::switch_variant_count`, recovered
    /// from the match shape itself (see [`Gate::switch_shape`]). Both the
    /// layout re-emission and codegen answer the `SwitchTag`-vs-ladder
    /// question from this map, so they cannot disagree.
    switch_counts: HashMap<TypeId, u8>,
}

/// The representation class a local's `RTy` proves, deciding its unboxed
/// register views and how much RC machinery its dup/drop sites can skip.
/// Every arm is backed by a `value.rs` invariant; anything the types cannot
/// prove stays [`Repr::Dyn`], which is always correct.
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
            bool: prelude.bool,
            binary: prelude.binary,
        }
    }
}

/// The single place type facts become codegen decisions.
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
            bool: prelude.bool,
            true_: prelude.true_,
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
) -> Option<NativePlan> {
    let bools = BoolCtors::of(prelude);
    let mut gate = Gate {
        pool,
        bools,
        tys: ReprTys::of(prelude),
        reprs: TiVec::new(),
        local_tys: TiVec::new(),
        switch_counts: HashMap::new(),
    };
    for p in &f.params {
        gate.record(p);
    }
    gate.expr(&f.body)?;
    Some(NativePlan {
        func_idx,
        fun: f.clone(),
        bools,
        reprs: gate.reprs,
        switch_counts: gate.switch_counts,
    })
}

impl NativePlan {
    fn repr_of(&self, id: LocalId) -> Repr {
        self.reprs.get(id).copied().unwrap_or_default()
    }

    fn is_int(&self, id: LocalId) -> bool {
        self.repr_of(id) == Repr::Int
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

struct Gate<'a> {
    pool: &'a ResolvedPool,
    bools: BoolCtors,
    tys: ReprTys,
    reprs: TiVec<LocalId, Repr>,
    /// Each recorded local's full `RTy`, for gate clauses needing more than
    /// its [`Repr`]. Pool-relative, so it dies with the gate, not the plan.
    local_tys: TiVec<LocalId, Option<RTy>>,
    switch_counts: HashMap<TypeId, u8>,
}

impl Gate<'_> {
    fn record(&mut self, b: &CoreBind) {
        self.reprs.resize_at_least(b.id, Repr::default());
        self.reprs[b.id] = classify(self.pool, self.tys, b.ty);
        self.local_tys.resize_at_least(b.id, None);
        self.local_tys[b.id] = Some(b.ty);
    }

    fn ty_of(&self, id: LocalId) -> Option<RTy> {
        self.local_tys.get(id).copied().flatten()
    }

    /// The plan-time half of emit's `switch_plan`: does this match compile to
    /// a `SwitchTag`, and over how many variants?
    ///
    /// Without the compiler's type table the variant count comes from the
    /// match itself. An all-constructor match with every variant distinct is
    /// exhaustive, so its arm count *is* the type's variant count, which is
    /// what `EmitCtx::switch_variant_count` returns. `Bool` never switches
    /// (immediate scrutinee, no tag word) and >255 variants overflow
    /// `SwitchTag.a` on both sides. Any other all-constructor shape ladders
    /// under the real context too, so omitting it here cannot diverge.
    fn switch_shape(&self, arms: &[(CorePat, CoreExpr)]) -> Option<(TypeId, u8)> {
        if arms.is_empty() || arms.len() > 255 {
            return None;
        }
        let mut tid: Option<TypeId> = None;
        let mut seen = vec![false; arms.len()];
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
                _ => {}
            }
            let i = variant.variant_idx as usize;
            if i >= arms.len() || seen[i] {
                return None;
            }
            seen[i] = true;
        }
        Some((tid?, arms.len() as u8))
    }

    fn is_int(&self, id: LocalId) -> bool {
        self.reprs.get(id).copied().unwrap_or_default() == Repr::Int
    }

    /// The pool proves `id` an `Array`: a persistent tree or a lazy `Range`,
    /// the two shapes the seq shims are total over.
    fn is_array(&self, id: LocalId) -> bool {
        match self.ty_of(id) {
            Some(t) => matches!(
                self.pool.node(t),
                ResolvedNode::Con { id, .. } if id == self.pool.prims().array
            ),
            None => false,
        }
    }

    /// The pool proves `id` a `Binary` (nominal — see [`ReprTys`]).
    fn is_binary(&self, id: LocalId) -> bool {
        match self.ty_of(id) {
            Some(t) => matches!(
                self.pool.node(t),
                ResolvedNode::Con { id, .. } if self.tys.binary.is(id)
            ),
            None => false,
        }
    }

    fn atom(&self, a: &Atom) -> Option<()> {
        match a {
            Atom::Local(_) | Atom::Const(_) => Some(()),
            // `TupleIndex` elides the interpreter's bounds/type errors, so
            // the pool must prove the receiver a wide-enough tuple.
            // `GetFieldUnchecked` carries the checker's own field proof.
            Atom::PrimOp {
                op: Op::TupleIndex,
                args,
                imm: Imm::Index(i),
            } => {
                let [recv] = args.as_slice() else {
                    return None;
                };
                let t = self.ty_of(*recv)?;
                self.pool.tuple_elem(t, *i as usize).map(|_| ())
            }
            Atom::PrimOp {
                op: Op::GetFieldUnchecked,
                args,
                imm: Imm::Index(_),
            } => (args.len() == 1).then_some(()),
            // `PushGlobal` reads the entry frame's slot, written once by
            // `__main__` before any body that reads it runs. A shim call: the
            // global area is a `Vec` with no ABI-stable base.
            Atom::PrimOp {
                op: Op::PushGlobal,
                args,
                imm: Imm::Index(_),
            } => args.is_empty().then_some(()),
            // A bare Bool head from `&&`/`||` lowering: a compile-time
            // immediate constant, no heap cell, no operand.
            Atom::PrimOp {
                op: Op::PushTrue | Op::PushFalse,
                args,
                imm: Imm::None,
            } => args.is_empty().then_some(()),
            // One allocator-shim call over an owned element buffer. `lower`
            // guarantees argc matches the operand list; codegen reads only
            // the list.
            Atom::PrimOp {
                op: Op::MakeArray | Op::MakeTuple,
                args,
                imm: Imm::Argc(n),
            } => (args.len() == *n as usize).then_some(()),
            // `args[0]` (Append) / `args[k]` (Prepend) is the sequence, the
            // rest the pushed elements. The shims elide the interpreter's
            // non-sequence error, so the pool must prove it an `Array`.
            Atom::PrimOp {
                op: Op::Append,
                args,
                imm: Imm::Argc(m),
            } => (args.len() == *m as usize + 1 && self.is_array(args[0])).then_some(()),
            Atom::PrimOp {
                op: Op::Prepend,
                args,
                imm: Imm::Argc(k),
            } => (args.len() == *k as usize + 1 && self.is_array(args[*k as usize])).then_some(()),
            // The pure HTTP whole-ops over proven operands. Their only
            // interpreter error is a shape check the proofs make unreachable.
            // The parking IO ops are deliberately not here: suspension cannot
            // cross a native frame.
            Atom::PrimOp {
                op: Op::HttpParseHead,
                args,
                imm: Imm::None,
            } => match args.as_slice() {
                [buf, off] => (self.is_binary(*buf) && self.is_int(*off)).then_some(()),
                _ => None,
            },
            Atom::PrimOp {
                op: Op::HttpHeadersValid | Op::HttpFraming,
                args,
                imm: Imm::None,
            } => match args.as_slice() {
                [headers] => self.is_array(*headers).then_some(()),
                _ => None,
            },
            Atom::PrimOp {
                op: Op::HttpHeaderHas,
                args,
                imm: Imm::None,
            } => match args.as_slice() {
                [headers, name] => (self.is_array(*headers) && self.is_binary(*name)).then_some(()),
                _ => None,
            },
            Atom::PrimOp {
                op: Op::HttpSerializeHead,
                args,
                imm: Imm::None,
            } => match args.as_slice() {
                [code, reason, headers] => {
                    (self.is_int(*code) && self.is_binary(*reason) && self.is_array(*headers))
                        .then_some(())
                }
                _ => None,
            },
            // Length reads, total under the same proofs. Narrower than
            // `seq_len`, which also accepts a Tuple; no stdlib body needs it.
            Atom::PrimOp {
                op: Op::ArrayLen,
                args,
                imm: Imm::None,
            } => match args.as_slice() {
                [recv] => self.is_array(*recv).then_some(()),
                _ => None,
            },
            Atom::PrimOp {
                op: Op::BinByteSize,
                args,
                imm: Imm::None,
            } => match args.as_slice() {
                [recv] => self.is_binary(*recv).then_some(()),
                _ => None,
            },
            Atom::PrimOp { op, args, imm } => {
                if *imm != Imm::None {
                    return None;
                }
                let (_, poly) = nop_of(*op)?;
                if poly && !args.iter().all(|&x| self.is_int(x)) {
                    return None;
                }
                Some(())
            }
            // Known/self dispatch through the entry table; a dynamic callee
            // goes to `al_rt_call_value`, which does the interpreter's
            // `Op::Call` checks and dispatch.
            Atom::Call { .. } => Some(()),
            // One allocator-shim call. The closure's own body gates
            // independently, and its capture reads are uncovered primops.
            Atom::Closure { .. } => Some(()),
            // `True`/`False` are VM immediates: no heap cell, any perceus
            // reuse pairing ignored. Every other constructor reuses the
            // paired dropped cell or calls `al_shim_enum_alloc`.
            Atom::Ctor {
                variant, fields, ..
            } => {
                if self.bools.polarity(variant).is_some() && !fields.is_empty() {
                    return None;
                }
                Some(())
            }
        }
    }

    fn expr(&mut self, mut e: &CoreExpr) -> Option<()> {
        loop {
            match e {
                CoreExpr::Let { bind, rhs, body } => {
                    self.atom(rhs)?;
                    self.record(bind);
                    e = body;
                }
                CoreExpr::LetJoin { bind, join, body } => {
                    self.expr(join)?;
                    self.record(bind);
                    e = body;
                }
                CoreExpr::LetCont { cont, body, .. } => {
                    // A cont references only locals bound before this node,
                    // so walking it here sees them recorded.
                    self.expr(cont)?;
                    e = body;
                }
                CoreExpr::Drop { body, .. } => e = body,
                CoreExpr::If { then, els, .. } => {
                    self.expr(then)?;
                    e = els;
                }
                CoreExpr::Match { arms, .. } => {
                    if let Some((tid, count)) = self.switch_shape(arms) {
                        // Two full matches over one type must agree on its
                        // variant count. A mismatch means a non-exhaustive
                        // match slipped past the checker, so interpret rather
                        // than emit a wrong jump table.
                        if *self.switch_counts.entry(tid).or_insert(count) != count {
                            return None;
                        }
                    }
                    for (pat, body) in arms {
                        match pat {
                            CorePat::Wild | CorePat::Lit(_) => {}
                            CorePat::Bind(b) => self.record(b),
                            // Bool's heads compare as immediates and are
                            // always nullary; enum heads bind their payload
                            // fields whether the match switches or ladders.
                            CorePat::Ctor { variant, fields } => {
                                if self.bools.polarity(variant).is_some() {
                                    if !fields.is_empty() {
                                        return None;
                                    }
                                } else {
                                    for f in fields {
                                        self.record(f);
                                    }
                                }
                            }
                        }
                        self.expr(body)?;
                    }
                    return Some(());
                }
                CoreExpr::Tail(a) => return self.atom(a),
                CoreExpr::Goto(_) => return Some(()),
            }
        }
    }
}

/// Diagnostic mirror of [`Gate`]: walks a whole body without short-circuiting
/// and names every node the gate rejects. Mirrors [`plan`] clause for clause,
/// so a body with no reasons here is exactly a body `plan` accepts.
#[cfg(test)]
pub(crate) mod gate_diag {
    use super::*;

    /// Every rejection reason in `f`, in walk order. Empty iff [`plan`]
    /// accepts the body.
    pub(crate) fn reasons(
        f: &CoreFn,
        pool: &ResolvedPool,
        prelude: &PreludeBindings,
    ) -> Vec<String> {
        let mut g = Gate {
            pool,
            bools: BoolCtors::of(prelude),
            tys: ReprTys::of(prelude),
            reprs: TiVec::new(),
            local_tys: TiVec::new(),
            switch_counts: HashMap::new(),
        };
        for p in &f.params {
            g.record(p);
        }
        let mut out = Vec::new();
        walk(&mut g, &f.body, &mut out);
        out
    }

    /// [`Gate::atom`], with `Some(reason)` at every `None` site.
    fn atom_reason(g: &Gate, a: &Atom) -> Option<String> {
        match a {
            Atom::Local(_) | Atom::Const(_) => None,
            Atom::PrimOp {
                op: Op::TupleIndex,
                args,
                imm: Imm::Index(i),
            } => {
                let [recv] = args.as_slice() else {
                    return Some("tuple-index-arity".into());
                };
                match g.ty_of(*recv) {
                    Some(t) if g.pool.tuple_elem(t, *i as usize).is_some() => None,
                    _ => Some("tuple-index-unproven".into()),
                }
            }
            Atom::PrimOp {
                op: Op::GetFieldUnchecked,
                args,
                imm: Imm::Index(_),
            } => (args.len() != 1).then(|| "get-field-arity".into()),
            Atom::PrimOp {
                op: Op::PushGlobal,
                args,
                imm: Imm::Index(_),
            } => (!args.is_empty()).then(|| "push-global-args".into()),
            Atom::PrimOp {
                op: Op::PushTrue | Op::PushFalse,
                args,
                imm: Imm::None,
            } => (!args.is_empty()).then(|| "bool-head-args".into()),
            Atom::PrimOp {
                op: Op::MakeArray | Op::MakeTuple,
                args,
                imm: Imm::Argc(n),
            } => (args.len() != *n as usize).then(|| "aggregate-argc-mismatch".into()),
            Atom::PrimOp {
                op: Op::Append,
                args,
                imm: Imm::Argc(m),
            } => (!(args.len() == *m as usize + 1 && g.is_array(args[0])))
                .then(|| "append-unproven-seq".into()),
            Atom::PrimOp {
                op: Op::Prepend,
                args,
                imm: Imm::Argc(k),
            } => (!(args.len() == *k as usize + 1 && g.is_array(args[*k as usize])))
                .then(|| "prepend-unproven-seq".into()),
            Atom::PrimOp {
                op:
                    op @ (Op::HttpParseHead
                    | Op::HttpHeadersValid
                    | Op::HttpFraming
                    | Op::HttpHeaderHas
                    | Op::HttpSerializeHead),
                args,
                imm: Imm::None,
            } => {
                let ok = match (op, args.as_slice()) {
                    (Op::HttpParseHead, [buf, off]) => g.is_binary(*buf) && g.is_int(*off),
                    (Op::HttpHeadersValid | Op::HttpFraming, [headers]) => g.is_array(*headers),
                    (Op::HttpHeaderHas, [headers, name]) => {
                        g.is_array(*headers) && g.is_binary(*name)
                    }
                    (Op::HttpSerializeHead, [code, reason, headers]) => {
                        g.is_int(*code) && g.is_binary(*reason) && g.is_array(*headers)
                    }
                    _ => false,
                };
                (!ok).then(|| format!("http-unproven:{op:?}"))
            }
            Atom::PrimOp {
                op: Op::ArrayLen,
                args,
                imm: Imm::None,
            } => match args.as_slice() {
                [recv] if g.is_array(*recv) => None,
                _ => Some("array-len-unproven".into()),
            },
            Atom::PrimOp {
                op: Op::BinByteSize,
                args,
                imm: Imm::None,
            } => match args.as_slice() {
                [recv] if g.is_binary(*recv) => None,
                _ => Some("bin-byte-size-unproven".into()),
            },
            Atom::PrimOp { op, args, imm } => {
                if *imm != Imm::None {
                    return Some(format!("op:{op:?}"));
                }
                match nop_of(*op) {
                    None => Some(format!("op:{op:?}")),
                    Some((_, poly)) => (poly && !args.iter().all(|&x| g.is_int(x)))
                        .then(|| format!("poly-non-int:{op:?}")),
                }
            }
            Atom::Call { .. } | Atom::Closure { .. } => None,
            Atom::Ctor {
                variant, fields, ..
            } => (g.bools.polarity(variant).is_some() && !fields.is_empty())
                .then(|| "bool-ctor-fields".into()),
        }
    }

    /// [`Gate::expr`], continuing past rejections and collecting them all.
    fn walk(g: &mut Gate, mut e: &CoreExpr, out: &mut Vec<String>) {
        loop {
            match e {
                CoreExpr::Let { bind, rhs, body } => {
                    if let Some(r) = atom_reason(g, rhs) {
                        out.push(r);
                    }
                    g.record(bind);
                    e = body;
                }
                CoreExpr::LetJoin { bind, join, body } => {
                    walk(g, join, out);
                    g.record(bind);
                    e = body;
                }
                CoreExpr::LetCont { cont, body, .. } => {
                    walk(g, cont, out);
                    e = body;
                }
                CoreExpr::Drop { body, .. } => e = body,
                CoreExpr::If { then, els, .. } => {
                    walk(g, then, out);
                    e = els;
                }
                CoreExpr::Match { arms, .. } => {
                    if let Some((tid, count)) = g.switch_shape(arms)
                        && *g.switch_counts.entry(tid).or_insert(count) != count
                    {
                        out.push("switch-count-mismatch".into());
                    }
                    for (pat, body) in arms {
                        match pat {
                            CorePat::Wild | CorePat::Lit(_) => {}
                            CorePat::Bind(b) => g.record(b),
                            CorePat::Ctor { variant, fields } => {
                                if g.bools.polarity(variant).is_some() {
                                    if !fields.is_empty() {
                                        out.push("bool-pat-fields".into());
                                    }
                                } else {
                                    for f in fields {
                                        g.record(f);
                                    }
                                }
                            }
                        }
                        walk(g, body, out);
                    }
                    return;
                }
                CoreExpr::Tail(a) => {
                    if let Some(r) = atom_reason(g, a) {
                        out.push(r);
                    }
                    return;
                }
                CoreExpr::Goto(_) => return,
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

/// The `EmitCtx` for the layout-only re-emission. On a gated body only
/// `bool_variant` and `switch_variant_count` shape the emitted stream, and
/// both come from the plan, so this run takes exactly the branches the real
/// emission took. The interning answers only fill operands, never branch, so
/// their stand-ins are constants.
struct LayoutCtx<'a> {
    bools: BoolCtors,
    switch_counts: &'a HashMap<TypeId, u8>,
}

impl EmitCtx for LayoutCtx<'_> {
    fn resolve_str(&self, _id: StrId) -> &str {
        ""
    }
    fn intern_int(&mut self, _i: i64) -> i32 {
        0
    }
    fn intern_str(&mut self, _s: &str) -> i32 {
        0
    }
    fn intern_labels(&mut self, _tid: TypeId, _variant_idx: u16) -> i32 {
        0
    }
    fn switch_variant_count(&self, tid: TypeId) -> Option<u8> {
        self.switch_counts.get(&tid).copied()
    }
    fn bool_variant(&self, tid: TypeId, variant_idx: u16) -> Option<bool> {
        self.bools.bool_variant(tid, variant_idx)
    }
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
    if v.is_heap() {
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
    freeze: Option<&mut crate::frozen::FrozenBuilder>,
) -> Option<Vec<EnumCtorSite>> {
    let mut atoms = Vec::new();
    collect_ctor_atoms(&plan.fun.body, &mut atoms);
    atoms.retain(|(v, _)| plan.bools.polarity(v).is_none());
    if atoms.is_empty() {
        return Some(Vec::new());
    }
    let f = program.functions.get(plan.func_idx.index())?;
    let start = usize::try_from(f.code_start).ok()?;
    let len = usize::try_from(f.code_len).ok()?;
    let code = program.code.get(start..start.checked_add(len)?)?;
    let consts: &[Value] = &program.constants;
    let mut freeze = freeze;
    let mut makes = code
        .iter()
        .enumerate()
        .filter(|(_, i)| i.op == Op::MakeEnumPayload);
    let mut sites = Vec::with_capacity(atoms.len());
    for (variant, arity) in atoms {
        let (at, make) = makes.next()?;
        if make.b as usize != arity {
            return None;
        }
        let reuse = make.a != 0;
        // `emit_ctor`'s site shape, back to front: [id, en, vn, labels]
        // consts, `arity` field pushes, an optional `Reuse`, the make.
        let header = at.checked_sub(4 + arity + usize::from(reuse))?;
        let cid = |i: usize| -> Option<usize> {
            let ins = code.get(header + i)?;
            (ins.op == Op::PushConst).then_some(ins.operand as usize)
        };
        let packed = consts.get(cid(0)?)?.as_int()? as u64;
        // The packed word must name the atom's own variant, or the cursor
        // pairing has drifted.
        let expect = (variant.type_id.0 as u32 as u64) | ((variant.variant_idx as u64) << 32);
        if packed != expect {
            return None;
        }
        let en = consts.get(cid(1)?)?;
        let vn = consts.get(cid(2)?)?;
        if en.as_str().is_none() || vn.as_str().is_none() {
            return None;
        }
        let arr_const = consts.get(cid(3)?)?;
        let arr = arr_const.as_array()?;
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
            reuse,
        });
    }
    if makes.next().is_some() {
        return None;
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

    fn atom(&mut self, a: &Atom) {
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
                Op::PushGlobal | Op::PushTrue | Op::PushFalse => {}
                _ => match nop_of(*op) {
                    Some((NOp::Not, _)) => {
                        for &x in args {
                            self.need_word(x);
                        }
                    }
                    Some(_) => {
                        for &x in args {
                            self.need_int(x);
                        }
                    }
                    None => unsupported_node("primop"),
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
                    self.atom(rhs);
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
                            if cv.boolean.is_some() {
                                // Bool words compare as bits.
                            } else if cv.int.is_some() {
                                // Decoding the scrutinee is sound only when
                                // the pool proved it Int. A spilled BigInt
                                // still decodes; anything else must not.
                                if !plan.is_int(*scrut) {
                                    return None;
                                }
                                self.need_int(*scrut);
                            } else {
                                // Other literal arms are uncovered: their
                                // equality is not a bit comparison.
                                return None;
                            }
                        }
                        self.expr(body, plan, cmap)?;
                    }
                    return Some(());
                }
                CoreExpr::Tail(a) => {
                    self.atom(a);
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
         Please report this as a compiler bug."
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
    int_box: ir::FuncRef,
    div_int: ir::FuncRef,
    mod_int: ir::FuncRef,
    rt_call: ir::FuncRef,
    rt_tail_call: ir::FuncRef,
    rt_push_frame: ir::FuncRef,
    rt_direct_result: ir::FuncRef,
    make_closure: ir::FuncRef,
    rt_call_value: ir::FuncRef,
    rt_tail_call_value: ir::FuncRef,
    rt_checkpoint: ir::FuncRef,
    rt_frame_base: ir::FuncRef,
    rt_ret: ir::FuncRef,
}

fn declare_imports<M: Module>(
    module: &mut M,
    func: &mut ir::Function,
) -> Result<RtRefs, Box<ModuleError>> {
    let ptr = module.target_config().pointer_type();
    let i64t = types::I64;
    let import = |module: &mut M,
                  name: &str,
                  params: &[ir::Type],
                  ret: Option<ir::Type>|
     -> Result<FuncId, Box<ModuleError>> {
        let mut sig = module.make_signature();
        sig.params.extend(params.iter().map(|&t| AbiParam::new(t)));
        sig.returns.extend(ret.map(AbiParam::new));
        Ok(module.declare_function(name, Linkage::Import, &sig)?)
    };
    let release = import(module, NATIVE_RELEASE_AT_ZERO_SYMBOL, &[ptr], None)?;
    let hollow = import(module, NATIVE_HOLLOW_FOR_REUSE_SYMBOL, &[ptr], None)?;
    let enum_alloc = import(
        module,
        SYM_ENUM_ALLOC,
        &[ptr, i64t, i64t, i64t, i64t, ptr, i64t],
        Some(i64t),
    )?;
    let make_array = import(module, SYM_MAKE_ARRAY, &[ptr, ptr, i64t], Some(i64t))?;
    let make_tuple = import(module, SYM_MAKE_TUPLE, &[ptr, ptr, i64t], Some(i64t))?;
    let seq_len = import(module, SYM_SEQ_LEN, &[ptr, i64t], Some(i64t))?;
    let seq_append = import(module, SYM_SEQ_APPEND, &[ptr, ptr, i64t], Some(i64t))?;
    let seq_prepend = import(module, SYM_SEQ_PREPEND, &[ptr, ptr, i64t], Some(i64t))?;
    let bin_byte_size = import(module, SYM_BIN_BYTE_SIZE, &[ptr, i64t], Some(i64t))?;
    let http_parse_head = import(module, SYM_HTTP_PARSE_HEAD, &[ptr, i64t, i64t], Some(i64t))?;
    let http_headers_valid = import(module, SYM_HTTP_HEADERS_VALID, &[i64t], Some(i64t))?;
    let http_header_has = import(module, SYM_HTTP_HEADER_HAS, &[i64t, i64t], Some(i64t))?;
    let http_serialize_head = import(
        module,
        SYM_HTTP_SERIALIZE_HEAD,
        &[ptr, i64t, i64t, i64t],
        Some(i64t),
    )?;
    let http_framing = import(module, SYM_HTTP_FRAMING, &[ptr, i64t], Some(i64t))?;
    let push_global = import(module, SYM_PUSH_GLOBAL, &[ptr, i64t], Some(i64t))?;
    let int_box = import(module, NATIVE_INT_BOX_SYMBOL, &[ptr, i64t], Some(i64t))?;
    let div_int = import(module, SYM_DIV_INT, &[i64t, i64t], Some(i64t))?;
    let mod_int = import(module, SYM_MOD_INT, &[i64t, i64t], Some(i64t))?;
    let rt_call = import(
        module,
        SYM_RT_CALL,
        &[ptr, i64t, i64t, ptr, i64t, ptr],
        Some(i64t),
    )?;
    let rt_tail_call = import(
        module,
        SYM_RT_TAIL_CALL,
        &[ptr, i64t, ptr, i64t],
        Some(i64t),
    )?;
    let rt_push_frame = import(
        module,
        SYM_RT_PUSH_FRAME,
        &[ptr, i64t, i64t, ptr, i64t],
        Some(i64t),
    )?;
    let rt_direct_result = import(module, SYM_RT_DIRECT_RESULT, &[ptr, i64t, ptr], Some(i64t))?;
    let make_closure = import(
        module,
        SYM_RT_MAKE_CLOSURE,
        &[ptr, i64t, ptr, i64t],
        Some(i64t),
    )?;
    let rt_call_value = import(
        module,
        SYM_RT_CALL_VALUE,
        &[ptr, i64t, i64t, ptr, i64t, ptr],
        Some(i64t),
    )?;
    let rt_tail_call_value = import(
        module,
        SYM_RT_TAIL_CALL_VALUE,
        &[ptr, i64t, ptr, i64t],
        Some(i64t),
    )?;
    let rt_checkpoint = import(module, SYM_RT_CHECKPOINT, &[ptr], Some(i64t))?;
    let rt_frame_base = import(module, SYM_RT_FRAME_BASE, &[ptr], Some(ptr))?;
    let rt_ret = import(module, SYM_RT_RET, &[ptr, i64t], None)?;
    Ok(RtRefs {
        release: module.declare_func_in_func(release, func),
        hollow: module.declare_func_in_func(hollow, func),
        enum_alloc: module.declare_func_in_func(enum_alloc, func),
        make_array: module.declare_func_in_func(make_array, func),
        make_tuple: module.declare_func_in_func(make_tuple, func),
        seq_len: module.declare_func_in_func(seq_len, func),
        seq_append: module.declare_func_in_func(seq_append, func),
        seq_prepend: module.declare_func_in_func(seq_prepend, func),
        bin_byte_size: module.declare_func_in_func(bin_byte_size, func),
        http_parse_head: module.declare_func_in_func(http_parse_head, func),
        http_headers_valid: module.declare_func_in_func(http_headers_valid, func),
        http_header_has: module.declare_func_in_func(http_header_has, func),
        http_serialize_head: module.declare_func_in_func(http_serialize_head, func),
        http_framing: module.declare_func_in_func(http_framing, func),
        push_global: module.declare_func_in_func(push_global, func),
        int_box: module.declare_func_in_func(int_box, func),
        div_int: module.declare_func_in_func(div_int, func),
        mod_int: module.declare_func_in_func(mod_int, func),
        rt_call: module.declare_func_in_func(rt_call, func),
        rt_tail_call: module.declare_func_in_func(rt_tail_call, func),
        rt_push_frame: module.declare_func_in_func(rt_push_frame, func),
        rt_direct_result: module.declare_func_in_func(rt_direct_result, func),
        make_closure: module.declare_func_in_func(make_closure, func),
        rt_call_value: module.declare_func_in_func(rt_call_value, func),
        rt_tail_call_value: module.declare_func_in_func(rt_tail_call_value, func),
        rt_checkpoint: module.declare_func_in_func(rt_checkpoint, func),
        rt_frame_base: module.declare_func_in_func(rt_frame_base, func),
        rt_ret: module.declare_func_in_func(rt_ret, func),
    })
}

/// Every statically-known call target the body reaches, for
/// [`declare_native_peers`]. `Callee::Self_` counts as a known call to
/// `self_idx`: covered bodies never read captures.
fn known_callees(self_idx: FuncIdx, fun: &CoreFn) -> HashSet<FuncIdx> {
    let mut out = HashSet::new();
    let mut stack = vec![&fun.body];
    let atom = |a: &Atom, out: &mut HashSet<FuncIdx>| match a {
        Atom::Call {
            callee: Callee::Known(f),
            ..
        } => {
            out.insert(*f);
        }
        Atom::Call {
            callee: Callee::Self_,
            ..
        } => {
            out.insert(self_idx);
        }
        _ => {}
    };
    while let Some(mut e) = stack.pop() {
        loop {
            match e {
                CoreExpr::Let { rhs, body, .. } => {
                    atom(rhs, &mut out);
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
                    for (_, body) in arms {
                        stack.push(body);
                    }
                    break;
                }
                CoreExpr::Tail(a) => {
                    atom(a, &mut out);
                    break;
                }
                CoreExpr::Goto(_) => break,
            }
        }
    }
    out
}

/// Declare a `FuncRef` for every native peer this body statically calls,
/// under the same `al_fn_{idx}` name and signature the peer's own [`compile`]
/// declares. A callee outside `native` gets no entry and falls back to the
/// trampoline shims.
fn declare_native_peers<M: Module>(
    module: &mut M,
    func: &mut ir::Function,
    self_idx: FuncIdx,
    fun: &CoreFn,
    native: &HashSet<FuncIdx>,
) -> Result<HashMap<FuncIdx, ir::FuncRef>, Box<ModuleError>> {
    let ptr_ty = module.target_config().pointer_type();
    let mut sig = module.make_signature();
    sig.params.push(AbiParam::new(ptr_ty));
    sig.returns.push(AbiParam::new(types::I64));
    let mut peers = HashMap::new();
    for f in known_callees(self_idx, fun) {
        if !native.contains(&f) {
            continue;
        }
        let name = format!("al_fn_{}", f.index());
        let id = module.declare_function(&name, Linkage::Export, &sig)?;
        peers.insert(f, module.declare_func_in_func(id, func));
    }
    Ok(peers)
}

/// The `FuncIdx`s whose [`compile`] will actually define a body: plans that
/// also pass the compile-time-only gates. Direct native→native call sites
/// must consult this. Naming a body outside it as a peer leaves its
/// `al_fn_{idx}` symbol undefined and `finalize_definitions` fails.
pub fn native_set(plans: &[NativePlan], program: &crate::bytecode::Program) -> HashSet<FuncIdx> {
    let consts: &[Value] = &program.constants;
    plans
        .iter()
        .filter(|p| {
            let Some(cmap) = resolve_consts(&p.fun, consts) else {
                return false;
            };
            Uses::scan(&p.fun, p, &cmap).is_some() && enum_ctor_sites(p, program, None).is_some()
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
    native: &HashSet<FuncIdx>,
    program: &crate::bytecode::Program,
) -> Result<Option<CompiledBody>, Box<ModuleError>> {
    let consts: &[Value] = &program.constants;
    let layout = emit::emit(
        &plan.fun,
        &mut LayoutCtx {
            bools: plan.bools,
            switch_counts: &plan.switch_counts,
        },
    )
    .layout;
    let Some(cmap) = resolve_consts(&plan.fun, consts) else {
        return Ok(None);
    };
    let Some(uses) = Uses::scan(&plan.fun, plan, &cmap) else {
        return Ok(None);
    };
    let mut frozen = program.frozen.builder();
    let Some(ctor_sites) = enum_ctor_sites(plan, program, Some(&mut frozen)) else {
        return Ok(None);
    };

    let ptr_ty = module.target_config().pointer_type();
    let mut sig = module.make_signature();
    sig.params.push(AbiParam::new(ptr_ty));
    sig.returns.push(AbiParam::new(types::I64));
    let name = format!("al_fn_{}", plan.func_idx.index());
    let func_id = module.declare_function(&name, Linkage::Export, &sig)?;

    let mut ctx = module.make_context();
    ctx.func.signature = sig;
    let mut fbc = FunctionBuilderContext::new();
    {
        let b = FunctionBuilder::new(&mut ctx.func, &mut fbc);
        let fns = declare_imports(module, b.func)?;
        let peers = declare_native_peers(module, b.func, plan.func_idx, &plan.fun, native)?;
        let g = BodyGen::prologue(b, plan, peers, layout, uses, cmap, ctor_sites, fns, ptr_ty);
        g.run();
    }
    let clif = ctx.func.display().to_string();
    module.define_function(func_id, &mut ctx)?;
    let code_size = ctx
        .compiled_code()
        .map(|c| c.code_info().total_size)
        .unwrap_or(0);
    Ok(Some(CompiledBody {
        func_idx: plan.func_idx,
        func_id,
        clif,
        code_size,
    }))
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

struct BodyGen<'a> {
    b: FunctionBuilder<'a>,
    plan: &'a NativePlan,
    /// Direct-call targets for `Callee::Known` sites whose callee is in this
    /// JIT round's native set. A `Known` callee absent here falls back to the
    /// `al_rt_*` trampoline.
    peers: HashMap<FuncIdx, ir::FuncRef>,
    layout: FrameLayout,
    uses: Uses,
    cmap: ConstMap,
    facts: ValueBits,
    fns: RtRefs,
    ptr_ty: ir::Type,
    /// The frame-base pointer (`&stack[frame.base_slot]`), re-fetched after
    /// every runtime call that can grow the value stack.
    base: Variable,
    out_slot: ir::StackSlot,
    words: TiVec<LocalId, Option<Variable>>,
    ints: TiVec<LocalId, Option<Variable>>,
    joins: TiVec<JoinId, Option<ir::Block>>,
    loop_head: ir::Block,
    /// Cursor into [`FrameLayout::call_resume_ips`], advanced once per
    /// non-tail call in walk order, the order emit recorded them in.
    resume_cursor: usize,
    /// Non-Bool constructor sites in walk order, paired with `Atom::Ctor`s by
    /// [`Self::next_ctor_site`]'s cursor.
    ctor_sites: Vec<EnumCtorSite>,
    ctor_cursor: usize,
}

impl<'a> BodyGen<'a> {
    /// Entry-block prologue, ending with the self-tail loop header open.
    #[allow(clippy::too_many_arguments)]
    fn prologue(
        mut b: FunctionBuilder<'a>,
        plan: &'a NativePlan,
        peers: HashMap<FuncIdx, ir::FuncRef>,
        layout: FrameLayout,
        uses: Uses,
        cmap: ConstMap,
        ctor_sites: Vec<EnumCtorSite>,
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
        b.ins().set_pinned_reg(ctx);
        let vm = b
            .ins()
            .load(ptr_ty, MemFlagsData::trusted(), ctx, NativeCtx::VM_OFFSET);

        let base = b.declare_var(ptr_ty);
        let call = b.ins().call(fns.rt_frame_base, &[vm]);
        let base0 = b.inst_results(call)[0];
        b.def_var(base, base0);

        let out_slot =
            b.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3));
        let loop_head = b.create_block();

        let mut g = BodyGen {
            b,
            plan,
            peers,
            layout,
            uses,
            cmap,
            facts: value_bits(),
            fns,
            ptr_ty,
            base,
            out_slot,
            words: TiVec::new(),
            ints: TiVec::new(),
            joins: TiVec::new(),
            loop_head,
            resume_cursor: 0,
            ctor_sites,
            ctor_cursor: 0,
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
        let head = g.loop_head;
        g.b.ins().jump(head, &[]);
        g.b.switch_to_block(head);
    }

    fn run(mut self) {
        // Split borrow: the walk takes `&mut self`, so the body pointer must
        // not run through `self`.
        let body: &CoreExpr = &self.plan.fun.body;
        self.expr(body, Dest::Ret);
        if self.resume_cursor != self.layout.call_resume_ips.len() {
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
        FrameSlots::new(&self.layout, base).load_slot(&mut self.b, slot)
    }

    /// `StoreLocal` parity: release the old word, store the new. `bits` must
    /// carry an owned reference; the slot takes it.
    fn store_slot(&mut self, slot: i32, bits: ir::Value) {
        let base = self.b.use_var(self.base);
        FrameSlots::new(&self.layout, base).store_slot(&mut self.b, slot, bits, self.fns.release);
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
            FrameSlots::new(&self.layout, base).store_slot_no_release(&mut self.b, slot, bits);
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
        FrameSlots::new(&self.layout, base).store_slot_no_release(&mut self.b, slot, zero);
    }

    fn word_of(&mut self, id: LocalId) -> ir::Value {
        match self.words.get(id).copied().flatten() {
            Some(v) => self.b.use_var(v),
            None => unsupported_node("word view"),
        }
    }

    fn int_of(&mut self, id: LocalId) -> ir::Value {
        match self.ints.get(id).copied().flatten() {
            Some(v) => self.b.use_var(v),
            None => unsupported_node("int view"),
        }
    }

    /// An owned word for a consuming use. A slotted local keeps its slot's
    /// reference and hands out a retained copy; a slotless local is a
    /// single-use temp whose one owned word transfers.
    fn owned_word(&mut self, id: LocalId) -> ir::Value {
        let w = self.word_of(id);
        if self.layout.slot(id).is_some()
            && let Some(gate) = self.plan.rc_gate(id)
        {
            emit_dup(&mut self.b, w, gate);
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

    /// Evaluate a non-call atom. `want_word` asks for an owned boxed word,
    /// `want_int` for the raw `i64` view.
    fn eval_pure(&mut self, a: &Atom, want_word: bool, want_int: bool) -> AtomVal {
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
                // The gate proved a wide-enough Tuple, so the word is a heap
                // cell `[count][elements…]` and element `i` is payload word
                // `1 + i`. The interpreter's bounds/type errors are
                // unreachable under that proof.
                let w = self.word_of(*recv);
                let obj = self.b.ins().band_imm(w, NATIVE_PTR_MASK as i64);
                let f = self.load_payload_word(obj, 1 + *i as usize);
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
                imm: Imm::Argc(_),
            } => {
                if want_int {
                    unsupported_node("int view of a sequence");
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
                let f = self.load_payload_word(obj, 6 + *i as usize);
                self.b.ins().jump(merge, &[f.into()]);
                self.b.seal_block(merge);
                self.b.switch_to_block(merge);
                let f = self.b.block_params(merge)[0];
                self.field_result(f, want_word, want_int)
            }
            Atom::PrimOp { op, args, .. } => {
                let Some((nop, _)) = nop_of(*op) else {
                    unsupported_node("primop")
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
        match self.eval_pure(a, true, false).word {
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

    fn next_resume(&mut self) -> i32 {
        let Some(&ip) = self.layout.call_resume_ips.get(self.resume_cursor) else {
            resume_walk_mismatch()
        };
        self.resume_cursor += 1;
        ip
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
    fn unwind_unless_done(&mut self, status: ir::Value) {
        let cont = self.b.create_block();
        let bail = self.b.create_block();
        self.b.set_cold_block(bail);
        let ok = self
            .b
            .ins()
            .icmp_imm(IntCC::Equal, status, NativeStatus::Done as u64 as i64);
        self.b.ins().brif(ok, cont, &[], bail, &[]);
        self.b.seal_block(cont);
        self.b.seal_block(bail);
        self.b.switch_to_block(bail);
        self.b.ins().return_(&[status]);
        self.b.switch_to_block(cont);
    }

    /// A non-tail call, returning the callee's owned result word. A known
    /// native callee splits the `al_rt_call` sequence around a direct machine
    /// call to the peer entry; frame state is bit-identical at every seam,
    /// only the table lookup and indirect dispatch are elided. Anything else
    /// keeps the trampoline.
    ///
    /// `moved` are [`peel_call_arg_drops`]'s peeled last-use drops. They must
    /// run after the operand copies but *before* the call, so the callee is
    /// sole owner and its Perceus `Drop` sees rc==1.
    fn call_value(
        &mut self,
        callee: Callee,
        args: &[LocalId],
        moved: &[(LocalId, bool)],
    ) -> ir::Value {
        let resume = self.next_resume();
        let buf = self.arg_buffer(args);
        // Emit's operand order: args, then a dynamic callee, then the peeled
        // drops. The dispatch word must be resolved before the drops. `Self_`
        // is an immediate like `Known`, since covered bodies never read
        // captures. A dynamic callee is an owned closure word that becomes
        // the callee frame's `captures` handle.
        let dispatch = match callee {
            Callee::Known(f) => {
                let t = self.b.ins().iconst(types::I64, i64::from(f.to_operand()));
                Ok((f, t))
            }
            Callee::Self_ => {
                let fi = self.plan.func_idx;
                let t = self.b.ins().iconst(types::I64, i64::from(fi.to_operand()));
                Ok((fi, t))
            }
            Callee::Local(x) => Err(self.owned_word(x)),
        };
        for &(m, reusable) in moved {
            self.drop_local(m, reusable);
        }
        let outp = self.b.ins().stack_addr(self.ptr_ty, self.out_slot, 0);
        let r = self.b.ins().iconst(types::I64, i64::from(resume));
        let n = self.b.ins().iconst(types::I64, args.len() as i64);
        let status = match dispatch {
            Ok((fi, t)) => match self.peers.get(&fi).copied() {
                // Direct native→native. `al_rt_push_frame` is `al_rt_call`
                // up to but not including `drive_top_frame`, leaving the
                // callee frame bit-identical to the interpreter's at ip 0, so
                // a `Yield` here resumes the callee from the top.
                // `al_rt_direct_result` is the trampoline's Done tail and
                // drives a collapsed `TailCall` chain.
                Some(entry) => {
                    let vmx = self.vmx();
                    let push = self
                        .b
                        .ins()
                        .call(self.fns.rt_push_frame, &[vmx, t, r, buf, n]);
                    let s = self.b.inst_results(push)[0];
                    self.unwind_unless_done(s);
                    // Peer entries are NativeEntry, not shims: they take the
                    // ctx and re-pin and re-load their own vm.
                    let ctx = self.ctx();
                    let direct = self.b.ins().call(entry, &[ctx]);
                    let s = self.b.inst_results(direct)[0];
                    let vmx = self.vmx();
                    let after = self
                        .b
                        .ins()
                        .call(self.fns.rt_direct_result, &[vmx, s, outp]);
                    self.b.inst_results(after)[0]
                }
                // Interpreter-only callee: the trampoline stays.
                None => {
                    let vmx = self.vmx();
                    let call = self
                        .b
                        .ins()
                        .call(self.fns.rt_call, &[vmx, t, r, buf, n, outp]);
                    self.b.inst_results(call)[0]
                }
            },
            Err(cw) => {
                let vmx = self.vmx();
                let call = self
                    .b
                    .ins()
                    .call(self.fns.rt_call_value, &[vmx, cw, r, buf, n, outp]);
                self.b.inst_results(call)[0]
            }
        };
        self.unwind_unless_done(status);
        // The callee chain may have grown (and reallocated) the value stack.
        let vmx = self.vmx();
        let refetch = self.b.ins().call(self.fns.rt_frame_base, &[vmx]);
        let nb = self.b.inst_results(refetch)[0];
        self.b.def_var(self.base, nb);
        self.b.ins().stack_load(types::I64, self.out_slot, 0)
    }

    /// A cross-function tail call.
    ///
    /// `al_rt_tail_call` collapses the frame in place either way. Who
    /// dispatches it differs: a native callee under a calling convention that
    /// admits Cranelift `return_call` gets a direct `return_call`, keeping
    /// mutual native tail chains in machine code. Cranelift permits
    /// `return_call` only under `CallConv::Tail`, and `NativeEntry` is
    /// extern-C, so today `supports_tail_calls()` is false and only the
    /// fallback runs.
    fn tail_call_known(&mut self, target: FuncIdx, args: &[LocalId]) {
        let buf = self.arg_buffer(args);
        let t = self
            .b
            .ins()
            .iconst(types::I64, i64::from(target.to_operand()));
        let n = self.b.ins().iconst(types::I64, args.len() as i64);
        let vmx = self.vmx();
        let call = self.b.ins().call(self.fns.rt_tail_call, &[vmx, t, buf, n]);
        let status = self.b.inst_results(call)[0];
        if self.b.func.signature.call_conv.supports_tail_calls()
            && let Some(&peer) = self.peers.get(&target)
        {
            let go = self.b.create_block();
            let bail = self.b.create_block();
            self.b.set_cold_block(bail);
            let ready =
                self.b
                    .ins()
                    .icmp_imm(IntCC::Equal, status, NativeStatus::TailCall as u64 as i64);
            self.b.ins().brif(ready, go, &[], bail, &[]);
            self.b.seal_block(bail);
            self.b.seal_block(go);
            self.b.switch_to_block(bail);
            self.b.ins().return_(&[status]);
            self.b.switch_to_block(go);
            let ctx = self.ctx();
            self.b.ins().return_call(peer, &[ctx]);
            return;
        }
        self.b.ins().return_(&[status]);
    }

    /// [`Self::tail_call_known`] with an owned closure word instead of an
    /// immediate target. The collapsed frame takes it as its `captures`
    /// handle.
    fn tail_call_value(&mut self, callee: LocalId, args: &[LocalId]) {
        let buf = self.arg_buffer(args);
        let cw = self.owned_word(callee);
        let n = self.b.ins().iconst(types::I64, args.len() as i64);
        let vmx = self.vmx();
        let call = self
            .b
            .ins()
            .call(self.fns.rt_tail_call_value, &[vmx, cw, buf, n]);
        let status = self.b.inst_results(call)[0];
        self.b.ins().return_(&[status]);
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
        // loop-carried value is scheduler-clean by construction: if a
        // checkpoint ever parks in place, a stale `base` would resume
        // pointing into the parking scheduler's Vec.
        let vmx = self.vmx();
        let refetch = self.b.ins().call(self.fns.rt_frame_base, &[vmx]);
        let nb = self.b.inst_results(refetch)[0];
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
                        let v = self.eval_pure(rhs, want_word, want_int);
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
                            self.drop_local(x, reusable);
                        }
                        self.self_tail(args, &moved);
                        return;
                    }
                    let CoreExpr::Drop { local, shape, body } = e else {
                        unsupported_node("drop shape")
                    };
                    self.drop_local(*local, shape.is_some());
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
                self.b.ins().call(self.fns.rt_ret, &[vmx, w]);
                let done = self
                    .b
                    .ins()
                    .iconst(types::I64, NativeStatus::Done as u64 as i64);
                self.b.ins().return_(&[done]);
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
            let f = self.load_payload_word(obj, 6 + i);
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
                    let flag = if boolean.is_some() {
                        self.b.ins().icmp_imm(IntCC::Equal, sw, bits as i64)
                    } else if let Some(k) = int {
                        // Compare decoded values, so a spilled BigInt
                        // scrutinee still matches its literal.
                        let si = self.int_of(scrut);
                        self.b.ins().icmp_imm(IntCC::Equal, si, k)
                    } else {
                        unsupported_node("literal arm kind")
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
                        let packed = (variant.type_id.0 as u32 as u64)
                            | ((variant.variant_idx as u64) << 32);
                        let hit = self.b.ins().icmp_imm(IntCC::Equal, w0, packed as i64);
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
         recorded resume ips. Please report this as a compiler bug."
    )
}

/// [`resume_walk_mismatch`] for constructor sites.
#[allow(clippy::panic)]
#[cold]
#[inline(never)]
fn ctor_walk_mismatch() -> ! {
    panic!(
        "internal compiler error: native backend constructor-site walk diverged from \
         the emitted construct headers. Please report this as a compiler bug."
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

    use cranelift_codegen::settings::{self, Configurable};
    use cranelift_jit::{JITBuilder, JITModule};
    use cranelift_module::default_libcall_names;

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
        PreludeBindings {
            bool: TypeRef {
                id: BOOL_TID,
                name: "Bool",
            },
            binary: TypeRef {
                id: BIN_TID,
                name: "Binary",
            },
            true_: CtorRef {
                type_id: BOOL_TID,
                variant_idx: 0,
                arity: 0,
            },
            false_: CtorRef {
                type_id: BOOL_TID,
                variant_idx: 1,
                arity: 0,
            },
            ..PreludeBindings::default()
        }
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

        /// `drive_top_frame`'s mock, consuming `TailCall` by re-dispatching.
        fn drive(&mut self) -> u64 {
            loop {
                let func = self.frames.last().unwrap().func;
                let entry = self.entries[func].expect("mock runtime only drives native fns");
                let status = Self::call_entry(self, entry) as u64;
                if status != NativeStatus::TailCall as u64 {
                    return status;
                }
            }
        }

        /// Mirrors `VM::call_native`, including the pinned-register bracket.
        /// `enable_pinned_reg` drops that register from Cranelift's
        /// callee-save set, so a compiled entry clobbers it while this
        /// caller's ABI says it survives. `#[inline(never)]` is a barrier
        /// only; it is not what makes the register safe.
        #[inline(never)]
        fn call_entry(vm: &mut TestVm, entry: NativeEntry) -> NativeStatus {
            vm.ctx.vm = (vm as *mut TestVm).cast();
            // SAFETY: a finalized entry from this harness's module, and this
            // TestVm's live ctx — the two arguments the shim forwards.
            unsafe {
                crate::bytecode::native::call_entry_preserving_pinned(
                    (&raw mut vm.ctx).cast(),
                    entry,
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

    unsafe extern "C" fn t_rt_call(
        vmx: *mut core::ffi::c_void,
        target: i64,
        resume: i64,
        args: *const u64,
        argc: i64,
        out: *mut u64,
    ) -> u64 {
        let vm = vm_of(vmx);
        vm.frames.last_mut().unwrap().ip = resume as i32;
        for i in 0..argc as usize {
            // SAFETY: owned words per the shim contract.
            vm.stack
                .push(unsafe { Value::from_bits(args.add(i).read()) });
        }
        vm.push_frame(target as usize, argc as usize);
        vm.reds -= 1;
        if vm.reds <= 0 {
            return NativeStatus::Yield as u64;
        }
        let status = vm.drive();
        if status != NativeStatus::Done as u64 {
            return status;
        }
        let result = vm.stack.pop().unwrap();
        // SAFETY: `out` is the caller's one-word result slot.
        unsafe { out.write(ManuallyDrop::new(result).to_bits()) };
        NativeStatus::Done as u64
    }

    unsafe extern "C" fn t_rt_push_frame(
        vmx: *mut core::ffi::c_void,
        target: i64,
        resume: i64,
        args: *const u64,
        argc: i64,
    ) -> u64 {
        let vm = vm_of(vmx);
        vm.frames.last_mut().unwrap().ip = resume as i32;
        for i in 0..argc as usize {
            // SAFETY: owned words per the shim contract.
            vm.stack
                .push(unsafe { Value::from_bits(args.add(i).read()) });
        }
        vm.push_frame(target as usize, argc as usize);
        vm.reds -= 1;
        if vm.reds <= 0 {
            return NativeStatus::Yield as u64;
        }
        NativeStatus::Done as u64
    }

    unsafe extern "C" fn t_rt_direct_result(
        vmx: *mut core::ffi::c_void,
        status: u64,
        out: *mut u64,
    ) -> u64 {
        let vm = vm_of(vmx);
        let status = if status == NativeStatus::TailCall as u64 {
            vm.drive()
        } else {
            status
        };
        if status != NativeStatus::Done as u64 {
            return status;
        }
        let result = vm.stack.pop().unwrap();
        // SAFETY: `out` is the caller's one-word result slot.
        unsafe { out.write(ManuallyDrop::new(result).to_bits()) };
        NativeStatus::Done as u64
    }

    unsafe extern "C" fn t_rt_tail_call(
        vmx: *mut core::ffi::c_void,
        target: i64,
        args: *const u64,
        argc: i64,
    ) -> u64 {
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
        vm.reds -= 1;
        if vm.reds <= 0 {
            return NativeStatus::Yield as u64;
        }
        NativeStatus::TailCall as u64
    }

    unsafe extern "C" fn t_rt_call_value(
        vmx: *mut core::ffi::c_void,
        callee: u64,
        resume: i64,
        args: *const u64,
        argc: i64,
        out: *mut u64,
    ) -> u64 {
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
        vm.reds -= 1;
        if vm.reds <= 0 {
            return NativeStatus::Yield as u64;
        }
        let status = vm.drive();
        if status != NativeStatus::Done as u64 {
            return status;
        }
        let result = vm.stack.pop().unwrap();
        // SAFETY: `out` is the caller's one-word result slot.
        unsafe { out.write(ManuallyDrop::new(result).to_bits()) };
        NativeStatus::Done as u64
    }

    unsafe extern "C" fn t_rt_tail_call_value(
        vmx: *mut core::ffi::c_void,
        callee: u64,
        args: *const u64,
        argc: i64,
    ) -> u64 {
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
        vm.reds -= 1;
        if vm.reds <= 0 {
            return NativeStatus::Yield as u64;
        }
        NativeStatus::TailCall as u64
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

    unsafe extern "C" fn t_rt_ret(vmx: *mut core::ffi::c_void, result: u64) {
        let vm = vm_of(vmx);
        let frame = vm.frames.pop().unwrap();
        vm.stack.truncate(frame.base);
        // SAFETY: `result` is an owned value word per the shim contract.
        vm.stack.push(unsafe { Value::from_bits(result) });
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
                root = seq::push_back(&mut vm.heap, &root, e.clone());
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
                root = seq::push_front(&mut vm.heap, &root, e.clone());
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
        entries: Vec<Option<NativeEntry>>,
        metas: Vec<FnMeta>,
        clifs: Vec<String>,
    }

    /// Plan + compile every function against the mock runtime symbols and
    /// finalize. Panics if a function the test expects to cover is rejected.
    fn jit(fns: &[CoreFn], pool: &ResolvedPool, consts: &[Value]) -> Jit {
        // Bodies without enum ctors never consult the program's bytecode,
        // so a constants-only program suffices.
        let program = crate::bytecode::Program {
            constants: consts.to_vec(),
            ..Default::default()
        };
        jit_with(fns, pool, program)
    }

    /// [`jit`] against a full test `Program`, which the enum-ctor tests need:
    /// `compile` reads header constants back from the emitted bytecode.
    fn jit_with(fns: &[CoreFn], pool: &ResolvedPool, program: crate::bytecode::Program) -> Jit {
        let mut flags = settings::builder();
        flags.set("use_colocated_libcalls", "false").unwrap();
        flags.set("enable_pinned_reg", "true").unwrap();
        flags.set("is_pic", "false").unwrap();
        flags.set("opt_level", "speed").unwrap();
        let isa = cranelift_native::builder()
            .unwrap()
            .finish(settings::Flags::new(flags))
            .unwrap();
        let mut jb = JITBuilder::with_isa(isa, default_libcall_names());
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
        jb.symbol(SYM_RT_CALL, t_rt_call as *const u8);
        jb.symbol(SYM_RT_TAIL_CALL, t_rt_tail_call as *const u8);
        jb.symbol(SYM_RT_PUSH_FRAME, t_rt_push_frame as *const u8);
        jb.symbol(SYM_RT_DIRECT_RESULT, t_rt_direct_result as *const u8);
        jb.symbol(SYM_RT_CALL_VALUE, t_rt_call_value as *const u8);
        jb.symbol(SYM_RT_TAIL_CALL_VALUE, t_rt_tail_call_value as *const u8);
        jb.symbol(SYM_RT_MAKE_CLOSURE, t_make_closure as *const u8);
        jb.symbol(SYM_RT_CHECKPOINT, t_rt_checkpoint as *const u8);
        jb.symbol(SYM_RT_FRAME_BASE, t_frame_base as *const u8);
        jb.symbol(SYM_RT_RET, t_rt_ret as *const u8);
        let mut module = JITModule::new(jb);

        let mut ids = Vec::new();
        let mut metas = Vec::new();
        let mut clifs = Vec::new();
        let prelude = test_prelude();
        let native: HashSet<FuncIdx> = (0..fns.len()).map(FuncIdx::from_usize).collect();
        for (i, f) in fns.iter().enumerate() {
            let p =
                plan(FuncIdx::from_usize(i), f, pool, &prelude).expect("test fn must be covered");
            let body = compile(&mut module, &p, &native, &program)
                .expect("module error")
                .expect("test fn must compile");
            assert!(!body.clif.is_empty());
            clifs.push(body.clif);
            let layout = emit::emit(
                f,
                &mut LayoutCtx {
                    bools: p.bools,
                    switch_counts: &p.switch_counts,
                },
            )
            .layout;
            metas.push(FnMeta {
                arity: f.params.len(),
                locals: layout.locals.max(f.params.len() as i32) as usize,
            });
            ids.push(body.func_id);
        }
        module.finalize_definitions().unwrap();
        let entries = ids
            .iter()
            .map(|&id| {
                let code = module.get_finalized_function(id);
                // SAFETY: defined with the NativeEntry signature above.
                Some(unsafe { std::mem::transmute::<*const u8, NativeEntry>(code) })
            })
            .collect();
        Jit {
            _module: module,
            _program: program,
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
        fn switch_variant_count(&self, _tid: TypeId) -> Option<u8> {
            None
        }
        fn bool_variant(&self, tid: TypeId, variant_idx: u16) -> Option<bool> {
            self.bools.bool_variant(tid, variant_idx)
        }
    }

    /// Emit `fns` for real into a test `Program`. The frozen area is shared
    /// with the program, so `compile`'s label tuples outlive the baked code.
    fn ctor_program(fns: &[CoreFn], base_consts: &[Value]) -> crate::bytecode::Program {
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
        for (i, f) in fns.iter().enumerate() {
            let start = program.code.len() as i32;
            let out = emit::emit(f, &mut ctx);
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
        program
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
        assert!(plan(FuncIdx(0), &fib_fn(int), &pool, &pre).is_some());
        assert!(plan(FuncIdx(0), &count_fn(int), &pool, &pre).is_some());

        // A non-Bool constructor allocates through the enum-ctor path:
        // covered.
        let f = testkit::func(vec![], CoreExpr::Tail(testkit::ctor(&[])), int);
        assert!(plan(FuncIdx(0), &f, &pool, &pre).is_some());

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
        assert!(plan(FuncIdx(0), &f, &pool, &pre).is_some());

        // A closure allocates through the MakeClosure shim: covered.
        let f = testkit::func(
            vec![],
            CoreExpr::Tail(Atom::Closure {
                func_idx: FuncIdx(1),
                captures: vec![],
            }),
            int,
        );
        assert!(plan(FuncIdx(0), &f, &pool, &pre).is_some());

        // A dynamic call through a closure-valued local: covered.
        let f = testkit::func(
            vec![testkit::bind(0, int)],
            CoreExpr::Tail(Atom::Call {
                callee: Callee::Local(local(0)),
                args: vec![],
            }),
            int,
        );
        assert!(plan(FuncIdx(0), &f, &pool, &pre).is_some());

        // A capture *read* stays uncovered: the body needs the frame's
        // closure handle, which only the interpreter maintains.
        let f = testkit::func(
            vec![],
            CoreExpr::Tail(Atom::prim(Op::PushCapture, vec![])),
            int,
        );
        assert!(plan(FuncIdx(0), &f, &pool, &pre).is_none());

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
        assert!(plan(FuncIdx(0), &poly_eq(bound), &pool2, &pre).is_none());
        assert!(plan(FuncIdx(0), &poly_eq(intt), &pool2, &pre).is_some());
    }

    #[test]
    fn compile_rejects_heap_constants() {
        let (pool, int) = int_pool();
        let f = testkit::func(vec![], CoreExpr::Tail(Atom::Const(ConstId(0))), int);
        let p = plan(FuncIdx(0), &f, &pool, &test_prelude()).unwrap();

        let mut flags = settings::builder();
        flags.set("use_colocated_libcalls", "false").unwrap();
        flags.set("enable_pinned_reg", "true").unwrap();
        flags.set("is_pic", "false").unwrap();
        let isa = cranelift_native::builder()
            .unwrap()
            .finish(settings::Flags::new(flags))
            .unwrap();
        let mut module = JITModule::new(JITBuilder::with_isa(isa, default_libcall_names()));

        let mut heap = ProcHeap::new();
        let heap_const = Value::str_in(&mut heap, "not an immediate");
        let heap_program = crate::bytecode::Program {
            constants: vec![heap_const],
            ..Default::default()
        };
        let native = HashSet::from([FuncIdx(0)]);
        let rejected = compile(&mut module, &p, &native, &heap_program).unwrap();
        assert!(rejected.is_none(), "heap constants are not embeddable");

        // The same body over an immediate constant compiles.
        let ok_program = crate::bytecode::Program {
            constants: vec![Value::small_int(7)],
            ..Default::default()
        };
        let ok = compile(&mut module, &p, &native, &ok_program).unwrap();
        assert!(ok.is_some());
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
        let nil = pool.mk_con(pre.nil.id, StrId(0), &[]);
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
        let bin = pool.mk_con(pre.binary.id, StrId(0), &[]);
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
    fn known_native_callee_is_a_direct_call_not_the_trampoline() {
        let (pool, int) = int_pool();
        let j = jit(&[fib_fn(int)], &pool, &test_consts());
        let clif = &j.clifs[0];
        assert!(
            clif.contains("colocated"),
            "known-native call should reference a same-module (colocated) peer FuncRef; \
             CLIF was:\n{clif}"
        );
        let (v, _) = run(&j, 0, &[Value::small_int(15)], i64::MAX / 2);
        assert_eq!(v.as_int(), Some(610));

        // Negative: with an empty native set the peer is absent and the same
        // body routes both call sites through `al_rt_call`.
        let mut flags = settings::builder();
        flags.set("use_colocated_libcalls", "false").unwrap();
        flags.set("enable_pinned_reg", "true").unwrap();
        flags.set("is_pic", "false").unwrap();
        let isa = cranelift_native::builder()
            .unwrap()
            .finish(settings::Flags::new(flags))
            .unwrap();
        let mut module = JITModule::new(JITBuilder::with_isa(isa, default_libcall_names()));
        let p = plan(FuncIdx(0), &fib_fn(int), &pool, &test_prelude()).unwrap();
        let program = crate::bytecode::Program {
            constants: test_consts(),
            ..Default::default()
        };
        let body = compile(&mut module, &p, &HashSet::new(), &program)
            .unwrap()
            .unwrap();
        assert!(
            !body.clif.contains("colocated"),
            "with no native peers every known call goes through the trampoline; \
             CLIF was:\n{}",
            body.clif
        );
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
        let p = plan(FuncIdx(0), &f, &pool, &test_prelude()).unwrap();
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
        let p = plan(FuncIdx(0), &f, &pool, &test_prelude()).unwrap();
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
        assert!(plan(FuncIdx(0), &f, &pool, &test_prelude()).is_none());
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
        let program = ctor_program(std::slice::from_ref(&f), &test_consts());
        let j = jit_with(&[f], &pool, program);
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
        let program = ctor_program(std::slice::from_ref(&f), &test_consts());
        let j = jit_with(&[f], &pool, program);

        // Drive by hand: the parked cell must sit in the slot at rc == 1,
        // which `run`'s argument clone would break.
        let mut vm = TestVm {
            ctx: NativeCtx::new(),
            stack: Vec::new(),
            frames: Vec::new(),
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
        let program = ctor_program(std::slice::from_ref(&f), &test_consts());
        let j = jit_with(&[f], &pool, program);
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
        let program = ctor_program(std::slice::from_ref(&f), &test_consts());
        let j = jit_with(&[f], &pool, program);
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
        let program = ctor_program(std::slice::from_ref(&f), &test_consts());
        let j = jit_with(&[f], &pool, program);
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
        let program = ctor_program(std::slice::from_ref(&f), &test_consts());
        let j = jit_with(&[f], &pool, program);

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

    #[test]
    fn layout_walk_consumes_every_resume_ip() {
        // The cursor invariant, checked structurally: emit records one
        // resume ip per non-tail call, and fib's body contains exactly two.
        let (_, int) = int_pool();
        let bools = BoolCtors::of(&test_prelude());
        let switch_counts = HashMap::new();
        let layout = emit::emit(
            &fib_fn(int),
            &mut LayoutCtx {
                bools,
                switch_counts: &switch_counts,
            },
        )
        .layout;
        assert_eq!(layout.call_resume_ips.len(), 2);
    }
}
