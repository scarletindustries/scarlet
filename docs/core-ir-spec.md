# Core IR: typed ANF between the compiler and the VM

This was the plan for Core IR, and the IR it describes is built. Some of the plan has since changed. The old bytecode VM is gone, so there's no `emit` to bytecode: the new VM (`docs/vm-design.md`) reads Core IR itself. The IR's types live in `crates/scarlet_ir/src/core_ir/`, and the passes over it live in `crates/scarlet_lower/`. The sections below say where the code now differs from the plan.

**Goal**: replace the single-pass compiler from AST to bytecode with `AST → Core → passes → backend`, where Core is typed A-normal form (ANF). In ANF every operand is a named local, so nested expressions become a chain of `Let`s. Every optimisation in `docs/type-directed-memory-design.md` runs as a pass from Core to Core. Types are erased exactly once, when the backend reads Core.

## Why we need it

See `docs/type-directed-memory-design.md`. Phase 2 put Perceus on the AST walker and made `bench_typed` slower. That's because a forward walk over the AST can't know where a value is last used, so it hedged by leaving `Nop` holes at every read. In ANF, finding the last use is a simple linear scan. Koka, Lean (LCNF), Swift (SIL) and OCaml (Lambda) all run Perceus, ARC or mode inference on an IR of this shape.

## The IR

This is the shape today, cut down (`crates/scarlet_ir/src/core_ir/mod.rs`):

```rust
pub struct CoreFn {
    pub params: Vec<CoreBind>,
    pub body: CoreExpr,
    pub ret_ty: RTy,
}

pub struct CoreBind {
    pub id: LocalId,
    pub ty: RTy,                      // indexes the elaborator's ResolvedPool
    pub global: Option<GlobalSlot>,   // a toplevel decl's global slot
}

pub enum CoreExpr {
    Let { bind: CoreBind, rhs: Atom, body: Box<CoreExpr> },
    LetJoin { bind: CoreBind, join: Box<CoreExpr>, body: Box<CoreExpr> },
    LetCont { id: JoinId, cont: Box<CoreExpr>, body: Box<CoreExpr> },
    Drop { local: LocalId, shape: Option<ReuseShape>, body: Box<CoreExpr> },
    Match { scrut: LocalId, arms: Vec<(CorePat, CoreExpr)>, ty: RTy },
    If { cond: LocalId, then: Box<CoreExpr>, els: Box<CoreExpr>, ty: RTy },
    Tail(Atom),                       // return a value, or tail-call
    Goto(JoinId),
}

pub enum Atom {
    Local(LocalId),
    Const(ConstId),
    Load(Load),
    Nil,
    Bool(bool),
    Ctor { variant: VariantRef, fields: Vec<LocalId>, reuse: Option<LocalId> },
    PrimOp { op: PrimOp, args: Vec<LocalId> },
    Intrinsic { intrinsic: Intrinsic, args: Vec<LocalId> },
    Closure { func_idx: FuncIdx, captures: Vec<LocalId> },
    Call { callee: Callee, args: Vec<LocalId> },
}

pub enum Callee { Known(FuncIdx), Self_, Local(LocalId) }
```

Every operand is a `LocalId`, every intermediate value is bound by a `Let`, and every bind has a type. A function's name travels next to it on `LoweredFn`, not on `CoreFn`.

The plan also gave each bind a `region` (frame, process or immortal) for mode inference and an `alias` (unique or shared) for Perceus. Neither was built. Perceus records what it decides in the IR instead, as `Drop` nodes and as the `reuse` on a `Ctor`.

## Pipeline

1. **`lower(&TypedProgram) → CoreProgram`** (`crates/scarlet_lower/src/lower.rs`). This walks the elaborator's typed IR and builds ANF. Each subexpression becomes a `Let`, so the evaluation order that was implicit in the tree becomes explicit `Let` nesting. The plan had `lower` walk the typechecked AST and resolve each type through the inference engine. `docs/typed-ir-spec.md` explains why it takes the typed IR instead, with every type already resolved.
2. **`immediates::rewrite`** turns the prelude's `True`, `False` and `Nil` constructors into `Atom::Bool` and `Atom::Nil`, and a `match` on a Bool into an `If`. This pass wasn't in the plan.
3. **`perceus(pool, f) → CoreFn`** (`crates/scarlet_lower/src/perceus.rs`) runs Koka's algorithm on Core. A backward linear scan finds each last use and puts a `Drop` there. Then it pairs each drop with a later constructor of the same shape, so the constructor can reuse the cell. It is frame-limited, as in the ICFP'22 paper, so a pairing never crosses a `Call`.
4. **The VM** loads the result and turns each function into register instructions once, when the program loads. The plan had an `emit` step to stack bytecode here instead.

Later passes can slot in between `lower` and the backend: mode inference to set regions, a `simplify` pass for constant folding and dead-code removal, and so on.

## Constraints the plan set

- **Keep behaviour first.** `lower` and `emit` without Perceus had to reproduce the old bytecode's behaviour, with every test passing and `bench_typed` no slower than Phase 1. Only then would the Perceus pass go in.
- Phase 2's VM opcodes (`Op::Drop`, `Op::Reuse`, `is_unique`, `reuse_or_alloc` and the reuse path in `MakeEnum`) were the target, so they stayed. Phase 2's analysis in the AST compiler (`last_use`, `reuse_candidates`, `slot_tys` and the `Nop` holes) was deleted.
- Core is per function. A `CoreProgram` holds the functions and the module's toplevel.
- Reuse can carry across loop turns. On a tail-recursive function, Perceus may pair a `Drop` at the end of the body with a `Ctor` at the start, because a self tail call keeps the same frame. So the VM must not free those cells on a self tail call.
- Core has a printer (`impl Display for CoreExpr`) for debugging and for golden tests (`crates/scarlet/tests/core_ir.rs`, which snapshots Core for a few programs).
- Once Perceus was on, `examples/bench_typed.sca` had to be no slower than Phase 1, and `dot_loop` had to show real reuse, with far fewer allocations than its two million.

## Prior art to read

Koka's `core/core.kk` and `backend/c/perceus.kk`, Lean's `Lean/Compiler/LCNF/*`, and the algorithm in Fig. 5 of the ICFP'22 frame-limited reuse paper.

## Out of scope

Mode inference (Phase 3), process arenas (Phase 4) and lowering to registers were left for later passes over the same IR. Core is what all of them build on.
