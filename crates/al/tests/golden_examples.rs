use std::path::{Path, PathBuf};

mod common;
use common::run_al;

fn examples_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples")
}

// Internal regression-test programs live with the test suite, not in examples/.
fn programs_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/programs")
}

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

fn programs_golden_dir() -> PathBuf {
    golden_dir().join("programs")
}

fn run_file(source: &Path, name: &str) -> String {
    let out = run_al("run", source);
    if !out.success {
        panic!(
            "example {name} exited {:?}\nstdout:\n{}\nstderr:\n{}",
            out.code, out.stdout, out.stderr
        );
    }
    out.stdout
}

fn run_example(name: &str) -> String {
    run_file(&examples_dir().join(format!("{name}.al")), name)
}

fn run_program(name: &str) -> String {
    run_file(&programs_dir().join(format!("{name}.al")), name)
}

fn assert_stdout_matches(name: &str, golden: &Path, got: &str) {
    let want = std::fs::read_to_string(golden)
        .unwrap_or_else(|e| panic!("missing golden for {name}: {e}"));
    if got != want {
        let diff = diff_lines(&want, got);
        panic!("output mismatch for {name}:\n{diff}");
    }
}

fn assert_golden(name: &str) {
    let golden = golden_dir().join(format!("{name}.stdout"));
    let got = run_example(name);
    assert_stdout_matches(name, &golden, &got);
}

fn assert_golden_program(name: &str) {
    let golden = programs_golden_dir().join(format!("{name}.stdout"));
    let got = run_program(name);
    assert_stdout_matches(name, &golden, &got);
}

// Servers and timing-dependent demos have no deterministic output to golden
// test, but they must still type check.
fn assert_example_checks(name: &str) {
    let example = examples_dir().join(format!("{name}.al"));
    let out = run_al("check", &example);
    if !out.success {
        panic!(
            "al check {name} exited {:?}\nstdout:\n{}\nstderr:\n{}",
            out.code, out.stdout, out.stderr
        );
    }
}

fn diff_lines(want: &str, got: &str) -> String {
    let w: Vec<_> = want.lines().collect();
    let g: Vec<_> = got.lines().collect();
    let mut out = String::new();
    let n = w.len().max(g.len());
    for i in 0..n {
        let wl = w.get(i).copied().unwrap_or("<missing>");
        let gl = g.get(i).copied().unwrap_or("<missing>");
        if wl != gl {
            out.push_str(&format!(
                "  line {}: want {:?}\n           got  {:?}\n",
                i + 1,
                wl,
                gl
            ));
        }
    }
    if out.is_empty() {
        out.push_str(&format!(
            "  (line content matches but lengths differ: want {} lines, got {} lines)\n",
            w.len(),
            g.len()
        ));
    }
    out
}

// Golden test for a showcase example: runs examples/<name>.al and compares
// stdout to tests/golden/<name>.stdout.
macro_rules! golden {
    ($name:ident) => {
        #[test]
        fn $name() {
            assert_golden(stringify!($name));
        }
    };
}

// Golden test for an internal regression program: runs tests/programs/<name>.al
// and compares stdout to tests/golden/programs/<name>.stdout.
macro_rules! golden_program {
    ($name:ident) => {
        #[test]
        fn $name() {
            assert_golden_program(stringify!($name));
        }
    };
}

// Type-check-only test for examples whose output is not deterministic
// (servers, scheduler demos).
macro_rules! checks {
    ($test_name:ident, $example:literal) => {
        #[test]
        fn $test_name() {
            assert_example_checks($example);
        }
    };
}

// Core language — must pass for parity
golden!(hello);
golden!(factorial);
golden!(fibonacci);
golden!(fizzbuzz);
golden!(collatz);
golden!(gcd);
golden!(power);
golden!(bigint);
golden!(primes);
golden!(leap_year);
golden!(temperature);
golden!(triangle);
golden!(roman);
golden!(grades);
golden!(age);
golden!(shapes);
golden!(tco);
golden!(tco_mutual);
golden!(doc_comment);

// Stdlib-gated
golden!(string_split);
golden!(password);
golden!(bench_list);

// Data structures and algorithms
golden!(calculator);
golden!(tree);
golden!(life);
golden!(mergesort);
golden!(wordcount);
golden!(maps);
golden!(money);

// Binaries and wire protocols
golden!(packet);

// Internal regression programs (crates/al/tests/programs/), formerly in examples/.
golden_program!(basic);
golden_program!(program_type);
golden_program!(conrad_fib);

// HTTP/1.1 protocol surface: parsing, framing, smuggling rejects, header
// lookup, serialization. Locks the native (VM-builtin) scanners behind
// al/http/h1 to the sans-IO contract the AL reference parser defined.
golden_program!(http_parse);

// Type system / generics / inference regression programs
golden_program!(awesome_inference);
golden_program!(generic_test);
golden_program!(enum_equality_test);
golden_program!(match_patterns_test);
golden_program!(trying_out_tuples);
golden_program!(trying_out_generic_structs_and_enums);
golden_program!(all_language_features);

// Servers and timing demos: output is not deterministic, so no golden file.
// They must still type check.
checks!(check_http_hello, "http_hello");
checks!(check_http_server, "http_server");
checks!(check_echo_server, "echo_server");
checks!(check_processes, "processes");
checks!(check_read_within, "read_within");

// bench is timing-sensitive; just check it runs without error
#[test]
fn bench_runs() {
    run_example("bench");
}
