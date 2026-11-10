---
name: verify
description: Verify a change to the Scarlet compiler or VM by driving the real `scarlet` binary end to end. Use before committing anything that touches the syntax, types, elaborator, lowering, compiler, VM or driver crates (crates/scarlet_*).
---

# Verifying a Scarlet change

Everything a user reaches comes out of the `scarlet` binary: type errors,
lowering, the VM, the REPL. So build it once and drive it. This is not a
replacement for CI. `CLAUDE.md` lists the gates to run before a commit, and
this skill is the part those gates don't do, which is using the program the
way a person would.

## Build

```bash
cargo build --release          # ./target/release/scarlet
```

Write probe programs (`.sca`) in the session's scratchpad directory, not in
the repo.

## Drive it

```bash
B=./target/release/scarlet
$B check prog.sca                      # typecheck and lower, no run. exit 1 on error
$B run prog.sca [args]                 # the whole pipeline, run on scarlet_vm
$B dis prog.sca                        # the Core IR the VM is handed
printf 'expr\nexpr\n' | $B repl
```

`check` stops before Perceus and the VM, so a program `check` accepts and
`run` then crashes on is always a bug.

A program that reaches something the VM hasn't built yet stops with "the new
VM does not run ... yet". That is `Stop::NotBuiltYet`, not a crash, but it
does mean the probe didn't test what you meant it to.

### Seeing what the compiler made

`scarlet dis` prints the whole program's Core IR. `CORE_DBG=1` prints each
function's Core IR after Perceus to stderr, as it is lowered, which is the
quickest way to check op choice (`IntAdd` against a generic add), `drop`
placement and `reuse`:

```bash
CORE_DBG=1 $B run prog.sca 2>&1 | grep -A 15 '^=== myfn'
```

Type ids (`:4933`) are arena indices and move with any stdlib change. To
compare two builds, normalise them first:

```bash
norm() { sed -E 's/:[0-9]+/:T/g; s/%[0-9]+/%N/g; s/c[0-9]+/cN/g' "$1"; }
diff <(norm a.txt) <(norm b.txt)
```

## Probes worth running

- **`check` against `run`** on the same program. Both must accept it, or both
  reject it with the same diagnostic.
- **Inferred against annotated.** Write the program twice, once with the type
  spelled out and once letting inference find it.
- **The REPL**, for anything touching compiler state. It is the only path
  through `session.rs`'s `reset_to`, which rewinds the type arena between
  entries, so stale indices show up nowhere else.
- **A second module.** `Span` carries no file id, so anything keyed on a span
  can collide across files. Make two files whose bodies sit at the same line
  and column with different types, and check both still work.
- **Plain errors stay plain.** An ordinary type error and an ordinary parse
  error must not turn into an internal compiler error.
- **A constructor as a value** (`array::map(xs, W)`) inside a branch, with
  both arms taken. It makes lowering build an eta wrapper partway through.
- **Ints either side of the small-Int edge.** A small Int is 48 bits, so try
  values around `140737488355327` (2^47 - 1), not just `1` and `2^64`.
- **Output to a terminal and to a pipe.** `scarlet run` flushes every write
  to a terminal and buffers writes to a pipe (`crates/scarlet/src/output.rs`).
  A pipe is `$B run prog.sca | cat`. For a terminal, run it under a pty and
  watch when each chunk arrives:

  ```python
  import os, pty, sys, time, select
  pid, fd = pty.fork()
  if pid == 0:
      os.execv(sys.argv[1], sys.argv[1:])
  t0 = time.time()
  while True:
      if select.select([fd], [], [], 0.1)[0]:
          try: data = os.read(fd, 4096)
          except OSError: break
          if not data: break
          print(f"{time.time() - t0:.2f}s {data!r}")
  ```

  A new pty reports its size as 0 by 0, so `os::terminal_size()` gives
  `Err(Nil)` until you set one with `fcntl.ioctl(fd, termios.TIOCSWINSZ,
  struct.pack('HHHH', rows, cols, 0, 0))`. Set it before the child starts, or
  the child can read the size first.

## Benchmarking

The machine is busy, so one run's number means little. Build both binaries
first and never measure while something compiles. Then alternate the two run
by run, and take the minimum of at least 9 runs. `scripts/bench.sh` does this
for the bench programs (`examples/bench*.sca`), and `--save`/`--baseline`
compare two builds.

To get a baseline binary without touching the shared checkout, unpack the
commit into the scratchpad and build it there:

```bash
mkdir -p "$SCRATCH/base" && git archive HEAD | tar -x -C "$SCRATCH/base"
(cd "$SCRATCH/base" && cargo build --release -p scarlet)
```

Never stash, reset or check out in the main checkout to get a baseline. Other
sessions may be editing it.

Two traps:

1. **Layout noise is about 1%.** Two builds of the same meaning can place the
   interpreter loop differently. Before you believe a small change, build a
   placebo (the baseline plus a dead function) and measure that too. If it
   moves as much as your change did, you measured nothing.
2. **Check the code changed at all.** If `CORE_DBG` with `norm` shows the same
   Core IR and the VM didn't change, the benchmark can't really have moved.

For a profile, build with symbols into its own target dir, then sample the
running process:

```bash
CARGO_PROFILE_RELEASE_DEBUG=true CARGO_PROFILE_RELEASE_STRIP=false \
  cargo build --release -p scarlet --target-dir "$SCRATCH/prof"
"$SCRATCH/prof/release/scarlet" run prog.sca & sleep 1
sample $! 5 -file "$SCRATCH/s.txt"
grep -A 25 'Sort by top of stack' "$SCRATCH/s.txt"
```
