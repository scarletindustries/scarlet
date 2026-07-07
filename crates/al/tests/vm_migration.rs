// Multi-scheduler smoke coverage: programs that fan CPU-bound processes out
// across two schedulers (`AL_SCHEDULERS=2`). With several runnable processes
// and an idle peer, the yield path donates run-queue processes to the other
// scheduler, so every worker (stack, frames, captures, heap) is moved whole
// to another scheduler mid-execution. The assertions are purely
// semantic — exact output lines, not timing — so a migration bug shows up as
// corrupted/missing output or a hang, never as flakiness on a loaded machine.

use std::process::{Command, Stdio};

mod common;
use common::Project;
use common::wait_or_kill;

/// Run `al run <prog>` with `AL_SCHEDULERS=<schedulers>` and capture output.
/// A generous wall-clock cap turns a scheduler deadlock (e.g. a donated
/// process lost in transit, or a claimed-but-never-notified peer sleeping
/// forever) into a test failure instead of a hung CI job. The cap is an
/// anti-hang guard only; no assertion depends on how fast the run finishes.
fn run_al_with_schedulers(proj: &Project, src: &str, schedulers: u32) -> String {
    let prog = proj.dir.join("prog.al");
    std::fs::write(&prog, src).unwrap();

    let child = Command::new(env!("CARGO_BIN_EXE_al"))
        .arg("run")
        .arg(&prog)
        .env("AL_SCHEDULERS", schedulers.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn al");

    let out = wait_or_kill(child, 120);
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        out.status.success(),
        "al run failed (or hung past 120s) under AL_SCHEDULERS={schedulers}\n\
         --- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
    );
    assert!(
        !stderr.contains("panicked"),
        "runtime panicked under AL_SCHEDULERS={schedulers}:\n{stderr}"
    );
    stdout
}

/// Assert `stdout` is exactly the lines of `expected` modulo order. Process
/// completion order across schedulers is nondeterministic, so the comparison
/// is on the sorted multiset of lines: every expected line appears exactly
/// once, nothing extra, nothing duplicated, nothing torn mid-line.
fn assert_lines_unordered(stdout: &str, expected: &[String], context: &str) {
    let mut got: Vec<&str> = stdout.lines().collect();
    got.sort_unstable();
    let mut want: Vec<&str> = expected.iter().map(|s| s.as_str()).collect();
    want.sort_unstable();
    assert_eq!(
        got, want,
        "wrong output line multiset ({context})\n--- raw stdout ---\n{stdout}"
    );
}

/// Reference fibonacci mirroring the al program's `fib`, so expected outputs
/// are computed rather than hand-pinned.
fn fib(n: u64) -> u64 {
    if n < 2 { n } else { fib(n - 1) + fib(n - 2) }
}

/// Shared imports + `fib` source every migration program starts with.
const FIB_PREAMBLE: &str = r#"import al/experiments/scheduler
import al/array

fn fib(n) {
	match n {
		0 -> 0
		1 -> 1
		else -> fib(n - 1) + fib(n - 2)
	}
}

"#;

/// Prepend [`FIB_PREAMBLE`] to `body`, run it under `schedulers` schedulers
/// in a fresh project named `tag`, and return stdout.
fn run_fib_program(tag: &str, schedulers: u32, body: &str) -> String {
    let proj = Project::new(tag);
    run_al_with_schedulers(&proj, &format!("{FIB_PREAMBLE}{body}"), schedulers)
}

/// Shared runner for the CPU-bound smoke tests: `spawns` workers each compute
/// `fib(base + i % modulo)` — tens of thousands of reductions
/// (REDUCTION_BUDGET is 4000), so every worker is preempted many times and
/// sits in a run queue while a sibling runs — and print a per-id line whose
/// value pins the *result* of the computation: a process whose
/// stack/frames/captures were corrupted in transit prints the wrong number
/// (or crashes), it doesn't just reorder.
fn cpu_bound_smoke(tag: &str, schedulers: u32, spawns: u64, base: u64, modulo: u64) {
    let body = format!(
        r#"fn work(i) {{
	println('${{i}} done ${{fib({base} + i % {modulo})}}')
}}

array.each(1..{end}, fn(i) scheduler.spawn(fn() work(i)))
println('main done')
"#,
        end = spawns + 1
    );
    let stdout = run_fib_program(tag, schedulers, &body);

    let mut expected: Vec<String> = (1..=spawns)
        .map(|i| format!("{i} done {}", fib(base + i % modulo)))
        .collect();
    expected.push("main done".to_string());
    assert_lines_unordered(
        &stdout,
        &expected,
        &format!("{spawns} spawns, AL_SCHEDULERS={schedulers}"),
    );
}

/// Eight CPU-bound workers under two schedulers — exactly the run-queue state
/// the donation policy migrates.
#[test]
fn cpu_bound_spawns_complete_correctly_under_two_schedulers() {
    cpu_bound_smoke("sched2_smoke", 2, 8, 20, 4);
}

/// Same workload pinned to one scheduler. With a single scheduler there is
/// never an idle peer, so the donation path must stay completely inert; the
/// program must still finish with identical output. Guards against the
/// migration machinery breaking the degenerate single-core configuration.
#[test]
fn cpu_bound_spawns_complete_correctly_under_one_scheduler() {
    cpu_bound_smoke("sched1_smoke", 1, 6, 19, 3);
}

/// Deep recursion across a migration boundary: one heavyweight worker
/// (`fib(26)`, hundreds of preemptions) racing several light siblings under
/// two schedulers. If migration moves a process whose frame stack is many
/// recursive activations deep — all sharing one closure — any error in frame
/// metadata (ip/base_slot/captures wiring) or in the moved state derails the
/// recursion and the final value comes out wrong. The single deep worker is
/// the likeliest donation victim since it outlives every sibling in the run
/// queue.
#[test]
fn deep_recursive_worker_survives_two_schedulers() {
    let stdout = run_fib_program(
        "sched2_deep",
        2,
        r#"scheduler.spawn(fn() println('deep ${fib(26)}'))
array.each(1..6, fn(i) scheduler.spawn(fn() println('light ${i} ${fib(18)}')))
"#,
    );

    let mut expected: Vec<String> = (1u64..6)
        .map(|i| format!("light {i} {}", fib(18)))
        .collect();
    expected.push(format!("deep {}", fib(26)));
    assert_lines_unordered(&stdout, &expected, "deep recursion, AL_SCHEDULERS=2");
}

/// Migration while holding loaded globals. Top-level bindings are published
/// once into the program-wide frozen area; loading one is a plain copy of the
/// frozen word, so a `Value` loaded from the globals table must stay valid on
/// whatever scheduler the process lands on — nothing it points at may be
/// scheduler-local. Each worker loads the globals into locals FIRST, then
/// burns tens of thousands of reductions (many preemptions, so the loaded
/// words sit on a run-queue process's stack across donations), and only then
/// prints them. A frozen pointer that dangled in transit shows up as corrupt
/// output or a crash, never as flakiness.
#[test]
fn migrated_process_keeps_loaded_globals_valid() {
    let stdout = run_fib_program(
        "sched2_globals",
        2,
        r#"fn work(label, tag, i) {
	n = fib(20 + i % 3)
	println('${label} ${tag} ${i} ${n}')
}

labels = ['alpha', 'beta', 'gamma', 'delta']
banner = 'frozen-global'

array.each(1..9, fn(i) scheduler.spawn(fn() work(labels[i % 4] or '?', banner, i)))
println('main done')
"#,
    );

    let labels = ["alpha", "beta", "gamma", "delta"];
    let mut expected: Vec<String> = (1u64..9)
        .map(|i| {
            format!(
                "{} frozen-global {i} {}",
                labels[(i % 4) as usize],
                fib(20 + i % 3)
            )
        })
        .collect();
    expected.push("main done".to_string());
    assert_lines_unordered(&stdout, &expected, "loaded globals, AL_SCHEDULERS=2");
}
