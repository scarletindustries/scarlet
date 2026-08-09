// Multi-scheduler smoke coverage. With several runnable processes and an idle
// peer, the yield path donates run-queue processes to the other scheduler, so
// a worker is moved whole (stack, frames, captures, heap) mid-execution.
// Assertions are on exact output lines, never timing.

use std::process::{Command, Stdio};

mod common;
use common::Project;
use common::wait_or_kill;

/// Run `al run <prog>` with `AL_SCHEDULERS=<schedulers>` and capture output.
/// The wall-clock cap only turns a scheduler deadlock into a failure instead
/// of a hung CI job; no assertion depends on how fast the run finishes.
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
/// completion order across schedulers is nondeterministic.
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

/// Mirrors the al program's `fib`, so expected outputs are computed.
fn fib(n: u64) -> u64 {
    if n < 2 { n } else { fib(n - 1) + fib(n - 2) }
}

/// Shared imports + `fib` source every migration program starts with.
const FIB_PREAMBLE: &str = r#"import al/scheduler
import al/array

fn fib(n) {
	match n {
		0 -> 0
		1 -> 1
		else -> fib(n - 1) + fib(n - 2)
	}
}

"#;

/// Prepend [`FIB_PREAMBLE`] to `body` and run it in a fresh project.
fn run_fib_program(tag: &str, schedulers: u32, body: &str) -> String {
    let proj = Project::new(tag);
    run_al_with_schedulers(&proj, &format!("{FIB_PREAMBLE}{body}"), schedulers)
}

/// `spawns` workers each compute `fib(base + i % modulo)`. Sized well past
/// the 4000-reduction budget so every worker is preempted many times and sits
/// in a run queue while a sibling runs. Printing the result, not just the id,
/// is what catches a process corrupted in transit.
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

/// Eight CPU-bound workers under two schedulers: the run-queue state the
/// donation policy migrates.
#[test]
fn cpu_bound_spawns_complete_correctly_under_two_schedulers() {
    cpu_bound_smoke("sched2_smoke", 2, 8, 20, 4);
}

/// Same workload on one scheduler, where there is never an idle peer: the
/// donation path must stay inert and the output must be identical.
#[test]
fn cpu_bound_spawns_complete_correctly_under_one_scheduler() {
    cpu_bound_smoke("sched1_smoke", 1, 6, 19, 3);
}

/// Deep recursion across a migration boundary. The heavy worker outlives
/// every sibling, so it is the likeliest donation victim; any error in the
/// moved frame metadata (ip/base_slot/captures) derails the recursion and the
/// final value comes out wrong.
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

/// Migration while holding loaded globals. A `Value` copied out of the
/// globals table must stay valid on whatever scheduler the process lands on,
/// so nothing it points at may be scheduler-local. Each worker loads the
/// globals first, burns many reductions, then prints them.
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
