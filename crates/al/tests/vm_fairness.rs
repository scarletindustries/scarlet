//! Reduction-accounting fairness, pinned.
//!
//! The preemption contract these tests pin (see `exec.rs`): every function
//! application — including a `TailCallSelf` back-edge — costs one reduction
//! from a REDUCTION_BUDGET of 4000, and exhaustion yields the process. At a
//! self-tail back-edge the yield happens *after* the frame collapse, so the
//! suspended frame is exactly `ip = 0` with the next iteration's arguments
//! already in the locals: resume re-enters the function from the top and the
//! loop continues correctly. Any alternative execution backend must reproduce
//! this observable behavior bit-for-bit — same yield points, same
//! frame-at-yield shape — which is what makes these assertions
//! backend-independent: they run the `al` binary with the ambient
//! environment, so an execution-mode env toggle set by the harness applies
//! here too.
//!
//! Fairness is asserted through ORDER, never timing: under `AL_SCHEDULERS=1`
//! the scheduler is FIFO round-robin (a yielded process goes to the back of
//! the run queue), so a lightweight sibling finishing *before* a
//! million-iteration self-tail spinner is only possible if the spinner was
//! preempted at its back-edges. If the reduction checkpoint were missing, the
//! spinner would run to completion in one slice and the order would invert —
//! a deterministic failure, not a flake.

use std::process::{Command, Stdio};

mod common;
use common::Project;
use common::wait_or_kill;

use al::dis;
use al::{STDLIB, ast, bytecode, parser, scanner};

/// Run `al run <prog>` pinned to one scheduler and return stdout. One
/// scheduler means no parallelism: interleaving can only come from
/// reduction-budget preemption, which is exactly what these tests observe.
fn run_single_scheduler(tag: &str, src: &str) -> String {
    let proj = Project::new(tag);
    let prog = proj.dir.join("prog.al");
    std::fs::write(&prog, src).unwrap();

    let child = Command::new(env!("CARGO_BIN_EXE_al"))
        .arg("run")
        .arg(&prog)
        .env("AL_SCHEDULERS", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn al");

    let out = wait_or_kill(child, 120);
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        out.status.success(),
        "al run failed (or hung past 120s) under AL_SCHEDULERS=1\n\
         --- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
    );
    stdout
}

/// Byte offset of `needle` in `stdout`, with a readable failure.
fn pos(stdout: &str, needle: &str) -> usize {
    stdout
        .find(needle)
        .unwrap_or_else(|| panic!("missing line {needle:?} in stdout:\n{stdout}"))
}

/// Assert `src`'s function `name` compiles with a `TailCallSelf` back-edge.
/// The fairness assertions below are only meaningful if the spinner really
/// is a self-tail loop — a codegen change that turned it into plain
/// recursion would blow the stack or shift the yield points, and this guard
/// names the actual culprit instead.
fn assert_self_tail(src: &str, name: &str) {
    let mut sc = scanner::new_scanner(src.to_string());
    let p = parser::new_parser(&mut sc);
    let parsed = p.parse_program();
    let expr = ast::Expression::BlockExpression(parsed.ast);
    let r = bytecode::compile(&expr, None, Some(&STDLIB));
    assert!(r.success(), "compile failed: {:?}", r.diagnostics);
    let program = r.emitted.expect("a successful compile emits").program;
    let text = dis::disassemble_fn(&program, name);
    assert!(
        text.contains("TailCallSelf"),
        "`{name}` did not compile to a self-tail loop:\n{text}"
    );
}

/// The canonical spinner: a million self-tail iterations, ~250 preemptions
/// at REDUCTION_BUDGET=4000. A lightweight sibling spawned *after* it must
/// still finish first under one scheduler — the pinned fairness property.
#[test]
fn self_tail_spinner_yields_to_lightweight_sibling() {
    let src = r#"import al/scheduler

fn count(n) {
	if n == 0 {
		0
	} else {
		count(n - 1)
	}
}

scheduler.spawn(fn() {
	_ = count(1000000)
	println('heavy done')
})
scheduler.spawn(fn() {
	println('light done')
})
println('main done')
"#;
    assert_self_tail(src, "count");

    let stdout = run_single_scheduler("fair_spinner", src);
    for line in ["main done", "light done", "heavy done"] {
        assert_eq!(
            stdout.matches(line).count(),
            1,
            "expected {line:?} exactly once:\n{stdout}"
        );
    }
    assert!(
        pos(&stdout, "light done") < pos(&stdout, "heavy done"),
        "spinner was not preempted: the sibling only ran after count() \
         finished, so the self-tail back-edge is not charging reductions\n{stdout}"
    );
}

/// Resume-from-the-top correctness across many preemptions: the suspended
/// frame at a self-tail yield holds the *next* iteration's arguments, so an
/// accumulator threaded through the loop witnesses every one of the ~500
/// suspend/resume cycles. A backend that yielded with stale locals — or
/// resumed anywhere but ip 0 — prints the wrong sum, deterministically.
#[test]
fn self_tail_accumulator_survives_preemption() {
    let heavy_n: u64 = 2_000_000;
    let light_n: u64 = 100_000;
    let gauss = |n: u64| n * (n + 1) / 2;

    let src = format!(
        r#"import al/scheduler

fn sum(n, acc) {{
	if n == 0 {{
		acc
	}} else {{
		sum(n - 1, acc + n)
	}}
}}

scheduler.spawn(fn() {{
	println('heavy ${{sum({heavy_n}, 0)}}')
}})
scheduler.spawn(fn() {{
	println('light ${{sum({light_n}, 0)}}')
}})
"#
    );
    assert_self_tail(&src, "sum");

    let stdout = run_single_scheduler("fair_accum", &src);
    let heavy_line = format!("heavy {}", gauss(heavy_n));
    let light_line = format!("light {}", gauss(light_n));
    assert!(
        stdout.contains(&heavy_line),
        "wrong heavy sum (locals corrupted across a yield?):\n{stdout}"
    );
    assert!(stdout.contains(&light_line), "wrong light sum:\n{stdout}");
    assert!(
        pos(&stdout, &light_line) < pos(&stdout, &heavy_line),
        "the 20x-lighter process finished after the heavy one under a \
         single scheduler — self-tail preemption is broken\n{stdout}"
    );
}
