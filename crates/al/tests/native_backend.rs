//! The native (Cranelift) backend's behavioral contract, pinned end-to-end
//! against the interpreter as the differential oracle.
//!
//! Every test here runs the same program twice — `AL_NATIVE=off` (interpret
//! everything) and `AL_NATIVE=native` (compile every eligible function) — and
//! asserts the observable output is byte-for-byte identical. The env vars are
//! set explicitly per child, overriding whatever mode the surrounding
//! `cargo test` run inherited, so these tests pin the *cross-mode* contract
//! no matter which mode the rest of the suite is exercising.

mod common;

use common::{AlOutput, Project, wait_or_kill};

/// `al <args..>` with explicit env overrides, bounded like `common::run_al`.
fn run_al_env(args: &[&str], envs: &[(&str, &str)]) -> AlOutput {
    let bin = env!("CARGO_BIN_EXE_al");
    let mut cmd = std::process::Command::new(bin);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let child = cmd.spawn().expect("spawn al");
    let out = wait_or_kill(child, common::CHILD_TIMEOUT_SECS);
    assert!(
        out.status.code().is_some(),
        "`al {args:?}` died by signal (wedged or crashed)\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    AlOutput {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        success: out.status.success(),
        code: out.status.code(),
    }
}

/// Run `src` under one AL_NATIVE mode. `schedulers` pins AL_SCHEDULERS when
/// the test's scheduling shape matters (fairness, park/resume).
fn run_mode(
    proj: &Project,
    name: &str,
    src: &str,
    mode: &str,
    schedulers: Option<u32>,
) -> AlOutput {
    let path = proj.dir.join(name);
    std::fs::write(&path, src).unwrap();
    let path = path.to_string_lossy().into_owned();
    let scheds = schedulers.map(|n| n.to_string());
    let mut envs: Vec<(&str, &str)> = vec![("AL_NATIVE", mode)];
    if let Some(s) = scheds.as_deref() {
        envs.push(("AL_SCHEDULERS", s));
    }
    run_al_env(&["run", &path], &envs)
}

/// The oracle: off vs native must be observably identical — same stdout, same
/// exit code, and a clean (empty) stderr in both modes. Returns the shared
/// stdout for follow-up exact-value assertions.
fn assert_parity(tag: &str, src: &str, schedulers: Option<u32>) -> String {
    let proj = Project::new(tag);
    let off = run_mode(&proj, "prog.al", src, "off", schedulers);
    let native = run_mode(&proj, "prog.al", src, "native", schedulers);
    let dump = |m: &str, o: &AlOutput| {
        format!(
            "--- {m}: exit {:?} ---\n--- stdout ---\n{}--- stderr ---\n{}",
            o.code, o.stdout, o.stderr
        )
    };
    assert!(
        off.success,
        "[{tag}] interpreter run failed:\n{src}\n{}",
        dump("off", &off)
    );
    assert!(
        native.success,
        "[{tag}] native run failed:\n{src}\n{}",
        dump("native", &native)
    );
    assert!(
        off.stderr.is_empty() && native.stderr.is_empty(),
        "[{tag}] stderr must stay empty by default:\n{}\n{}",
        dump("off", &off),
        dump("native", &native)
    );
    assert_eq!(
        off.stdout,
        native.stdout,
        "[{tag}] native output diverged from the interpreter for:\n{src}\n{}\n{}",
        dump("off", &off),
        dump("native", &native)
    );
    off.stdout
}

/// (a) interp -> native -> interp -> native sandwich.
///
/// The toplevel is always interpreted (strings, println). `outer` and `leaf`
/// are Int-only and native-eligible; `middle` builds an array (`Atom::Ctor`,
/// outside A0 coverage) so it must interpret. The call chain therefore
/// crosses the boundary in both directions twice, exercising the
/// interp->native entry and the native->interpreter-only frame-floor shim.
#[test]
fn sandwich_interp_native_interp_returns_correct_results() {
    let src = "import al/array

fn leaf(n Int) Int {
\tif n < 2 { 1 } else { n * leaf(n - 1) }
}

fn middle(n Int) Int {
\txs = [n, n + n]
\tleaf(array.length(xs) + n)
}

fn outer(n Int) Int {
\tmiddle(n) + leaf(n) + middle(n + 1)
}

println(outer(6))
";
    // leaf(8) + leaf(6) + leaf(9) = 40320 + 720 + 362880
    let out = assert_parity("sandwich", src, None);
    assert_eq!(out, "403920\n");
}

/// (b) a native caller whose interpreted callee PARKS.
///
/// `pause` calls the @vm sleep op, so it interprets; `caller` is
/// native-eligible. When `pause` parks, the suspension must unwind through
/// `caller`'s native frame (Status return, no native frame surviving the
/// park) and the resume must re-enter with `x` — a local bound *before* the
/// parking call and used after it — intact in its frame slot.
#[test]
fn native_caller_parks_and_resumes_through_interpreted_callee() {
    let src = "import al/scheduler

fn pause(n Int) Int {
\tscheduler.sleep(30)
\tn + 1
}

fn caller(a Int, b Int) Int {
\tx = a * 1000 + b
\ty = pause(x)
\tx + y + a
}

println(caller(4, 321))
";
    // x = 4321, y = 4322, + 4
    let out = assert_parity("native_park", src, Some(1));
    assert_eq!(out, "8647\n");
}

/// (c) scheduling fairness: reds accounting parity.
///
/// One scheduler, two processes. The first spins in a self-tail loop —
/// natively, a compiled loop back-edge — for thousands of reduction budgets;
/// the second just prints. If the native back-edge checkpoints reductions
/// like the interpreter's TailCallSelf does, the spinner yields long before
/// finishing and the sibling's line lands first. A native loop that never
/// yields inverts the order (and is exactly the starvation this pins).
#[test]
fn fairness_native_self_tail_loop_yields_to_sibling() {
    let src = "import al/scheduler

fn spin(n Int, acc Int) Int {
\tif n == 0 { acc } else { spin(n - 1, acc + 1) }
}

scheduler.spawn_local(fn() {
\tprintln(spin(20000000, 0))
})
scheduler.spawn_local(fn() {
\tprintln('sibling progressed')
})
";
    let out = assert_parity("native_fairness", src, Some(1));
    assert_eq!(
        out, "sibling progressed\n20000000\n",
        "the spinner must yield at reduction checkpoints, letting the sibling run first"
    );
}

/// (d) Int overflow spill parity past ±2^47.
///
/// AL Ints are NaN-boxed with a 47-bit payload; past that they spill to a
/// boxed representation, and arithmetic wraps at i64 exactly like the
/// interpreter's AddInt/SubInt/MulInt (`wrapping_*`). Every line below
/// crosses the spill boundary or the i64 wrap, and the expected strings are
/// the interpreter's own output — pinned literally so *both* modes are held
/// to the semantics, not just to each other.
#[test]
fn int_overflow_spill_is_identical_native_vs_off() {
    let src = "fn fact(n Int, acc Int) Int {
\tif n < 2 { acc } else { fact(n - 1, acc * n) }
}

fn fib_iter(n Int, a Int, b Int) Int {
\tif n == 0 { a } else { fib_iter(n - 1, b, a + b) }
}

println(fact(20, 1))
println(fact(30, 1))
println(fib_iter(90, 0, 1))
println(fib_iter(100, 0, 1))
println(140737488355327 + 1)
println({0 - 140737488355328} - 1)
println(12345678901 * 987654321)
";
    let out = assert_parity("native_spill", src, None);
    assert_eq!(
        out,
        "2432902008176640000\n\
         -8764578968847253504\n\
         2880067194370816120\n\
         3736710778780434371\n\
         140737488355328\n\
         -140737488355329\n\
         -6253480961458370395\n"
    );
}

// ---- (e) differential fuzz smoke ------------------------------------------

/// xorshift64 — deterministic, seedable, no dependency.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed | 1)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Constants straddling the NaN-box payload boundary and the i64 wrap, so
/// random programs hit the spill and overflow paths, not just small ints.
const BOUNDARY: [i64; 8] = [
    140737488355327,
    140737488355328,
    -140737488355328,
    -140737488355329,
    4611686018427387905,
    2432902008176640000,
    -2432902008176640000,
    999999937,
];

/// Render an Int literal as an operand. Negative literals are wrapped as
/// `{0 - n}` so they slot into any operand position without a unary-minus
/// precedence question.
fn lit(v: i64) -> String {
    if v < 0 {
        format!("{{0 - {}}}", (v as i128).unsigned_abs())
    } else {
        format!("{v}")
    }
}

/// A random operand: a live temp, a parameter, or a constant.
fn operand(r: &mut Rng, temps: usize) -> String {
    match r.below(4) {
        0 if temps > 0 => format!("t{}", r.below(temps as u64)),
        1 => lit(BOUNDARY[r.below(BOUNDARY.len() as u64) as usize]),
        2 => "a".to_string(),
        3 => "b".to_string(),
        _ => lit(r.below(199) as i64 - 99),
    }
}

/// Shared preamble prepended to every fuzz program: an enum with mixed-arity
/// variants, a recursive linked list, and helpers whose bodies are the A1
/// heap shapes — `Atom::Ctor` (fresh alloc in `mk_pick`/`chain_build`, in-place
/// reuse in `chain_map`), `CorePat::Ctor` variant dispatch with payload binds,
/// closure parameters called dynamically, and two reuse-loop shapes:
/// `spin_reuse` constructs inline and consumes per iteration, so its reuse
/// slot crosses the self-tail back-edge (the `dot_loop` pairing), while
/// `spin_step`'s match arms reconstruct the matched enum and self-tail-recurse,
/// putting `Reuse`+`MakeEnumPayload` right before `TailCallSelf` (the
/// `collapse_tail_frame_self` pairing). The generated `fN` bodies and the
/// toplevel call into these at random operands.
const PREAMBLE: &str = "\
type Pick {\n\
\tOne(x Int)\n\
\tTwo(x Int, y Int)\n\
\tZero\n\
}\n\
\n\
type Chain {\n\
\tCNil\n\
\tCCons(h Int, t Chain)\n\
}\n\
\n\
fn mk_pick(k Int, x Int, y Int) Pick {\n\
\tmatch k % 3 {\n\
\t\t0 -> One(x)\n\
\t\t1 -> Two(x, y)\n\
\t\telse -> Zero\n\
\t}\n\
}\n\
\n\
fn pick_val(p Pick, d Int) Int {\n\
\tmatch p {\n\
\t\tOne(x) -> x + 1\n\
\t\tTwo(x, y) -> x * 2 + y\n\
\t\tZero -> d\n\
\t}\n\
}\n\
\n\
fn chain_build(n Int, x Int) Chain {\n\
\tif n <= 0 { CNil } else { CCons(x + n, chain_build(n - 1, x)) }\n\
}\n\
\n\
fn chain_map(xs Chain, f fn(Int) Int) Chain {\n\
\tmatch xs {\n\
\t\tCNil -> CNil\n\
\t\tCCons(h, t) -> CCons(f(h), chain_map(t, f))\n\
\t}\n\
}\n\
\n\
fn chain_sum(xs Chain) Int {\n\
\tmatch xs {\n\
\t\tCNil -> 0\n\
\t\tCCons(h, t) -> h + chain_sum(t)\n\
\t}\n\
}\n\
\n\
fn spin_reuse(n Int, acc Int, s Int) Int {\n\
\tif n <= 0 {\n\
\t\tacc\n\
\t} else {\n\
\t\tp = Two(n + s, n * 3)\n\
\t\tspin_reuse(n - 1, acc + pick_val(p, s), s)\n\
\t}\n\
}\n\
\n\
fn spin_step(p Pick, n Int, acc Int) Int {\n\
\tmatch p {\n\
\t\tOne(x) -> if n <= 0 { acc + x } else { spin_step(One(x + 1), n - 1, acc + x) }\n\
\t\tTwo(x, y) -> if n <= 0 { acc + x + y } else { spin_step(Two(y, x + 1), n - 1, acc + y) }\n\
\t\tZero -> acc\n\
\t}\n\
}\n";

/// A small positive literal for loop/list lengths, keeping recursion depth
/// and iteration counts bounded (operands may be huge boundary constants,
/// so counts are never drawn from `operand`).
fn small_count(r: &mut Rng) -> u64 {
    1 + r.below(24)
}

/// One generated function: a chain of let-bound statements over Int operands
/// — binops (division and modulo are total in AL: x/0=0, x%0=x, so nothing
/// needs guarding), if-over-comparison, match on Int literals, enum
/// constructor + variant matches (payload binds over a runtime-selected
/// variant), closures capturing locals and called dynamically, reuse-loop
/// (both spin_reuse's back-edge slot and spin_step's match-reconstruct) and
/// list-map calls into the preamble, and known calls to previously
/// generated functions. Every temp and both params fold into the returned
/// sum (AL errors on unused bindings), and each function past the first
/// opens with a call to its predecessor so cross-function calls are always
/// present.
fn gen_fn(r: &mut Rng, idx: usize) -> String {
    let mut body = String::new();
    let stmts = 4 + r.below(7) as usize;
    for t in 0..stmts {
        let rhs = match r.below(12) {
            _ if t == 0 && idx > 0 => format!("f{}(a, b)", idx - 1),
            0..=3 => {
                let op = ["+", "-", "*", "/", "%"][r.below(5) as usize];
                format!("{{ {} }} {op} {{ {} }}", operand(r, t), operand(r, t))
            }
            4 => {
                let cmp = ["<", "<=", ">", ">=", "==", "!="][r.below(6) as usize];
                format!(
                    "if {} {cmp} {} {{ {} }} else {{ {} }}",
                    operand(r, t),
                    operand(r, t),
                    operand(r, t),
                    operand(r, t)
                )
            }
            5 => format!(
                "match {} % 3 {{\n\t\t0 -> {}\n\t\t1 -> {}\n\t\telse -> {}\n\t}}",
                operand(r, t),
                operand(r, t),
                operand(r, t),
                operand(r, t)
            ),
            // Enum ctor at a runtime-selected variant, immediately consumed
            // by an exhaustive variant match with payload binds.
            6 => format!(
                "match mk_pick({}, {}, {}) {{\n\
                 \t\tOne(x) -> x + {}\n\
                 \t\tTwo(x, y) -> x * y - {}\n\
                 \t\tZero -> {}\n\
                 \t}}",
                operand(r, t),
                operand(r, t),
                operand(r, t),
                operand(r, t),
                operand(r, t),
                operand(r, t)
            ),
            // A closure capturing surrounding locals, bound and called twice
            // dynamically within a block.
            7 => format!(
                "{{\n\t\tg = fn(x Int) x * {{ {} }} + {}\n\t\tg({}) + g({})\n\t}}",
                operand(r, t),
                lit(r.below(199) as i64 - 99),
                operand(r, t),
                operand(r, t)
            ),
            // Loop-carried reuse: inline ctor + match-consume per iteration,
            // the reuse slot crossing spin_reuse's self-tail back-edge.
            8 => format!(
                "spin_reuse({}, {}, {})",
                small_count(r),
                operand(r, t),
                operand(r, t)
            ),
            // Build a list, map it through a capturing closure (Cons-for-Cons
            // reuse in chain_map), fold it back to an Int.
            9 => format!(
                "chain_sum(chain_map(chain_build({}, {}), fn(x Int) x * {} + {{ {} }}))",
                small_count(r),
                operand(r, t),
                1 + r.below(9),
                operand(r, t)
            ),
            // Match-reconstruct reuse: spin_step's arms rebuild the matched
            // enum and self-tail-recurse (Reuse right before TailCallSelf),
            // seeded with a runtime-selected variant.
            10 => format!(
                "spin_step(mk_pick({}, {}, {}), {}, {})",
                operand(r, t),
                operand(r, t),
                operand(r, t),
                small_count(r),
                operand(r, t)
            ),
            _ if idx > 0 => {
                let callee = r.below(idx as u64);
                format!("f{callee}({}, {})", operand(r, t), operand(r, t))
            }
            _ => format!("{{ {} }} + {{ {} }}", operand(r, t), operand(r, t)),
        };
        body.push_str(&format!("\tt{t} = {rhs}\n"));
    }
    let sum = (0..stmts)
        .map(|t| format!("t{t}"))
        .chain(["a".to_string(), "b".to_string()])
        .collect::<Vec<_>>()
        .join(" + ");
    body.push_str(&format!("\t{sum}\n"));
    format!("fn f{idx}(a Int, b Int) Int {{\n{body}}}\n")
}

/// One whole program: the heap-shape preamble, 2-3 chained functions,
/// toplevel prints of the last one at random constant arguments, and one
/// print that composes every preamble shape (enum ctor/match, closure,
/// list-map reuse, both reuse loops) so no program skips the A1 coverage
/// even when the per-statement dice never landed on those shapes.
fn gen_program(r: &mut Rng) -> String {
    let nfns = 2 + r.below(2) as usize;
    let mut src = String::from(PREAMBLE);
    src.push('\n');
    for i in 0..nfns {
        src.push_str(&gen_fn(r, i));
        src.push('\n');
    }
    for _ in 0..3 {
        let a = lit(r.below(199) as i64 - 99);
        let b = lit(BOUNDARY[r.below(BOUNDARY.len() as u64) as usize]);
        src.push_str(&format!("println(f{}({a}, {b}))\n", nfns - 1));
    }
    src.push_str(&format!(
        "println(spin_step(mk_pick({}, {}, {}), {}, spin_reuse({}, chain_sum(chain_map(chain_build({}, {}), fn(x Int) x + {})), pick_val(mk_pick({}, {}, {}), {}))))\n",
        lit(r.below(199) as i64 - 99),
        lit(r.below(199) as i64 - 99),
        lit(r.below(199) as i64 - 99),
        small_count(r),
        small_count(r),
        small_count(r),
        lit(BOUNDARY[r.below(BOUNDARY.len() as u64) as usize]),
        lit(r.below(199) as i64 - 99),
        lit(r.below(199) as i64 - 99),
        lit(r.below(199) as i64 - 99),
        lit(r.below(199) as i64 - 99),
        lit(r.below(199) as i64 - 99),
    ));
    src
}

/// (e) ~200 seeded random small programs — Int expressions plus the A1 heap
/// shapes (enum ctor/variant match, closures, list-map and loop-carried
/// reuse): native output must equal the interpreter's byte-for-byte.
/// Deterministic — the seed is fixed, and any divergence panics with the
/// program index and full source so the case reproduces exactly.
#[test]
fn differential_fuzz_native_matches_interpreter() {
    const SEED: u64 = 0x5eed_a10c_0de5_eed1;
    const PROGRAMS: usize = 200;
    let mut r = Rng::new(SEED);
    let proj = Project::new("native_fuzz");
    for i in 0..PROGRAMS {
        let src = gen_program(&mut r);
        let off = run_mode(&proj, "fuzz.al", &src, "off", None);
        assert!(
            off.success && off.stderr.is_empty(),
            "fuzz #{i} (seed {SEED:#x}): interpreter rejected a generated program:\n{src}\n\
             --- stdout ---\n{}--- stderr ---\n{}",
            off.stdout,
            off.stderr
        );
        let native = run_mode(&proj, "fuzz.al", &src, "native", None);
        assert!(
            native.success && native.stderr.is_empty(),
            "fuzz #{i} (seed {SEED:#x}): native run failed:\n{src}\n\
             --- stdout ---\n{}--- stderr ---\n{}",
            native.stdout,
            native.stderr
        );
        assert_eq!(
            off.stdout, native.stdout,
            "fuzz #{i} (seed {SEED:#x}): native diverged from the interpreter for:\n{src}\n\
             --- off ---\n{}--- native ---\n{}",
            off.stdout, native.stdout
        );
    }
}

/// (f) `al dis <file> --native fib` prints the CLIF (and works at all).
///
/// The CLIF body is the contract — `block0` is the entry block every
/// non-trivial compiled function has, and the function's name must appear so
/// the listing is attributable.
#[test]
fn dis_native_prints_clif_for_fib() {
    let proj = Project::new("native_dis");
    let path = proj.dir.join("fib.al");
    std::fs::write(
        &path,
        "fn fib(n Int) Int {\n\tif n < 2 { n } else { fib(n - 1) + fib(n - 2) }\n}\n\nprintln(fib(10))\n",
    )
    .unwrap();
    let path = path.to_string_lossy().into_owned();
    let out = run_al_env(
        &["dis", &path, "--native", "fib"],
        &[("AL_NATIVE", "native")],
    );
    assert!(
        out.success,
        "`al dis --native fib` failed:\n--- stdout ---\n{}--- stderr ---\n{}",
        out.stdout, out.stderr
    );
    assert!(
        out.stdout.contains("fib"),
        "the listing must name the function:\n{}",
        out.stdout
    );
    assert!(
        out.stdout.contains("block0"),
        "no CLIF body (expected an entry `block0`):\n{}",
        out.stdout
    );
}
