# Benchmarks

The same programs in Scarlet, Lua, Python, JavaScript and Erlang, to race
Scarlet against other runtimes on one core. `benches/run.py` runs them, checks
every language prints the same answer, and prints the tables. Its docstring
says how to use it.

Each directory is one benchmark. The comment at the top of each file says what
it stresses and where it came from.

- `bounce`, `json`, `list`, `mandelbrot`, `nbody`, `permute`, `queens`,
  `richards`, `sieve`, `storage` and `towers` are ports from
  [Are We Fast Yet](https://github.com/smarr/are-we-fast-yet) (MIT licence).
- `cfold`, `deriv` and `rbtree` are ports from
  [Koka's benchmarks](https://github.com/koka-lang/koka) (Apache-2.0).
- `binary_trees`, `fannkuch` and `spectral_norm` are ports from the
  [Computer Language Benchmarks Game](https://benchmarksgame-team.pages.debian.net/benchmarksgame/)
  (BSD-3-Clause).
- `closures`, `fib`, `map_churn`, `string_keys`, `strings` and `tak` were
  written here.

Ratios are against LuaJIT with its JIT off, the fastest interpreter we know
of.

Judge a change by the instruction counts, which `run.py` prints first. They
repeat to about one part in a thousand from run to run, so a change smaller
than that is not a change. Cycles are printed beside the instructions, and
time last: on a busy machine time is noise.

To judge a change to the VM, save a run before it and compare after it:

    benches/run.py --runtimes scarlet --json before.json
    benches/run.py --runtimes scarlet --baseline before.json

The second prints how far each benchmark's instruction count moved. It also
prints the VM's own counts for each benchmark, which are exact: how many VM
instructions ran, which kinds ran most, and how many heap cells were made and
reused. They come from `scarlet run --counters FILE`, and
`scarlet dis --instrs FILE` shows the instructions they count.
