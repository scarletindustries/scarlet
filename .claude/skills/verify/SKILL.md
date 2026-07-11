---
name: verify
description: Verify a change to the AL compiler/VM by driving the real `al` binary end-to-end. Use before committing anything that touches crates/al_core (parser, types, core_ir, bytecode) or crates/al (vm, cli, lsp, repl).
---

# Verifying an AL change

The surface is the `al` **CLI**. Everything a user reaches — type errors,
codegen, the VM, the REPL — comes out of `al check`, `al run`, or `al repl`.
Compile once, then drive those. Do not run `cargo test` here; CI does that.

## Handle

```bash
cargo build --release          # ~15s warm; ./target/release/al
```

Never build under `/tmp` or `$TMPDIR` — Santa blocks binaries executed from
there. Never set `CARGO_TARGET_DIR`. AL *source* files under `$TMPDIR` are
fine; only the binary must live under `~/code`.

## Drive it

```bash
./target/release/al check prog.al     # typecheck + lower, no codegen. exit 1 on error
./target/release/al run   prog.al     # full pipeline + execute
printf 'expr\nexpr\n' | ./target/release/al repl
```

`al check` runs everything except `perceus` + `emit`, so it catches lowering
failures too. If `check` passes and `run` crashes, that is always a bug.

### Seeing what the compiler produced

`CORE_DBG=1` dumps the post-Perceus Core IR (typed ANF) per function — the
fastest way to confirm op selection (`AddInt` vs the dynamic `Add`), `drop`
placement, and `reuse` tokens:

```bash
CORE_DBG=1 ./target/release/al run prog.al 2>&1 | grep -A 15 '^=== myfn'
```

Ty ids (`:4933`) are arena indices and shift on any stdlib change. To compare
two builds, normalise first:

```bash
norm() { sed -E 's/:[0-9]+/:T/g; s/s[0-9]+/sN/g; s/c[0-9]+/cN/g' "$1"; }
diff <(norm a.txt) <(norm b.txt)
```

## Probes worth running

- **`al check` vs `al run`** on the same program. They must agree: both
  accept, or both reject with the same diagnostic. A `check`-passes /
  `run`-crashes split is the classic AL bug shape.
- **Inferred vs annotated.** Write the program twice — once with the type
  spelled out, once letting inference find it. `lower` historically only
  worked on the annotated form. `fn f(o Option(User)) …` vs
  `match Some(User(..)) { … }`.
- **The REPL**, for anything touching `Compiler` state. It is the only path
  through `session.rs::reset_to`, which rewinds the type arena between
  compiles; stale indices survive nowhere else.
- **A second module.** `Span` carries no file id, so any span-keyed compiler
  map (`closure_info`, `expr_tys`) can collide across files. Make two files
  whose bodies sit at the *same* line/column with *different* types and check
  both still work.
- **Ordinary diagnostics still ordinary.** A plain type error and a plain
  parse error must not turn into an internal-compiler-error.
- **Constructor as a first-class value** (`array.map(xs, W)`) — this makes
  `lower` synthesise an eta-wrapper into `program.code` mid-lowering, which
  has broken jump targets and `Function.code_start` before. Always pair it
  with a branch: `if cond { a } else { b }`, and exercise *both* arms — the
  fall-through arm hides the bug.

## Benchmarking

The box is loaded; absolute numbers lie. Build both binaries **first**, never
bench while compiling, alternate run-by-run, take min-of-N (N ≥ 9) of `user`
time (`resource.getrusage(RUSAGE_CHILDREN).ru_utime` around each child).

Baselines: `examples/bench_typed.al`, `examples/bench_map.al`, and an
amplified `fib(36)` + `count(80000000, 0)`.

Two traps:

1. **Layout noise is ±1%.** Two builds of semantically identical source place
   the VM's interpreter loop differently. Before believing a small delta,
   build a *placebo* — the baseline plus a dead function — and measure it too.
   If the placebo moves as much as the change did, you measured nothing.
2. **Build path matters.** A binary built in a `git worktree` has a different
   `OUT_DIR` and different layout. Build baseline *and* candidate at the same
   path (apply/revert the diff in one throwaway worktree under `~/code`, never
   `/tmp`) and `git worktree remove` it when done.

First check whether the emitted code even changed (`CORE_DBG` + `norm` above).
If the bytecode is identical, the benchmark cannot legitimately move.
