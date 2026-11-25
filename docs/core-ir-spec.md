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
    pub ty: RTy,                      // indexes Program::pool
}

pub enum CoreExpr {
    Let { bind: CoreBind, rhs: Atom, body: Box<CoreExpr> },
    SetGlobal { slot: GlobalSlot, value: LocalId, body: Box<CoreExpr> },
    Join { id: JoinId, params: Vec<CoreBind>, cont: Box<CoreExpr>, body: Box<CoreExpr> },
    Drop { local: LocalId, body: Box<CoreExpr> },
    Match { scrut: LocalId, arms: Vec<(CorePat, CoreExpr)> },
    If { cond: LocalId, then: Box<CoreExpr>, els: Box<CoreExpr> },
    Ret(LocalId),
    TailCall { callee: Callee, args: Vec<LocalId> },
    Jump { to: JoinId, args: Vec<LocalId> },
}

pub enum Atom {
    Local(LocalId),
    Const(ConstId),
    Load(Load),
    Nil,
    Bool(bool),
    Ctor { variant: VariantRef, fields: Vec<LocalId> },
    Update { base: LocalId, variant: VariantRef, sets: Vec<(u16, LocalId)> },
    PrimOp { op: PrimOp, args: Vec<LocalId> },
    Intrinsic { intrinsic: Intrinsic, args: Vec<LocalId> },
    Closure { func_idx: FuncIdx, captures: Vec<LocalId> },
    Call { callee: Callee, args: Vec<LocalId> },
}

pub enum Callee { Known(FuncIdx), Self_, Local(LocalId) }
```

`Update` is `base`, a `variant`, with other values at the fields in `sets`. It is what a record update `R(..base, f: x)` becomes when holding `base` until `x` is computed cannot be seen and the variant of `base` is known (`scarlet_lower/src/update.rs`). Any other record update reads each field it keeps, straight after the base, and makes a new `Ctor`.

`Join` is the one place control comes together. `body` runs, and a `Jump` in it to `id` gives `params` their values and goes on with `cont`. `cont` reads what was bound before the join, sees nothing bound in `body`, and cannot jump to `id`, so every jump goes forward and there are no loops. Everything that used to be a node of its own is a `Join`:

- A branch whose value is used further on is the body of one with a parameter, and each of its ends jumps with the value. `x = if c { 1 } else { y }` is

```
join j0(%3:0) after
  if %0
    let %2:0 = c0
    jump j0(%2)
  else
    jump j0(%1)
```

  and what comes next is the continuation, which the printer puts level with the join, because it runs next.
- The arms after one that may fail are the continuation of a join with no parameters, and each failure in that arm jumps there.
- The body of an arm whose or-pattern binds names is the continuation of one that takes them, so it is lowered once. The rest of the pattern and the guard are lowered at each alternative, because a miss there tries the next alternative.

`CoreExpr::always_reached` gives the joins that every way through their body comes to. A value's join is one, unless a branch stops the run everywhere. What comes before such a join can be moved past it, and what is live after it is live all through its body.

The end of a spine is a `Ret` of a local, a `TailCall`, a `Jump` or a branch, so a constructor in tail position is a `Let` like any other. `SetGlobal` puts a value in a module-level slot, only in a toplevel and once for each slot. The slot has a reference of its own. Each local is bound in one place and each join declared in one.

Every operand is a `LocalId`, every intermediate value is bound by a `Let`, and every bind has a type. A type is an `RTy`, an index into `Program::pool`, and `ResolvedPool::repr` says whether its values can be heap cells (`Repr`): a Float and a type whose constructors have no fields are a `Word`, and every other type is `Counted`. The compiler gives each named type its `Repr` when it gives it a `TypeId`, so no type in the pool is without one. A function's name travels next to it on `LoweredFn`, not on `CoreFn`.

The plan also gave each bind a `region` (frame, process or immortal) for mode inference and an `alias` (unique or shared) for Perceus. Neither was built. Perceus records what it decides in the IR instead, as `Drop` nodes.

## Pipeline

1. **`lower(&TypedProgram) → CoreProgram`** (`crates/scarlet_lower/src/lower.rs`). This walks the elaborator's typed IR and builds ANF. Each subexpression becomes a `Let`, so the evaluation order that was implicit in the tree becomes explicit `Let` nesting. The plan had `lower` walk the typechecked AST and resolve each type through the inference engine. `docs/typed-ir-spec.md` explains why it takes the typed IR instead, with every type already resolved.
2. **`immediates::rewrite`** turns the prelude's `True`, `False` and `Nil` constructors into `Atom::Bool` and `Atom::Nil`, and a `match` on a Bool into an `If`. This pass wasn't in the plan.
3. **`perceus(pool, locals, f)`** (`crates/scarlet_lower/src/perceus.rs`) runs Koka's algorithm on Core. A backward linear scan finds each last use and puts a `Drop` there, inside the body of a join and out of it, and for a value whose type is a variable as for any other. A value whose type is a `Repr::Word` gets none, and neither does what a `match` arm found to be a constructor with no fields, because neither can be a heap cell. An Int and a String get none either (`ResolvedPool::gets_drop`). A `Jump` hands over its arguments and what its continuation owns, so that is dropped on neither side of it, and everything else is dropped in front of it. A `Ret` gives up the value it returns and a `TailCall` what it passes, and everything else is dropped before. `scarlet_lower`'s `perceus/tests/ownership.rs` checks all of that on every way through a function, for functions made at random and for every function of every program in the repository. Perceus also paired each drop with a later constructor of the same shape, so the constructor could reuse the cell, until step 13 of `docs/perf-plan.md`. The VM takes a cell apart in one instruction instead, and the next constructor of its size gets the space (`docs/vm-design.md`, "Running code").
4. **`tail_args::schedule`** computes the arguments of each tail call in the order that lets each take the place of the parameter it replaces. It moves only constants and Int and Float operations. This pass wasn't in the plan.
5. **The VM** loads the result and turns each function into register instructions once, when the program loads. The plan had an `emit` step to stack bytecode here instead.

Later passes can slot in between `lower` and the backend: mode inference to set regions, a `simplify` pass for constant folding and dead-code removal, and so on.

## Constraints the plan set

- **Keep behaviour first.** `lower` and `emit` without Perceus had to reproduce the old bytecode's behaviour, with every test passing and `bench_typed` no slower than Phase 1. Only then would the Perceus pass go in.
- Phase 2's VM opcodes (`Op::Drop`, `Op::Reuse`, `is_unique`, `reuse_or_alloc` and the reuse path in `MakeEnum`) were the target, so they stayed. Phase 2's analysis in the AST compiler (`last_use`, `reuse_candidates`, `slot_tys` and the `Nop` holes) was deleted.
- Core is per function. A `CoreProgram` holds the functions and the module's toplevel.
- Core has a printer (`impl Display for CoreExpr`) for debugging and for golden tests (`crates/scarlet/tests/core_ir.rs`, which snapshots Core for a few programs).
- Once Perceus was on, `examples/bench_typed.sca` had to be no slower than Phase 1, and `dot_loop` had to show real reuse, with far fewer allocations than its two million. The second no longer holds, because nothing is reused in place.

## Prior art to read

Koka's `core/core.kk` and `backend/c/perceus.kk`, Lean's `Lean/Compiler/LCNF/*`, and Fig. 5 of the ICFP'22 frame-limited reuse paper, for where drops go.

## Out of scope

Mode inference (Phase 3), process arenas (Phase 4) and lowering to registers were left for later passes over the same IR. Core is what all of them build on.
