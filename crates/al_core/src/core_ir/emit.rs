//! Core → bytecode.
//!
//! [`emit`] walks a [`CoreFn`]'s ANF spine and produces a flat
//! `Vec<Instruction>` plus the frame's local-slot count. Type erasure happens
//! here and only here: the `Ty` on each bind is discarded, `LocalId`s become
//! stack slots, and every `Atom` lowers to the opcode sequence the VM's
//! dispatch loop expects (see `crate::bytecode::Op`).
//!
//! The mapping is direct — Core is already linearised, so there is no
//! expression stack to manage:
//!
//! | Core                              | Bytecode                                  |
//! |-----------------------------------|-------------------------------------------|
//! | `Let{rhs=PrimOp{op,args}}`        | `PushLocal args; op; StoreLocal`          |
//! | `Let{rhs=Ctor{v,fs,reuse}}`       | header consts; `PushLocal fs`;            |
//! |                                   | `[Reuse r;]` `MakeEnumPayload{a,b,hash}`  |
//! | `Let{rhs=Call{Known f}}`          | `PushLocal args; CallKnown{b=argc,f}`     |
//! | `Tail(Call{Self_})`               | `PushLocal args; TailCallSelf argc`       |
//! | `Match` (all-Ctor, exhaustive)    | `PushLocal s; SwitchTag; jump-table; arms`|
//! | `Match` (otherwise)               | per-arm `MatchEnum`/`Eq` ladder           |
//!
//! Jump targets are **function-relative**: every operand of a jump op (and the
//! `SwitchTag` table base) indexes the returned `code` vector, so emit never
//! spells an absolute `program.code` address and needs no `base` argument.
//!
//! Relocation keys off the *frame*, not the splice. The VM resolves a jump as
//! `Function.code_start + operand`, so a block needs [`relocate`] exactly when
//! its owning frame's `code_start` differs from the address the block is
//! spliced at. A function body needs none: its `code_start` **is** its splice
//! address, and the VM adds it back. Only the module toplevel does — it runs
//! in the entry frame, whose `code_start` is 0 while the block starts at
//! `base`.

use super::{
    Atom, Callee, CodeAddr, CoreBind, CoreExpr, CoreFn, CorePat, Imm, LocalId, VariantRef,
};
use crate::bytecode::{Instruction, Op, enum_name_prefix_hash, op, op_ab, op_arg};
use crate::tivec::TiVec;
use crate::type_def::TypeId;
use crate::types::StrId;

/// Services `emit` needs from the enclosing compilation: string-interner
/// resolution and constant-pool interning. `Compiler` implements this so the
/// header constants a `Ctor` needs (`type_id | variant_idx`, names, labels,
/// prehash) are pooled exactly once alongside every other program constant.
pub trait EmitCtx {
    /// Resolve an engine-interned string id.
    fn resolve_str(&self, id: StrId) -> &str;
    /// Pool a frozen `Int` constant, returning its `PushConst` operand.
    fn intern_int(&mut self, i: i64) -> i32;
    /// Pool a frozen string constant.
    fn intern_str(&mut self, s: &str) -> i32;
    /// Pool the frozen field-label array for `(tid, variant_idx)`.
    fn intern_labels(&mut self, tid: TypeId, variant_idx: u16) -> i32;
    /// Total variant count of `tid` when it is a boxed enum eligible for
    /// `SwitchTag` (resolved `Con`, not `Bool`, ≤ 255 variants). `None`
    /// forces the `MatchEnum` ladder.
    fn switch_variant_count(&self, tid: TypeId) -> Option<u8>;
    /// `Some(true)` for prelude `Bool.True`, `Some(false)` for `Bool.False`,
    /// `None` for any other constructor. `Bool` is unboxed at the VM level so
    /// its constructors emit `PushTrue`/`PushFalse` and its patterns compare
    /// with `Eq` — never `MakeEnumPayload`/`MatchEnum`.
    fn bool_variant(&self, tid: TypeId, variant_idx: u16) -> Option<bool>;
}

/// Result of [`emit`].
pub struct EmitOut {
    pub code: Vec<Instruction>,
    /// High-water stack-slot count for the frame — becomes `Function.locals`.
    pub locals: i32,
}

/// Shift every jump operand in a freshly emitted block by `base`, turning the
/// function-relative addresses [`emit`] produces into absolute `program.code`
/// addresses. Call this only for a block whose frame's `code_start` is not the
/// block's own start address — today that is the module toplevel alone.
///
/// [`Op::has_jump_target`] is the one authority on which operands are
/// addresses, and it is an exhaustive match, so a new jump op cannot be added
/// without classifying it here. It covers the `SwitchTag` table base and the
/// raw entries of the table itself, which are ordinary `Jump` instructions.
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
    EmitOut {
        code: e.code.into_vec(),
        locals: e.max_locals,
    }
}

/// Lower a bare expression (module toplevel). `slot_base` is the first
/// entry-frame slot this file may allocate. The slot pinnings for the
/// top-level `fn`/`const` decls ride the body's own binds
/// ([`CoreExpr::toplevel_globals`]) — the exact slots `analyse_module`
/// already handed out. The fn bodies emitted during that walk reference
/// those slots via `PushGlobal`, so the entry-frame init **must**
/// `StoreLocal` to the same indices; deriving the pinning here, from the
/// body being emitted, makes a disagreeing pinning unrepresentable. A
/// `GlobalSlot` unwraps to a plain frame slot at this point because the
/// entry frame *is* the global table — this function's premise. Every
/// other bind (compound-init temporaries, top-level `let`s, expression
/// spills) is allocated linearly past the preassigned range.
pub fn emit_toplevel<C: EmitCtx>(body: &CoreExpr, slot_base: i32, ctx: &mut C) -> EmitOut {
    let preassigned = body.toplevel_globals();
    let mut e = Emitter::new(ctx);
    e.scan = Scan::of(body);
    // The entry frame's `StoreLocal`s double as the program's global-table
    // init (they freeze the bound graph), so no toplevel bind may be elided
    // into a `PushConst` alias. The entry frame runs once — there is nothing
    // to win here anyway.
    e.alias_consts = false;
    // Decl slots are the contiguous `[slot_base, slot_base+N)` range from
    // `analyse_module` Pass 3; spill temps start immediately after so a linear
    // `next_slot++` never collides with a pinned slot.
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
    // Not `emit_expr`: the entry frame is `__main__`, whose slots *are* the
    // program's globals — a `TailCall`/`Ret` here would collapse that frame
    // and clobber every `PushGlobal` the callee (and everything it transitively
    // calls) performs. The trailing `Halt` the caller appends is the real exit.
    e.emit_expr_in(body, false);
    EmitOut {
        code: e.code.into_vec(),
        locals: e.max_locals,
    }
}

/// An emitted jump whose operand is not yet known is named by the [`CodeAddr`]
/// of the instruction itself, and [`Emitter::patch`] resolves it to the offset
/// emission has since reached. A label *is* a code address — both index the
/// emitter's own `code` — which is why no jump ever needs the block's eventual
/// absolute address, and why `code` is a `TiVec<CodeAddr, _>`: nothing but a
/// `CodeAddr` can subscript it.
struct Emitter<'a, C: EmitCtx> {
    code: TiVec<CodeAddr, Instruction>,
    /// `LocalId → stack slot`. Dense (lower mints ids in order) so a vector
    /// indexed by id, growing on demand, beats a hashmap. `None` (or out of
    /// range) means the id was never bound — reading it is a compiler bug,
    /// aborted by [`unbound_local`].
    slots: TiVec<LocalId, Option<i32>>,
    /// `LocalId → pinned slot` for the module-toplevel decl binds; consulted
    /// by [`Self::bind`] before falling through to `next_slot`. Empty for
    /// ordinary function bodies.
    preassigned: TiVec<LocalId, Option<i32>>,
    /// `LocalId → constant-pool index` for `let x = Const(c)` binds that were
    /// elided instead of materialised into a slot: every `PushLocal x` becomes
    /// `PushConst c`. This is what restores the fused walk's
    /// `PushLocal s; PushConst k; op` operand shape from ANF — without it every
    /// literal costs a `PushConst; StoreLocal` pair and no `*IntLC`
    /// superinstruction can ever form.
    const_of: TiVec<LocalId, Option<i32>>,
    /// Whole-body facts gathered before emission (use counts, reuse claims,
    /// which locals must own a real slot).
    scan: Scan,
    /// Off for the module toplevel, whose `StoreLocal`s are the global-table
    /// init and must all be emitted.
    alias_consts: bool,
    next_slot: i32,
    max_locals: i32,
    ctx: &'a mut C,
}

/// A `LocalId` reached emission without ever being bound to a slot — a broken
/// emit invariant. Aborts, in release as well as debug: any slot returned
/// instead (the old fallback was 0) compiles to a `PushLocal 0` that silently
/// clobbers or reads parameter 0. Mirrors `lower`'s `read_before_bind`.
#[allow(clippy::panic)]
#[cold]
#[inline(never)]
fn unbound_local(id: LocalId) -> ! {
    panic!(
        "internal compiler error: emit reached unbound local {id}. \
         Please report this as a compiler bug."
    )
}

/// Flatten a typed [`Imm`] to the instruction's raw `i32` operand — the one
/// point where the tag is erased. `Op::IndexOr` is the only op whose operand
/// encodes a constant default (`ConstId`, or `-1` for "default was pushed");
/// any other op/variant pairing is a lowering bug, and aborts rather than
/// emit an operand the VM would misread.
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
            scan: Scan::default(),
            alias_consts: false,
            next_slot: 0,
            max_locals: 0,
            ctx,
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

    /// Push `ins` — whose operand must be a jump target — and return the
    /// [`CodeAddr`] naming it. The ONE place a label is minted, so every jump
    /// the emitter writes is address-classified by [`Op::has_jump_target`], the
    /// same predicate [`relocate`] and the peephole pass consult.
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

    /// Allocate a stack slot for `bind` and record the `LocalId → slot` entry.
    /// A `preassigned` hit (module-toplevel `fn`/`const` decl) uses that slot
    /// verbatim so the `StoreLocal` matches the `PushGlobal` operand fn bodies
    /// were already emitted with; every other bind takes the next linear slot.
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

    /// True when `id` is a module-toplevel decl bind pinned to an entry-frame
    /// slot. Those values are the program's globals — fn bodies read them via
    /// `PushGlobal` — so a Perceus `Drop`/`Reuse` on them is unsound and is
    /// suppressed at emit time.
    #[inline]
    fn is_pinned(&self, id: LocalId) -> bool {
        self.preassigned.get(id).copied().flatten().is_some()
    }

    /// The one place `Op::Drop` is written. A pinned `id` emits nothing:
    /// globals are never dropped — the entry-frame slot it lives in *is* the
    /// program's global table, and fn bodies read it via `PushGlobal` for the
    /// rest of the run.
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

    /// Record every `let x = Const(c)` that nothing addresses by slot, ahead of
    /// emission: `x`'s reads become `PushConst c` and the `Let` itself emits
    /// nothing. Done up-front, not on the fly, so a literal sitting between two
    /// definitions never breaks the [`Self::plan_run`] window spanning it. A
    /// `PushConst` is context-free, so registering the aliases of every arm of
    /// every branch in one walk is sound.
    ///
    /// Skipped entirely for the module toplevel, whose `StoreLocal`s are the
    /// program's global-table init and must all be emitted.
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
                CoreExpr::Tail(_) => return,
            }
        }
    }

    /// True when this `Let` was folded into a constant alias and emits nothing.
    #[inline]
    fn is_const_alias(&self, bind: &CoreBind, rhs: &Atom) -> bool {
        matches!(rhs, Atom::Const(_)) && self.const_idx(bind.id).is_some()
    }

    /// `(slot, const)` when `args` is exactly `[local-in-a-slot, aliased-const]`
    /// and both fit the packed `a: u8` / `b: u16` operand fields — the operand
    /// shape every `*IntLC` superinstruction expects.
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
fn peel_call_arg_drops<'a>(
    mut body: &'a CoreExpr,
    args: &[LocalId],
    callee: Option<LocalId>,
    scan: &Scan,
) -> (Vec<LocalId>, &'a CoreExpr) {
    let mut moved = Vec::new();
    while let CoreExpr::Drop { local, body: b, .. } = body {
        if !args.contains(local) && callee != Some(*local) {
            break;
        }
        if scan.reuse_claimed(*local) {
            break;
        }
        moved.push(*local);
        body = b;
    }
    (moved, body)
}

/// For `drop a; drop b; tail self(args)`: split the drop run into the drops to
/// emit right here (locals that merely go dead at the tail edge) and the ones
/// to *sink* past the operand pushes (those that name an argument), plus the
/// call's arguments.
///
/// `Op::TailCallSelf` keeps its frame — `collapse_tail_frame_self` swaps the
/// new arguments into the parameter slots and leaves the other locals in place
/// because those slots *are* the per-frame reuse table — so, unlike every
/// other tail call (whose frame is drained), it never releases an argument's
/// source slot. [`perceus::release_self_tail_args`] therefore parks a `Drop`
/// for each heap argument just before the `Tail`, and it has to land between
/// the `PushLocal`s and the call: the operand copy keeps the value alive while
/// the slot's own reference goes, leaving the next iteration's parameter the
/// sole owner (rc==1) so its `Drop` can hollow the cell for reuse.
///
/// [`perceus::release_self_tail_args`]: super::perceus
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

/// Whole-body facts gathered in one pre-pass, before a single instruction is
/// emitted. Everything here is a *conservative* guard on the two elisions the
/// emitter performs (constant aliasing and if-condition fusion); a local that
/// fails any of them simply keeps the plain `StoreLocal`/`PushLocal` shape.
#[derive(Default)]
struct Scan {
    /// `Ctor{reuse: Some(x)}` targets. [`peel_call_arg_drops`] must not peel
    /// such a drop into the callee: the cell is this frame's reuse token, not
    /// ownership to hand off.
    reuse_claimed: TiVec<LocalId, bool>,
    /// Locals the emitter addresses *by slot* rather than by pushing them:
    /// `Match` scrutinees (`emit_payload_binds` re-reads the slot per arm),
    /// `Drop` targets (`Op::Drop` names a slot), and reuse targets
    /// (`Op::Reuse` names a slot). These can never be const-aliased.
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
    /// never saw `id` at all; the conservative answer keeps its slot.
    fn needs_slot(&self, id: LocalId) -> bool {
        self.needs_slot.get(id).copied().unwrap_or(true)
    }

    /// Occurrence count of `id` over every operand position in the body.
    fn uses(&self, id: LocalId) -> u32 {
        self.uses.get(id).copied().unwrap_or(0)
    }

    /// Whether some `Ctor{reuse: Some(id)}` in the body claims `id`'s cell.
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
        // `reuse` is not an operand: it names the slot `Op::Reuse` reads, which
        // must therefore survive as a real slot.
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
            }
        }
    }
}

impl<'a, C: EmitCtx> Emitter<'a, C> {
    // ── expressions ───────────────────────────────────────────────────────

    fn emit_expr(&mut self, e: &CoreExpr) {
        self.emit_expr_in(e, true);
    }

    /// Walk the Let/Drop spine iteratively; only Match/If arms recurse.
    /// `tail` distinguishes function-tail position (each `Tail(a)` emits `Ret`
    /// / `TailCall*`) from `LetJoin` operand position (each `Tail(a)` leaves
    /// the value on the stack for the merge-point `StoreLocal`).
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
                    // `let c = cmp(..); if c { .. }` — the comparison's only
                    // consumer is the branch it feeds, so leave the Bool on the
                    // stack instead of round-tripping it through a slot. This
                    // is also what lets `Op::JumpNeIntLC` / `Op::JumpGeIntLC`
                    // form at all.
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
                    // `let a = n+1; let b = n+2; C(.., a, b)` — compute `a`/`b`
                    // straight onto the operand stack at their push positions,
                    // the way the fused AST walk did. Slot-free.
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
                CoreExpr::Drop { local, body, .. } => {
                    // `drop arg…; tail self(args)`: the arg releases belong
                    // between the operand pushes and `TailCallSelf`, which
                    // keeps the frame and so never drains them itself.
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
            }
        }
    }

    /// Emit `let bind = rhs` and return where the spine continues. A `Call` rhs
    /// additionally peels the operands' `Drop`s out of `body` into the call
    /// itself, so the continuation is `body` past those drops.
    fn emit_bound_atom<'e>(
        &mut self,
        bind: &CoreBind,
        rhs: &Atom,
        body: &'e CoreExpr,
        defs: &[Def],
    ) -> &'e CoreExpr {
        // Allocate the slot *before* the rhs so a loop-carried `reuse %n` on
        // `let %n = Ctor(..)` (Perceus self-slot preference) can address it. On
        // the first iteration the slot reads nil and `Reuse` falls through to
        // alloc.
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

    /// Emit `then`/`els` after the already-emitted conditional jump at
    /// `else_j`. Both arms restart slot allocation from the current mark: they
    /// are mutually exclusive, so their temps may overlap. In tail position
    /// there is no merge jump to emit: every tail arm ends in its own
    /// `Ret`/`TailCall*`, so a jump after it could never execute.
    fn emit_branches(&mut self, else_j: CodeAddr, then: &CoreExpr, els: &CoreExpr, tail: bool) {
        let mark = self.next_slot;
        self.emit_expr_in(then, tail);
        self.next_slot = mark;
        let end_j = (!tail).then(|| self.jump(Op::Jump));
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

    /// `Op::JumpNeIntLC` / `Op::JumpGeIntLC` for `local {==,<} const`, with the
    /// *inverted* sense: the fused op jumps when the source condition is false,
    /// exactly like the `JumpIfFalse` it replaces. Returns the label to patch.
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

    /// The longest suffix of the pure-`Let` run starting at `e` whose binds are
    /// exactly, and in the same order as, a subsequence of the operands of the
    /// consumer that follows the run. Those binds are [`Def`]s: the emitter
    /// skips their slots and recomputes them at their operand position.
    ///
    /// Order-preservation is the whole safety argument. Each bind is used once,
    /// in the consumer, so no bind in the run feeds another; hoisting them into
    /// the operand pushes keeps their relative order, and the only instructions
    /// that overtake them are `PushLocal`/`PushConst`, which have no effects.
    /// Anything that fails the check — a `Ctor`/`Call` in the run, a bind read
    /// twice, operands permuted relative to the definitions — trims the run
    /// (possibly to nothing) and those `Let`s emit their `StoreLocal` as usual.
    fn plan_run<'e>(&self, e: &'e CoreExpr) -> Option<(Vec<Def<'e>>, Consumer<'e>)> {
        let mut run: Vec<Def<'e>> = Vec::new();
        let mut cur = e;
        while let CoreExpr::Let { bind, rhs, body } = cur {
            // A const-aliased `Let` emits nothing, so it neither joins the run
            // nor ends it.
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
        // Walk the run backwards, matching each bind against a strictly earlier
        // operand position than the one before it; the first miss ends the
        // suffix. A surviving prefix means this `Let` is not hoistable — emit it
        // normally and re-plan from the next one, which then starts at `start`.
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
    /// lowers to its `Tail*` opcode (which is itself the return); every other
    /// atom is followed by `Ret`. `defs` are the operand definitions inlined
    /// into this atom's operand pushes (see [`Self::plan_run`]).
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
                // `Print` consumes and pushes nothing; supply the `()` here so
                // the enclosing StoreLocal / Ret has a value.
                if *prim == Op::Print {
                    self.push(op(Op::PushNil));
                }
                // `BinReadUtf8` pushes two values (codepoint, nbits); Core sees
                // it as a single `(Int, Int)` tuple so `Let` binds one local.
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

    /// `moved` is the set of last-use args whose frame reference must be
    /// released *between* the arg pushes and the call — so the callee, not
    /// this frame, is the sole owner and its Perceus `Drop` sees rc==1. See
    /// [`peel_call_arg_drops`].
    fn emit_call(
        &mut self,
        callee: Callee,
        args: &[LocalId],
        moved: &[LocalId],
        tail: bool,
        defs: &[Def],
    ) {
        self.push_args(args, defs);
        // Push a dynamic callee before the moved-drops so its own last-use
        // `Drop` (peeled alongside the args') releases the slot without
        // touching the already-stacked closure word.
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
    }

    /// Push the four-constant construct header + payload fields, then
    /// `MakeEnumPayload{a=reuse?, b=arity, operand=prehash}`. When Perceus
    /// paired the constructor with a dropped cell (`reuse = Some(slot)`), an
    /// `Op::Reuse slot` sits between the payloads and the make so `a=1` and
    /// the VM overwrites the cell in place.
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

    // ── match ─────────────────────────────────────────────────────────────

    fn emit_match(&mut self, scrut: LocalId, arms: &[(CorePat, CoreExpr)], tail: bool) {
        if let Some((count, tags)) = self.switch_plan(arms) {
            self.emit_switch(scrut, arms, count, &tags, tail);
        } else {
            self.emit_ladder(scrut, arms, tail);
        }
    }

    /// A match lowers to `SwitchTag` when every arm is a `Ctor` head of the
    /// same enum, each variant appears exactly once, and the enum is
    /// switch-eligible per [`EmitCtx::switch_variant_count`]. Payload binds
    /// in Core are always irrefutable, so no fail edge exists.
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
        // The table sits immediately after the `SwitchTag` itself; like every
        // other target this offset is function-relative.
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
            // A tail arm ends in `Ret`/`TailCall*`; a merge jump after it
            // would be unreachable.
            if !tail {
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
            // A tail arm ends in `Ret`/`TailCall*`; a merge jump after it
            // would be unreachable.
            if !tail {
                ends.push(self.jump(Op::Jump));
            }
            if let Some(f) = fail {
                self.patch(f);
            }
            self.next_slot = mark;
        }
        // Match fell through: exhaustiveness bug — halt rather than propagate
        // nil. Unreachable when the source match was exhaustive (checked at
        // lowering); a fallthrough here means a checker or lowering defect,
        // and stopping the VM beats returning a nil that flows onward.
        self.push(op(Op::Halt));
        self.patch_all(&ends);
    }

    /// `PushLocal scrut; UnwrapEnum; StoreLocal fN-1; …; StoreLocal f0` —
    /// spill the payload words into fresh slots for `fields`. Used by both
    /// the `SwitchTag` fast path and the `MatchEnum` ladder after the tag has
    /// been proven. Perceus-inserted `Drop`/`Reuse` for the scrutinee cell
    /// live in the arm body itself (a Core→Core insertion), so — unlike the
    /// old AST emitter — this helper does not emit them.
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
        // The table's own entries are relative too, and land on distinct arm
        // bodies past the table itself — each arm here is `PushConst; Ret`.
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
    /// opcodes and every non-jump operand are untouched, so a block emitted
    /// today at base 0 and relocated is byte-identical to what the old
    /// `emit(f, base)` produced, whose jump operands were larger by `base`.
    #[test]
    fn relocate_shifts_only_jump_operands() {
        const B: i32 = 100;

        // A `SwitchTag` fixture: table base + raw table entries + arm-end
        // `Jump`s, i.e. every operand shape that must move.
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
        // The fused `*IntLC` jump is the one label `jump()` does not emit — it
        // carries `a`/`b` operands, so `emit_lc_jump` mints it directly.
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
