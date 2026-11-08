//! `scarlet lint` — the CLI half of the `.sca` illegal-state census.
//!
//! The census used to be `cargo xtask scrl-census` and is now
//! `scarlet_core::lint`, so every assertion here drives the real binary rather
//! than calling the library: a census reachable only from `scarlet_core` would
//! leave the unit tests green while `scarlet lint` did nothing at all, which is
//! the one failure the move can introduce.
//!
//! What these do not witness is the census logic — which shapes it reports and
//! which it declines to — that is unit-tested beside it in `scarlet_core::lint`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Two `Bool` fields in one constructor: the shape the census reports.
const CLUSTER: &str = "pub opaque type GateFlags {\n\tarmed Bool\n\tlatched Bool\n}\n";

/// One `Bool` is under the floor, and the `Int` beside it is not a flag. This
/// is the honest control — it runs in the same suite and stays quiet, so a red
/// arm above is a red the fixture earned.
const CLEAN: &str = "pub type Counter {\n\tcount Int\n}\n";

fn fixture(name: &str, source: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("scarlet_lint_{name}_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create fixture dir");
    fs::write(dir.join("fixture.sca"), source).expect("write fixture");
    dir
}

fn lint(args: &[&str], cwd: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_scarlet"))
        .env("NO_COLOR", "1")
        .arg("lint")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("run scarlet lint")
}

fn stdout_of(out: &std::process::Output) -> String {
    assert!(
        out.status.success(),
        "scarlet lint rc={:?} stderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// No path argument, so the root is the working directory. As an xtask this
/// defaulted to `CARGO_MANIFEST_DIR`, which in a released binary would name the
/// build machine's checkout instead of the caller's.
#[test]
fn lint_surveys_the_working_directory_by_default() {
    let dir = fixture("default_root", CLUSTER);
    let text = stdout_of(&lint(&[], &dir));
    assert!(
        text.contains("GateFlags") && text.contains("2 Bool fields, 4 states: armed, latched"),
        "census did not report the planted cluster:\n{text}"
    );
    assert!(
        text.contains("1 finding(s)") && text.contains("0 file(s) not inspected"),
        "census did not read the fixture:\n{text}"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// Run from a directory holding no corpus of its own, so a finding can only
/// have come from the argument.
#[test]
fn lint_surveys_an_explicit_root() {
    let dir = fixture("explicit_root", CLUSTER);
    let elsewhere = fixture("explicit_elsewhere", CLEAN);
    let text = stdout_of(&lint(&[&dir.to_string_lossy()], &elsewhere));
    assert!(
        text.contains("GateFlags") && text.contains("1 finding(s)"),
        "explicit root was not surveyed:\n{text}"
    );
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::remove_dir_all(&elsewhere);
}

#[test]
fn a_corpus_without_a_cluster_reports_none() {
    let dir = fixture("clean", CLEAN);
    let text = stdout_of(&lint(&[], &dir));
    // `in 1 file(s)` and not just `0 finding(s)`: a census that surveyed
    // nothing at all reports zero findings too, so the count of what was read
    // is the half that separates quiet from inert.
    assert!(
        text.contains("0 finding(s) over 1 type declaration(s) in 1 file(s)")
            && text.contains("0 file(s) not inspected"),
        "clean corpus did not report an inspected, empty census:\n{text}"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// `--min-bools` is a clap `value_parser`, so the floor is enforced at the
/// parse boundary and the census never runs.
#[test]
fn min_bools_below_the_floor_is_refused() {
    let dir = fixture("floor", CLUSTER);
    let out = lint(&["--min-bools", "1"], &dir);
    assert!(
        !out.status.success(),
        "--min-bools 1 was accepted; stdout={}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("finding(s)"),
        "a refused parse still printed a census"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// Raising the floor past the fixture's width takes the finding away — the
/// flag reaches `lint::run` rather than being parsed and dropped.
#[test]
fn min_bools_above_the_cluster_width_reports_nothing() {
    let dir = fixture("raised_floor", CLUSTER);
    let text = stdout_of(&lint(&["--min-bools", "3"], &dir));
    assert!(
        text.contains("0 finding(s) over 1 type declaration(s) in 1 file(s)"),
        "--min-bools 3 still reported a 2-Bool cluster:\n{text}"
    );
    let _ = fs::remove_dir_all(&dir);
}
