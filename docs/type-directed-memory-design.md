# Type-directed memory and code generation for Scarlet

**Status**: this was the design for the old bytecode VM, and the results below were measured on it. The old VM is gone. Perceus (tiers 2 and 4) carries on in `crates/scarlet_lower/src/perceus.rs` and the new VM, but the opcodes, `MiHeap`, `heap/space.rs` and the file and line references here are the old VM's. `docs/vm-design.md` has the plan for memory in the new VM.

Each tier comes from a peer-reviewed primary source. Putting them together follows Cyclone's proven template, fitted to Scarlet's constraints.

## The one idea

Every heap type carries an inferred **`(region ρ, aliasability α)`** pair. Cyclone splits memory this way into two independent questions [Swamy et al. SCP; Grossman et al. PLDI'02]:

- `ρ ∈ {frame ⊑ process ⊑ immortal}` says how long a value lives. Each region outlives the one before it, so LIFO nesting becomes subtyping. It works with HM unification and needs no metadata at run time.
- `α ∈ {unique, shared}` decides reuse and reference counting (RC).

The five tiers below are just the combinations of those two. There's one set of typing rules, one inference pass and one rule for falling back, with nothing bolted on.

## Why this fits Scarlet

Scarlet's constraints are the ones the best techniques in the literature were designed for:

| Scarlet property | what it unlocks | source |
|---|---|---|
| HM types at compile time | region + multiplicity + mode inference is decidable, no annotations | Tofte'04; Lorenzen'24 |
| acyclic-by-construction | RC is **complete**, so no cycle collector is ever needed | Ullrich & de Moura IFL'19 |
| immutable values | Perceus reuse is sound; DAG sharing well-defined | Reinking et al. PLDI'21 |
| process isolation | non-atomic RC everywhere; process = one region | Johansson et al. ISMM'02 |
| no JIT | all placement decisions AOT; monomorphise freely | Weeks MLton'06 |

## The tiers, from `(ρ, α)`

### Tier 1 — frame-local (`ρ=frame`)
**Placement**: activation record / bump slab freed at return.
**Analysis**: Jane Street's mode inference on the locality axis [Lorenzen et al. ICFP'24]. It's a two-point `local ⊑ global` lattice on bindings, fully inferred, and deliberately simpler than Tofte-Talpin region variables. That avoids the problem Tofte reported, where "region-annotated terms become large and fragile".
**Win**: MLKit found that over 90% of allocations at run time land in finite (bound-1) regions, which become the activation record [Tofte et al. HOSC'04]. Region inference "serves the purpose of generations" [Elsman & Hallenberg PADL'20], so there's no need for a separate nursery.

### Tier 2 — reuse-in-place (`α=unique`, same-shape reconstruction)
**Placement**: overwrite the dying cell, with no allocation or freeing and constant stack.
**Analysis**: FIP calculus check + **drop-guided (frame-limited) reuse** [Lorenzen & Leijen ICFP'22; Lorenzen et al. ICFP'23]. The emitted code is `if is_unique(x) { overwrite } else { dec(x); alloc() }`, and the else branch *is* tier 4.
**Important**: use the ICFP'22 frame-limited algorithm, **not** Lean 4's unrestricted borrow inference. Leijen's own paper proves Lean's isn't safe for space, because the peak heap can grow without bound. Bounding each call by a constant factor is what lets this work with tier 3's arena bounds.

### Tier 3 — process arena (`ρ=process`)
**Placement**: a bump `Space` per process (bring back or rewrite `heap/space.rs`), freed in O(1) when the process dies.
**Analysis**: values that escape the frame but not the process, like the state of an actor's receive loop.
**Important**: this **must not be the only way memory is freed**. Three independent sources agree that lexical regions blow up 10 to 100 times over on lifetimes that don't nest, on higher-order functions and on server loops (barnes-hut: 284MB with regions alone, against 2.2MB in MLton) [Tofte'04; Cyclone SCP; PADL'20]. An actor's receive loop is exactly that worst case, so tier 4 has to back it up.

### Tier 4 — RC over mimalloc (`α=shared`, or `ρ` the analysis can't pin)
**Placement**: the `MiHeap` of the time, with a non-atomic refcount before each object and `mi_free` when it reaches zero.
**Analysis**: Perceus inserts precise ownership [Reinking et al. PLDI'21]. Scarlet already did what Lean 4's single-threaded tag does [Ullrich'19]: RC is non-atomic, because a process is single-threaded between messages.
**Between processes**: keep BEAM-style **copy on send** (`rc_copy_graph`). That avoids Lean's `markMT` promotion, which costs O(reachable) anyway, and keeps processes isolated, which we want. Large `Binary` bytes stay shared as `Arc<[u8]>`, with no copy.

### Tier 5 — frozen immortal (`ρ=immortal`)
**Placement**: `FrozenArea`, with no refcount and never freed.
**Already done.** This is exactly Lean's `persistent` tag.

## Type-directed code generation

The same analysis feeds both memory and code generation. The `(ρ, α)` inference passes type information to the compiler, and research on the codebase (`wjqk2f2qb`) confirmed most of the wiring was already there:

- `resolved_prim` (infer.rs:748) already resolves `Ty → {Int,Float,String}` at emit time, and typed arithmetic ops (`AddInt`, `AddFloat`) already exist (mod.rs:78-188). **Gap**: no typed compares and no typed collection ops.
- `ValueKind::Constructor{variant_idx}` is filled in but thrown away at emit. Pass it through to get **jump-table dispatch on the integer tag** for exhaustive matches, which skips the tag check on every arm.
- `Op::Call` does 5-step dynamic dispatch (tag check, heap read, arity check, …). A callee whose target and arity are known gets a **direct `CallKnown func_idx`** op.
- `GetField` is already an O(1) offset. What's left to cut is a tag check and a bounds check that the types already prove.
- With NaN boxing, the leaves of an `Array(Int)` or `Array(Float)` are already unboxed 8-byte words, so **a monomorphic `map` or `fold` needs no box check per element**.
- If `engine.find(ty)` gives `TypeNode::Var` at emit time, the type is polymorphic, so emit the existing dynamic op. That needs nothing new.

## New opcodes, from the `vm-opcode-design` findings

`Instruction` is a fixed 8 bytes, `{op:u8, a:u8, b:u16, operand:i32}`, which leaves plenty of room. Add `CallKnown`, `SwitchTag`, `GetFieldUnchecked`, `CmpInt/CmpFloat`, `Reuse`/`ReuseDrop` (Perceus), `AllocFrame n` (tier-1 bump).

## Open questions (measure, don't guess)

1. **Mode inference or full region inference** for tier 1? Jane Street's is simpler, but does it get the same 90% of the wins? Start with modes, and add region polymorphism only if benchmarks show it's missing allocations.
2. **`markMT` or copy on send?** Both cost O(reachable). Keep copying, because isolation is worth having, and look again if message passing turns out to be the bottleneck.
3. **How much to monomorphise?** MLton reports at most 30% code growth in practice. Measure Scarlet's `Program.code: Vec<Instruction>` on `bench_heavy.sca` before choosing between whole-program and on-demand.

## Bench baseline

`scripts/bench.sh` runs `bench_heavy.sca` five times and keeps the best. The comment on the `Instruction` fetch layout (mod.rs:374) already noted swings of about 30% on this bench. Use the same harness before and after.

## Phase 1 result

`scripts/bench.sh` before/after (baseline binary at `target/bench-baseline/`, best-of-5; hyperfine 8-run mean in parens):

| build | best-of-5 | mean ± σ |
|---|---|---|
| baseline (Jun 1) | 0.33 s | 348.0 ms ± 4.1 |
| Phase 1 (e460235) | 0.33 s | 343.4 ms ± 3.5 |
| **delta** | **0.00 s** | **−4.6 ms (1.01× ± 0.02)** |

There's no measurable win, and on a closer look that's expected. It isn't a bug in the new ops. `bench_heavy.sca` is `fib(33)` + `count(10⁷,0)`: pure self-recursion and `Int` `<`/`==`. The baseline already emitted `CallSelf`/`TailCallSelf` for self-recursion and `LtInt`/`JumpGeIntLC`/`JumpNeIntLC` for the guards, so the hot loop's opcode stream is byte-identical before and after. None of the Phase 1 ops fire on this workload: `CallKnown` targets *cross-function* known callees (self-calls hit the older `CallSelf` fast path), and `SwitchTag`/`GetFieldUnchecked` need an enum/record scrutinee the bench doesn't have. The spec assumed fib, being all self-calls, would gain the most. That was wrong, because self-calls were already the fast case.

Before we can quote Phase 1's real speedup, `bench_heavy.sca` needs a second workload that uses it: a pair of mutually recursive functions (for `CallKnown`), an exhaustive enum match in the hot loop (for `SwitchTag`) and a record field read (for `GetFieldUnchecked`). The `LeInt`, `GeInt`, `LeFloat` and `GeFloat` ops from spec item 3 weren't added. The baseline already had `LtInt`, `GtInt`, `EqInt`, `LtFloat` and `GtFloat`, so typed compares are the same as before.

`examples/bench_typed.sca` (tree sum + is_even/is_odd + record dot-product loop, best-of-5):

| build | real |
|---|---|
| pre-Phase-1 (d684d74) | 0.67 s |
| Phase 1 | 0.61 s |
| **delta** | **−9%** |

Most of the remaining time goes on allocating `Point`s in `dot_loop`. Phase 2 (Perceus reuse) was meant to remove it.

### Phase 2 — dot_loop reuse

`dot_loop(1_000_000, 0)` allocates two `Point(x y z)` per iteration. That's 2M heap objects, each built, read once by `dot`, and dead straight away. hyperfine, 10 runs, 2 warmup, macOS arm64, both rows measured back-to-back on the same machine (Phase-1 binary preserved as `target/release/al_phase1_baseline`):

| build | `bench_typed.sca` mean ± σ | `dot_loop` only mean ± σ |
|---|---|---|
| Phase 1 (62c8795) | 660.0 ms ± 12.0 | 208.4 ms ± 2.9 |
| Phase 2 | 701.5 ms ± 16.4 | 223.9 ms ± 4.5 |

Phase 2 made `dot_loop` **7% slower**, not faster as this section expected. Drop-guided reuse, as it landed, pairs a *drop with a later constructor of the same shape that it dominates, in one straight-line frame*. Then, that meant the match-scrutinee pattern (`match xs { Cons(h, t) -> Cons(..) }`). `dot_loop`'s body runs the other way round: it builds `p` and `q`, and then uses them for the last time. So no drop dominates a constructor, and no `Op::Reuse` is emitted. The cross-iteration pairing originally assumed here (iteration *N*'s dead `p` feeding iteration *N+1*'s `Point(..)`) does not survive `TailCallSelf`: `collapse_tail_frame` drains the local slots, so the held cell is freed before the next body runs. Until reuse tokens survive the tail-call frame collapse, or `dot_loop` is rewritten as a `match` over a boxed accumulator, Phase 2 pays for `Op::Drop` and a `Nop` hole on every read of a heap local, and gets no reuse back. The 7% is that cost. `dot_loop` alone is about 31% of Phase 1's wall time, which is the most this could save once pairing covers this shape.

## References

- Grossman, Morrisett, Jim, Hicks, Wang, Cheney. *Region-Based Memory Management in Cyclone*. PLDI'02.
- Swamy, Hicks, Morrisett, Grossman, Jim. *Safe Manual Memory Management in Cyclone*. SCP.
- Tofte, Birkedal, Elsman, Hallenberg. *A Retrospective on Region-Based Memory Management*. HOSC'04.
- Hallenberg, Elsman, Tofte. *Combining Region Inference and Garbage Collection*. PLDI'02.
- Elsman, Hallenberg. *On the Effects of Integrating Region-Based Memory Management and Generational GC*. PADL'20.
- Ullrich, de Moura. *Counting Immutable Beans*. IFL'19. arXiv:1908.05647.
- Reinking, Xie, de Moura, Leijen. *Perceus: Garbage Free Reference Counting with Reuse*. PLDI'21.
- Lorenzen, Leijen. *Reference Counting with Frame-Limited Reuse*. ICFP'22.
- Lorenzen, Leijen, Swierstra. *FP²: Fully in-Place Functional Programming*. ICFP'23.
- Lorenzen, White, Dolan, Eisenberg, Lindley. *Oxidizing OCaml with Modal Memory Management*. ICFP'24.
- Johansson, Sagonas, Wilhelmsson. *Heap Architectures for Concurrent Languages using Message Passing*. ISMM'02.
- Weeks. *Whole-Program Compilation in MLton*. 2006.
