//! Strings use double quotes. These drive the binary over a program in single
//! quotes: only `scarlet fmt` reads it, and it rewrites it.

mod common;

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use common::{Project, run_al};

const SINGLE: &str =
    "pub fn main() {\n\tname = 'it\\'s \"me\"'\n\tprintln('hi ${name}, ${'bye'}')\n}\n";
const DOUBLE: &str =
    "pub fn main() {\n\tname = \"it's \\\"me\\\"\"\n\tprintln(\"hi ${name}, ${\"bye\"}\")\n}\n";
const ERROR: &str = "Strings use double quotes";

fn fmt(args: &[&str], file: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_scarlet"))
        .arg("fmt")
        .args(args)
        .arg(file)
        .output()
        .expect("spawn scarlet")
}

fn fmt_stdin(source: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_scarlet"))
        .args(["fmt", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn scarlet");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(source.as_bytes())
        .expect("write stdin");
    child.wait_with_output().expect("wait")
}

#[test]
fn check_run_and_dis_refuse_single_quotes() {
    let p = Project::new("quotes_refused");
    p.write("old.sca", SINGLE);
    for command in ["check", "run", "dis"] {
        let out = run_al(command, &p.dir.join("old.sca"));
        assert_eq!(out.code, Some(1), "{command}: {}", out.combined());
        assert!(out.stdout.is_empty(), "{command} printed: {}", out.stdout);
        // One for each of the three strings, and nothing else.
        assert_eq!(out.stderr.matches(ERROR).count(), 3, "{}", out.stderr);
        assert!(out.stderr.contains("Found 3 errors"), "{}", out.stderr);
    }
}

#[test]
fn an_imported_file_in_single_quotes_is_refused() {
    let p = Project::new("quotes_import");
    p.write("lib.sca", "pub fn name() String { 'lib' }\n");
    p.write(
        "main.sca",
        "import ./lib\n\npub fn main() {\n\tprintln(lib::name())\n}\n",
    );
    let out = run_al("run", &p.dir.join("main.sca"));
    assert_eq!(out.code, Some(1), "{}", out.combined());
    assert!(out.stdout.is_empty(), "{}", out.stdout);
    assert_eq!(out.stderr.matches(ERROR).count(), 1, "{}", out.stderr);
}

#[test]
fn fmt_rewrites_a_file_that_then_runs() {
    let p = Project::new("quotes_fmt");
    p.write("old.sca", SINGLE);
    let file = p.dir.join("old.sca");
    let read = || std::fs::read_to_string(&file).expect("read old.sca");

    let check = fmt(&["--check"], &file);
    assert_eq!(check.status.code(), Some(1), "{check:?}");
    assert_eq!(read(), SINGLE, "--check wrote the file");

    let write = fmt(&[], &file);
    assert_eq!(write.status.code(), Some(0), "{write:?}");
    assert_eq!(read(), DOUBLE);

    let check = fmt(&["--check"], &file);
    assert_eq!(check.status.code(), Some(0), "{check:?}");
    let out = run_al("run", &file);
    assert_eq!(out.code, Some(0), "{}", out.combined());
    assert_eq!(out.stdout, "hi it's \"me\", bye\n");
}

#[test]
fn fmt_rewrites_stdin() {
    let out = fmt_stdin(SINGLE);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), DOUBLE);
}

#[test]
fn fmt_leaves_a_file_with_another_error() {
    let p = Project::new("quotes_broken");
    let broken = "x = 'a'\ny = )\n";
    p.write("broken.sca", broken);
    let file = p.dir.join("broken.sca");
    let out = fmt(&[], &file);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert_eq!(
        std::fs::read_to_string(&file).expect("read broken.sca"),
        broken
    );
    // The one error it names is the one to fix by hand.
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains(ERROR), "{stderr}");
    assert!(stderr.contains("Expected an expression"), "{stderr}");
}
