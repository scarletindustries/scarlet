# Typed IR: make `lower` total by construction

**Goal**: correctness above all. Slower compile times are an acceptable price; they can be recovered later. The interpreter's speed is *not* negotiable.

## The thesis

Every compiler bug found on 2026-07-09 has one shape: **`lower` was handed mutable access to things it should only have been allowed to read, and re-derived facts the typechecker had already established.**

| bug | what `lower` re-derived or mutated | symptom |
|---|---|---|
| stale `base` | appended an eta-wrapper to `program.code` mid-lower | jumps land inside the wrapper; VM index-out-of-bounds |
| invented types | 22 × `fresh_var()` instead of the typechecker's `Ty` | `ctor_field` → `None` → panic; `is_heap` → false → no `Drop`; `resolved_prim` → `None` → dynamic opcode |
| `PushGlobal 219` | `operand: i32` means slot *or* addr *or* const-id *or* func-idx | read an unrelated temp |
| `internal_error` | exists only because the three above can fail | a runtime apology for an unencoded invariant |

The typechecker already knows every one of these facts — it cannot typecheck `u.id` without resolving the field index, nor `Some(x)` without resolving the variant. It computes them, then throws them away, and `lower` guesses again.

**The interim fix (`8e5d5c7`) proved the point by reintroducing the class.** It threaded types via `HashMap<Span, Ty>`. `Span` is `{start_line, start_column, end_line, end_column}` — no file, no offset, no identity. Two confirmed consequences:

1. The REPL scanned each entry separately and replayed the parsed nodes, so every retained entry's spans restarted at line 1. `const a = 'x' + 'y'` then `const b = 100 + 200` collide on `(1,11)-(1,20)`; last write wins; `specialize_binop` picks `AddInt`; the VM runs an integer add over two heap strings. Release printed `0`, debug tripped `as_int_typed`'s `debug_assert`. Fixed in `d9a1add` by replaying *source* — which removes the only current producer of duplicate spans, not the possibility.
2. The justification comment claims "a body's walk writes every span in it immediately before that body is lowered." That is true for `fn` bodies and **false for the module toplevel**: `analyse_module` walks the entire toplevel, then `lower_body("__main__")` reads every span back. All writes precede all reads. Nothing produces duplicate toplevel spans today; an `--eval` prelude, an LSP scratch buffer, or a harness that concatenates programs would, silently.

A side table keyed on something that is not an identity is the bug. `TypedExpr` carrying `ty` as a **field** deletes the table and the key with it.

**Fix: the typechecker emits a typed IR. `lower` consumes it and cannot fail.**

This is Rust's `HIR → typeck → THIR → MIR`. Our `ast::Expression` is HIR pretending to be THIR.

## Target shape

```
parse → ast::Expression
      → typecheck/elaborate → TypedProgram { fns: Vec<TypedFn>, consts, .. }
      → lower  (TOTAL: no Result, no engine, no Program)   → CoreProgram
      → perceus (Core → Core)
      → emit   (Core → EmittedFn, jumps are Labels)
      → link   (resolves Labels against the real insertion point)
```

### `TypedExpr` — every fact resolved, none re-derivable

- `ty: Ty` is a **field on every node**, already `find()`-resolved. Not a lookup.
- `Field { recv, idx: FieldIdx, ty }` — the elaborator computed `idx`; there is no `ctor_field` call in `lower`.
- `Ctor { variant: VariantIdx, args }` — labels already reordered into declared order.
- `Call { callee: Callee, args }` where `Callee = Known(FuncIdx) | SelfRec | Local(LocalId) | Builtin(Op)` — name resolution is done.
- `Match { scrut, arms }` — variant indices resolved, exhaustiveness already proven.
- Names resolved to a `VarLoad`; no `resolve_name` in `lower`.

### Eta-wrappers stop being a side effect

Today `LowerCtx::eta_wrapper` synthesises a function into `program.code` **during** lowering. That is the miscompile.

Instead: **the elaborator emits them as ordinary `TypedFn`s.** A constructor or builtin used as a first-class value elaborates to a reference to a synthesized `TypedFn` that already exists in `TypedProgram.fns`. `lower` sees a plain function reference. Nothing appends to anything mid-pass.

### Resolved types are a different arena, in which `Var` is unrepresentable

This is the load-bearing mechanism; the rest follows from it.

Today `pub type Ty = u32` — a bare index into `InferEngine.nodes`, shared by every phase. That is why `fresh_var()`'s output is indistinguishable from a real type: same type, same arena, same `find()`.

```rust
// inference world: mutable, may hold unsolved variables
pub type Ty = u32;                 // indexes InferEngine.nodes
enum TypeNode { Var(i32), Bound(u32), Con{..}, Fun{..}, Tuple{..} }

// post-inference world: immutable, unsolved state is UNREPRESENTABLE
pub struct RTy(u32);               // indexes ResolvedPool
enum ResolvedNode { Bound(u32), Con{..}, Fun{..}, Tuple{..} }   // no Var arm
```

One bridge, and it is the only one:

```rust
fn zonk(eng: &InferEngine, t: Ty) -> Result<RTy, UnsolvedVar>
```

The elaborator calls `zonk` while building `TypedExpr`. Everything downstream traffics in `RTy`. `fresh_var()` still exists and still returns a `Ty` — **and nothing downstream will accept one.** It is not discouraged or linted; there is no function with that signature. That is the whole point.

Keep `Bound` (a rigid quantified variable) and drop `Var` (an unsolved inference variable). The distinction is currently invisible and is exactly what let 22 bugs hide: `fn id(x) { x }` is *honestly* polymorphic and must dispatch dynamically, while a surviving `Var` is *always* a compiler bug. Today `lower` cannot tell them apart.

Consumers then become total, and their answers become decisions rather than accidents:

```rust
fn is_heap(t: RTy) -> bool {
    match node(t) {
        Con(id, _)          => !is_prim(id),
        Tuple{..} | Fun{..} => true,
        Bound(_)            => false,   // polymorphic: must stay dynamic
    }                                    // no Var arm to silently mis-answer
}
```

Precedent in this codebase: `types: phantom-tag ArenaSlice by pool so cross-pool index is a type error` — `ArenaSlice<P>` already carries which pool it indexes. `Ty` is the index that never got the treatment, and it is the one that mattered.

### `lower`'s signature is the enforcement

```rust
pub fn lower(p: &TypedProgram) -> CoreProgram   // no &mut, no InferEngine, no Program
```

- No `&mut InferEngine` ⇒ `fresh_var()` is unreachable *and* its result type is unusable. The 22 sites cannot come back.
- No `&mut Program` ⇒ appending mid-pass does not compile. The miscompile cannot come back.
- Input is `TypedProgram` (carrying `RTy`, resolved field/variant indices, resolved callees) ⇒ there is no type to fail to find and no index to fail to resolve. **`LowerError` becomes uninhabited; delete it and `internal_error` and `DiagnosticCode::InternalError` with it.**

If any of these three still needs an escape hatch, that is a finding — report it, do not widen the signature.

### Addresses stop being spellable

`emit` produces `Target::Label(LabelId)` and resolves every label against the function's own code vector. There is no `base` parameter.

**And then no absolute address is ever constructed, because jump operands become function-relative.** The VM already round-trips them for nothing:

```rust
let addr = code_start + ip;          // exec.rs:388, 482 — every fetch
ip = instr.operand - code_start;     // exec.rs:148, 256, 263, 604 — every jump
```

It stores an absolute operand, subtracts `code_start` to recover a function-relative `ip`, then re-adds `code_start` on the next fetch. The absolute form exists only because `emit` was handed a `base` and folded it in — which *is* the miscompile: `base` was captured before `lower` appended an eta-wrapper.

So: `ip = instr.operand;`. Four subtractions leave the interpreter's hot path (report the bench; do not treat it as the motivation). A function's code becomes position-independent, so appending it anywhere is correct by construction, and `link` reduces to "append `out.code`, record `Function.code_start`" — no patching pass at all. `fn_body_start` is deleted.

This is strictly stronger than resolving addresses in one careful place: an absolute address is never *spelled*.

Safe because `CoreExpr`'s control flow is structured (`If`/`Match`/`Tail`), so every jump `emit` produces is intra-function by construction. Calls take a `FuncIdx`, not an address.

Two things to verify rather than assume:
1. `Function.code_start` must still be recorded — it is how the VM enters a function. It is written by the appender, which knows the insertion point.
2. `crates/al_core/src/bytecode/peephole.rs` is the other consumer of jump operands. Its module comment (line 13) documents the round-trip verbatim, and line 28 does `let t = instr.operand as usize;` to index the code slice. It already works within one function's code, so relative targets simplify it. Update that comment.

If some jump genuinely crosses a function boundary, that is a real finding: report it and fall back to `link`-resolves-absolute, naming the jump that forced it.

### Operand spaces become distinct types

`Instruction.operand: i32` currently means jump-target OR local-slot OR const-id OR func-idx OR arity, chosen by opcode. Introduce `CodeAddr`, `LocalSlot`, `ConstId`, `FuncIdx`, `Arity`. Consider `typed-index-collections`' `TiVec<CodeAddr, Instruction>` so indexing code with a `LocalSlot` does not compile. al already did this once, for `TypeId`.

The wire format stays a packed `i32`; the newtypes exist above the encoding boundary.

## Phasing — one destination, a green tree at every step

Size is not a constraint; **an unreviewable diff is.** The last two times this compiler was rewritten in a single pass it shipped a miscompile (stale `base`) and 22 invented types. Both were found by adversarial review *after* the fact, not by the swarm that wrote them. So the phases below are not a smaller plan — they are the same plan, ordered so the build is green and the suite passes after each one, and so each can be reviewed on its own.

Each phase is a separate commit. Do not begin the next until the previous is green.

**Every phase must be part of the destination, never a repair of what the destination deletes.** Test each one by asking: *does this code still exist after T5?* If the answer is no, do not write it.

An earlier draft of this spec failed that test. It proposed, as T1, giving `ast::Expression` a `NodeId` and re-keying the `HashMap<Span, Ty>` side table on it — to fix the span collisions that produced the REPL wrong-answer bug. But `TypedExpr` carries `ty` as a **field**: T5 deletes the table, so there is no key to give an identity to. T1 was work whose only purpose was to make a doomed data structure correct. It has been removed.

The surviving phases each *are* part of T5's end state — T2 and T3 are literally pieces of T5's signature, T4 is orthogonal and permanent.

| # | phase | eliminates | proof it worked |
|---|---|---|---|
| **T2** | `LowerCtx` loses `fn engine(&mut self) -> &mut InferEngine`. Types reach `lower` only as data. | `fresh_var()` inside `lower` | `lower.rs` cannot name `fresh_var`; it does not compile if you try |
| **T3** | `LowerCtx` loses `&mut Program`. `eta_wrapper` becomes a *request* the caller materialises. | the eta-wrapper jump miscompile | `type_semantics.rs::a_branch_after_an_eta_wrapper_jumps_to_the_right_place` green, and `lower` cannot reach `program.code` |
| **T4** | Jump operands become function-relative; `emit` resolves its own labels; `fn_body_start` is deleted. | stale `base`, and absolute addresses entirely | no absolute address is constructed anywhere |
| **T5** | `RTy` + `ResolvedNode` (no `Var` arm) + `zonk`. `TypedExpr` carries `ty: RTy` and resolved field/variant/callee indices. `lower(p: &TypedProgram) -> CoreProgram`, total. | `LowerError`, `internal_error`, `DiagnosticCode::InternalError`, and the `Span`-keyed type table | those identifiers do not exist |

T2–T4 are each independently valuable and each kill a class outright. T5 is what they converge on: after T4 the only thing it buys is **totality**, and by then the signature changes that make it possible have already landed.

Ordering is a hedge against an unreviewable diff, nothing more. If a phase can be folded into T5 *without* writing code that T5 would delete, fold it.

If at T5 it turns out `lower` still needs an escape hatch, that is the most interesting finding in the whole exercise — report it rather than widening the signature.

## Constraints — outcomes, not tokens

- **Correctness over compile speed.** A slower `al build` is acceptable and expected. Say how much slower.
- **The interpreter must not regress.** `examples/bench_typed.al`, `examples/bench_map.al`, and an amplified `fib(36)`+`count(8e7)` must hold. This work is compile-time only, so a runtime change is a bug — except where recovering lost typed opcodes makes it *faster*, which is a win worth reporting.
- Recovering the typed opcodes that `fresh_var()` was suppressing may change the emitted instruction mix. That is the point. Report before/after opcode histograms for an inferred-type function.
- **Every guard must be a type, an exhaustive match, or a behavioural test.** Never a test that greps source text. Never a hardcoded list of symbol names. A previous attempt produced `const COLD_INLINE_NEVER_HANDLERS: &[&str]` plus a `.rs`-file scanner; it was reverted.
- Do not weaken, skip, or delete a test. Do not add a cargo feature. Do not bypass git hooks.

## Regressions this must keep fixed

Both are live regression tests today; they must still pass, and they must pass *because the shape makes them impossible*, not because a check was added.

```al
// eta-wrapper jump miscompile — was a VM index-out-of-bounds
import al/array
type W { W(v Int) }
fn pick(xs Array(Int)) Int {
	ws = array.map(xs, W)
	if array.length(ws) > 2 { 111 } else { 222 }
}
println(pick([1]))        // 222
println(pick([1, 2, 3]))  // 111
```
```al
// field access on an inferred match binding — was a lower panic
type User { User(id Int name String) }
fn f() Int { match Some(User(7, 'al')) { None -> 0  Some(u) -> u.id } }
println(f())              // 7
```

## Acceptance

1. `lower`'s signature takes no `&mut` and returns no `Result`. `LowerError` and `DiagnosticCode::InternalError` are deleted.
2. `grep -c 'fresh_var()' crates/al_core/src/core_ir/lower.rs` → 0, because it does not have an engine. (This is an *outcome* of the signature, not a rule to satisfy.)
3. No `emit` path produces an absolute address; `link` is the only place addresses are written.
4. All tests green (currently 727+). Both regressions above green.
5. `cargo clippy --all-targets` → 0 errors, 0 warnings. `cargo fmt --check` clean.
6. Interpreter benches hold or improve, measured interleaved, min-of-N `user` time, N≥7, both binaries built before benching, never benching while compiling (the box's load swings 3–170; absolute thresholds are meaningless).

## Sequencing

The typed IR is a prerequisite for the effects/comptime work in `docs/effects-comptime-spec.md`, whose Phase A0/A1/A2 assume a compiler that does not mutate itself mid-pass. Land this first.

## Risks, ranked

1. **The elaborator is a rewrite of the typecheck walk.** `compile_expr` currently typechecks *and* drives name resolution *and* records `closure_info`. Splitting "check" from "emit TypedExpr" is the bulk of the work.
2. **`IncrementalSession` caches `ModuleInterface` + `ModuleReferences` keyed on watermarks.** A new IR between typecheck and lower must not break incremental invalidation. Check `bytecode/session.rs`.
3. **Spans.** `closure_info` is keyed by `Span`. If `TypedExpr` nodes carry their own identity, that table can die; verify no other consumer depends on span-keying.
4. **Memory.** A second full IR for a large program doubles peak AST memory. Acceptable; measure it.
5. **The stdlib is precompiled at build time** (`precompile_stdlib()` from `crates/al/build.rs`). The new pipeline must work there too, and `build.rs`'s only dependency is `al_core`.
