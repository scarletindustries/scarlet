//! The REPL, driven as a subprocess over stdin. The compiler keys expression
//! types on `Span`, which carries no file identity, so the REPL must replay
//! source text (spans stay unique) and not parsed entries (spans all restart
//! at line 1 and collide).

mod common;

use std::io::Write;
use std::process::{Command, Stdio};

/// Feed `entries` to `al repl` on stdin, one per line, and return stdout.
/// Bounded so a wedged REPL fails the test instead of hanging the suite.
fn repl(entries: &str) -> String {
    session(entries).0
}

/// [`repl`], keeping stderr too: diagnostics and command errors go there.
fn session(entries: &str) -> (String, String) {
    let bin = env!("CARGO_BIN_EXE_scarlet");
    let mut child = Command::new(bin)
        .arg("repl")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn al repl");
    // Dropping stdin is the REPL's EOF, and its exit signal.
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
    (
        strip_banner(&String::from_utf8_lossy(&out.stdout)),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Drop the three banner lines the REPL prints before the first entry —
/// version, help, blank.
///
/// The version is a build stamp: a release build carries a canary timestamp
/// like `0.0.1-canary.20260812.0745`. Its digits are indistinguishable from
/// program output to the single-character `contains` assertions below, which
/// made them read the banner instead of the session — `0745` alone is enough
/// to fail a `!contains('4')`. Every assertion here is about what the session
/// printed, so the banner never reaches one.
fn strip_banner(stdout: &str) -> String {
    let mut lines = stdout.lines();
    let version = lines.next().unwrap_or_default();
    let help = lines.next().unwrap_or_default();
    let blank = lines.next().unwrap_or_default();
    assert!(
        version.contains("scarlet")
            && version.contains("REPL")
            && help.contains(":help")
            && blank.is_empty(),
        "REPL banner is not the three lines this strips; stdout began:\n{stdout}"
    );
    lines.collect::<Vec<_>>().join("\n")
}

/// `'x' + 'y'` and `100 + 200` occupy the identical `Span` when each entry is
/// parsed on its own, so the second must not retype the first.
#[test]
#[ignore = "needs the VM"]
fn a_later_entry_does_not_retype_an_earlier_one() {
    let out = repl("const a = 'x' + 'y'\nconst b = 100 + 200\nprintln(a)\n");
    assert!(out.contains("xy"), "want the string concat, got:\n{out}");
    assert!(
        !out.lines().any(|l| l.trim() == "0"),
        "integer add over two heap strings:\n{out}"
    );
}

#[test]
#[ignore = "needs the VM"]
fn definitions_persist_across_entries() {
    let out = repl("fn add(a Int, b Int) Int { a + b }\nprintln(add(1, 2))\n");
    assert!(out.contains('3'), "want 3, got:\n{out}");
}

/// Replaying a bare expression would repeat its effects on every later entry.
#[test]
#[ignore = "needs the VM"]
fn a_bare_expression_is_not_replayed() {
    let out = repl("println('once')\nprintln('twice')\n");
    assert_eq!(out.matches("once").count(), 1, "replayed an effect:\n{out}");
    assert!(out.contains("twice"), "{out}");
}

#[test]
#[ignore = "needs the VM"]
fn a_multi_line_entry_still_evaluates() {
    let out = repl(
        "fn tri(n Int) Int {\n\tif n == 0 { 0 } else { n + tri(n - 1) }\n}\nprintln(tri(3))\n",
    );
    assert!(out.contains('6'), "want 6, got:\n{out}");
}

/// The language requires imports to precede every other declaration, but a
/// session is typed in whatever order the user thinks of things.
#[test]
#[ignore = "needs the VM"]
fn an_import_after_a_definition_still_resolves() {
    let out = repl("const s = 'a,b'\nimport scarlet/string\nprintln(string.split(s, ','))\n");
    assert!(out.contains('b'), "want the split, got:\n{out}");
}

/// A definition evaluates to `Nil`, which is noise at a prompt.
#[test]
fn a_definition_prints_no_value() {
    let out = repl("const x = 1\nfn f() Int { 2 }\n");
    assert!(!out.contains("Nil"), "printed the unit value:\n{out}");
}

#[test]
fn the_type_command_reports_an_inferred_type() {
    let out = repl("const n = 41\n:t n + 1\n:type println\n");
    assert!(out.contains("n + 1  Int"), "{out}");
    assert!(out.contains("fn(a) Nil"), "{out}");
}

#[test]
fn an_unknown_command_is_reported_rather_than_evaluated() {
    let (out, err) = session(":hlep\n");
    assert!(err.contains("unknown command ':hlep'"), "{err}");
    assert!(
        !out.contains("Unknown identifier"),
        "ran it as source:\n{out}"
    );
}

#[test]
fn reset_forgets_the_session() {
    let (out, err) = session("const gone = 1\n:reset\ngone\n");
    assert!(err.contains("Unknown identifier 'gone'"), "{err}{out}");
}

#[test]
#[ignore = "needs the VM"]
fn a_session_round_trips_through_save_and_load() {
    let dir = std::env::temp_dir().join(format!("scarlet_repl_save_{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let path = dir.join("session.scrl");
    let saved = repl(&format!(
        "import scarlet/string\nfn size(s String) Int {{ string.length(s) }}\n:save {}\n",
        path.display()
    ));
    assert!(saved.contains("wrote"), "{saved}");
    let text = std::fs::read_to_string(&path).expect("read saved session");
    assert!(
        text.starts_with("import scarlet/string\n"),
        "imports must lead a saved session:\n{text}"
    );

    let out = repl(&format!(":load {}\nprintln(size('hi'))\n", path.display()));
    assert!(out.contains('2'), "want 2, got:\n{out}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// `:dis` filters: the emitted program carries the whole stdlib.
#[test]
fn dis_lists_only_the_matching_function() {
    let (out, err) = session("fn only_mine() Int { 7 }\n:dis only_mine\n:dis nope\n");
    assert!(out.contains("only_mine"), "{out}");
    assert!(!out.contains("fn#0 "), "listed the whole program:\n{out}");
    assert!(err.contains("no function matching 'nope'"), "{err}");
}

/// `/` is the marker half the world's tools use; no Scarlet entry starts with
/// one, so it spells a command too.
#[test]
#[ignore = "needs the VM"]
fn a_slash_command_runs_rather_than_erroring() {
    let (out, err) = session("1 + 1\n/quit\n2 + 2\n");
    assert!(out.contains('2'), "{out}");
    assert!(!out.contains('4'), "kept reading after /quit:\n{out}");
    assert!(!err.contains("error"), "ran /quit as source:\n{err}");
}

/// A command that prints nothing is indistinguishable from one that did not
/// run — `:reset` says so.
#[test]
fn reset_says_it_ran() {
    let (out, _) = session("const x = 1\n:reset\n");
    assert!(out.contains("session reset"), "{out}");
}

/// Binding a name now and using it in the next entry is what a session is, so
/// the unused-binding check must not reject the entry that binds it.
#[test]
#[ignore = "needs the VM"]
fn a_binding_used_by_a_later_entry_is_not_called_unused() {
    let (out, err) = session("import scarlet/http\nserve = http.serve\nprintln(serve)\n");
    assert!(!err.contains("unused"), "{err}");
    assert!(out.contains("<fn#serve>"), "{out}{err}");
}

/// The entry after a binding must still *run*. An unused-binding error does
/// not merely print: it makes the module dirty, and a dirty module emits no
/// toplevel, so the entry silently evaluates to nothing.
#[test]
#[ignore = "needs the VM"]
fn an_entry_after_a_binding_still_evaluates() {
    let out = repl("x = 5\n42\n");
    assert!(out.contains("42"), "want 42, got:\n{out}");
    assert!(!out.lines().any(|l| l.trim() == "0"), "{out}");
}

/// Only an entry ending in an expression has a value to show; what a binding
/// leaves behind is a compiler artifact.
#[test]
fn a_binding_entry_prints_nothing() {
    let (out, _) = session("import scarlet/http\nserve = http.serve\n");
    assert!(
        !out.lines().any(|l| l.trim() == "0"),
        "printed the binding's stack leftover:\n{out}"
    );
}
