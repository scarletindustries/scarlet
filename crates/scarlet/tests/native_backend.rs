//! These tests check the native (Cranelift) backend against the language's
//! intended semantics, not against a second engine. Every test runs a program
//! once, in the one production configuration with lazy warmup, and asserts the
//! exact bytes it must print. The generator computes the fuzz test's expected
//! output in Rust (wrapping i64, `x/0 = 0`, `x%0 = x`), so a miscompile fails
//! against ground truth even if it is consistent.
//!
//! Warmup is part of what we test. A body compiles after
//! `NativeTable::WARM_CALLS` interpreted calls, so each scenario is built to
//! cross that threshold mid-run. That way the program itself crosses from
//! interpreter to native and back, with no mode switch.

mod common;

use common::{AlOutput, Project, wait_or_kill};

/// Run `al <args..>` with explicit env overrides and a time limit, like
/// `common::run_al`.
fn run_al_env(args: &[&str], envs: &[(&str, &str)]) -> AlOutput {
    let bin = env!("CARGO_BIN_EXE_scarlet");
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

/// Run `src` once. `schedulers` pins SCARLET_SCHEDULERS when the test's
/// scheduling shape matters.
fn run(proj: &Project, name: &str, src: &str, schedulers: Option<u32>) -> AlOutput {
    let path = proj.dir.join(name);
    std::fs::write(&path, src).unwrap();
    let path = path.to_string_lossy().into_owned();
    let scheds = schedulers.map(|n| n.to_string());
    let mut envs: Vec<(&str, &str)> = Vec::new();
    if let Some(s) = scheds.as_deref() {
        envs.push(("SCARLET_SCHEDULERS", s));
    }
    run_al_env(&["run", &path], &envs)
}

/// One run must succeed, keep stderr empty, and print exactly `expected`.
fn assert_prints(tag: &str, src: &str, schedulers: Option<u32>, expected: &str) {
    let proj = Project::new(tag);
    let out = run(&proj, "prog.sca", src, schedulers);
    assert!(
        out.success,
        "[{tag}] run failed (exit {:?}) for:\n{src}\n--- stdout ---\n{}--- stderr ---\n{}",
        out.code, out.stdout, out.stderr
    );
    assert!(
        out.stderr.is_empty(),
        "[{tag}] stderr must stay empty by default:\n{}",
        out.stderr
    );
    assert_eq!(
        out.stdout, expected,
        "[{tag}] wrong output for:\n{src}\n--- stdout ---\n{}",
        out.stdout
    );
}

/// (a) An interp -> native -> interp sandwich. `leaf`'s recursion crosses the
/// warmth threshold mid-run (leaf(8) alone is 8 calls), so later `leaf` frames
/// run native while `middle`/`outer` stay interpreted. So one program crosses
/// the boundary in both directions.
#[test]
fn sandwich_crosses_the_warmup_boundary_mid_run() {
    let src = "import scarlet/array

fn leaf(n Int) Int {
\tif n < 2 { 1 } else { n * leaf(n - 1) }
}

fn middle(n Int) Int {
\txs = [n, n + n]
\tleaf(array::length(xs) + n)
}

fn outer(n Int) Int {
\tmiddle(n) + leaf(n) + middle(n + 1)
}

pub fn main() {
\tprintln(outer(6))
}
";
    // leaf(8) + leaf(6) + leaf(9) = 40320 + 720 + 362880
    assert_prints("sandwich", src, None, "403920\n");
}

/// (b) A native caller whose interpreted callee parks. `caller` warms over the
/// first 16 iterations, and `pause` is reached only on the last 4. So by then a
/// *native* `caller` frame sits under an interpreted callee that parks on a
/// timer. The suspension must unwind through the native frame, and the resume
/// must find `x` intact. `x` is bound before the parking call and used after.
#[test]
#[ignore = "needs the JIT and processes"]
fn native_caller_parks_and_resumes_through_interpreted_callee() {
    let src = "import scarlet/process

fn pause(n Int) Int {
\tprocess::sleep(1)
\tn + 1
}

fn caller(i Int) Int {
\tx = i * 1000 + 7
\ty = if i <= 4 { pause(x) } else { x + 1 }
\tx + y + i
}

fn drive(i Int, acc Int) Int {
\tif i == 0 { acc } else { drive(i - 1, acc + caller(i)) }
}

pub fn main() {
\tprintln(drive(24, 0))
}
";
    // caller(i) = 2*(1000i + 7) + 1 + i = 2001i + 15;
    // sum over i = 1..=24: 2001*300 + 15*24 = 600660.
    assert_prints("native_park", src, Some(1), "600660\n");
}

/// (c) Scheduling fairness, with one scheduler running a self-tail spinner and
/// a printer. `kick` warms `spin` past the threshold with plain calls, so the
/// 20M-step loop runs native. Its compiled back-edge must checkpoint reductions
/// like the interpreter's TailCallSelf. Otherwise the spinner starves the
/// sibling and the output order flips.
#[test]
#[ignore = "needs the JIT and processes"]
fn fairness_native_self_tail_loop_yields_to_sibling() {
    let src = "import scarlet/process

fn spin(n Int, acc Int) Int {
\tif n == 0 { acc } else { spin(n - 1, acc + 1) }
}

fn kick(n Int) Int {
\tif n == 0 { 0 } else { spin(1, 0) + kick(n - 1) }
}

pub fn main() {
\t_ = process::spawn(fn() {
\t\tprintln(kick(10) + spin(20000000, 0))
\t})
\t_ = process::spawn(fn() {
\t\tprintln('sibling progressed')
\t})
}
";
    assert_prints(
        "native_fairness",
        src,
        Some(1),
        "sibling progressed\n20000010\n",
    );
}

/// A body entered exactly once must still warm. The self-tail back-edge counts
/// toward the threshold, and the edge that crosses it compiles the body and
/// moves the running frame onto the fresh entry mid-loop. The debug line shows
/// the compile fired inside the single call.
#[test]
#[ignore = "needs the JIT"]
fn single_call_loop_warms_and_flips_mid_run() {
    let src = "fn spin(n Int, acc Int) Int {
\tif n == 0 { acc } else { spin(n - 1, acc + 1) }
}

pub fn main() {
\tprintln(spin(3000000, 0))
}
";
    let proj = Project::new("midloop_warm");
    let path = proj.dir.join("prog.sca");
    std::fs::write(&path, src).unwrap();
    let path = path.to_string_lossy().into_owned();
    let out = run_al_env(&["run", &path], &[("SCARLET_NATIVE_DEBUG", "1")]);
    assert!(out.success, "run failed:\n{}", out.stderr);
    assert_eq!(out.stdout, "3000000\n");
    assert!(
        out.stderr.contains("warmed"),
        "a single-call loop must warm via its back-edge; stderr:\n{}",
        out.stderr
    );
}

/// This runs a qualified constructor's match arm beside a catch-all, hot. The
/// ladder tests in `modules.rs` all run cold, and cold is the one setup where
/// we can't see the two engines disagree. A ladder that tested the wrong name
/// sent the interpreted iterations to the catch-all but not the compiled ones.
/// So one program returned two answers inside one process, with no error on
/// either stream.
///
/// `hit` is a separate body so it warms on call count, and 20,000 iterations
/// put both sides of the threshold in the sum.
///
/// `make` alternates the two constructors so the count catches a wrong arm in
/// both directions. A comparison that is wrongly false loses `Green`s and comes
/// in under 10,000, and one that is wrongly true picks up `Red`s and comes in
/// over. If `make` returned `Green` every time, a comparison that always said
/// yes would still print the right count, so only false negatives would be
/// caught. `assert_warmed` checks that `hit` itself warmed, by its fn index.
/// `drive` and `make` warm here too, so a check that "some body warmed" passes
/// even when the body holding the match never leaves the interpreter.
#[test]
#[ignore = "needs the JIT"]
fn a_warmed_qualified_match_agrees_across_the_warmup_boundary() {
    let src = "import ./color

fn hit(c color::Color) Int {
	match c {
		color::Green(_s) -> 1
		color::Red -> 0
	}
}

fn drive(i Int, acc Int) Int {
	if i == 0 { acc } else { drive(i - 1, acc + hit(color::make(i))) }
}

pub fn main() {
	println(drive(20000, 0))
}
";
    let proj = Project::new("qual_warm");
    proj.write(
        "color.sca",
        "pub type Color {\n\tRed\n\tGreen(shade Int)\n}\n\npub fn make(n Int) Color {\n\tif n % 2 == 0 { Red } else { Green(n) }\n}\n",
    );
    let path = proj.dir.join("prog.sca");
    std::fs::write(&path, src).unwrap();
    let path = path.to_string_lossy().into_owned();
    let out = run_al_env(&["run", &path], &[("SCARLET_NATIVE_DEBUG", "1")]);
    assert!(out.success, "run failed:\n{}", out.stderr);
    // 10,000 odd `i` in 1..=20,000, so every `Green` and no `Red`.
    assert_eq!(
        out.stdout, "10000\n",
        "a qualified arm must match the same constructors under both engines; stderr:\n{}",
        out.stderr
    );
    assert_warmed(&out.stderr, "hit");
}

/// (d) Ints spill past ±2^47, where they leave the NaN-box payload, and past
/// 64 bits, where they stay exact. The recursion warms both functions mid-run,
/// so the expected values must hold across the switch to native.
#[test]
fn int_overflow_spill_prints_pinned_values() {
    let src = "fn fact(n Int, acc Int) Int {
\tif n < 2 { acc } else { fact(n - 1, acc * n) }
}

fn fib_iter(n Int, a Int, b Int) Int {
\tif n == 0 { a } else { fib_iter(n - 1, b, a + b) }
}

pub fn main() {
\tprintln(fact(20, 1))
\tprintln(fact(30, 1))
\tprintln(fib_iter(90, 0, 1))
\tprintln(fib_iter(100, 0, 1))
\tprintln(140737488355327 + 1)
\tprintln({0 - 140737488355328} - 1)
\tprintln(12345678901 * 987654321)
}
";
    assert_prints(
        "native_spill",
        src,
        None,
        "2432902008176640000\n\
         265252859812191058636308480000000\n\
         2880067194370816120\n\
         354224848179261915075\n\
         140737488355328\n\
         -140737488355329\n\
         12193263112251181221\n",
    );
}

/// How many times each bitwise loop in [`bitwise_ops_survive_the_native_bridge`]
/// runs. It's far past `NativeTable::WARM_CALLS`, so all but the first few
/// steps run compiled.
const BITWISE_ROUNDS: i64 = 3_000_000;

/// `rotor`'s step: rotate `bitwise_not(acc)` left by 7, then fold in `n`.
/// We write it from the language's stated semantics rather than call into the
/// VM, so it's ground truth and not a second opinion from the same code.
fn rotor_step(acc: i64, n: i64) -> i64 {
    let a = !acc;
    let hi = ((a as u64) << 7) as i64;
    let lo = (a >> 57) & 127;
    (hi | lo) ^ n
}

/// `mixer`'s step. `0x4444…` is a strict subset of the `0xCCCC…` mask, so the
/// bits that OR adds survive the AND and reach the printed value.
fn mixer_step(acc: i64, n: i64) -> i64 {
    ((acc | 0x4444_4444_4444_4444_u64 as i64) & 0xCCCC_CCCC_CCCC_CCCC_u64 as i64) ^ n
}

/// Apply `step` for `n = BITWISE_ROUNDS, …, 1`, matching the tail recursion.
fn fold_rounds(seed: i64, step: fn(i64, i64) -> i64) -> i64 {
    let mut acc = seed;
    let mut n = BITWISE_ROUNDS;
    while n != 0 {
        acc = step(acc, n);
        n -= 1;
    }
    acc
}

/// The `FuncIdx` `SCARLET_NATIVE_DEBUG` assigned to `name`, from its
/// `al-native: selected fn#N <name>` line. Panics if the body was never even
/// planned for compilation.
fn selected_index(stderr: &str, name: &str) -> String {
    stderr
        .lines()
        .filter_map(|l| l.strip_prefix("al-native: selected "))
        .filter_map(|rest| rest.split_once(' '))
        .find(|(_, fname)| *fname == name)
        .map(|(idx, _)| idx.to_owned())
        .unwrap_or_else(|| {
            panic!("`{name}` was never selected for native compilation; stderr:\n{stderr}")
        })
}

/// Assert that `name` really crossed to native in this run, meaning it was
/// planned AND warmed. The bitwise test used to lack this control. If you raise
/// `NativeTable::WARM_CALLS` past the loop count, the JIT never runs, and a
/// test named for the native bridge must notice.
fn assert_warmed(stderr: &str, name: &str) {
    let idx = selected_index(stderr, name);
    let needle = format!("al-native: warmed {idx} in");
    assert!(
        stderr.contains(&needle),
        "`{name}` ({idx}) never warmed to native, so nothing crossed the bridge; stderr:\n{stderr}"
    );
}

/// (e) The integer bitwise ops across the switch to native. They lower as
/// `OpCoverage::Bridge`, so the interpreter-only golden program can't see a
/// wrong arm in `run_bridge_op` (`bitwise.sca` compiles only `ok`). This test
/// is the only thing that keeps the interpreter and the JIT in step on these
/// six ops.
///
/// **No operand here is an identity element, and the test depends on that.**
/// The first version made every loop an identity (`x ^ k ^ k`, `x << 1 >> 1`,
/// `(x & -1) | 0`). It stayed green with `BIT_OR` wired to `bit_xor`, because
/// `0` is the identity of OR *and* of XOR, so no later value can tell those two
/// arms apart. `-1` does the same for AND and XOR. That planted bug was caught
/// only because the number of natively run iterations happened to be odd, and
/// moving `WARM_CALLS` by one would have hidden it.
///
/// So both loops evolve state instead. They fold `n` in on every step so there
/// is no fixed point to collapse onto, and they pick operands that tell each
/// arm apart from its siblings:
///
/// - `rotor` rotates `bitwise_not(acc)` left by 7, using `shift_left 7`,
///   `shift_right 57` and `and 127`. The two shift counts differ, so SHL and
///   SHR never see the same operand, and neither count is 0.
/// - `mixer` ORs in `0x4444…`, ANDs with `0xCCCC…`, and XORs `n`. The OR bits
///   are a strict subset of the AND mask, so the AND doesn't mask away what OR
///   adds. The accumulator carries those bits into the next round's OR, and
///   that's what makes OR and XOR disagree there.
///
/// We recompute the expected output in Rust from the same recurrence, so a
/// miscompile fails against ground truth rather than against a second run.
#[test]
#[ignore = "needs the JIT, and its answers assume 64-bit Ints"]
fn bitwise_ops_survive_the_native_bridge() {
    let src = "import scarlet/int

fn rotor(n Int, acc Int) Int {
\ta = int::bitwise_not(acc)
\thi = int::bitwise_shift_left(a, 7)
\tlo = int::bitwise_and(int::bitwise_shift_right(a, 57), 127)
\tnext = int::bitwise_xor(int::bitwise_or(hi, lo), n)
\tif n == 0 { acc } else { rotor(n - 1, next) }
}

fn mixer(n Int, acc Int) Int {
\ta = int::bitwise_or(acc, 4919131752989213764)
\tb = int::bitwise_and(a, 0 - 3689348814741910324)
\tnext = int::bitwise_xor(b, n)
\tif n == 0 { acc } else { mixer(n - 1, next) }
}

pub fn main() {
\tprintln(rotor(ROUNDS, 12345))
\tprintln(mixer(ROUNDS, 0 - 42))
}
";
    // Keep one source of truth for the count, because the Rust fold below must
    // run exactly as many steps as the program does.
    let src = src.replace("ROUNDS", &BITWISE_ROUNDS.to_string());
    let expected = format!(
        "{}\n{}\n",
        fold_rounds(12345, rotor_step),
        fold_rounds(-42, mixer_step),
    );

    let proj = Project::new("bitwise_bridge");
    let path = proj.dir.join("prog.sca");
    std::fs::write(&path, src).unwrap();
    let path = path.to_string_lossy().into_owned();
    let out = run_al_env(&["run", &path], &[("SCARLET_NATIVE_DEBUG", "1")]);
    assert!(out.success, "run failed:\n{}", out.stderr);
    assert_eq!(out.stdout, expected, "stderr:\n{}", out.stderr);
    assert_warmed(&out.stderr, "rotor");
    assert_warmed(&out.stderr, "mixer");
}

/// `crypto.random_bytes` across the switch to native. The length tells this
/// arm apart from a sibling that returns some other `Result(Binary, Nil)` of a
/// fixed size. Two draws after warmup must differ, so a constant fill can't
/// hide a wrong arm the way `0` did for OR/XOR in the bitwise test above.
///
/// This does NOT prove cryptographic quality, or that the source is getrandom
/// rather than another CSPRNG. The two-process test below is the one that
/// rejects a fixed-seed userspace generator.
#[test]
#[ignore = "needs the JIT"]
fn random_bytes_survives_the_native_bridge() {
    let src = "import scarlet/binary
import scarlet/crypto

fn take(n Int) Int {
\tmatch crypto::random_bytes(n) {
\t\tOk(b) -> binary::byte_size(b)
\t\tErr(Nil) -> 0 - 1
\t}
}

fn walk(i Int, n Int) Int {
\tgot = take(n)
\tif got != n {
\t\tgot
\t} else if i <= 1 {
\t\tn
\t} else {
\t\twalk(i - 1, n)
\t}
}

fn pair(_n Int) Bool {
\tmatch (crypto::random_bytes(16), crypto::random_bytes(16)) {
\t\t(Ok(a), Ok(b)) -> a != b
\t\t(Err(Nil), _) | (Ok(_), Err(Nil)) -> False
\t}
}

fn all_differ(i Int) Bool {
\tif i <= 0 {
\t\tTrue
\t} else if pair(i) {
\t\tall_differ(i - 1)
\t} else {
\t\tFalse
\t}
}

pub fn main() {
\tprintln(walk(32, 0))
\tprintln(walk(32, 1))
\tprintln(walk(32, 16))
\tprintln(walk(32, 64))
\tprintln(all_differ(32))
}
";
    let proj = Project::new("random_bytes_bridge");
    let path = proj.dir.join("prog.sca");
    std::fs::write(&path, src).unwrap();
    let path = path.to_string_lossy().into_owned();
    let out = run_al_env(&["run", &path], &[("SCARLET_NATIVE_DEBUG", "1")]);
    assert!(out.success, "run failed:\n{}", out.stderr);
    assert_eq!(
        out.stdout, "0\n1\n16\n64\nTrue\n",
        "stderr:\n{}",
        out.stderr
    );
    assert_warmed(&out.stderr, "take");
    assert_warmed(&out.stderr, "pair");
}

/// Two process runs must not produce the same 16 bytes. A `StdRng` seeded
/// once in the source would pass every in-process check and fail here. The
/// chance that two honest 16-byte CSPRNG draws collide is 2^-128.
#[test]
fn random_bytes_is_not_a_fixed_seed() {
    let src = "import scarlet/binary
import scarlet/crypto

fn dump(b Binary, i Int, size Int) {
\tif i < size {
\t\tprintln(binary::byte_at(b, i))
\t\tdump(b, i + 1, size)
\t} else {
\t\tNil
\t}
}

pub fn main() {
\tmatch crypto::random_bytes(16) {
\t\tOk(b) -> dump(b, 0, binary::byte_size(b))
\t\tErr(Nil) -> println('FAIL')
\t}
}
";
    let proj = Project::new("random_bytes_seed");
    let a = run(&proj, "a.sca", src, None);
    let b = run(&proj, "b.sca", src, None);
    assert!(a.success, "first run failed:\n{}", a.stderr);
    assert!(b.success, "second run failed:\n{}", b.stderr);
    assert_ne!(
        a.stdout, "FAIL\n",
        "first run's OS CSPRNG failed, so this test can't check for a fixed seed"
    );
    assert_ne!(
        a.stdout, b.stdout,
        "two process runs produced the same 16 bytes"
    );
}

/// An xorshift64 generator. It's deterministic and seedable, and needs no
/// dependency.
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
/// generated programs hit the spill and overflow paths.
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

// These are the intended Int semantics in Rust, with two's-complement wrap and
// total div/mod. We check the generated programs against them.

fn wadd(a: i64, b: i64) -> i64 {
    a.wrapping_add(b)
}
fn wsub(a: i64, b: i64) -> i64 {
    a.wrapping_sub(b)
}
fn wmul(a: i64, b: i64) -> i64 {
    a.wrapping_mul(b)
}
fn sdiv(a: i64, b: i64) -> i64 {
    if b == 0 { 0 } else { a.wrapping_div(b) }
}
fn smod(a: i64, b: i64) -> i64 {
    if b == 0 { a } else { a.wrapping_rem(b) }
}

/// Render an Int literal as an operand. Negatives become `{0 - n}` so they
/// slot into any position regardless of unary-minus precedence.
fn lit(v: i64) -> String {
    if v < 0 {
        format!("{{0 - {}}}", (v as i128).unsigned_abs())
    } else {
        format!("{v}")
    }
}

/// One generated expression. It covers enough of the language to exercise
/// arithmetic at the boundary constants, control flow, enum construction and
/// dispatch, closures, reuse loops and cross-function calls. [`Expr::eval`]
/// gives its intended value.
enum Expr {
    Lit(i64),
    Temp(usize),
    A,
    B,
    /// `{ l } op { r }` over `+ - * / %`.
    Bin(usize, Box<Expr>, Box<Expr>),
    /// `if l cmp r { t } else { e }` over `< <= > >= == !=`.
    IfCmp(usize, Box<Expr>, Box<Expr>, Box<Expr>, Box<Expr>),
    /// `match s % 3 { 0 -> a0  1 -> a1  _ -> aw }`.
    Mod3(Box<Expr>, Box<Expr>, Box<Expr>, Box<Expr>),
    /// `match mk_pick(k, x, y) { One(x) -> x + p  Two(x, y) -> x * y - q  Zero -> z }`.
    PickMatch {
        k: Box<Expr>,
        x: Box<Expr>,
        y: Box<Expr>,
        p: Box<Expr>,
        q: Box<Expr>,
        z: Box<Expr>,
    },
    /// `g = fn(x Int) x * { m } + c` called at `a1` and `a2`, summed.
    Closure {
        m: Box<Expr>,
        c: i64,
        a1: Box<Expr>,
        a2: Box<Expr>,
    },
    /// `spin_reuse(n, acc, s)`. See the preamble.
    SpinReuse {
        n: u64,
        acc: Box<Expr>,
        s: Box<Expr>,
    },
    /// `chain_sum(chain_map(chain_build(n, x), fn(x Int) x * m + { c }))`.
    ChainPipe {
        n: u64,
        x: Box<Expr>,
        m: u64,
        c: Box<Expr>,
    },
    /// `pick_val(mk_pick(k, x, y), d)`. See the preamble.
    PickVal {
        k: Box<Expr>,
        x: Box<Expr>,
        y: Box<Expr>,
        d: Box<Expr>,
    },
    /// `spin_step(mk_pick(k, x, y), n, acc)`. See the preamble.
    SpinStep {
        k: Box<Expr>,
        x: Box<Expr>,
        y: Box<Expr>,
        n: u64,
        acc: Box<Expr>,
    },
    /// `f{idx}(a, b)`, a call to an earlier generated function.
    Call(usize, Box<Expr>, Box<Expr>),
}

const BIN_OPS: [&str; 5] = ["+", "-", "*", "/", "%"];
const CMP_OPS: [&str; 6] = ["<", "<=", ">", ">=", "==", "!="];

impl Expr {
    fn render(&self) -> String {
        match self {
            Expr::Lit(v) => lit(*v),
            Expr::Temp(t) => format!("t{t}"),
            Expr::A => "a".into(),
            Expr::B => "b".into(),
            Expr::Bin(op, l, r) => {
                format!("{{ {} }} {} {{ {} }}", l.render(), BIN_OPS[*op], r.render())
            }
            Expr::IfCmp(c, l, r, t, e) => format!(
                "if {} {} {} {{ {} }} else {{ {} }}",
                l.render(),
                CMP_OPS[*c],
                r.render(),
                t.render(),
                e.render()
            ),
            Expr::Mod3(s, a0, a1, aw) => format!(
                "match {} % 3 {{\n\t\t0 -> {}\n\t\t1 -> {}\n\t\t_ -> {}\n\t}}",
                s.render(),
                a0.render(),
                a1.render(),
                aw.render()
            ),
            Expr::PickMatch { k, x, y, p, q, z } => format!(
                "match mk_pick({}, {}, {}) {{\n\
                 \t\tOne(x) -> x + {}\n\
                 \t\tTwo(x, y) -> x * y - {}\n\
                 \t\tZero -> {}\n\
                 \t}}",
                k.render(),
                x.render(),
                y.render(),
                p.render(),
                q.render(),
                z.render()
            ),
            Expr::Closure { m, c, a1, a2 } => format!(
                "{{\n\t\tg = fn(x Int) x * {{ {} }} + {}\n\t\tg({}) + g({})\n\t}}",
                m.render(),
                lit(*c),
                a1.render(),
                a2.render()
            ),
            Expr::SpinReuse { n, acc, s } => {
                format!("spin_reuse({n}, {}, {})", acc.render(), s.render())
            }
            Expr::ChainPipe { n, x, m, c } => format!(
                "chain_sum(chain_map(chain_build({n}, {}), fn(x Int) x * {m} + {{ {} }}))",
                x.render(),
                c.render()
            ),
            Expr::PickVal { k, x, y, d } => format!(
                "pick_val(mk_pick({}, {}, {}), {})",
                k.render(),
                x.render(),
                y.render(),
                d.render()
            ),
            Expr::SpinStep { k, x, y, n, acc } => format!(
                "spin_step(mk_pick({}, {}, {}), {n}, {})",
                k.render(),
                x.render(),
                y.render(),
                acc.render()
            ),
            Expr::Call(f, a, b) => format!("f{f}({}, {})", a.render(), b.render()),
        }
    }

    /// The intended value, mirroring the preamble functions exactly.
    fn eval(&self, a: i64, b: i64, temps: &[i64], fns: &[Vec<Expr>]) -> i64 {
        let ev = |e: &Expr| e.eval(a, b, temps, fns);
        match self {
            Expr::Lit(v) => *v,
            Expr::Temp(t) => temps[*t],
            Expr::A => a,
            Expr::B => b,
            Expr::Bin(op, l, r) => {
                let (l, r) = (ev(l), ev(r));
                match op {
                    0 => wadd(l, r),
                    1 => wsub(l, r),
                    2 => wmul(l, r),
                    3 => sdiv(l, r),
                    _ => smod(l, r),
                }
            }
            Expr::IfCmp(c, l, r, t, e) => {
                let (l, r) = (ev(l), ev(r));
                let cond = match c {
                    0 => l < r,
                    1 => l <= r,
                    2 => l > r,
                    3 => l >= r,
                    4 => l == r,
                    _ => l != r,
                };
                if cond { ev(t) } else { ev(e) }
            }
            Expr::Mod3(s, a0, a1, aw) => match smod(ev(s), 3) {
                0 => ev(a0),
                1 => ev(a1),
                _ => ev(aw),
            },
            Expr::PickMatch { k, x, y, p, q, z } => match smod(ev(k), 3) {
                0 => wadd(ev(x), ev(p)),
                1 => wsub(wmul(ev(x), ev(y)), ev(q)),
                _ => ev(z),
            },
            Expr::Closure { m, c, a1, a2 } => {
                let m = ev(m);
                let g = |x: i64| wadd(wmul(x, m), *c);
                wadd(g(ev(a1)), g(ev(a2)))
            }
            Expr::SpinReuse { n, acc, s } => {
                let s = ev(s);
                let mut acc = ev(acc);
                let mut i = *n as i64;
                while i > 0 {
                    // p = Two(i + s, i * 3); pick_val(p, s) = (i + s) * 2 + i * 3
                    acc = wadd(acc, wadd(wmul(wadd(i, s), 2), wmul(i, 3)));
                    i -= 1;
                }
                acc
            }
            Expr::ChainPipe { n, x, m, c } => {
                let (x, c) = (ev(x), ev(c));
                let mut sum = 0i64;
                for i in 1..=*n as i64 {
                    // chain_build's element x + i, mapped through x*m + c.
                    sum = wadd(sum, wadd(wmul(wadd(x, i), *m as i64), c));
                }
                sum
            }
            Expr::PickVal { k, x, y, d } => match smod(ev(k), 3) {
                0 => wadd(ev(x), 1),
                1 => wadd(wmul(ev(x), 2), ev(y)),
                _ => ev(d),
            },
            Expr::SpinStep { k, x, y, n, acc } => {
                let mut n = *n as i64;
                let mut acc = ev(acc);
                match smod(ev(k), 3) {
                    0 => {
                        // One(x): acc += x, x += 1 per step; ends acc + x.
                        let mut x = ev(x);
                        while n > 0 {
                            acc = wadd(acc, x);
                            x = wadd(x, 1);
                            n -= 1;
                        }
                        wadd(acc, x)
                    }
                    1 => {
                        // Two(x, y): acc += y, (x, y) = (y, x + 1); ends
                        // acc + x + y.
                        let (mut x, mut y) = (ev(x), ev(y));
                        while n > 0 {
                            acc = wadd(acc, y);
                            let nx = y;
                            y = wadd(x, 1);
                            x = nx;
                            n -= 1;
                        }
                        wadd(wadd(acc, x), y)
                    }
                    _ => acc,
                }
            }
            Expr::Call(f, ca, cb) => eval_fn(fns, *f, ev(ca), ev(cb)),
        }
    }
}

/// Evaluate generated `f{idx}(a, b)`. Each statement binds the next temp, and
/// the body returns every temp plus both params, summed left to right.
fn eval_fn(fns: &[Vec<Expr>], idx: usize, a: i64, b: i64) -> i64 {
    let mut temps: Vec<i64> = Vec::with_capacity(fns[idx].len());
    for e in &fns[idx] {
        let v = e.eval(a, b, &temps, fns);
        temps.push(v);
    }
    let sum = temps.iter().fold(0i64, |s, v| wadd(s, *v));
    wadd(wadd(sum, a), b)
}

/// The preamble every fuzz program shares. Its helpers cover the heap shapes
/// the generated bodies call into. They are enum construction and variant
/// dispatch with payload binds, closure parameters called dynamically, and two
/// reuse loops. `spin_reuse` carries its reuse slot across a self-tail
/// back-edge, and `spin_step` puts `Reuse` right before `TailCallSelf`.
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
\t\t_ -> Zero\n\
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

/// A small positive literal for loop and list lengths. Counts never come from
/// `operand`, which can return huge boundary constants.
fn small_count(r: &mut Rng) -> u64 {
    1 + r.below(24)
}

fn small_lit(r: &mut Rng) -> i64 {
    r.below(199) as i64 - 99
}

/// A random operand: a live temp, a parameter, or a constant.
fn operand(r: &mut Rng, temps: usize) -> Expr {
    match r.below(4) {
        0 if temps > 0 => Expr::Temp(r.below(temps as u64) as usize),
        1 => Expr::Lit(BOUNDARY[r.below(BOUNDARY.len() as u64) as usize]),
        2 => Expr::A,
        3 => Expr::B,
        _ => Expr::Lit(small_lit(r)),
    }
}

/// One generated function, a chain of let-bound statements over Int operands.
/// Division and modulo need no guard because they are total in Scarlet
/// (x/0=0, x%0=x). Every temp and both params fold into the returned sum,
/// because Scarlet errors on unused bindings. Each function after the first
/// opens with a call to the one before it.
fn gen_fn(r: &mut Rng, idx: usize) -> Vec<Expr> {
    let stmts = 4 + r.below(7) as usize;
    let mut body = Vec::with_capacity(stmts);
    for t in 0..stmts {
        let o = |r: &mut Rng| Box::new(operand(r, t));
        let e = match r.below(12) {
            _ if t == 0 && idx > 0 => Expr::Call(idx - 1, Box::new(Expr::A), Box::new(Expr::B)),
            0..=3 => Expr::Bin(r.below(5) as usize, o(r), o(r)),
            4 => Expr::IfCmp(r.below(6) as usize, o(r), o(r), o(r), o(r)),
            5 => Expr::Mod3(o(r), o(r), o(r), o(r)),
            // Build an enum at a variant picked at runtime, and consume it with
            // an exhaustive match with payload binds.
            6 => Expr::PickMatch {
                k: o(r),
                x: o(r),
                y: o(r),
                p: o(r),
                q: o(r),
                z: o(r),
            },
            // A closure over the surrounding locals, called dynamically twice.
            7 => Expr::Closure {
                m: o(r),
                c: small_lit(r),
                a1: o(r),
                a2: o(r),
            },
            // Reuse carried across spin_reuse's self-tail back-edge.
            8 => Expr::SpinReuse {
                n: small_count(r),
                acc: o(r),
                s: o(r),
            },
            // Reuse one Cons for another in chain_map, folded back to an Int.
            9 => Expr::ChainPipe {
                n: small_count(r),
                x: o(r),
                m: 1 + r.below(9),
                c: o(r),
            },
            // Reuse a matched value to rebuild it, with Reuse right before
            // TailCallSelf.
            10 => Expr::SpinStep {
                k: o(r),
                x: o(r),
                y: o(r),
                n: small_count(r),
                acc: o(r),
            },
            _ if idx > 0 => Expr::Call(r.below(idx as u64) as usize, o(r), o(r)),
            _ => Expr::Bin(0, o(r), o(r)),
        };
        body.push(e);
    }
    body
}

fn render_fn(idx: usize, body: &[Expr]) -> String {
    let mut src = String::new();
    for (t, e) in body.iter().enumerate() {
        src.push_str(&format!("\tt{t} = {}\n", e.render()));
    }
    let sum = (0..body.len())
        .map(|t| format!("t{t}"))
        .chain(["a".to_string(), "b".to_string()])
        .collect::<Vec<_>>()
        .join(" + ");
    src.push_str(&format!("\t{sum}\n"));
    format!("fn f{idx}(a Int, b Int) Int {{\n{src}}}\n")
}

/// One whole program plus its expected stdout. It has the preamble, 2-3
/// chained functions, and a `main` that prints the last one at random
/// arguments. `main` also prints one value that uses every preamble shape, so
/// no program misses that coverage when the per-statement dice skip it.
fn gen_program(r: &mut Rng) -> (String, String) {
    let nfns = 2 + r.below(2) as usize;
    let mut fns = Vec::with_capacity(nfns);
    for i in 0..nfns {
        fns.push(gen_fn(r, i));
    }

    let mut prints: Vec<Expr> = Vec::new();
    for _ in 0..3 {
        prints.push(Expr::Call(
            nfns - 1,
            Box::new(Expr::Lit(small_lit(r))),
            Box::new(Expr::Lit(BOUNDARY[r.below(BOUNDARY.len() as u64) as usize])),
        ));
    }
    prints.push(Expr::SpinStep {
        k: Box::new(Expr::Lit(small_lit(r))),
        x: Box::new(Expr::Lit(small_lit(r))),
        y: Box::new(Expr::Lit(small_lit(r))),
        n: small_count(r),
        acc: Box::new(Expr::SpinReuse {
            n: small_count(r),
            acc: Box::new(Expr::ChainPipe {
                n: small_count(r),
                x: Box::new(Expr::Lit(BOUNDARY[r.below(BOUNDARY.len() as u64) as usize])),
                m: 1,
                c: Box::new(Expr::Lit(small_lit(r))),
            }),
            s: Box::new(Expr::PickVal {
                k: Box::new(Expr::Lit(small_lit(r))),
                x: Box::new(Expr::Lit(small_lit(r))),
                y: Box::new(Expr::Lit(small_lit(r))),
                d: Box::new(Expr::Lit(small_lit(r))),
            }),
        }),
    });

    let mut src = String::from(PREAMBLE);
    src.push('\n');
    for (i, f) in fns.iter().enumerate() {
        src.push_str(&render_fn(i, f));
        src.push('\n');
    }
    let mut expected = String::new();
    src.push_str("pub fn main() {\n");
    for p in &prints {
        src.push_str(&format!("\tprintln({})\n", p.render()));
        expected.push_str(&format!("{}\n", p.eval(0, 0, &[], &fns)));
    }
    src.push_str("}\n");
    (src, expected)
}

/// (e) About 200 seeded random programs. Each must print exactly the output
/// the generator computed from the intended semantics. The seed is fixed, and a
/// mismatch panics with the program index and full source.
#[test]
#[ignore = "needs the JIT, and its answers assume 64-bit Ints"]
fn fuzz_generated_programs_print_their_computed_values() {
    const SEED: u64 = 0x5eed_a10c_0de5_eed1;
    const PROGRAMS: usize = 200;
    let mut r = Rng::new(SEED);
    let proj = Project::new("native_fuzz");
    for i in 0..PROGRAMS {
        let (src, expected) = gen_program(&mut r);
        let out = run(&proj, "fuzz.sca", &src, None);
        assert!(
            out.success && out.stderr.is_empty(),
            "fuzz #{i} (seed {SEED:#x}): run failed:\n{src}\n\
             --- stdout ---\n{}--- stderr ---\n{}",
            out.stdout,
            out.stderr
        );
        assert_eq!(
            out.stdout, expected,
            "fuzz #{i} (seed {SEED:#x}): output diverged from the intended \
             semantics for:\n{src}\n--- got ---\n{}--- want ---\n{expected}",
            out.stdout
        );
    }
}

/// How many `wire.encode`/`wire.decode` round trips the JIT test below runs.
/// It only has to clear `NativeTable::WARM_CALLS` (8) by enough that most
/// iterations run natively. Wire allocates on every round, so this is
/// deliberately far smaller than `BITWISE_ROUNDS`.
const WIRE_ROUNDS: i64 = 2_000;

/// (f) Both wire ops across the switch to native. Nothing else keeps the
/// interpreter and the JIT in step on them, because every other wire test runs
/// the interpreter only. So none of them can see a wrong dispatch arm, such as
/// `WIRE_ENCODE` wired to `wire_decode`, or an `op_coverage` class that
/// disagrees with the arm the op is really dispatched from. Here it shows up as
/// a wrong answer or an abort.
///
/// Moving their `OpCoverage` (T-466, T-340) also relies on this parity check.
/// They lower as `Bridge`, whose failure arm is a `proof_violation` abort, so a
/// wrong arm here doesn't misbehave. It kills the process.
///
/// The accumulator folds the decoded `n` in on every round, so there is no
/// fixed point to collapse onto. A decode that returns a constant, a stale
/// value or the wrong field changes the sum. A decode that fails at all lands
/// on `0 - 1` and diverges at once, rather than reading as a quiet zero. We
/// recompute the expected total in Rust from the same recurrence, so a
/// miscompile fails against ground truth rather than against a second run of
/// the same code.
///
/// **This deliberately does NOT check** the byte format. That is T-341's
/// golden, and a round trip shares its encoder and decoder, so it can't see a
/// fault that is the same on both sides. Nor does it check how decode handles
/// hostile bytes (T-343). This test is about the bridge, not the codec.
#[test]
#[ignore = "needs the JIT and wire"]
fn wire_ops_survive_the_native_bridge() {
    let src = "import scarlet/wire

pub type Tag {
\tTag(n Int, label String)
}

fn churn(n Int, acc Int) Int {
\tbytes = wire::encode(Tag(n, 'x'))
\tnext = match wire::decode(bytes) {
\t\tOk(Tag(m, _)) -> acc + m
\t\tErr(_) -> 0 - 1
\t}
\tif n == 0 { next } else { churn(n - 1, next) }
}

pub fn main() {
\tprintln(churn(ROUNDS, 0))
}
";
    let src = src.replace("ROUNDS", &WIRE_ROUNDS.to_string());

    // Take ground truth from the recurrence, not from the VM. The fold adds
    // every `n` from WIRE_ROUNDS down to 0, and each `n` only arrives by
    // surviving a full encode/decode round trip.
    let expected = format!("{}\n", (0..=WIRE_ROUNDS).sum::<i64>());

    let proj = Project::new("wire_bridge");
    let path = proj.dir.join("prog.sca");
    std::fs::write(&path, src).unwrap();
    let path = path.to_string_lossy().into_owned();
    let out = run_al_env(&["run", &path], &[("SCARLET_NATIVE_DEBUG", "1")]);
    assert!(out.success, "run failed:\n{}", out.stderr);
    assert_eq!(out.stdout, expected, "stderr:\n{}", out.stderr);
    assert_warmed(&out.stderr, "churn");
}

/// (g) **Bad bytes are a value, not a VM error.** This checks it on the native
/// arm rather than arguing it.
///
/// `OpCoverage::Try` is for ops that "can raise a runtime error *on user
/// data*", and this is why that no longer describes `wire.decode` (T-466): a
/// refusal is `Err(DecodeError)`, which the program matches on. Any future move
/// to `Bridge` also depends on it. `al_shim_op` answers a returned `Err` with
/// `proof_violation`, a `std::process::abort`, so an op that raised on bad input
/// would abort the process on the JIT arm and nowhere else. We test it here so
/// it stays true, rather than working it out from the code each time.
///
/// The bytes are deliberately not wire's at all, and a fixed `<<1, 2, 3>>`
/// fails the magic check. We assert on the printed refusal AND on the exit
/// status. An abort and a clean `Err` differ in the status, not in stdout, and
/// `out.success` is the only place that shows.
///
/// **This does NOT check** which `DecodeError` fits which corruption, or cover
/// truncation and single-byte mutation. That is T-343. Here the only question
/// is abort or value.
#[test]
#[ignore = "needs the JIT and wire"]
fn a_wire_decode_refusal_is_a_value_on_the_native_bridge_not_an_abort() {
    let src = "import scarlet/wire

fn churn(n Int, acc Int) Int {
\tnext = match wire::decode(<<1, 2, 3>>) {
\t\tOk(v) -> acc + v
\t\tErr(_) -> acc + 1
\t}
\tif n == 0 { next } else { churn(n - 1, next) }
}

pub fn main() {
\tprintln(churn(ROUNDS, 0))
}
";
    let src = src.replace("ROUNDS", &WIRE_ROUNDS.to_string());
    // Every round must take the `Err` arm, so the count is the round count.
    // `Ok(v) -> acc + v` also fixes the payload type to `Int`, which gives the
    // call a descriptor for decode to refuse against in the first place.
    let expected = format!("{}\n", WIRE_ROUNDS + 1);

    let proj = Project::new("wire_bridge_refusal");
    let path = proj.dir.join("prog.sca");
    std::fs::write(&path, src).unwrap();
    let path = path.to_string_lossy().into_owned();
    let out = run_al_env(&["run", &path], &[("SCARLET_NATIVE_DEBUG", "1")]);
    assert!(
        out.success,
        "a decode refusal aborted or unwound instead of returning a value:\n{}",
        out.stderr
    );
    assert_eq!(out.stdout, expected, "stderr:\n{}", out.stderr);
    assert_warmed(&out.stderr, "churn");
}
