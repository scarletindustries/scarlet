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
    Open { cell: LocalId, variant: VariantRef, fields: Vec<(u16, CoreBind)>, body: Box<CoreExpr> },
    Switch { scrut: LocalId, cases: Cases, default: Option<Box<CoreExpr>> },
    If { cond: LocalId, then: Box<CoreExpr>, els: Box<CoreExpr> },
    Probe { op: ProbeOp, args: Vec<LocalId>, binds: Vec<Option<CoreBind>>,
            hit: Box<CoreExpr>, miss: Box<CoreExpr> },
    Ret(LocalId),
    TailCall { callee: Callee, args: Vec<LocalId> },
    Jump { to: JoinId, args: Vec<LocalId> },
    NoArm,
}

pub enum Cases {                      // each arm with the keys that lead to it
    Variant { ty: TypeId, arms: Arms<u16> },
    Int(Arms<i64>),
    Float(Arms<FloatKey>),            // 0.0 and -0.0 are one key
    Text(Arms<ConstId>),
}

pub enum ProbeOp {
    ArrayGet,
    ArraySlice,
    ArraySet,
    ArrayUncons,                      // the first element, and the array of the others
    ArraySplit { len: u32, more: bool, from: u32 },
    MapGet,
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

`Join` is the one place control comes together. `body` runs, and a `Jump` in it to `id` gives `params` their values and goes on with `cont`. `cont` reads what was bound before the join, sees nothing bound in `body`, and cannot jump to `id`, so every jump goes forward and there are no loops. What is done for several ways at once is a `Join`:

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
- What a `match` goes on with where nothing in front has fitted is the continuation of a join with no parameters, when several ways lead there.
- The body of an arm that several ways through the tree lead to is the continuation of one that takes the names the body reads, so it is lowered once.

A `Switch` goes to the arm that has the value's key, and to the default when none has. One with no default has an arm for every value there can be, because the checker saw to it that a match is exhaustive. Where a way through a tree is one no value takes, it ends in `NoArm`, which stops the run. No value's fields are read by a `Switch`: a field is a `Let` of a `Field`, on the ways that use it. `Open` reads fields out of a cell that is known to be of `variant` and gives the cell up, all in one. A field it does not name is given up with the cell.

A `Probe` asks for something that may not be there: an element of an array, a slice of one, an array with another element, some of the elements of an array of some length, or what a map has for a key. Where it is there, `binds` has a place for each value that the op finds, and `hit` runs. Where it is not, `miss` runs, with nothing bound and every operand as it was. A place may be none, for a value that nothing reads. The result of `array::put(%0, %1, %2)` is

```
join j0(%4:0) after
  probe ArraySet(%0, %1, %2)
    | hit(%3:0) ->
      drop %0
      jump j0(%3)
    | miss ->
      jump j0(%0)
```

Nothing else makes one of these operations: they are no `PrimOp`s, and a backend refuses an `Intrinsic` of their names. Another partial built-in is one more `ProbeOp`.

`ArrayUncons` and `ArraySplit` are what an array pattern is, and find several values. `ArrayUncons` is for `[h, ..t]`, and has a place for the first element and one for the array of the others. `ArraySplit` is for every other pattern, and for an array that has `len` elements, or `len` or more when it may have `more`. Its places are for the elements from `from` on, and, when there may be more, the last is for the array of what is behind the first `len`. A miss is an array of another length. One that cannot miss, because the length is known, has `NoArm` for its miss. `array::fold` is

```
probe ArrayUncons(%0)
  | hit(%3:2, %4:1) ->
    drop %0
    let %5:3 = call %2(%1, %3)
    tail call self(%4, %5, %2)
  | miss ->
    drop %0
    drop %2
    ret %1
```

`CoreExpr::always_reached` gives the joins that every way through their body comes to. A value's join is one, unless a branch stops the run everywhere. What comes before such a join can be moved past it, and what is live after it is live all through its body.

The end of a spine is a `Ret` of a local, a `TailCall`, a `Jump` or a branch, so a constructor in tail position is a `Let` like any other. `SetGlobal` puts a value in a module-level slot, only in a toplevel and once for each slot. The slot has a reference of its own. Each local is bound in one place and each join declared in one.

Every operand is a `LocalId`, every intermediate value is bound by a `Let`, and every bind has a type. A type is an `RTy`, an index into `Program::pool`, and `ResolvedPool::repr` says whether its values can be heap cells (`Repr`): a Float and a type whose constructors have no fields are a `Word`, and every other type is `Counted`. The compiler gives each named type its `Repr` when it gives it a `TypeId`, so no type in the pool is without one. A function's name travels next to it on `LoweredFn`, not on `CoreFn`.

The plan also gave each bind a `region` (frame, process or immortal) for mode inference and an `alias` (unique or shared) for Perceus. Neither was built. Perceus records what it decides in the IR instead, as `Drop` nodes.

## Pipeline

1. **`lower`** (`crates/scarlet_lower/src/lower.rs`). This walks the elaborator's typed IR and builds ANF. Each subexpression becomes a `Let`, so the evaluation order that was implicit in the tree becomes explicit `Let` nesting. A `match` becomes a decision tree first ("Matches" below). The plan had `lower` walk the typechecked AST and resolve each type through the inference engine. `docs/typed-ir-spec.md` explains why it takes the typed IR instead, with every type already resolved.
2. **`immediates::rewrite`** turns the prelude's `True`, `False` and `Nil` constructors into `Atom::Bool` and `Atom::Nil`, and a `match` on a Bool into an `If`. This pass wasn't in the plan.
3. **`perceus(pool, locals, f)`** (`crates/scarlet_lower/src/perceus.rs`) runs Koka's algorithm on Core. A backward linear scan finds each last use and puts a `Drop` there, inside the body of a join and out of it, and for a value whose type is a variable as for any other. A value whose type is a `Repr::Word` gets none, and neither does what a `Switch` found to be a constructor with no fields, because neither can be a heap cell. Where a run of `Field` reads takes apart a value whose constructor a `Switch` around it has found, and the value's drop comes straight behind them, the reads and the drop are one `Open`. An Int and a String get none either (`ResolvedPool::gets_drop`). A `Jump` hands over its arguments and what its continuation owns, so that is dropped on neither side of it, and everything else is dropped in front of it. A `Probe` takes nothing, so each of its arms owns its operands and drops those it does not read again, and a place for what nothing reads becomes none. A `Ret` gives up the value it returns and a `TailCall` what it passes, and everything else is dropped before. `scarlet_lower`'s `perceus/tests/ownership.rs` checks all of that on every way through a function, for functions made at random and for every function of every program in the repository. Perceus also paired each drop with a later constructor of the same shape, so the constructor could reuse the cell, until step 13 of `docs/perf-plan.md`. The VM takes a cell apart in one instruction instead, and the next constructor of its size gets the space (`docs/vm-design.md`, "Running code").
4. **`tail_args::schedule`** computes the arguments of each tail call in the order that lets each take the place of the parameter it replaces. It moves only constants and Int and Float operations. This pass wasn't in the plan.
5. **The VM** loads the result and turns each function into register instructions once, when the program loads. The plan had an `emit` step to stack bytecode here instead.

Later passes can slot in between `lower` and the backend: mode inference to set regions, a `simplify` pass for constant folding and dead-code removal, and so on.

## Matches

`crates/scarlet_lower/src/matching.rs` makes a decision tree of a `match`, and `lower/arms.rs` writes the tree as Core IR. An arm is a row of checks, each of which is a place in the matched value and a pattern that it has to fit. The tree asks what the first row still has to know, and every row that asks it too is split by the answer, so nothing is asked twice on one way. This is the algorithm of Jules Jacobs' "How to compile pattern matching", which Gleam uses too. A place is read on the ways that use it and no other, and a name that only a guard or a body reads is read there. Where everything that is wanted of a constructor is wanted straight behind its test, its fields are read together, so that Perceus can make an `Open` of them.

A `match` on a `Probe` is the probe, whose arms are those of the tree's first `Switch`, and what a hit finds is where the tree would read the field. The `Option` or the `Result` is made only on a way that reads it whole, and a tree that does not begin with a switch on it is written over a value that is made. So it is when the value has a name that only the match straight after it reads. Any other use of a probe makes the value in both arms, which jump to a join or return (`lower/probes.rs`).

What a program can see of a match keeps its order. A guard runs when all the checks of its row hold, and the rows below are tried when it has failed. A size in a `<<>>` pattern is an expression of any kind, so what is behind it in its row waits until it has been read, unless the size is made of constants, which nothing can tell the reading of. An or-pattern that binds names tries each alternative with the rest of its row and the guard. One that binds none goes on with the first alternative that fits, so its row's guard and sizes run once. A row is never two rows with one guard or one size between them.

A row that does not ask what a question answers goes on in every branch of it, and that is what can make a tree grow without bound. Each match has an allowance for such copies, in proportion to its size. Once that is spent, the rows in front of the first one that would have to be copied are a tree by themselves, and the rest is the continuation of a join, which every way that fits none of them jumps to. So a tree is larger than the match by a factor at the most, but for one thing. An or-pattern that binds names, in a row with a guard or with a size that is not made of constants, tries each alternative with everything behind it. Where it is plain that only one alternative can fit, the first that does is the only one tried, as far as the allowance does not go. Otherwise the row is a row for each alternative, with no allowance, and several such or-patterns beside each other are a row for each combination. No match in this repository comes near its allowance.

An array's length is a question like any other, and the probe that asks it finds something for the first row that fits. Where nothing is left to ask for that row, it finds every element that the row names, and its tail when the array is the matched value itself. Where an element has to be asked of next, it finds that one alone, because the others are for nothing when it does not fit. Where nothing shorter is left and a row that takes anything longer comes next, the longer is asked for first, so a walk of `[]` and `[h, ..t]` is one `ArrayUncons` a turn and nothing at its end. Where several exact lengths are asked for, the length is read once (`ArrayLen`), and each is a test of that number. What else of an array is needed is found by another probe, where it is first needed, up to eight elements at a time once one has been found, so that a long pattern does not need a register for each of its elements at once. A tail that a field or an element has to be asked of first is taken by a probe of its own, once nothing is left to ask, and behind everything else that the arm names.

A guard, or a size that is not made of constants, may fail, and then the rows behind it may read the array again. A tail that the guard does not read is then taken where it has held. Where nothing reads the array again it is taken in front of the guard, so that the array, and what only it holds, is given up before the guard runs. A tail that waited is taken in front of a later guard once nothing else of the array is read. And where all that is asked of an array behind a guard is its length, that is read in front of it. What is read behind a guard is looked for in the next sixteen rows. Where there are more than that, they may ask for a length.

The tests hold the tree against an interpreter of the patterns as they are written (`matching/tests/oracle.rs`), over matches made at random, a third of them on a probe, and over every match of every program in the repository, and count the guards and sizes that run and what they are given.

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
