//! These test `scarlet dis` end to end: how the command is wired up and its exit
//! status. `scarlet::dis` has its own tests for what a listing contains.

use std::process::Command;

mod common;
use common::{Project, run_al};

const SQUARE: &str = "fn square(x Int) Int { x * x }\npub fn main() {\n\tprintln(square(3))\n}\n";

#[test]
fn dis_lists_the_files_own_functions() {
    let p = Project::new("dis_entry");
    p.write("sq.sca", SQUARE);
    let out = run_al("dis", &p.dir.join("sq.sca"));
    assert!(out.success, "{}", out.combined());
    assert!(out.stdout.contains("fn main.square("), "{}", out.stdout);
    assert!(out.stdout.contains("; toplevel"), "{}", out.stdout);
}

#[test]
fn dis_fails_on_a_name_nothing_has() {
    let p = Project::new("dis_nope");
    p.write("sq.sca", SQUARE);
    let out = Command::new(env!("CARGO_BIN_EXE_scarlet"))
        .arg("dis")
        .arg(p.dir.join("sq.sca"))
        .args(["--fn", "nope"])
        .output()
        .expect("spawn scarlet");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("no function matching 'nope'"), "{stderr}");
}

/// A program that does not compile has nothing to list, and says why.
#[test]
fn dis_reports_a_compile_error_instead_of_a_listing() {
    let p = Project::new("dis_bad");
    p.write("bad.sca", "pub fn main() {\n\tprintln(1 + 'a')\n}\n");
    let out = run_al("dis", &p.dir.join("bad.sca"));
    assert!(!out.success);
    assert!(out.stderr.contains("Type mismatch"), "{}", out.stderr);
    assert!(out.stdout.is_empty(), "{}", out.stdout);
}

/// `--instrs` adds, after each function's Core IR, the instructions the VM
/// loads it as. Without it the listing is the Core IR alone.
#[test]
fn dis_instrs_adds_the_loaded_instructions() {
    let p = Project::new("dis_instrs");
    p.write("sq.sca", SQUARE);
    let plain = run_al("dis", &p.dir.join("sq.sca"));
    assert!(plain.success, "{}", plain.combined());
    assert!(!plain.stdout.contains("; loaded"), "{}", plain.stdout);

    let out = Command::new(env!("CARGO_BIN_EXE_scarlet"))
        .arg("dis")
        .arg("--instrs")
        .arg(p.dir.join("sq.sca"))
        .output()
        .expect("spawn scarlet");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let square = stdout
        .split("\n; fn#")
        .find(|f| f.contains("fn main.square("))
        .expect("square is listed");
    assert!(square.contains("IntMul(%0, %0)"), "{square}");
    assert!(
        square.contains("; loaded: 1 register, parameters in r0\n"),
        "{square}"
    );
    assert!(square.contains("    0  r0 = IntMul(r0, r0)\n"), "{square}");
    assert!(square.contains("    1  Ret r0\n"), "{square}");
    let (_, toplevel) = stdout.split_once("\n; toplevel\n").expect("a toplevel");
    assert!(toplevel.contains("; loaded: "), "{toplevel}");
}
