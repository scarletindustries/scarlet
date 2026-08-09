//! Core → bytecode.
//!
//! [`emit`] walks a [`CoreFn`]'s ANF spine into a flat `Vec<Instruction>` plus
//! the frame's slot count. Type erasure happens here and only here.
//!
//! Jump targets are **function-relative**: every jump operand (and the
//! `SwitchTag` table base) indexes the returned `code` vector, so emit never
//! spells an absolute `program.code` address.
//!
//! Relocation keys off the *frame*, not the splice. The VM resolves a jump as
//! `Function.code_start + operand`, so a block needs [`relocate`] exactly when
//! its frame's `code_start` differs from where the block is spliced. A
//! function body never does; the module toplevel always does, since it runs in
//! the entry frame whose `code_start` is 0 while the block starts at `base`.

use super::{
    Atom, Callee, CodeAddr, CoreBind, CoreExpr, CoreFn, CorePat, Imm, JoinId, LocalId, VariantRef,
};
use crate::bytecode::{Instruction, Op, enum_name_prefix_hash, op, op_ab, op_arg};
use crate::tivec::TiVec;
use crate::type_def::TypeId;
use crate::types::StrId;

/// String-interner resolution and constant-pool interning, so a `Ctor`'s
/// header constants are pooled once alongside every other program constant.
pub trait EmitCtx {
    /// Resolve an engine-interned string id.
    fn resolve_str(&self, id: StrId) -> &str;
    /// Pool a frozen `Int` constant, returning its `PushConst` operand.
    fn intern_int(&mut self, i: i64) -> i32;
    /// Pool a frozen string constant.
    fn intern_str(&mut self, s: &str) -> i32;
    /// Pool the frozen field-label array for `(tid, variant_idx)`.
    fn intern_labels(&mut self, tid: TypeId, variant_idx: u16) -> i32;
    /// Variant count of `tid` when it is a boxed enum eligible for
    /// `SwitchTag`. `None` forces the `MatchEnum` ladder.
    fn switch_variant_count(&self, tid: TypeId) -> Option<u8>;
    /// `Some(b)` for prelude `Bool.True`/`Bool.False`, `None` otherwise.
    /// `Bool` is unboxed in the VM, so its constructors emit
    /// `PushTrue`/`PushFalse` and its patterns compare with `Eq`.
    fn bool_variant(&self, tid: TypeId, variant_idx: u16) -> Option<bool>;
}

/// Result of [`emit`].
pub struct EmitOut {
    pub code: Vec<Instruction>,
    /// High-water stack-slot count for the frame — becomes `Function.locals`.
    pub locals: i32,
    /// Frame-layout facts this emission fixed. See [`FrameLayout`].
    pub layout: FrameLayout,
}

/// The frame-layout facts one [`emit`] run fixed, for the native (Cranelift)
/// backend.
///
/// Compiled code must reproduce the interpreter's frame layout bit for bit:
/// at every call site and reduction checkpoint the `(stack, frames)` pair must
/// match what the interpreter would hold at that bytecode ip. That is what
/// keeps suspension, migration, per-function fallback and the
/// interpreter-as-oracle working. Both halves are decided during emission, so
/// they are recorded here rather than re-derived — a second slot allocator or
/// ip computation would drift:
///
/// - which stack slot each `LocalId` occupies, including the locals emit
///   elides (they own none) and the slots shared across branch arms;
/// - the resume ip of every non-tail call, i.e. the post-increment ip the
///   interpreter's `enter_frame!` stores, so a suspended callee can resume
///   through the interpreter with the caller finishing interpreted.
///
/// Offsets are function-relative, like every jump operand, which is also
/// `frame.ip`'s coordinate space. The peephole pass rewrites fused windows in
/// place and never fuses across a call, so these ips survive it.
#[derive(Debug, Clone, Default)]
pub struct FrameLayout {
    slots: TiVec<LocalId, Option<i32>>,
    /// High-water slot count of this body alone. The final `Function.locals`
    /// maxes this with the reserved param count.
    pub locals: i32,
    /// Function-relative resume ip per non-tail call, in emission order —
    /// which is the order a straight walk of the same Core meets the call
    /// atoms, since calls are never hoisted (see [`is_hoistable`]).
    pub call_resume_ips: Vec<i32>,
}

impl FrameLayout {
    /// The frame-relative slot `id` occupies — the value word lives at
    /// `stack[frame.base_slot + slot]` — or `None` when emit elided the local
    /// and it never touches the frame.
    pub fn slot(&self, id: LocalId) -> Option<i32> {
        self.slots.get(id).copied().flatten()
    }
}

/// Shift every jump operand by `base`, turning [`emit`]'s function-relative
/// addresses into absolute `program.code` ones. Only for a block whose frame's
/// `code_start` is not the block's own start address — today that is the
/// module toplevel alone.
///
/// [`Op::has_jump_target`] is the one authority on which operands are
/// addresses, and it is an exhaustive match, so a new jump op cannot skip
/// classification.
pub fn relocate(code: &mut [Instruction], base: i32) {
    if base == 0 {
        return;
    }
    for ins in code {
        if ins.op.has_jump_target() {
            ins.operand += base;
        }
    }
}

/// Lower one Core function to bytecode. Jump operands index the returned
/// `code` vector; see [`relocate`].
pub fn emit<C: EmitCtx>(f: &CoreFn, ctx: &mut C) -> EmitOut {
    let mut e = Emitter::new(ctx);
    e.scan = Scan::of(&f.body);
    e.alias_consts = true;
    e.register_const_aliases(&f.body);
    for p in &f.params {
        e.bind(p);
    }
    e.emit_expr(&f.body);
    e.finish()
}

/// Lower a bare expression (module toplevel). `slot_base` is the first
/// entry-frame slot this file may allocate.
///
/// The entry frame *is* the global table. Fn bodies already emitted reference
/// the top-level decls via `PushGlobal`, so the entry-frame init must
/// `StoreLocal` to the same indices; the pinnings are read back off the body
/// being emitted ([`CoreExpr::toplevel_globals`]) so a disagreement is
/// unrepresentable. Every other bind allocates linearly past that range.
pub fn emit_toplevel<C: EmitCtx>(body: &CoreExpr, slot_base: i32, ctx: &mut C) -> EmitOut {
    let preassigned = body.toplevel_globals();
    let mut e = Emitter::new(ctx);
    e.scan = Scan::of(body);
    // The entry frame's `StoreLocal`s are the global-table init, so no
    // toplevel bind may be elided into a `PushConst` alias.
    e.alias_consts = false;
    // Spill temps start past the pinned decl range, so a linear `next_slot++`
    // never collides with a pinned slot.
    let spill = preassigned
        .iter()
        .map(|&(_, s)| s.0 + 1)
        .fold(slot_base, i32::max);
    e.next_slot = spill;
    e.max_locals = spill;
    for &(id, slot) in &preassigned {
        e.preassigned.resize_at_least(id, None);
        e.preassigned[id] = Some(slot.0);
    }
    // Not `emit_expr`: a `TailCall`/`Ret` would collapse the entry frame and
    // clobber every `PushGlobal` a callee performs. The caller's trailing
    // `Halt` is the real exit.
    e.emit_expr_in(body, false);
    e.finish()
}

/// An unresolved jump is named by the [`CodeAddr`] of the instruction itself,
/// and [`Emitter::patch`] resolves it to the offset emission has reached. A
/// label *is* a code address, which is why no jump needs the block's eventual
/// absolute address.
struct Emitter<'a, C: EmitCtx> {
    code: TiVec<CodeAddr, Instruction>,
    /// `LocalId → stack slot`. `None` or out of range means the id was never
    /// bound, which is a compiler bug ([`unbound_local`]).
    slots: TiVec<LocalId, Option<i32>>,
    /// `LocalId → pinned slot` for module-toplevel decl binds. Empty for
    /// ordinary function bodies.
    preassigned: TiVec<LocalId, Option<i32>>,
    /// `LocalId → constant-pool index` for elided `let x = Const(c)` binds:
    /// every `PushLocal x` becomes `PushConst c`. This is what recovers the
    /// `PushLocal s; PushConst k; op` shape from ANF; without it no `*IntLC`
    /// superinstruction can form.
    const_of: TiVec<LocalId, Option<i32>>,
    /// Whole-body facts gathered before emission.
    scan: Scan,
    /// Per-join pending `Goto` jumps, patched together when the join's cont
    /// block is emitted. `Some` only while the declaring `LetCont`'s body is
    /// being emitted; a `Goto` finding `None` is a scoping bug. Layout makes
    /// every `Goto` a forward jump.
    joins: TiVec<JoinId, Option<Vec<CodeAddr>>>,
    /// Off for the module toplevel, whose `StoreLocal`s are the global-table
    /// init and must all be emitted.
    alias_consts: bool,
    call_resume_ips: Vec<i32>,
    next_slot: i32,
    max_locals: i32,
    ctx: &'a mut C,
}

/// A `LocalId` reached emission unbound. Aborts in release too: any slot
/// returned instead compiles to a `PushLocal 0` that silently reads or
/// clobbers parameter 0.
#[allow(clippy::panic)]
#[cold]
#[inline(never)]
fn unbound_local(id: LocalId) -> ! {
    panic!(
        "internal compiler error: emit reached unbound local {id}. \
         Please report this as a compiler bug."
    )
}

/// A `Goto` named a join no enclosing `LetCont` is collecting: never declared,
/// or already sealed. Both are lowering bugs, and any jump emitted instead
/// would land at a stale offset and execute unrelated code.
#[allow(clippy::panic)]
#[cold]
#[inline(never)]
fn unbound_join(id: JoinId) -> ! {
    panic!(
        "internal compiler error: emit reached goto to undeclared join {id}. \
         Please report this as a compiler bug."
    )
}

/// A `LetCont` redeclared a `JoinId` an enclosing one is still collecting.
/// Overwriting the pending vec would send the outer `Goto`s to a stale offset.
#[allow(clippy::panic)]
#[cold]
#[inline(never)]
fn redeclared_join(id: JoinId) -> ! {
    panic!(
        "internal compiler error: emit declared join {id} twice on one path. \
         Please report this as a compiler bug."
    )
}

/// Flatten a typed [`Imm`] to the raw `i32` operand. `Op::IndexOr` is the only
/// op whose operand encodes a constant default (or `-1` for "default was
/// pushed"); any other pairing aborts rather than emit an operand the VM would
/// misread.
#[allow(clippy::panic)]
fn imm_operand(op: Op, imm: Imm) -> i32 {
    match (op, imm) {
        (Op::IndexOr, Imm::Const(c)) => c.0 as i32,
        (Op::IndexOr, Imm::PushedDefault) => -1,
        (Op::IndexOr, Imm::None | Imm::Index(_) | Imm::Argc(_))
        | (_, Imm::Const(_) | Imm::PushedDefault) => panic!(
            "internal compiler error: {op:?} cannot carry immediate {imm:?}. \
             Please report this as a compiler bug."
        ),
        (_, Imm::None) => 0,
        (_, Imm::Index(i)) => i32::from(i),
        (_, Imm::Argc(n)) => n as i32,
    }
}

impl<'a, C: EmitCtx> Emitter<'a, C> {
    fn new(ctx: &'a mut C) -> Self {
        Self {
            code: TiVec::new(),
            slots: TiVec::new(),
            preassigned: TiVec::new(),
            const_of: TiVec::new(),
            joins: TiVec::new(),
            scan: Scan::default(),
            alias_consts: false,
            call_resume_ips: Vec::new(),
            next_slot: 0,
            max_locals: 0,
            ctx,
        }
    }

    fn finish(self) -> EmitOut {
        EmitOut {
            code: self.code.into_vec(),
            locals: self.max_locals,
            layout: FrameLayout {
                slots: self.slots,
                locals: self.max_locals,
                call_resume_ips: self.call_resume_ips,
            },
        }
    }

    /// The function-relative address the next instruction will occupy.
    #[inline]
    fn here(&self) -> CodeAddr {
        self.code.next_idx()
    }

    #[inline]
    fn push(&mut self, ins: Instruction) {
        self.code.push(ins);
    }

    /// Push a jump instruction and return the [`CodeAddr`] naming it. The ONE
    /// place a label is minted, so every jump the emitter writes is classified
    /// by [`Op::has_jump_target`] — the same predicate [`relocate`] and the
    /// peephole pass consult.
    fn mint_label(&mut self, ins: Instruction) -> CodeAddr {
        debug_assert!(ins.op.has_jump_target());
        self.code.push(ins)
    }

    /// Emit `o` with an unresolved target and return its label.
    fn jump(&mut self, o: Op) -> CodeAddr {
        self.mint_label(op_arg(o, 0))
    }

    /// Resolve `l` to the current offset.
    fn patch(&mut self, l: CodeAddr) {
        let here = self.here();
        self.code[l].operand = here.to_operand();
    }

    fn patch_all(&mut self, ls: &[CodeAddr]) {
        for &l in ls {
            self.patch(l);
        }
    }

    /// Allocate a stack slot for `bind`. A `preassigned` hit uses that slot
    /// verbatim, so the `StoreLocal` matches the `PushGlobal` operand fn
    /// bodies were already emitted with.
    fn bind(&mut self, bind: &CoreBind) -> i32 {
        let id = bind.id;
        self.slots.resize_at_least(id, None);
        let slot = match self.preassigned.get(id).copied().flatten() {
            Some(s) => s,
            None => {
                let s = self.next_slot;
                self.next_slot += 1;
                s
            }
        };
        self.slots[id] = Some(slot);
        if slot + 1 > self.max_locals {
            self.max_locals = slot + 1;
        }
        slot
    }

    #[inline]
    fn slot(&self, id: LocalId) -> i32 {
        self.slots
            .get(id)
            .copied()
            .flatten()
            .unwrap_or_else(|| unbound_local(id))
    }

    /// True when `id` is a module-toplevel decl pinned to an entry-frame slot.
    /// Those are the program's globals, read by `PushGlobal` for the rest of
    /// the run, so a Perceus `Drop`/`Reuse` on them is unsound.
    #[inline]
    fn is_pinned(&self, id: LocalId) -> bool {
        self.preassigned.get(id).copied().flatten().is_some()
    }

    /// The one place `Op::Drop` is written. A pinned `id` emits nothing:
    /// globals are never dropped.
    fn emit_drop(&mut self, id: LocalId) {
        if !self.is_pinned(id) {
            self.push(op_arg(Op::Drop, self.slot(id)));
        }
    }

    /// The constant-pool index `id` was aliased to, if [`Self::try_alias_const`]
    /// elided its `Let`.
    #[inline]
    fn const_idx(&self, id: LocalId) -> Option<i32> {
        self.const_of.get(id).copied().flatten()
    }

    /// Record every `let x = Const(c)` nothing addresses by slot: `x`'s reads
    /// become `PushConst c` and the `Let` emits nothing. Done up front, not on
    /// the fly, so a literal between two definitions never breaks the
    /// [`Self::plan_run`] window spanning it. A `PushConst` is context-free,
    /// so walking every branch arm in one pass is sound.
    ///
    /// Skipped for the module toplevel, whose `StoreLocal`s are the
    /// global-table init.
    fn register_const_aliases(&mut self, mut e: &CoreExpr) {
        if !self.alias_consts {
            return;
        }
        loop {
            match e {
                CoreExpr::Let { bind, rhs, body } => {
                    let id = bind.id;
                    if let Atom::Const(c) = rhs
                        && !self.scan.needs_slot(id)
                        && !self.is_pinned(id)
                    {
                        self.const_of.resize_at_least(id, None);
                        self.const_of[id] = Some(c.0 as i32);
                    }
                    e = body;
                }
                CoreExpr::LetJoin { join, body, .. } => {
                    self.register_const_aliases(join);
                    e = body;
                }
                CoreExpr::LetCont { cont, body, .. } => {
                    self.register_const_aliases(cont);
                    e = body;
                }
                CoreExpr::Drop { body, .. } => e = body,
                CoreExpr::If { then, els, .. } => {
                    self.register_const_aliases(then);
                    e = els;
                }
                CoreExpr::Match { arms, .. } => {
                    for (_, b) in arms {
                        self.register_const_aliases(b);
                    }
                    return;
                }
                CoreExpr::Tail(_) | CoreExpr::Goto(_) => return,
            }
        }
    }

    /// True when this `Let` was folded into a constant alias and emits nothing.
    #[inline]
    fn is_const_alias(&self, bind: &CoreBind, rhs: &Atom) -> bool {
        matches!(rhs, Atom::Const(_)) && self.const_idx(bind.id).is_some()
    }

    /// `(slot, const)` when `args` is `[local-in-a-slot, aliased-const]` and
    /// both fit the packed `a: u8` / `b: u16` fields — the operand shape every
    /// `*IntLC` superinstruction expects.
    fn lc_operands(&self, args: &[LocalId]) -> Option<(u8, u16)> {
        let [a, b] = args else { return None };
        if self.const_idx(*a).is_some() {
            return None;
        }
        let slot = u8::try_from(self.slot(*a)).ok()?;
        let k = u16::try_from(self.const_idx(*b)?).ok()?;
        Some((slot, k))
    }

    fn push_local(&mut self, id: LocalId) {
        match self.const_idx(id) {
            Some(c) => self.push(op_arg(Op::PushConst, c)),
            None => self.push(op_arg(Op::PushLocal, self.slot(id))),
        }
    }

    /// Push one operand of a consumer: either the value of a slot/constant, or
    /// — when the operand names an inlined definition (see [`Self::plan_run`])
    /// — the definition's own instruction sequence, computed straight onto the
    /// operand stack.
    fn push_operand(&mut self, id: LocalId, defs: &[Def]) {
        match defs.iter().find(|(d, _)| *d == id) {
            Some(&(_, rhs)) => self.emit_atom(rhs, false, &[]),
            None => self.push_local(id),
        }
    }

    fn push_args(&mut self, args: &[LocalId], defs: &[Def]) {
        for &a in args {
            self.push_operand(a, defs);
        }
    }

    /// True when `id` is bound by a definition inlined into the current
    /// consumer, i.e. its value is produced on the stack and it owns no slot.
    #[inline]
    fn is_inlined(id: LocalId, defs: &[Def]) -> bool {
        defs.iter().any(|(d, _)| *d == id)
    }
}

/// An ANF definition whose single use is an operand of the consumer that
/// immediately follows it, and whose right-hand side is a pure single-value
/// atom. `emit` skips its slot entirely and re-emits the rhs at the use site,
/// recovering the fused walk's operand-stack code from linearised Core.
type Def<'e> = (LocalId, &'e Atom);

/// The expression a hoisted run of [`Def`]s feeds: either a `Let` whose rhs
/// takes them as operands, or the function's tail atom.
enum Consumer<'e> {
    Bound(&'e CoreBind, &'e Atom, &'e CoreExpr),
    Tail(&'e Atom),
}

/// Atoms an ANF `Let` may hoist into its consumer's operand position. Pure,
/// leaves exactly one value, and cannot be observed out of order: the only
/// operands that overtake it are `PushLocal`/`PushConst`, which have no
/// effects. `Ctor`/`Call`/`Closure` allocate or transfer control and are never
/// hoisted; a primop that [`Op::pushes_extra`] flags (`Print`/`BinReadUtf8`)
/// does not leave exactly one value.
fn is_hoistable(rhs: &Atom) -> bool {
    match rhs {
        Atom::Local(_) | Atom::Const(_) => true,
        Atom::PrimOp { op, .. } => !op.pushes_extra(),
        Atom::Ctor { .. } | Atom::Closure { .. } | Atom::Call { .. } => false,
    }
}

/// True when no path through `e` falls through to the following instruction
/// in value position: every leaf is a `Goto`, jumping away instead of leaving
/// a value. A merge/skip jump emitted after such an expression could never
/// execute — the same reasoning the emitter already applies to tail arms,
/// whose every exit is its own `Ret`/`TailCall*`. A `Tail` leaf falls through
/// (it leaves its value on the stack), so any path reaching one defeats this.
fn diverges(mut e: &CoreExpr) -> bool {
    loop {
        match e {
            CoreExpr::Goto(_) => return true,
            CoreExpr::Tail(_) => return false,
            CoreExpr::Let { body, .. }
            | CoreExpr::LetJoin { body, .. }
            | CoreExpr::Drop { body, .. } => e = body,
            // Body `Goto`s may land in this very cont, which then falls
            // through — so the cont must diverge too.
            CoreExpr::LetCont { cont, body, .. } => {
                if !diverges(cont) {
                    return false;
                }
                e = body;
            }
            CoreExpr::If { then, els, .. } => {
                if !diverges(then) {
                    return false;
                }
                e = els;
            }
            CoreExpr::Match { arms, .. } => {
                return arms.iter().all(|(_, b)| diverges(b));
            }
        }
    }
}

/// The operands a consumer atom pushes, in push order — `Call` pushes a
/// dynamic callee last, after its arguments.
fn consumer_operands(a: &Atom) -> Option<Vec<LocalId>> {
    match a {
        Atom::Local(_) | Atom::Const(_) => None,
        Atom::Ctor { .. } | Atom::PrimOp { .. } | Atom::Closure { .. } | Atom::Call { .. } => {
            Some(a.operands().collect())
        }
    }
}

/// For `let r = call f(args); drop a; drop b; ...body` where `a`,`b` ∈ `args`
/// (or `= callee` for a dynamic call): return the run of such drops (they are
/// the operands' *last uses* — Perceus placed them right after the call) plus
/// the body after them. Emitting those drops between the operand pushes and
/// the `Call` op transfers ownership into the callee (its Perceus `Drop` then
/// sees rc==1 and can reuse) instead of the caller holding a dup across the
/// whole call. The dynamic callee is included so its `Drop` never blocks an
/// arg's from being peeled.
///
/// A drop of a local that some `Ctor{reuse: Some(_)}` in *this* body claims is
/// **not** peeled: Perceus has already paired that hollowed cell with a
/// downstream (or loop-carried) constructor here, so ownership must stay in
/// this frame's slot for `Op::Reuse` to find it. Peeling would emit the
/// `Op::Drop` while `PushLocal` still holds a dup (rc==2), zeroing the slot
/// and handing the cell to the callee — the paired `Reuse` then reads nil and
/// allocates fresh (the `dot_loop` bench-gate regression).
///
/// A drop whose local a directly-following `tail self(..)` passes again is
/// also **not** peeled: that drop was parked by
/// `perceus::release_self_tail_args` for the back-edge, not placed after this
/// call as a last use, and it must stay put for [`split_self_tail_drops`] to
/// sink between the tail's operand pushes and the `TailCallSelf` (the pushed
/// dup keeps the value alive while the slot's reference goes). Peeling it
/// would release the slot *before* the tail re-reads it, handing the next
/// iteration a dead word — `loop_apply(f, ..) = .. loop_apply(f, .., f(x))`
/// called `f` once and looped with nil.
fn peel_call_arg_drops<'a>(
    body: &'a CoreExpr,
    args: &[LocalId],
    callee: Option<LocalId>,
    scan: &Scan,
) -> (Vec<LocalId>, &'a CoreExpr) {
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
    while let CoreExpr::Drop { local, body: b, .. } = body {
        if !args.contains(local) && callee != Some(*local) {
            break;
        }
        if scan.reuse_claimed(*local) {
            break;
        }
        if self_tail_args.contains(local) {
            break;
        }
        moved.push(*local);
        body = b;
    }
    (moved, body)
}

/// For `drop a; drop b; tail self(args)`: split the drop run into drops to
/// emit here (locals that just go dead) and drops to sink past the operand
/// pushes (those naming an argument), plus the call's arguments.
///
/// `Op::TailCallSelf` keeps its frame, so unlike every other tail call it
/// never releases an argument's source slot. The sunk drops must land between
/// the `PushLocal`s and the call: the operand copy keeps the value alive while
/// the slot's reference goes, leaving the next iteration's parameter sole
/// owner so its `Drop` can hollow the cell for reuse.
fn split_self_tail_drops(mut e: &CoreExpr) -> Option<(Vec<LocalId>, Vec<LocalId>, &[LocalId])> {
    let mut run: Vec<LocalId> = Vec::new();
    while let CoreExpr::Drop { local, body, .. } = e {
        run.push(*local);
        e = body;
    }
    let CoreExpr::Tail(Atom::Call {
        callee: Callee::Self_,
        args,
    }) = e
    else {
        return None;
    };
    let (moved, now): (Vec<LocalId>, Vec<LocalId>) =
        run.into_iter().partition(|x| args.contains(x));
    if moved.is_empty() {
        return None;
    }
    Some((now, moved, args))
}

/// Whole-body facts gathered in one pre-pass. All are conservative guards on
/// the emitter's two elisions (constant aliasing, if-condition fusion): a
/// local failing any of them keeps the plain `StoreLocal`/`PushLocal` shape.
#[derive(Default)]
struct Scan {
    /// `Ctor{reuse: Some(x)}` targets. The cell is this frame's reuse token,
    /// not ownership to hand off, so [`peel_call_arg_drops`] must leave it.
    reuse_claimed: TiVec<LocalId, bool>,
    /// Locals the emitter addresses by slot rather than by pushing them:
    /// `Match` scrutinees, `Drop` targets, reuse targets. Never const-aliased.
    needs_slot: TiVec<LocalId, bool>,
    /// Occurrence count per local, over every operand position in the body.
    uses: TiVec<LocalId, u32>,
}

impl Scan {
    fn of(e: &CoreExpr) -> Self {
        let mut s = Scan::default();
        s.walk(e);
        s
    }

    /// Whether `id` must own a real slot. Out of range means the pre-pass
    /// never saw it, and the conservative answer keeps the slot.
    fn needs_slot(&self, id: LocalId) -> bool {
        self.needs_slot.get(id).copied().unwrap_or(true)
    }

    fn uses(&self, id: LocalId) -> u32 {
        self.uses.get(id).copied().unwrap_or(0)
    }

    fn reuse_claimed(&self, id: LocalId) -> bool {
        self.reuse_claimed.get(id).copied().unwrap_or(false)
    }

    fn grow(&mut self, id: LocalId) {
        self.uses.resize_at_least(id, 0);
        self.needs_slot.resize_at_least(id, false);
        self.reuse_claimed.resize_at_least(id, false);
    }

    fn use_(&mut self, id: LocalId) {
        self.grow(id);
        self.uses[id] += 1;
    }

    fn pin(&mut self, id: LocalId) {
        self.grow(id);
        self.needs_slot[id] = true;
    }

    fn claim(&mut self, id: LocalId) {
        self.grow(id);
        self.reuse_claimed[id] = true;
        self.needs_slot[id] = true;
    }

    fn atom(&mut self, a: &Atom) {
        a.for_each_operand(|id| self.use_(id));
        // `reuse` is not an operand: it names the slot `Op::Reuse` reads, so
        // that slot must survive.
        if let Atom::Ctor { reuse: Some(r), .. } = a {
            self.use_(*r);
            self.claim(*r);
        }
    }

    fn walk(&mut self, mut e: &CoreExpr) {
        loop {
            match e {
                CoreExpr::Let { rhs, body, .. } => {
                    self.atom(rhs);
                    e = body;
                }
                CoreExpr::LetJoin { join, body, .. } => {
                    self.walk(join);
                    e = body;
                }
                CoreExpr::LetCont { cont, body, .. } => {
                    self.walk(cont);
                    e = body;
                }
                CoreExpr::Drop { local, body, .. } => {
                    self.use_(*local);
                    self.pin(*local);
                    e = body;
                }
                CoreExpr::If {
                    cond, then, els, ..
                } => {
                    self.use_(*cond);
                    self.walk(then);
                    e = els;
                }
                CoreExpr::Match { scrut, arms, .. } => {
                    self.use_(*scrut);
                    self.pin(*scrut);
                    for (_, b) in arms {
                        self.walk(b);
                    }
                    return;
                }
                CoreExpr::Tail(a) => {
                    self.atom(a);
                    return;
                }
                CoreExpr::Goto(_) => return,
            }
        }
    }
}

impl<'a, C: EmitCtx> Emitter<'a, C> {
    fn emit_expr(&mut self, e: &CoreExpr) {
        self.emit_expr_in(e, true);
    }

    /// `tail` distinguishes function-tail position, where each `Tail(a)` emits
    /// `Ret`/`TailCall*`, from `LetJoin` operand position, where it leaves the
    /// value on the stack for the merge-point `StoreLocal`.
    fn emit_expr_in(&mut self, mut e: &CoreExpr, tail: bool) {
        loop {
            match e {
                CoreExpr::Let { bind, rhs, body } => {
                    // `let x = 3` never needs a slot: `x`'s reads became
                    // `PushConst 3` in `register_const_aliases`.
                    if self.is_const_alias(bind, rhs) {
                        e = body;
                        continue;
                    }
                    // `let c = cmp(..); if c { .. }`: the comparison's only
                    // consumer is the branch, so leave the Bool on the stack.
                    // This is also what lets `Op::JumpNeIntLC` form.
                    if let CoreExpr::If {
                        cond, then, els, ..
                    } = &**body
                        && *cond == bind.id
                        && self.is_single_use_temp(bind.id)
                        && matches!(rhs, Atom::PrimOp { op, .. } if !op.pushes_extra())
                    {
                        self.emit_cond_if(rhs, then, els, tail);
                        return;
                    }
                    // `let a = n+1; let b = n+2; C(.., a, b)`: compute `a`/`b`
                    // straight onto the operand stack, owning no slot.
                    if let Some((defs, consumer)) = self.plan_run(e) {
                        match consumer {
                            Consumer::Bound(cb, crhs, cbody) => {
                                e = self.emit_bound_atom(cb, crhs, cbody, &defs);
                            }
                            Consumer::Tail(a) => {
                                self.emit_atom(a, tail, &defs);
                                return;
                            }
                        }
                        continue;
                    }
                    e = self.emit_bound_atom(bind, rhs, body, &[]);
                }
                CoreExpr::LetJoin { bind, join, body } => {
                    let mark = self.next_slot;
                    self.emit_expr_in(join, false);
                    self.next_slot = mark;
                    let slot = self.bind(bind);
                    self.push(op_arg(Op::StoreLocal, slot));
                    e = body;
                }
                // Body first, then the cont as a labelled block after it, with
                // every collected `Goto` patched to that one label. NOT the
                // `LetJoin` shape above: control only reaches the cont through
                // a `Goto`, so there is no inline evaluation and no merge.
                CoreExpr::LetCont { id, cont, body } => {
                    self.joins.resize_at_least(*id, None);
                    if self.joins[*id].is_some() {
                        redeclared_join(*id);
                    }
                    self.joins[*id] = Some(Vec::new());
                    let mark = self.next_slot;
                    self.emit_expr_in(body, tail);
                    // In value position the body falls through with its value
                    // and must skip the cont block. In tail position every
                    // body exit is its own `Ret`/`TailCall*`, so a skip jump
                    // could never execute.
                    let skip = (!tail && !diverges(body)).then(|| self.jump(Op::Jump));
                    let pending = match self.joins[*id].take() {
                        Some(p) => p,
                        None => unbound_join(*id),
                    };
                    self.patch_all(&pending);
                    // The cont only references locals bound before this node,
                    // so the body's temps are dead on every `Goto` edge.
                    self.next_slot = mark;
                    self.emit_expr_in(cont, tail);
                    self.next_slot = mark;
                    if let Some(skip) = skip {
                        self.patch(skip);
                    }
                    return;
                }
                CoreExpr::Drop { local, body, .. } => {
                    // `drop arg…; tail self(args)`: the arg releases belong
                    // between the pushes and `TailCallSelf`, which keeps its
                    // frame and so never drains them itself.
                    if tail && let Some((now, moved, args)) = split_self_tail_drops(e) {
                        for x in now {
                            self.emit_drop(x);
                        }
                        self.emit_call(Callee::Self_, args, &moved, true, &[]);
                        return;
                    }
                    self.emit_drop(*local);
                    e = body;
                }
                CoreExpr::If {
                    cond, then, els, ..
                } => {
                    self.push_local(*cond);
                    let else_j = self.jump(Op::JumpIfFalse);
                    self.emit_branches(else_j, then, els, tail);
                    return;
                }
                CoreExpr::Match { scrut, arms, .. } => {
                    self.emit_match(*scrut, arms, tail);
                    return;
                }
                CoreExpr::Tail(atom) => {
                    self.emit_atom(atom, tail, &[]);
                    return;
                }
                // A terminal jump to the join's cont block, patched when the
                // declaring `LetCont` seals. It carries no value: the cont's
                // own tail produces the result this position owes.
                CoreExpr::Goto(id) => {
                    let j = self.jump(Op::Jump);
                    match self.joins.get_mut(*id) {
                        Some(Some(pending)) => pending.push(j),
                        _ => unbound_join(*id),
                    }
                    return;
                }
            }
        }
    }

    /// Emit `let bind = rhs` and return where the spine continues. A `Call`
    /// rhs peels the operands' `Drop`s out of `body` into the call, so the
    /// continuation is `body` past those drops.
    fn emit_bound_atom<'e>(
        &mut self,
        bind: &CoreBind,
        rhs: &Atom,
        body: &'e CoreExpr,
        defs: &[Def],
    ) -> &'e CoreExpr {
        // Allocate the slot BEFORE the rhs so a loop-carried `reuse %n` on
        // `let %n = Ctor(..)` can address it. On the first iteration the slot
        // reads nil and `Reuse` falls through to alloc.
        let slot = self.bind(bind);
        let rest = if let Atom::Call { callee, args } = rhs {
            let cid = if let Callee::Local(id) = callee {
                Some(*id)
            } else {
                None
            };
            let (moved, rest) = peel_call_arg_drops(body, args, cid, &self.scan);
            self.emit_call(*callee, args, &moved, false, defs);
            rest
        } else {
            self.emit_atom(rhs, false, defs);
            body
        };
        self.push(op_arg(Op::StoreLocal, slot));
        rest
    }

    /// True when `id` is an ordinary frame temp read exactly once — safe to
    /// leave on the operand stack instead of storing it.
    fn is_single_use_temp(&self, id: LocalId) -> bool {
        !self.is_pinned(id) && !self.scan.needs_slot(id) && self.scan.uses(id) == 1
    }

    /// Emit `then`/`els` after the conditional jump at `else_j`. Both arms
    /// restart slot allocation from the current mark: they are mutually
    /// exclusive, so their temps may overlap. A tail or diverging arm gets no
    /// merge jump — it could never execute.
    fn emit_branches(&mut self, else_j: CodeAddr, then: &CoreExpr, els: &CoreExpr, tail: bool) {
        let mark = self.next_slot;
        self.emit_expr_in(then, tail);
        self.next_slot = mark;
        let end_j = (!tail && !diverges(then)).then(|| self.jump(Op::Jump));
        self.patch(else_j);
        self.emit_expr_in(els, tail);
        self.next_slot = mark;
        if let Some(end_j) = end_j {
            self.patch(end_j);
        }
    }

    /// `if <prim>(..) { then } else { els }` with the condition still on the
    /// stack. `local CMP const` fuses into a single `*IntLC` conditional jump;
    /// anything else evaluates the primop and branches on its result.
    fn emit_cond_if(&mut self, rhs: &Atom, then: &CoreExpr, els: &CoreExpr, tail: bool) {
        let else_j = match self.emit_lc_jump(rhs) {
            Some(j) => j,
            None => {
                self.emit_atom(rhs, false, &[]);
                self.jump(Op::JumpIfFalse)
            }
        };
        self.emit_branches(else_j, then, els, tail);
    }

    /// `Op::JumpNeIntLC` / `Op::JumpGeIntLC` for `local {==,<} const`. The
    /// sense is inverted: the fused op jumps when the source condition is
    /// false, like the `JumpIfFalse` it replaces.
    fn emit_lc_jump(&mut self, rhs: &Atom) -> Option<CodeAddr> {
        let Atom::PrimOp {
            op: prim,
            args,
            imm: Imm::None,
        } = rhs
        else {
            return None;
        };
        let fused = match prim {
            Op::EqInt => Op::JumpNeIntLC,
            Op::LtInt => Op::JumpGeIntLC,
            _ => return None,
        };
        let (slot, k) = self.lc_operands(args)?;
        Some(self.mint_label(op_ab(fused, slot, k, 0)))
    }

    /// The longest suffix of the pure-`Let` run at `e` whose binds are, in the
    /// same order, a subsequence of the following consumer's operands. Those
    /// binds become [`Def`]s: no slot, recomputed at their operand position.
    ///
    /// Order preservation is the whole safety argument. Each bind is used once
    /// in the consumer, so no bind feeds another; hoisting keeps their relative
    /// order and only effect-free `PushLocal`/`PushConst` overtake them.
    /// Anything failing the check trims the run and emits `StoreLocal` as
    /// usual.
    fn plan_run<'e>(&self, e: &'e CoreExpr) -> Option<(Vec<Def<'e>>, Consumer<'e>)> {
        let mut run: Vec<Def<'e>> = Vec::new();
        let mut cur = e;
        while let CoreExpr::Let { bind, rhs, body } = cur {
            // A const-aliased `Let` emits nothing, so it neither joins nor
            // ends the run.
            if self.is_const_alias(bind, rhs) {
                cur = body;
                continue;
            }
            if !is_hoistable(rhs) || !self.is_single_use_temp(bind.id) {
                break;
            }
            run.push((bind.id, rhs));
            cur = body;
        }
        if run.is_empty() {
            return None;
        }
        let (consumer, ops) = match cur {
            CoreExpr::Let { bind, rhs, body } => {
                (Consumer::Bound(bind, rhs, body), consumer_operands(rhs)?)
            }
            CoreExpr::Tail(a) => (Consumer::Tail(a), consumer_operands(a)?),
            _ => return None,
        };
        // Walk backwards, matching each bind against a strictly earlier
        // operand position than the last; the first miss ends the suffix. A
        // surviving prefix means this `Let` is not hoistable, so emit it
        // normally and re-plan from the next one.
        let mut start = run.len();
        let mut limit = ops.len();
        for i in (0..run.len()).rev() {
            match ops[..limit].iter().position(|o| *o == run[i].0) {
                Some(p) => {
                    limit = p;
                    start = i;
                }
                None => break,
            }
        }
        if start != 0 {
            return None;
        }
        Some((run, consumer))
    }

    /// Leave the atom's value on top of the stack. In tail position a `Call`
    /// lowers to its `Tail*` opcode, which is itself the return; every other
    /// atom is followed by `Ret`. `defs` are the operand definitions inlined
    /// into this atom's pushes (see [`Self::plan_run`]).
    fn emit_atom(&mut self, a: &Atom, tail: bool, defs: &[Def]) {
        // `local ± const` collapses three dispatches into one.
        if let Atom::PrimOp {
            op: prim @ (Op::AddInt | Op::SubInt),
            args,
            imm: Imm::None,
        } = a
            && !args.iter().any(|&x| Self::is_inlined(x, defs))
            && let Some((slot, k)) = self.lc_operands(args)
        {
            let fused = if *prim == Op::AddInt {
                Op::AddIntLC
            } else {
                Op::SubIntLC
            };
            self.push(op_ab(fused, slot, k, 0));
            if tail {
                self.push(op(Op::Ret));
            }
            return;
        }
        match a {
            Atom::Local(id) => {
                self.push_local(*id);
                if tail {
                    self.push(op(Op::Ret));
                }
            }
            Atom::Const(c) => {
                self.push(op_arg(Op::PushConst, c.0 as i32));
                if tail {
                    self.push(op(Op::Ret));
                }
            }
            Atom::PrimOp {
                op: prim,
                args,
                imm,
            } => {
                self.push_args(args, defs);
                self.push(op_arg(*prim, imm_operand(*prim, *imm)));
                // `Print` pushes nothing; supply the `()` so the enclosing
                // StoreLocal / Ret has a value.
                if *prim == Op::Print {
                    self.push(op(Op::PushNil));
                }
                // `BinReadUtf8` pushes (codepoint, nbits); Core sees one
                // `(Int, Int)` tuple, so `Let` binds one local.
                if *prim == Op::BinReadUtf8 {
                    self.push(op_arg(Op::MakeTuple, 2));
                }
                if tail {
                    self.push(op(Op::Ret));
                }
            }
            Atom::Ctor {
                variant,
                fields,
                reuse,
            } => {
                self.emit_ctor(variant, fields, *reuse, defs);
                if tail {
                    self.push(op(Op::Ret));
                }
            }
            Atom::Closure { func_idx, captures } => {
                self.push_args(captures, defs);
                self.push(op_ab(Op::MakeClosure, 0, 0, func_idx.to_operand()));
                if tail {
                    self.push(op(Op::Ret));
                }
            }
            Atom::Call { callee, args } => {
                self.emit_call(*callee, args, &[], tail, defs);
            }
        }
    }

    /// `moved` is the last-use args whose frame reference must be released
    /// between the pushes and the call, so the callee is sole owner and its
    /// Perceus `Drop` sees rc==1. See [`peel_call_arg_drops`].
    fn emit_call(
        &mut self,
        callee: Callee,
        args: &[LocalId],
        moved: &[LocalId],
        tail: bool,
        defs: &[Def],
    ) {
        self.push_args(args, defs);
        // Push a dynamic callee before the moved drops, so its own last-use
        // `Drop` releases the slot without touching the stacked closure word.
        if let Callee::Local(id) = callee {
            self.push_operand(id, defs);
        }
        for &m in moved {
            self.emit_drop(m);
        }
        let argc = args.len() as i32;
        match callee {
            Callee::Known(func_idx) => {
                let o = if tail {
                    Op::TailCallKnown
                } else {
                    Op::CallKnown
                };
                self.push(op_ab(o, 0, argc as u16, func_idx.to_operand()));
            }
            Callee::Self_ => {
                let o = if tail { Op::TailCallSelf } else { Op::CallSelf };
                self.push(op_arg(o, argc));
            }
            Callee::Local(_) => {
                let o = if tail { Op::TailCall } else { Op::Call };
                self.push(op_arg(o, argc));
            }
        }
        // `enter_frame!` stores the post-increment ip into the caller frame;
        // record it so a native caller stores the identical value. Tail calls
        // collapse the frame and store no resume point.
        if !tail {
            self.call_resume_ips.push(self.here().to_operand());
        }
    }

    /// Push the four header constants and the payload fields, then
    /// `MakeEnumPayload{a=reuse?, b=arity, operand=prehash}`. With a Perceus
    /// `reuse`, an `Op::Reuse slot` sits between the payloads and the make, so
    /// `a=1` and the VM overwrites the cell in place.
    fn emit_ctor(
        &mut self,
        v: &VariantRef,
        fields: &[LocalId],
        reuse: Option<LocalId>,
        defs: &[Def],
    ) {
        if let Some(b) = self.ctx.bool_variant(v.type_id, v.variant_idx) {
            self.push(op(if b { Op::PushTrue } else { Op::PushFalse }));
            return;
        }
        let type_name = self.ctx.resolve_str(v.type_name).to_owned();
        let variant_name = self.ctx.resolve_str(v.variant_name).to_owned();

        let id_c = self
            .ctx
            .intern_int((v.type_id.0 as u32 as i64) | ((v.variant_idx as i64) << 32));
        self.push(op_arg(Op::PushConst, id_c));
        let en_c = self.ctx.intern_str(&type_name);
        self.push(op_arg(Op::PushConst, en_c));
        let vn_c = self.ctx.intern_str(&variant_name);
        self.push(op_arg(Op::PushConst, vn_c));
        let lc = self.ctx.intern_labels(v.type_id, v.variant_idx);
        self.push(op_arg(Op::PushConst, lc));

        self.push_args(fields, defs);

        let a = match reuse {
            Some(id) if !self.is_pinned(id) => {
                self.push(op_arg(Op::Reuse, self.slot(id)));
                1
            }
            _ => 0,
        };
        let prehash = enum_name_prefix_hash(&type_name, &variant_name);
        let ph_c = self.ctx.intern_int(prehash as i64);
        self.push(op_ab(Op::MakeEnumPayload, a, fields.len() as u16, ph_c));
    }

    fn emit_match(&mut self, scrut: LocalId, arms: &[(CorePat, CoreExpr)], tail: bool) {
        if let Some((count, tags)) = self.switch_plan(arms) {
            self.emit_switch(scrut, arms, count, &tags, tail);
        } else {
            self.emit_ladder(scrut, arms, tail);
        }
    }

    /// A match lowers to `SwitchTag` when every arm is a `Ctor` head of the
    /// same enum, each variant appears once, and the enum is switch-eligible.
    /// Core payload binds are always irrefutable, so there is no fail edge.
    fn switch_plan(&self, arms: &[(CorePat, CoreExpr)]) -> Option<(u8, Vec<u16>)> {
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
        let count = self.ctx.switch_variant_count(tid?)?;
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
        Some((count, tags))
    }

    fn emit_switch(
        &mut self,
        scrut: LocalId,
        arms: &[(CorePat, CoreExpr)],
        count: u8,
        tags: &[u16],
        tail: bool,
    ) {
        self.push_local(scrut);
        // The table sits right after the `SwitchTag`; the offset is
        // function-relative like every other target.
        let table_base = self.here().next();
        self.push(op_ab(Op::SwitchTag, count, 0, table_base.to_operand()));
        let table: Vec<CodeAddr> = (0..count).map(|_| self.jump(Op::Jump)).collect();

        let mut ends = Vec::with_capacity(arms.len());
        let scrut_slot = self.slot(scrut);
        for ((pat, body), &tag) in arms.iter().zip(tags) {
            self.patch(table[tag as usize]);
            let mark = self.next_slot;
            if let CorePat::Ctor { fields, .. } = pat {
                self.emit_payload_binds(scrut_slot, fields);
            }
            self.emit_expr_in(body, tail);
            // A merge jump after a tail or diverging arm is unreachable.
            if !tail && !diverges(body) {
                ends.push(self.jump(Op::Jump));
            }
            self.next_slot = mark;
        }
        self.patch_all(&ends);
    }

    fn emit_ladder(&mut self, scrut: LocalId, arms: &[(CorePat, CoreExpr)], tail: bool) {
        let scrut_slot = self.slot(scrut);
        let mut ends: Vec<CodeAddr> = Vec::with_capacity(arms.len());
        for (pat, body) in arms {
            let mark = self.next_slot;
            let fail = match pat {
                CorePat::Ctor { variant, fields } => {
                    self.push(op_arg(Op::PushLocal, scrut_slot));
                    if let Some(b) = self.ctx.bool_variant(variant.type_id, variant.variant_idx) {
                        self.push(op(if b { Op::PushTrue } else { Op::PushFalse }));
                        self.push(op(Op::Eq));
                    } else {
                        let id_c = self.ctx.intern_int(variant.type_id.0 as i64);
                        self.push(op_arg(Op::PushConst, id_c));
                        let vn = self.ctx.resolve_str(variant.variant_name).to_owned();
                        let vn_c = self.ctx.intern_str(&vn);
                        self.push(op_arg(Op::PushConst, vn_c));
                        self.push(op(Op::MatchEnum));
                    }
                    let f = self.jump(Op::JumpIfFalse);
                    self.emit_payload_binds(scrut_slot, fields);
                    Some(f)
                }
                CorePat::Lit(c) => {
                    self.push(op_arg(Op::PushLocal, scrut_slot));
                    self.push(op_arg(Op::PushConst, c.0 as i32));
                    self.push(op(Op::Eq));
                    Some(self.jump(Op::JumpIfFalse))
                }
                CorePat::Bind(b) => {
                    let slot = self.bind(b);
                    self.push(op_arg(Op::PushLocal, scrut_slot));
                    self.push(op_arg(Op::StoreLocal, slot));
                    None
                }
                CorePat::Wild => None,
            };
            self.emit_expr_in(body, tail);
            // A merge jump after a tail or diverging arm is unreachable.
            if !tail && !diverges(body) {
                ends.push(self.jump(Op::Jump));
            }
            if let Some(f) = fail {
                self.patch(f);
            }
            self.next_slot = mark;
        }
        // Unreachable when the source match was exhaustive. Reaching it means
        // a checker or lowering defect, and halting beats returning a nil that
        // flows onward.
        self.push(op(Op::Halt));
        self.patch_all(&ends);
    }

    /// Spill the payload words into fresh slots for `fields`, once the tag is
    /// proven. Perceus `Drop`/`Reuse` for the scrutinee cell live in the arm
    /// body, so this does not emit them.
    fn emit_payload_binds(&mut self, scrut_slot: i32, fields: &[CoreBind]) {
        if fields.is_empty() {
            return;
        }
        self.push(op_arg(Op::PushLocal, scrut_slot));
        self.push(op(Op::UnwrapEnum));
        // VM pushes payloads in declaration order (last on top).
        let field_slots: Vec<i32> = fields.iter().map(|b| self.bind(b)).collect();
        for &slot in field_slots.iter().rev() {
            self.push(op_arg(Op::StoreLocal, slot));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::testkit::{ctx, func, vref};
    use super::super::{ConstId, FuncIdx};
    use super::*;
    use crate::typed_ir::RTy;

    fn ops(out: &EmitOut) -> Vec<Op> {
        out.code.iter().map(|i| i.op).collect()
    }

    fn bind(id: u32) -> CoreBind {
        super::super::testkit::bind(id, RTy(0))
    }

    #[test]
    fn primop_let_then_tail_local() {
        // let %2 = AddInt(%0, %1); ret %2
        let body = CoreExpr::Let {
            bind: bind(2),
            rhs: Atom::prim(Op::AddInt, vec![LocalId(0), LocalId(1)]),
            body: Box::new(CoreExpr::Tail(Atom::Local(LocalId(2)))),
        };
        let f = func(vec![bind(0), bind(1)], body, RTy(0));
        let mut cx = ctx(None);
        let out = emit(&f, &mut cx);
        assert_eq!(
            ops(&out),
            vec![
                Op::PushLocal,
                Op::PushLocal,
                Op::AddInt,
                Op::StoreLocal,
                Op::PushLocal,
                Op::Ret,
            ]
        );
        assert_eq!(out.code[3].operand, 2); // StoreLocal into slot 2
        assert_eq!(out.locals, 3);
    }

    #[test]
    fn tail_call_self_and_known() {
        let body = CoreExpr::Tail(Atom::Call {
            callee: Callee::Self_,
            args: vec![LocalId(0)],
        });
        let f = func(vec![bind(0)], body, RTy(0));
        let mut cx = ctx(None);
        let out = emit(&f, &mut cx);
        assert_eq!(ops(&out), vec![Op::PushLocal, Op::TailCallSelf]);
        assert_eq!(out.code[1].operand, 1);

        let body = CoreExpr::Let {
            bind: bind(1),
            rhs: Atom::Call {
                callee: Callee::Known(FuncIdx(7)),
                args: vec![LocalId(0)],
            },
            body: Box::new(CoreExpr::Tail(Atom::Local(LocalId(1)))),
        };
        let f = func(vec![bind(0)], body, RTy(0));
        let out = emit(&f, &mut cx);
        assert_eq!(out.code[1].op, Op::CallKnown);
        assert_eq!(out.code[1].b, 1);
        assert_eq!(out.code[1].operand, 7);
    }

    /// The [`FrameLayout`] must state exactly what this emission did: params
    /// own slots 0.., a stored local owns its `StoreLocal` slot, elided locals
    /// own none, and every non-tail call records its resume ip.
    #[test]
    fn frame_layout_mirrors_the_emitted_frame() {
        // let %2 = call fn#7(%0, %1); let %3 = AddInt(%2, %0);
        // ret call self(%3, %2)
        let body = CoreExpr::Let {
            bind: bind(2),
            rhs: Atom::Call {
                callee: Callee::Known(FuncIdx(7)),
                args: vec![LocalId(0), LocalId(1)],
            },
            body: Box::new(CoreExpr::Let {
                bind: bind(3),
                rhs: Atom::prim(Op::AddInt, vec![LocalId(2), LocalId(0)]),
                body: Box::new(CoreExpr::Tail(Atom::Call {
                    callee: Callee::Self_,
                    args: vec![LocalId(3), LocalId(2)],
                })),
            }),
        };
        let f = func(vec![bind(0), bind(1)], body, RTy(0));
        let mut cx = ctx(None);
        let out = emit(&f, &mut cx);

        // Params take slots 0..arity, the interpreter's argument layout.
        assert_eq!(out.layout.slot(LocalId(0)), Some(0));
        assert_eq!(out.layout.slot(LocalId(1)), Some(1));
        assert_eq!(out.layout.slot(LocalId(2)), Some(2));
        assert_eq!(out.layout.locals, out.locals);

        // The one non-tail call's resume ip names the instruction after the
        // `CallKnown`. The trailing self-call records nothing.
        assert_eq!(out.layout.call_resume_ips.len(), 1);
        let ip = out.layout.call_resume_ips[0];
        assert_eq!(out.code[ip as usize - 1].op, Op::CallKnown);
        assert_eq!(out.code[ip as usize].op, Op::StoreLocal);
        assert_eq!(
            out.code[ip as usize].operand,
            out.layout.slot(LocalId(2)).unwrap()
        );
    }

    /// A `let x = <const>` whose reads became `PushConst` owns no slot, and
    /// the layout must say so: the native backend keeps it in registers only.
    #[test]
    fn frame_layout_elides_const_aliases() {
        // let %1 = c0; ret AddInt(%0, %1)
        let body = CoreExpr::Let {
            bind: bind(1),
            rhs: Atom::Const(ConstId(0)),
            body: Box::new(CoreExpr::Tail(Atom::prim(
                Op::AddInt,
                vec![LocalId(0), LocalId(1)],
            ))),
        };
        let f = func(vec![bind(0)], body, RTy(0));
        let mut cx = ctx(None);
        let out = emit(&f, &mut cx);
        assert_eq!(out.layout.slot(LocalId(0)), Some(0));
        assert_eq!(out.layout.slot(LocalId(1)), None);
        assert!(out.layout.call_resume_ips.is_empty());
    }

    #[test]
    fn ctor_with_perceus_reuse() {
        let body = CoreExpr::Tail(Atom::Ctor {
            variant: vref(3, 1),
            fields: vec![LocalId(0), LocalId(1)],
            reuse: Some(LocalId(0)),
        });
        let f = func(vec![bind(0), bind(1)], body, RTy(0));
        let mut cx = ctx(None);
        let out = emit(&f, &mut cx);
        // 4×PushConst header, 2×PushLocal fields, Reuse, MakeEnumPayload{a=1,b=2}, Ret
        assert_eq!(
            ops(&out),
            vec![
                Op::PushConst,
                Op::PushConst,
                Op::PushConst,
                Op::PushConst,
                Op::PushLocal,
                Op::PushLocal,
                Op::Reuse,
                Op::MakeEnumPayload,
                Op::Ret,
            ]
        );
        let make = out.code[7];
        assert_eq!(make.a, 1);
        assert_eq!(make.b, 2);
        assert_eq!(out.code[6].operand, 0); // Reuse slot 0
    }

    #[test]
    fn match_switch_tag_vs_ladder() {
        let arms = vec![
            (
                CorePat::Ctor {
                    variant: vref(5, 0),
                    fields: vec![],
                },
                CoreExpr::Tail(Atom::Const(ConstId(0))),
            ),
            (
                CorePat::Ctor {
                    variant: vref(5, 1),
                    fields: vec![],
                },
                CoreExpr::Tail(Atom::Const(ConstId(1))),
            ),
        ];
        let body = CoreExpr::Match {
            scrut: LocalId(0),
            arms: arms.clone(),
            ty: RTy(0),
        };
        let f = func(vec![bind(0)], body, RTy(0));

        // Resolved 2-variant enum → SwitchTag.
        let mut cx = ctx(Some(2));
        let out = emit(&f, &mut cx);
        assert_eq!(out.code[0].op, Op::PushLocal);
        assert_eq!(out.code[1].op, Op::SwitchTag);
        assert_eq!(out.code[1].a, 2);
        assert_eq!(out.code[1].operand, 2); // table base: relative, right after the SwitchTag
        assert_eq!(out.code[2].op, Op::Jump);
        assert_eq!(out.code[3].op, Op::Jump);
        // The table's entries are relative too, and land on distinct arm
        // bodies past the table itself.
        let (a0, a1) = (out.code[2].operand, out.code[3].operand);
        assert_ne!(a0, a1);
        for arm in [a0, a1] {
            assert!(arm > 3, "arm target {arm} lands inside the jump table");
            assert_eq!(out.code[arm as usize].op, Op::PushConst);
        }

        // Unresolved → MatchEnum ladder.
        let mut cx = ctx(None);
        let f = func(
            vec![bind(0)],
            CoreExpr::Match {
                scrut: LocalId(0),
                arms,
                ty: RTy(0),
            },
            RTy(0),
        );
        let out = emit(&f, &mut cx);
        assert!(ops(&out).contains(&Op::MatchEnum));
        assert!(!ops(&out).contains(&Op::SwitchTag));
    }

    /// The relative→absolute bridge is exactly "add `base` to jump operands":
    /// opcodes and non-jump operands are untouched.
    #[test]
    fn relocate_shifts_only_jump_operands() {
        const B: i32 = 100;

        // A `SwitchTag` fixture: table base, raw table entries and arm-end
        // `Jump`s — every operand shape that must move.
        let arms = vec![
            (
                CorePat::Ctor {
                    variant: vref(5, 0),
                    fields: vec![],
                },
                CoreExpr::Tail(Atom::Const(ConstId(0))),
            ),
            (
                CorePat::Ctor {
                    variant: vref(5, 1),
                    fields: vec![],
                },
                CoreExpr::Tail(Atom::Const(ConstId(1))),
            ),
        ];
        let switch = func(
            vec![bind(0)],
            CoreExpr::Match {
                scrut: LocalId(0),
                arms,
                ty: RTy(0),
            },
            RTy(0),
        );
        // A `JumpIfFalse` fixture.
        let branch = func(
            vec![bind(0), bind(1)],
            CoreExpr::If {
                cond: LocalId(0),
                then: Box::new(CoreExpr::Tail(Atom::Local(LocalId(1)))),
                els: Box::new(CoreExpr::Tail(Atom::Const(ConstId(0)))),
                ty: RTy(0),
            },
            RTy(0),
        );
        // A `JumpNeIntLC` fixture: `let k = 7; let c = EqInt(%0, k); if c`.
        // The fused jump carries `a`/`b` operands, so it is the one label
        // `jump()` does not emit — `emit_lc_jump` mints it directly.
        let fused = func(
            vec![bind(0)],
            CoreExpr::Let {
                bind: bind(1),
                rhs: Atom::Const(ConstId(7)),
                body: Box::new(CoreExpr::Let {
                    bind: bind(2),
                    rhs: Atom::prim(Op::EqInt, vec![LocalId(0), LocalId(1)]),
                    body: Box::new(CoreExpr::If {
                        cond: LocalId(2),
                        then: Box::new(CoreExpr::Tail(Atom::Local(LocalId(0)))),
                        els: Box::new(CoreExpr::Tail(Atom::Const(ConstId(0)))),
                        ty: RTy(0),
                    }),
                }),
            },
            RTy(0),
        );
        let mut cx = ctx(Some(2));
        assert!(ops(&emit(&fused, &mut cx)).contains(&Op::JumpNeIntLC));

        for f in [&switch, &branch, &fused] {
            let mut cx = ctx(Some(2));
            let rel = emit(f, &mut cx).code;
            let mut abs = rel.clone();
            relocate(&mut abs, B);

            assert!(rel.iter().any(|i| i.op.has_jump_target()));
            for (r, a) in rel.iter().zip(&abs) {
                assert_eq!(r.op, a.op);
                assert_eq!((r.a, r.b), (a.a, a.b));
                let shift = if r.op.has_jump_target() { B } else { 0 };
                assert_eq!(a.operand, r.operand + shift);
                // Relative targets index this block, never `program.code`.
                if r.op.has_jump_target() {
                    assert!((r.operand as usize) <= rel.len());
                }
            }
        }
    }

    /// Every `Goto` is a forward `Jump` to the one shared cont label.
    #[test]
    fn letcont_gotos_share_one_label_after_the_body() {
        use super::super::JoinId;
        // letcont j0 = ret c1
        // in if %0 { goto j0 } else { if %1 { goto j0 } else { ret c0 } }
        let body = CoreExpr::LetCont {
            id: JoinId(0),
            cont: Box::new(CoreExpr::Tail(Atom::Const(ConstId(1)))),
            body: Box::new(CoreExpr::If {
                cond: LocalId(0),
                then: Box::new(CoreExpr::Goto(JoinId(0))),
                els: Box::new(CoreExpr::If {
                    cond: LocalId(1),
                    then: Box::new(CoreExpr::Goto(JoinId(0))),
                    els: Box::new(CoreExpr::Tail(Atom::Const(ConstId(0)))),
                    ty: RTy(0),
                }),
                ty: RTy(0),
            }),
        };
        let f = func(vec![bind(0), bind(1)], body, RTy(0));
        let mut cx = ctx(None);
        let out = emit(&f, &mut cx);
        assert_eq!(
            ops(&out),
            vec![
                Op::PushLocal,   // 0: %0
                Op::JumpIfFalse, // 1: -> 3
                Op::Jump,        // 2: goto j0
                Op::PushLocal,   // 3: %1
                Op::JumpIfFalse, // 4: -> 6
                Op::Jump,        // 5: goto j0
                Op::PushConst,   // 6: c0
                Op::Ret,         // 7: body's own exit — no skip jump in tail
                Op::PushConst,   // 8: cont (the shared label)
                Op::Ret,         // 9
            ]
        );
        // Both gotos jump forward to the one cont label, function-relative.
        assert_eq!(out.code[2].operand, 8);
        assert_eq!(out.code[5].operand, 8);
        assert_eq!(out.code[1].operand, 3);
        assert_eq!(out.code[4].operand, 6);
    }

    /// In value position the body must jump over the cont block so both paths
    /// meet at the merge `StoreLocal` with exactly one value on the stack.
    #[test]
    fn letcont_in_value_position_skips_the_cont_block() {
        use super::super::JoinId;
        // letj %2 = (letcont j0 = ret c1 in if %0 { goto j0 } else { ret c0 })
        // ret %2
        let body = CoreExpr::LetJoin {
            bind: bind(2),
            join: Box::new(CoreExpr::LetCont {
                id: JoinId(0),
                cont: Box::new(CoreExpr::Tail(Atom::Const(ConstId(1)))),
                body: Box::new(CoreExpr::If {
                    cond: LocalId(0),
                    then: Box::new(CoreExpr::Goto(JoinId(0))),
                    els: Box::new(CoreExpr::Tail(Atom::Const(ConstId(0)))),
                    ty: RTy(0),
                }),
            }),
            body: Box::new(CoreExpr::Tail(Atom::Local(LocalId(2)))),
        };
        let f = func(vec![bind(0)], body, RTy(0));
        let mut cx = ctx(None);
        let out = emit(&f, &mut cx);
        assert_eq!(
            ops(&out),
            vec![
                Op::PushLocal,   // 0: %0
                Op::JumpIfFalse, // 1: -> 3
                Op::Jump,        // 2: goto j0 -> 5 (no merge jump: arm diverges)
                Op::PushConst,   // 3: c0
                Op::Jump,        // 4: skip the cont block -> 6
                Op::PushConst,   // 5: cont: c1, falls through to the merge
                Op::StoreLocal,  // 6: %2
                Op::PushLocal,   // 7: %2
                Op::Ret,         // 8
            ]
        );
        assert_eq!(out.code[2].operand, 5); // goto lands on the cont label
        assert_eq!(out.code[4].operand, 6); // skip lands past the cont
        assert_eq!(out.code[1].operand, 3);
    }

    #[test]
    fn drop_and_if_emit() {
        let body = CoreExpr::Drop {
            local: LocalId(0),
            shape: Some(super::super::ReuseShape::enum_(0)),
            body: Box::new(CoreExpr::If {
                cond: LocalId(1),
                then: Box::new(CoreExpr::Tail(Atom::Local(LocalId(1)))),
                els: Box::new(CoreExpr::Tail(Atom::Const(ConstId(0)))),
                ty: RTy(0),
            }),
        };
        let f = func(vec![bind(0), bind(1)], body, RTy(0));
        let mut cx = ctx(None);
        let out = emit(&f, &mut cx);
        assert_eq!(out.code[0].op, Op::Drop);
        assert_eq!(out.code[0].operand, 0);
        assert_eq!(out.code[1].op, Op::PushLocal);
        assert_eq!(out.code[2].op, Op::JumpIfFalse);
        // else target patched past the then-arm's Ret (no merge jump in tail)
        let else_tgt = out.code[2].operand;
        assert_eq!(out.code[else_tgt as usize].op, Op::PushConst);
    }
}
