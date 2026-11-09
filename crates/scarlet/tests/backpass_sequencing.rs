//! A backpass over `result.then` short-circuits: when the bound step fails,
//! nothing after it runs.
//!
//! This is the *statement-order* guarantee of `<-`, and the stdlib depends on
//! it. Every one of the 19 backpass statements in `crates/scarlet_core/src/std`
//! is over `result.then`/`result.map`, and 10 of them bind `_`. One is
//! `_ <- result.then(flush(sock, pending))` in `http.sca`, whose own comment
//! says the pending bytes must reach the wire before the request body is
//! awaited, or the peer deadlocks. A lowering that ran the second step anyway
//! would break these silently, because the binder they don't mention is
//! exactly what makes them look independent.
//!
//! The programs are inline and run under a short bound rather than living in
//! `tests/programs/`, because `common::run_al` waits `CHILD_TIMEOUT_SECS` and
//! the failure this pins is a program that never ends. What tells the two
//! apart is whether there's an exit code at all. Otherwise a hung program and
//! one that printed nothing give the same string.
//!
//! We watched these fail before the fix; see T-211.

use std::path::PathBuf;
use std::process::{Command, Stdio};

mod common;
use common::wait_or_kill;

/// Long enough that a loaded machine doesn't look like a wedge, and short enough
/// that a real wedge is one slow failing test rather than a two-minute one.
const BOUND_SECS: u64 = 25;

/// A failing first step whose continuation would not terminate if it ran.
///
/// `chunks` terminates for every `per >= 1` and never for `per == 0`, and `0`
/// is what a lowering that makes up a value to keep going would supply. That's
/// the shape that made a quoted `"per"` hang instead of reporting a failure.
const DIVERGENT_CONTINUATION: &str = r#"import scarlet/result

type Step {
	Boom
}

fn first_step() Result(Int, Step) {
	Err(Boom)
}

fn chunks(n Int, per Int, acc Int) Int {
	if n <= 0 {
		acc
	} else {
		chunks(n - per, per, acc + 1)
	}
}

fn pipeline() Result(Int, Step) {
	per <- result::then(first_step())
	println('SECOND STEP RAN')
	Ok(chunks(8, per, 0))
}

pub fn main() {
	match pipeline() {
		Ok(v) -> println('ok ${v}')
		Err(Boom) -> println('short-circuited')
	}
	println('done')
}
"#;

/// Two `_ <-` steps in a row, neither mentioning the other's binder, which is
/// the `http.sca` shape. The second writes before it fails, so running it after
/// the first failed is visible in stdout without needing a timeout.
const TWO_INDEPENDENT_WRITES: &str = r#"import scarlet/result

type Step {
	Boom
}

fn write(tag String) Result(Nil, Step) {
	println('write ${tag}')
	Err(Boom)
}

fn both() Result(Nil, Step) {
	_ <- result::then(write('a'))
	_ <- result::then(write('b'))
	Ok(Nil)
}

pub fn main() {
	match both() {
		Ok(Nil) -> println('ok')
		Err(Boom) -> println('short-circuited')
	}
}
"#;

/// Writes `src` as a one-file program and runs it, giving up after `BOUND_SECS`.
/// Returns the exit code (`None` when the child had to be killed) and its
/// combined streams.
fn run_bounded(tag: &str, src: &str) -> (Option<i32>, String) {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("backpass_{tag}"));
    std::fs::create_dir_all(&dir).expect("mkdir");
    let entry = dir.join("main.sca");
    std::fs::write(&entry, src).expect("write program");
    std::fs::write(dir.join("package.sca"), "name = 'backpass_test'\n").expect("write package");

    let child = Command::new(env!("CARGO_BIN_EXE_scarlet"))
        .arg("run")
        .arg(&entry)
        .current_dir(&dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run scarlet");
    let out = wait_or_kill(child, BOUND_SECS);
    (
        out.status.code(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

#[test]
fn failed_step_does_not_run_a_continuation_that_would_not_end() {
    let (code, out) = run_bounded("divergent", DIVERGENT_CONTINUATION);
    assert_eq!(
        code,
        Some(0),
        "no exit code means the program was killed at {BOUND_SECS}s, so the continuation ran \
         against a value the failed step never produced\noutput:\n{out}"
    );
    assert!(
        !out.contains("SECOND STEP RAN"),
        "the continuation of a failed `result::then` ran\noutput:\n{out}"
    );
    assert_eq!(out, "short-circuited\ndone\n", "output:\n{out}");
}

#[test]
fn a_second_independent_step_does_not_run_after_the_first_fails() {
    let (code, out) = run_bounded("two_writes", TWO_INDEPENDENT_WRITES);
    assert_eq!(code, Some(0), "output:\n{out}");
    assert!(
        !out.contains("write b"),
        "the second `_ <-` step ran after the first failed. Neither continuation mentions \
         `_`, which is what makes these two look independent\noutput:\n{out}"
    );
    assert_eq!(out, "write a\nshort-circuited\n", "output:\n{out}");
}
