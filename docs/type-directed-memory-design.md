# Type-directed memory & codegen for al

**Status**: design spec for `/rdr-impl`. Every tier is grounded in a peer-reviewed primary source; the composition is Cyclone's proven template instantiated on al's constraints.

## The one idea

Every heap type carries an inferred **`(region ρ, aliasability α)`** pair — Cyclone's orthogonal factoring [Swamy et al. SCP; Grossman et al. PLDI'02]:

- `ρ ∈ {frame ⊑ process ⊑ immortal}` — an outlives lattice (LIFO nesting → subtyping; HM-unification-compatible; zero runtime metadata)
- `α ∈ {unique, shared}` — governs reuse and RC

The five "tiers" are just the cells of that product. One typing discipline, one inference pass, one degradation rule. Nothing bolted on.

## Why this fits al specifically

al's constraints are exactly the ones the literature's best techniques were designed for:

| al property | what it unlocks | source |
|---|---|---|
| HM types at compile time | region + multiplicity + mode inference is decidable, no annotations | Tofte'04; Lorenzen'24 |
| acyclic-by-construction | RC is **complete** — no cycle collector, ever | Ullrich & de Moura IFL'19 |
| immutable values | Perceus reuse is sound; DAG sharing well-defined | Reinking et al. PLDI'21 |
| process isolation | non-atomic RC everywhere; process = one region | Johansson et al. ISMM'02 |
| no JIT | all placement decisions AOT; monomorphize freely | Weeks MLton'06 |

## The tiers (falls out of `(ρ, α)`)

### Tier 1 — frame-local (`ρ=frame`)
**Placement**: activation record / bump slab freed at return.
**Analysis**: Jane Street's mode inference (locality axis) [Lorenzen et al. ICFP'24] — a two-point `local ⊑ global` lattice on bindings, fully inferred, deliberately simpler than Tofte-Talpin region variables. Avoids the "region-annotated terms become large and fragile" failure Tofte reported.
**Win**: MLKit found 90%+ of runtime allocations land in finite (bound-1) regions → activation record [Tofte et al. HOSC'04]. Region inference "serves the purpose of generations" [Elsman & Hallenberg PADL'20] — no separate nursery needed.

### Tier 2 — reuse-in-place (`α=unique`, same-shape reconstruction)
**Placement**: mutate the dying cell; zero (de)alloc, constant stack.
**Analysis**: FIP calculus check + **drop-guided (frame-limited) reuse** [Lorenzen & Leijen ICFP'22; Lorenzen et al. ICFP'23]. The emitted code is `if is_unique(x) { overwrite } else { dec(x); alloc() }` — the else branch IS tier 4.
**Critical**: use the ICFP'22 frame-limited algorithm, **NOT** Lean 4's unrestricted borrow inference — Leijen's own paper proves the latter is not safe-for-space (peak heap can blow up unboundedly). Per-call constant-factor bound is what makes this compose with tier 3's arena bounds.

### Tier 3 — process arena (`ρ=process`)
**Placement**: per-process bump `Space` (revive/rewrite `heap/space.rs`); freed O(1) at process death.
**Analysis**: values escaping frame but not process — the actor's receive-loop state.
**Critical**: **must NOT be the sole reclamation** — three independent sources agree lexical regions blow up 10-100× on non-nested lifetimes / higher-order fns / server loops (barnes-hut: 284MB region-only vs 2.2MB MLton) [Tofte'04; Cyclone SCP; PADL'20]. An actor's receive loop IS the pathological case. Tier 4 is the mandatory backstop.

### Tier 4 — RC over mimalloc (`α=shared` or `ρ` escapes analysis)
**Placement**: current `MiHeap` — non-atomic refcount prefix, `mi_free` at zero.
**Analysis**: Perceus precise ownership insertion [Reinking et al. PLDI'21]. Lean 4's single-threaded tag [Ullrich'19] — al already does this (non-atomic RC, process is single-threaded between messages).
**Cross-process boundary**: keep BEAM-style **copy-on-send** (`rc_copy_graph`) — sidesteps Lean's `markMT` promotion (which is O(reachable) anyway) and preserves process isolation as a feature. Large `Binary` bytes stay `Arc<[u8]>`-shared, no copy.

### Tier 5 — frozen immortal (`ρ=immortal`)
**Placement**: `FrozenArea` — no refcount prefix, never freed.
**Already done.** This is Lean's `persistent` tag verbatim.

## Type-directed codegen (same analysis feeds both)

The `(ρ, α)` inference threads type info to the compiler; the codebase research (`wjqk2f2qb`) confirms the wiring is nearly there:

- `resolved_prim` (infer.rs:748) already resolves `Ty → {Int,Float,String}` at emit time — typed arith ops (`AddInt`/`AddFloat`) already exist (mod.rs:78-188). **Gap**: no typed compare, no typed collection ops.
- `ValueKind::Constructor{variant_idx}` is populated but discarded at emit — thread it through for **integer-tag jump-table dispatch** on exhaustive matches (elides per-arm tag checks).
- `Op::Call` does 5-step dynamic dispatch (tag check, heap read, arity check, …). Known-target-known-arity callees → **direct `CallKnown func_idx`** op.
- `GetField` is already O(1) offset — remaining fat is a dead-by-typing tag check + bounds check the type system proves.
- NaN-box means `Array(Int)`/`Array(Float)` leaves are already unboxed 8-byte words — **monomorphic `map`/`fold` need no per-element box check**.
- Polymorphic fallback: `engine.find(ty)` → `TypeNode::Var` at emit time → emit the current dynamic op. No new mechanism needed.

## New opcodes (from `vm-opcode-design` findings)

`Instruction` is 8-byte fixed `{op:u8, a:u8, b:u16, operand:i32}` — plenty of encoding room. Add: `CallKnown`, `SwitchTag`, `GetFieldUnchecked`, `CmpInt/CmpFloat`, `Reuse`/`ReuseDrop` (Perceus), `AllocFrame n` (tier-1 bump).

## Open questions (measure, don't guess)

1. **Mode inference vs full region inference** for tier 1 — Jane Street's is simpler; does it capture the same 90% wins? Start with modes; add region polymorphism only if benchmarks show it's leaving allocations on the table.
2. **markMT vs copy-on-send** — both O(reachable). Keep copy (isolation is a feature); revisit if message-passing throughput is the bottleneck.
3. **Monomorphization budget** — MLton reports ≤30% code-size growth in practice. al's `Program.code: Vec<Instruction>` — measure on `bench_heavy.al` before deciding whole-program vs on-demand.

## Bench baseline

`scripts/bench.sh` → best-of-5 `bench_heavy.al`. The `Instruction` fetch layout comment (mod.rs:374) already cites ~30% swings on this bench — same harness for before/after.

## Phase 1 result

`scripts/bench.sh` before/after (baseline binary at `target/bench-baseline/`, best-of-5; hyperfine 8-run mean in parens):

| build | best-of-5 | mean ± σ |
|---|---|---|
| baseline (Jun 1) | 0.33 s | 348.0 ms ± 4.1 |
| Phase 1 (e460235) | 0.33 s | 343.4 ms ± 3.5 |
| **delta** | **0.00 s** | **−4.6 ms (1.01× ± 0.02)** |

No measurable win — and on inspection that's expected, not a bug in the new ops. `bench_heavy.al` is `fib(33)` + `count(10⁷,0)`: pure self-recursion and `Int` `<`/`==`. The baseline already emitted `CallSelf`/`TailCallSelf` for self-recursion and `LtInt`/`JumpGeIntLC`/`JumpNeIntLC` for the guards, so the hot loop's opcode stream is byte-identical before and after. None of the Phase 1 ops fire on this workload: `CallKnown` targets *cross-function* known callees (self-calls hit the older `CallSelf` fast path), and `SwitchTag`/`GetFieldUnchecked` need an enum/record scrutinee the bench doesn't have. The spec's "fib is all self-calls → biggest win" premise was wrong — self-calls were already the fast case.

Action item: `bench_heavy.al` needs a companion workload that actually exercises Phase 1 — a mutually-recursive pair (forces `CallKnown`), an exhaustive enum match in the hot loop (forces `SwitchTag`), and a record projection (forces `GetFieldUnchecked`) — before Phase 1's real speedup can be quoted. Le/Ge variants (`LeInt`/`GeInt`/`LeFloat`/`GeFloat`) from spec item 3 were not added — baseline already had `LtInt`/`GtInt`/`EqInt`/`LtFloat`/`GtFloat`, so typed compares are unchanged vs baseline.

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
