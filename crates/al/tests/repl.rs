//! The REPL, driven as a subprocess over stdin. It had no tests, which is how
//! a wrong-answer bug lived in it: the compiler keys expression types on
//! `Span`, and `Span` carries no file or line-offset identity, so replaying
//! *parsed* entries (whose spans all restart at line 1) let two same-shaped
//! entries collide. Replaying source text instead makes spans unique.

mod common;

use std::io::Write;
use std::process::{Command, Stdio};

/// Feed `entries` to `al repl` on stdin, one per line, and return stdout.
///
/// Bounded and reaped: the REPL exits on EOF, but a wedged one must fail this
/// test rather than hang the suite and outlive the runner.
fn repl(entries: &str) -> String {
    let bin = env!("CARGO_BIN_EXE_al");
    let mut child = Command::new(bin)
        .arg("repl")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn al repl");
    // Dropping stdin closes it, which is the REPL's EOF and its exit signal.
    {
        let mut stdin = child.stdin.take().expect("stdin");
        stdin.write_all(entries.as_bytes()).expect("write stdin");
    }
    let out = common::wait_or_kill(child, common::CHILD_TIMEOUT_SECS);
    assert!(
        out.status.code().is_some(),
        "`al repl` died by signal (wedged past {}s, or crashed)",
        common::CHILD_TIMEOUT_SECS
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// `'x' + 'y'` and `100 + 200` occupy the identical `Span` when each entry is
/// parsed on its own. With AST replay the second entry's `Int` overwrote the
/// first's `String`, `specialize_binop` chose `AddInt`, and the VM ran an
/// integer add over two heap strings — printing `0` in release and tripping a
/// `debug_assert` in debug.
#[test]
fn a_later_entry_does_not_retype_an_earlier_one() {
    let out = repl("const a = 'x' + 'y'\nconst b = 100 + 200\nprintln(a)\n");
    assert!(out.contains("xy"), "want the string concat, got:\n{out}");
    assert!(
        !out.lines().any(|l| l.trim() == "0"),
        "integer add over two heap strings:\n{out}"
    );
}

#[test]
fn definitions_persist_across_entries() {
    let out = repl("fn add(a Int, b Int) Int { a + b }\nprintln(add(1, 2))\n");
    assert!(out.contains('3'), "want 3, got:\n{out}");
}

/// A bare expression is a value the user already saw; replaying it would
/// repeat its effects on every later entry.
#[test]
fn a_bare_expression_is_not_replayed() {
    let out = repl("println('once')\nprintln('twice')\n");
    assert_eq!(out.matches("once").count(), 1, "replayed an effect:\n{out}");
    assert!(out.contains("twice"), "{out}");
}

#[test]
fn a_multi_line_entry_still_evaluates() {
    let out = repl(
        "fn tri(n Int) Int {\n\tif n == 0 { 0 } else { n + tri(n - 1) }\n}\nprintln(tri(3))\n",
    );
    assert!(out.contains('6'), "want 6, got:\n{out}");
}
