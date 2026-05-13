//! Negative tests: programs that SHOULD fail typechecking. We assert exit
//! code 1 and that the diagnostic output contains the expected error substring.

use std::io::Write;
use std::process::Command;

fn check_fails(source: &str, expected_substring: &str) {
    let mut tmp = std::env::temp_dir();
    // unique-ish name per test thread
    let pid = std::process::id();
    let tid = format!("{:?}", std::thread::current().id());
    tmp.push(format!("al_type_err_{pid}_{}.al", hash_str(&tid)));
    {
        let mut f = std::fs::File::create(&tmp).expect("create temp file");
        f.write_all(source.as_bytes()).expect("write temp file");
    }

    let bin = env!("CARGO_BIN_EXE_al");
    let out = Command::new(bin)
        .arg("check")
        .arg(&tmp)
        .output()
        .expect("spawn al check");

    let _ = std::fs::remove_file(&tmp);

    // Diagnostics are written to stdout (matching the V binary).
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let combined = format!("{stdout}{stderr}");

    assert!(
        !out.status.success(),
        "expected non-zero exit for program:\n{source}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );
    assert_eq!(
        out.status.code(),
        Some(1),
        "expected exit code 1 for program:\n{source}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );
    assert!(
        combined.contains(expected_substring),
        "expected output to contain {expected_substring:?} for program:\n{source}\n--- output ---\n{combined}"
    );
}

fn hash_str(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

#[test]
fn int_plus_string_is_type_error() {
    check_fails("x = 1 + 'a'\n", "Type mismatch");
}

#[test]
fn non_exhaustive_match_is_error() {
    check_fails(
        "f = fn(x) { match x { true -> 1 } }\nf(false)\n",
        "not exhaustive",
    );
}

#[test]
fn unknown_identifier_is_error() {
    check_fails("x = foo\n", "Unknown identifier");
}

#[test]
fn if_condition_must_be_bool() {
    check_fails("if 1 { 2 } else { 3 }\n", "Type mismatch");
}
