# Core IR: typed ANF between AST and bytecode

**Goal**: replace the single-pass `AST → bytecode` compiler with `AST → Core → passes → bytecode`, where Core is typed A-Normal Form. Every optimization in `docs/type-directed-memory-design.md` runs as a Core→Core pass. Type erasure happens exactly once, at Core→bytecode.

## Why (see `docs/type-directed-memory-design.md`)

Phase 2 (Perceus on the AST walker) regressed bench_typed 0.61→0.77 because last-use is unknowable during a forward AST emit — it hedged with Nop holes on every read. ANF makes last-use a trivial linear scan. Koka, Lean (LCNF), Swift (SIL), OCaml (Lambda) all put Perceus/ARC/mode-inference on exactly this shape of IR.

## The IR (`crates/al_core/src/core_ir/mod.rs`)

```rust
pub struct CoreFn {
    pub name: StrId,
    pub params: Vec<CoreBind>,
    pub body: CoreExpr,
    pub ret_ty: Ty,
}

pub struct CoreBind {
    pub id: LocalId,        // dense u32, per-fn
    pub ty: Ty,             // fully-resolved from HM (find() applied)
    pub region: Region,     // filled by mode-inference pass; default Process
    pub alias: Alias,       // filled by Perceus pass; default Shared
}

pub enum CoreExpr {
    Let { bind: CoreBind, rhs: Atom, body: Box<CoreExpr> },
    Match { scrut: LocalId, arms: Vec<(CorePat, CoreExpr)>, ty: Ty },
    If { cond: LocalId, then: Box<CoreExpr>, els: Box<CoreExpr>, ty: Ty },
    Tail(Atom),             // tail position: return or tail-call
}

pub enum Atom {
    Local(LocalId),
    Const(ConstId),
    Ctor { variant: VariantRef, fields: Vec<LocalId> },
    PrimOp { op: PrimOp, args: Vec<LocalId> },
    Call { callee: Callee, args: Vec<LocalId> },
}

pub enum Callee { Known(FuncIdx), Self_, Local(LocalId) }
pub enum Region { Frame, Process, Immortal }   // ρ
pub enum Alias  { Unique, Shared }             // α
```

Every operand is a `LocalId`. Every intermediate is a `Let`. Types on every bind.

## Pipeline (`crates/al_core/src/bytecode/compiler.rs` → orchestrates)

1. **`lower(ast, engine) → CoreFn`** — walk typechecked AST, emit ANF. Each subexpression becomes a `Let`; the AST's implicit evaluation order becomes explicit `Let` nesting. Resolve every `Ty` via `engine.find()` at lowering time (types are stable post-inference).
2. **`perceus(core) → core`** — Koka's algorithm on Core: linear backward scan marks last-use, inserts `Drop`, pairs same-shape drops with dominated `Ctor` for reuse. Frame-limited (ICFP'22): pairing never crosses `Call`. Sets `alias` on each bind.
3. **`emit(core) → Vec<Instruction>`** — Core→bytecode. Straightforward: `Let{rhs=PrimOp}` → `PushLocal args; Op; StoreLocal`; `Let{rhs=Ctor}` → `MakeEnum` (with `a=1` reuse token if perceus paired it); `Match` → `SwitchTag` when exhaustive+resolved else `MatchEnum` ladder; `Call{Known}` → `CallKnown`. This replaces the current `compile_*` methods.

Later passes slot in at step 2: `mode_infer(core)` sets `region`; a `simplify(core)` pass does constant-fold/DCE; register allocation (when we do register-bytecode) is a Core→RegCore lowering.

## Constraints

- **Semantics-preserving milestone first**: `lower + emit` (no perceus pass) must reproduce today's bytecode behavior — all 653 tests pass, bench_typed ≈ 0.61s (Phase 1). Only THEN add the perceus pass.
- Phase 2's VM opcodes (`Op::Drop`, `Op::Reuse`, `is_unique`, `reuse_or_alloc`, `MakeEnum a=1` reuse path) are the target — keep them. Phase 2's compiler.rs analysis (`last_use`, `reuse_candidates`, `slot_tys`, Nop-hole reservation) is deleted.
- Core is per-function; a `CoreProgram` holds `Vec<CoreFn>` + constants + module toplevel.
- Loop-carried reuse: perceus pass on a tail-recursive `CoreFn` may pair a `Drop` at end-of-body with a `Ctor` at start-of-body (same frame after `TailCallSelf`); VM's `collapse_tail_frame` must NOT drain reuse slots for self-tail-calls.
- Printer: `impl Display for CoreExpr` for debugging + golden tests (`crates/al/tests/core_ir.rs` — snapshot Core for a few programs).
- Bench gate: after perceus pass is on, `examples/bench_typed.al` must be ≤ 0.61s (Phase 1) AND `dot_loop` must show measurable reuse (alloc counter << 2M).

## Prior art to consult

Koka `core/core.kk` and `backend/c/perceus.kk`; Lean `Lean/Compiler/LCNF/*`; the ICFP'22 frame-limited paper's Fig. 5 algorithm.

## Out of scope (later passes on the same IR)

Mode inference (Phase 3), process arenas (Phase 4), register-bytecode lowering — Core is the substrate for all of them.
