//! These test `scarlet run --counters` and `scarlet run --audit` end to end:
//! how the flags are wired up, what the counters file holds, and the exit
//! status. `scarlet_vm` has its own tests for what a run counts.
//!
//! What is not tested here is an audit that fails. That needs a program
//! whose reference counts go wrong, and no Scarlet source is known to make
//! one. `scarlet_vm`'s own tests make the audit fail on a heap they have
//! tampered with (`exec::tests`), for an audited run and an unaudited one.
//! So if `scarlet run` stopped passing `--audit` on to the VM, no test would
//! notice.

use std::path::Path;
use std::process::{Command, Output, Stdio};

mod common;
use common::Project;

const LIST: &str = "type L {\n\tCons(h Int, t L)\n\tEnd\n}\n\
                    fn build(n Int, acc L) L {\n\
                    \tif n == 0 { acc } else { build(n - 1, Cons(n, acc)) }\n\
                    }\n\
                    fn sum(l L, acc Int) Int {\n\
                    \tmatch l {\n\
                    \t\tCons(h, t) -> sum(t, acc + h)\n\
                    \t\tEnd -> acc\n\
                    \t}\n\
                    }\n\
                    pub fn main() {\n\
                    \tprintln(sum(build(100, End), 0))\n\
                    }\n";

/// A program that prints a line and then reaches something the VM does not
/// run yet, so its run stops before its end.
const STOPS: &str = "import scarlet/internal\n\
                     fn later() Int { internal::live_subjects() }\n\
                     pub fn main() {\n\
                     \tprintln('before')\n\
                     \tprintln(later())\n\
                     }\n";

/// A program that prints more than a pipe holds.
const PRINTS_A_LOT: &str = "fn count(n Int) Int {\n\
                            \tif n == 0 { 0 } else {\n\
                            \t\tprintln(n)\n\
                            \t\tcount(n - 1)\n\
                            \t}\n\
                            }\n\
                            pub fn main() {\n\
                            \tprintln(count(200000))\n\
                            }\n";

/// `scarlet run <flags> prog.sca`, with exactly the flags given. This file
/// tests the flags, so it is the one place that does not start from
/// `common::scarlet_run`.
fn command(p: &Project, src: &str, flags: &[&str]) -> Command {
    p.write("prog.sca", src);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_scarlet"));
    cmd.arg("run").args(flags).arg(p.dir.join("prog.sca"));
    cmd
}

fn scarlet_run(p: &Project, flags: &[&str]) -> Output {
    command(p, LIST, flags).output().expect("spawn scarlet")
}

/// The file holds every kind of instruction and how often it ran, their
/// total, and the heap's counts. The program prints what it always prints.
#[test]
fn counters_writes_what_the_run_counted() {
    let p = Project::new("counters");
    let file = p.dir.join("counts.json");
    let out = scarlet_run(&p, &["--counters", file.to_str().expect("a UTF-8 path")]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "5050\n");
    assert!(out.stderr.is_empty(), "{out:?}");

    let text = std::fs::read_to_string(&file).expect("the counters file");
    let counts: serde_json::Value = serde_json::from_str(&text).expect("JSON");
    let ops = counts["ops"].as_object().expect("an object of kinds");
    let total: u64 = ops.values().map(|n| n.as_u64().expect("a count")).sum();
    assert_eq!(counts["instructions"].as_u64(), Some(total));
    assert!(total > 100, "{total}");
    assert_eq!(ops["Ctor"].as_u64(), Some(100), "one cell for each Cons");
    assert_eq!(
        ops["Bad"].as_u64(),
        Some(0),
        "a kind that never ran is listed"
    );
    assert!(counts["cells_made"].as_u64() >= Some(100), "{text}");
    assert!(counts["cells_reused"].is_u64(), "{text}");
    assert!(counts["cells_live"].is_u64(), "{text}");
}

/// Two runs of one program count the same, to the last instruction.
#[test]
fn counters_are_the_same_on_every_run() {
    let p = Project::new("counters_twice");
    let (a, b) = (p.dir.join("a.json"), p.dir.join("b.json"));
    for file in [&a, &b] {
        let out = scarlet_run(&p, &["--counters", file.to_str().expect("a UTF-8 path")]);
        assert!(out.status.success(), "{out:?}");
    }
    let read = |f| std::fs::read_to_string(f).expect("the counters file");
    assert_eq!(read(&a), read(&b));
}

/// A counters file that cannot be written fails the run and says so, before
/// the program has run at all.
#[test]
fn counters_to_a_path_that_cannot_be_written_fails() {
    let p = Project::new("counters_nowhere");
    let file = p.dir.join("no/such/dir/counts.json");
    let out = scarlet_run(&p, &["--counters", file.to_str().expect("a UTF-8 path")]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(out.stdout.is_empty(), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("cannot write the counters to"), "{stderr}");
}

/// A run that stops before its end has no counts. Its counters file is
/// left empty, not holding what an earlier run wrote there.
#[test]
fn a_run_that_stops_leaves_no_earlier_counts_behind() {
    let p = Project::new("counters_stopped");
    let file = p.dir.join("counts.json");
    std::fs::write(&file, "{\"stale\": true}\n").expect("write the earlier counts");
    let flags = ["--counters", file.to_str().expect("a UTF-8 path")];
    let out = command(&p, STOPS, &flags).output().expect("spawn scarlet");
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "before\n");
    assert_eq!(std::fs::read_to_string(&file).expect("the file"), "");
}

/// A program whose reader has left stops quietly and exits 0, as it does
/// under `| head`. It has no counts either, so its counters file is empty
/// too, and a reader that trusts the exit status reads no earlier run's.
#[test]
fn a_run_whose_output_closed_leaves_no_earlier_counts_behind() {
    let p = Project::new("counters_closed");
    let file = p.dir.join("counts.json");
    std::fs::write(&file, "{\"stale\": true}\n").expect("write the earlier counts");
    let flags = ["--counters", file.to_str().expect("a UTF-8 path")];
    let mut child = command(&p, PRINTS_A_LOT, &flags)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn scarlet");
    // Closing our end before the program has printed anything means its
    // first write finds nobody reading.
    drop(child.stdout.take());
    let out = child.wait_with_output().expect("the run ends");
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(out.stderr.is_empty(), "{out:?}");
    assert_eq!(std::fs::read_to_string(&file).expect("the file"), "");
}

/// A program whose counts are all right passes its audit and prints nothing
/// more. The audit and the counters can be asked for together.
#[test]
fn audit_passes_a_program_that_frees_everything() {
    let p = Project::new("audit");
    let out = scarlet_run(&p, &["--audit"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "5050\n");
    assert!(out.stderr.is_empty(), "{out:?}");

    let file = p.dir.join("counts.json");
    let both = [
        "--audit",
        "--counters",
        file.to_str().expect("a UTF-8 path"),
    ];
    let out = scarlet_run(&p, &both);
    assert!(out.status.success(), "{out:?}");
    assert!(file.is_file());
}

/// Every other test that runs a program starts from `common::scarlet_run`,
/// which audits the run. A test that spelt out its own `scarlet run` would
/// leave the audit off without anyone noticing, so no other test file may.
#[test]
fn only_this_file_spells_out_a_run_of_its_own() {
    fn spelt_out(dir: &Path, found: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).expect("the tests directory") {
            let path = entry.expect("a directory entry").path();
            if path.is_dir() {
                spelt_out(&path, found);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let text = std::fs::read_to_string(&path).expect("a test file");
                // Written in two pieces so this file's own text does not
                // hold what it looks for.
                let arg = [".arg(", "\"run\")"].concat();
                let args = ["[", "\"run\""].concat();
                if text.contains(&arg) || text.contains(&args) {
                    found.push(path.display().to_string());
                }
            }
        }
    }
    let tests = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut found = Vec::new();
    spelt_out(&tests, &mut found);
    found.sort();
    let allowed = ["tests/common/mod.rs", "tests/run_counters.rs"];
    let others: Vec<&String> = found
        .iter()
        .filter(|f| !allowed.iter().any(|a| f.ends_with(a)))
        .collect();
    assert!(
        others.is_empty(),
        "these start `scarlet run` without `common::scarlet_run`: {others:?}"
    );
    assert_eq!(found.len(), allowed.len(), "{found:?}");
}
