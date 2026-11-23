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
of. Read the instruction counts before the times: they repeat to several
digits, and time on a busy machine does not.
