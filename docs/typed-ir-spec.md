# Typed IR: make `lower` total by construction

This is the plan the typed IR was built from, and it's done. The checker builds a typed tree, `scarlet_elaborate` turns it into a `TypedProgram`, and `lower(p: &TypedProgram) -> CoreProgram` takes no `&mut` and can't fail. `LowerError` and `DiagnosticCode::InternalError` are gone. The parts about bytecode (`emit`, `link`, jump operands and `peephole.rs`) went with the old VM, because the new VM reads Core IR itself (`docs/vm-design.md`). We've kept the plan as a record of why the design looks the way it does.

**Goal**: correctness above all. Slower compiles are a fair price, and we can win the time back later. The interpreter must not get slower.

## The thesis

Every compiler bug found on 2026-07-09 had the same shape: **`lower` could change things it should only have read, and it worked out again facts the typechecker had already found.**

| bug | what `lower` re-derived or mutated | symptom |
|---|---|---|
| stale `base` | appended an eta-wrapper to `program.code` mid-lower | jumps land inside the wrapper; VM index-out-of-bounds |
| invented types | 22 × `fresh_var()` instead of the typechecker's `Ty` | `ctor_field` → `None` → panic; `is_heap` → false → no `Drop`; `resolved_prim` → `None` → dynamic opcode |
| `PushGlobal 219` | `operand: i32` means slot *or* addr *or* const-id *or* func-idx | read an unrelated temp |
| `internal_error` | exists only because the three above can fail | a runtime apology for an unencoded invariant |

The typechecker already knows every one of these facts. It can't typecheck `u.id` without finding the field's index, or `Some(x)` without finding the variant. It works them out, throws them away, and then `lower` guesses again.

**The stopgap fix (`8e5d5c7`) proved the point by bringing the same kind of bug back.** It passed types along in a `HashMap<Span, Ty>`. A `Span` is `{start_line, start_column, end_line, end_column}`, with no file, no offset and no identity. That had two confirmed effects:

1. The REPL scanned each entry separately and replayed the parsed nodes, so every retained entry's spans restarted at line 1. `const a = 'x' + 'y'` then `const b = 100 + 200` collide on `(1,11)-(1,20)`; last write wins; `specialize_binop` picks `AddInt`; the VM runs an integer add over two heap strings. Release printed `0`, debug tripped `as_int_typed`'s `debug_assert`. `d9a1add` fixed it by replaying the *source* instead. That removed the only thing making duplicate spans at the time, but not the chance of them.
2. The justification comment claims "a body's walk writes every span in it immediately before that body is lowered." That's true for `fn` bodies and **false for the module toplevel**. `analyse_module` walks the whole toplevel, and then `lower_body("__main__")` reads every span back, so all the writes come before all the reads. Nothing made duplicate toplevel spans then, but an `--eval` prelude, an LSP scratch buffer or a harness that joins programs together would have, silently.

The bug is a side table keyed on something that isn't an identity. If `TypedExpr` carries `ty` as a **field**, the table and its key both go.

**The fix is for the typechecker to emit a typed IR. `lower` reads it and can't fail.**

This is how Rust does it: `HIR → typeck → THIR → MIR`. Our `ast::Expression` was HIR pretending to be THIR.

## Target shape

```
parse → ast::Expression
      → typecheck/elaborate → TypedProgram { fns: Vec<TypedFn>, consts, .. }
      → lower  (TOTAL: no Result, no engine, no Program)   → CoreProgram
      → perceus (Core → Core)
      → emit   (Core → EmittedFn, jumps are Labels)
      → link   (resolves Labels against the real insertion point)
```

### `TypedExpr` resolves every fact, so nothing works one out again

- `ty: Ty` is a **field on every node**, already resolved with `find()`, not something to look up.
- `Field { recv, idx: FieldIdx, ty }`: the elaborator worked out `idx`, so `lower` has no `ctor_field` call.
- `Ctor { variant: VariantIdx, args }`: the labelled arguments are already in declared order.
- `Call { callee: Callee, args }`, where `Callee = Known(FuncIdx) | SelfRec | Local(LocalId) | Builtin(Op)`: names are already resolved.
- `Match { scrut, arms }`: variant indices are resolved and exhaustiveness is already proven.
- Each name is resolved to a `VarLoad`, so `lower` has no `resolve_name`.

### Eta-wrappers stop being a side effect

At the time, `LowerCtx::eta_wrapper` wrote a new function into `program.code` **during** lowering. That's what miscompiled.

Instead, **the elaborator emits them as ordinary `TypedFn`s.** A constructor or built-in used as a value elaborates to a reference to a generated `TypedFn` that's already in `TypedProgram.fns`. `lower` sees a plain function reference. Nothing appends to anything mid-pass.

### Resolved types live in a separate arena where `Var` can't exist

Everything else follows from this.

At the time, `pub type Ty = u32` was a bare index into `InferEngine.nodes`, shared by every phase. That's why you couldn't tell `fresh_var()`'s output from a real type: they had the same type, the same arena and the same `find()`.

```rust
// inference world: mutable, may hold unsolved variables
pub type Ty = u32;                 // indexes InferEngine.nodes
enum TypeNode { Var(i32), Bound(u32), Con{..}, Fun{..}, Tuple{..} }

// post-inference world: immutable, unsolved state is UNREPRESENTABLE
pub struct RTy(u32);               // indexes ResolvedPool
enum ResolvedNode { Bound(u32), Con{..}, Fun{..}, Tuple{..} }   // no Var arm
```

There's one bridge between the two, and only one:

```rust
fn zonk(eng: &InferEngine, t: Ty) -> Result<RTy, UnsolvedVar>
```

The elaborator calls `zonk` while it builds `TypedExpr`, and everything after it works in `RTy`. `fresh_var()` still exists and still returns a `Ty`, **but nothing downstream accepts one.** That isn't a lint or a convention. No function there takes a `Ty` at all, and that's the point. (Today the bridge is `zonk_or_opaque` in `crates/scarlet_elaborate/src/zonk.rs`.)

Keep `Bound` (a rigid quantified variable) and drop `Var` (an unsolved inference variable). Nothing showed the difference between them, and that's what let 22 bugs hide. `fn id(x) { x }` is *genuinely* polymorphic and must dispatch at run time, but a `Var` that survives inference is *always* a compiler bug. The old `lower` couldn't tell them apart.

Code that reads types then handles every case, and its answers are decisions rather than accidents:

```rust
fn is_heap(t: RTy) -> bool {
    match node(t) {
        Con(id, _)          => !is_prim(id),
        Tuple{..} | Fun{..} => true,
        Bound(_)            => false,   // polymorphic: must stay dynamic
    }                                    // no Var arm to silently mis-answer
}
```

This codebase has done it before. `types: phantom-tag ArenaSlice by pool so cross-pool index is a type error` made `ArenaSlice<P>` carry which pool it indexes. `Ty` was the index that never got the same treatment, and it was the one that mattered.

### `lower`'s signature enforces it

```rust
pub fn lower(p: &TypedProgram) -> CoreProgram   // no &mut, no InferEngine, no Program
```

- No `&mut InferEngine` ⇒ `fresh_var()` is unreachable *and* its result type is unusable. The 22 sites cannot come back.
- No `&mut Program` ⇒ appending mid-pass does not compile. The miscompile cannot come back.
- Input is `TypedProgram` (carrying `RTy`, resolved field/variant indices, resolved callees) ⇒ there is no type to fail to find and no index to fail to resolve. **`LowerError` becomes uninhabited; delete it and `internal_error` and `DiagnosticCode::InternalError` with it.**

If any of these three still needs a way out, that's a finding. Report it, and don't widen the signature.

### Addresses can no longer be written

`emit` produces `Target::Label(LabelId)` and resolves every label against the function's own code vector. It has no `base` parameter.

**Then no absolute address is ever built, because jump operands become relative to the function.** The VM already converted them back and forth for no reason:

```rust
let addr = code_start + ip;          // exec.rs:388, 482 — every fetch
ip = instr.operand - code_start;     // exec.rs:148, 256, 263, 604 — every jump
```

It stores an absolute operand, subtracts `code_start` to get back an `ip` relative to the function, then adds `code_start` again on the next fetch. The absolute form exists only because `emit` was given a `base` and added it in. That *is* the miscompile, because `base` was read before `lower` appended an eta wrapper.

So it becomes `ip = instr.operand;`. Four subtractions leave the interpreter's hot path (report the bench, but don't treat it as the reason). A function's code no longer depends on where it sits, so appending it anywhere is correct by construction. `link` then only has to append `out.code` and record `Function.code_start`, with no patching pass, and `fn_body_start` is deleted.

This is stronger than resolving addresses carefully in one place, because an absolute address is never *written* at all.

It's safe because `CoreExpr`'s control flow is structured (`If`, `Match`, `Tail`), so every jump `emit` produces stays inside its function. Calls take a `FuncIdx`, not an address.

Two things to verify rather than assume:
1. `Function.code_start` must still be recorded, because it's how the VM enters a function. The appender writes it, since it knows where the code went.
2. `crates/scarlet_core/src/bytecode/peephole.rs` is the other consumer of jump operands. Its module comment (line 13) documents the round-trip verbatim, and line 28 does `let t = instr.operand as usize;` to index the code slice. It already works inside one function's code, so relative targets make it simpler. Update that comment.

If some jump really does cross a function boundary, that's a real finding. Report it and fall back to `link` resolving absolute addresses, and name the jump that forced it.

### Each kind of operand gets its own type

`Instruction.operand: i32` meant a jump target, a local slot, a constant id, a function index or an arity, depending on the opcode. Introduce `CodeAddr`, `LocalSlot`, `ConstId`, `FuncIdx`, `Arity`. Consider `typed-index-collections`' `TiVec<CodeAddr, Instruction>` so indexing code with a `LocalSlot` does not compile. Scarlet already did this once, for `TypeId`.

The wire format stays a packed `i32`. The new types exist only above the encoding.

## Phases that keep the tree green at every step

Size doesn't matter, but **a diff nobody can review does.** The last two times this compiler was rewritten in one go, it shipped a miscompile (the stale `base`) and 22 invented types. Adversarial review found both *afterwards*, and the agents that wrote them didn't. So the phases below aren't a smaller plan. They're the same plan, ordered so the build is green and the tests pass after each one, and so each can be reviewed on its own.

Each phase is its own commit. Don't start the next until the last one is green.

**Every phase must be part of where we end up, never a repair of something the end state deletes.** Test each one by asking *does this code still exist after T5?* If it doesn't, don't write it.

An earlier draft of this spec failed that test. Its T1 gave `ast::Expression` a `NodeId` and keyed the `HashMap<Span, Ty>` side table on that instead, to fix the span collisions behind the REPL's wrong answer. But `TypedExpr` carries `ty` as a **field**, so T5 deletes the table and there's no key left to fix. T1's only purpose was to make a doomed data structure correct, so we removed it.

Each phase that's left *is* part of T5's end state. T2 and T3 are pieces of T5's signature, and T4 stands on its own and stays.

| # | phase | eliminates | proof it worked |
|---|---|---|---|
| **T2** | `LowerCtx` loses `fn engine(&mut self) -> &mut InferEngine`. Types reach `lower` only as data. | `fresh_var()` inside `lower` | `lower.rs` cannot name `fresh_var`; it does not compile if you try |
| **T3** | `LowerCtx` loses `&mut Program`. `eta_wrapper` becomes a *request* the caller materialises. | the eta-wrapper jump miscompile | `type_semantics.rs::a_branch_after_an_eta_wrapper_jumps_to_the_right_place` green, and `lower` cannot reach `program.code` |
| **T4** | Jump operands become function-relative; `emit` resolves its own labels; `fn_body_start` is deleted. | stale `base`, and absolute addresses entirely | no absolute address is constructed anywhere |
| **T5** | `RTy` + `ResolvedNode` (no `Var` arm) + `zonk`. `TypedExpr` carries `ty: RTy` and resolved field/variant/callee indices. `lower(p: &TypedProgram) -> CoreProgram`, total. | `LowerError`, `internal_error`, `DiagnosticCode::InternalError`, and the `Span`-keyed type table | those identifiers do not exist |

T2 to T4 are each worth having alone, and each removes a kind of bug for good. T5 is where they meet. After T4, all it adds is that `lower` can't fail, and by then the signature changes that make that possible have already landed.

The order is only there to keep each diff reviewable. If a phase can be folded into T5 *without* writing code that T5 would delete, fold it.

If at T5 it turns out `lower` still needs a way out, that's the most interesting finding of the whole job. Report it rather than widening the signature.

## Constraints on the outcome

- **Correctness over compile speed.** A slower build is fine and expected. Say how much slower.
- **The interpreter must not regress.** `examples/bench_typed.sca`, `examples/bench_map.sca`, and an amplified `fib(36)`+`count(8e7)` must hold. This work only touches the compiler, so a change at run time is a bug, unless getting back lost typed opcodes makes it *faster*. That's a win worth reporting.
- Recovering the typed opcodes that `fresh_var()` was suppressing may change the emitted instruction mix. That's the point. Report the opcode counts before and after for a function whose types were inferred.
- **Every guard must be a type, an exhaustive match or a test of behaviour.** Never a test that greps source text, and never a hardcoded list of symbol names. An earlier attempt added `const COLD_INLINE_NEVER_HANDLERS: &[&str]` and a scanner over `.rs` files, and it was reverted.
- Don't weaken, skip or delete a test. Don't add a cargo feature. Don't bypass git hooks.

## Regressions this must keep fixed

Both are regression tests. They must still pass, and they must pass *because the shape makes the bug impossible*, not because someone added a check.

```scarlet
// eta-wrapper jump miscompile — was a VM index-out-of-bounds
import scarlet/array
type W { W(v Int) }
fn pick(xs Array(Int)) Int {
	ws = array::map(xs, W)
	if array::length(ws) > 2 { 111 } else { 222 }
}
println(pick([1]))        // 222
println(pick([1, 2, 3]))  // 111
```
```scarlet
// field access on an inferred match binding — was a lower panic
type User { User(id Int name String) }
fn f() Int { match Some(User(7, 'Scarlet')) { None -> 0  Some(u) -> u.id } }
println(f())              // 7
```

## Acceptance

1. `lower`'s signature takes no `&mut` and returns no `Result`. `LowerError` and `DiagnosticCode::InternalError` are deleted.
2. `grep -c 'fresh_var()' crates/scarlet_lower/src/lower.rs` gives 0, because `lower` has no engine. That follows from the signature. It isn't a rule to satisfy.
3. No `emit` path produces an absolute address, and `link` is the only place that writes addresses.
4. Every test passes, including both regressions above.
5. `cargo clippy --all-targets` → 0 errors, 0 warnings. `cargo fmt --check` clean.
6. The interpreter's benchmarks hold or improve. Measure the two binaries in turn, taking the lowest `user` time of at least 7 runs. Build both before benching, and never bench while something is compiling. The machine's load swings a lot, so a fixed time limit means nothing.

## Sequencing

The typed IR had to come before the planned work on effects and compile-time evaluation, which assumed a compiler that doesn't change its own state halfway through a pass. That spec, `docs/effects-comptime-spec.md`, is no longer in the repo.

## Risks, ranked

1. **The elaborator rewrites the typecheck walk.** `compile_expr` typechecked, drove name resolution and recorded `closure_info`, all at once. Splitting checking from building `TypedExpr` is most of the work.
2. **`IncrementalSession` caches `ModuleInterface` and `ModuleReferences`, keyed on watermarks.** A new IR between typechecking and `lower` mustn't break how the cache is invalidated. Check `bytecode/session.rs`.
3. **Spans.** `closure_info` is keyed by `Span`. If `TypedExpr` nodes carry their own identity, that table can go. Check that nothing else depends on keying by span.
4. **Memory.** A second full IR for a large program doubles the peak memory the AST takes. That's fine, but measure it.
5. **The stdlib was precompiled at build time** (`precompile_stdlib()` in `crates/scarlet/build.rs`). The new pipeline had to work there too, and `build.rs` depended only on `scarlet_core`.
