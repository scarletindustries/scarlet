//! Differential integration tests: run every examples/*.al through the Rust
//! binary and compare stdout against golden output captured from the V binary.

use std::path::PathBuf;
use std::process::Command;

fn examples_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples")
}

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

fn run_example(name: &str, extra_flags: &[&str]) -> String {
    let bin = env!("CARGO_BIN_EXE_al");
    let example = examples_dir().join(format!("{name}.al"));
    let mut cmd = Command::new(bin);
    cmd.arg("run");
    for f in extra_flags {
        cmd.arg(f);
    }
    cmd.arg(&example);
    let out = cmd
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn al for {name}: {e}"));
    if !out.status.success() {
        panic!(
            "example {name} exited {:?}\nstdout:\n{}\nstderr:\n{}",
            out.status.code(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    String::from_utf8(out.stdout).expect("stdout not utf8")
}

fn assert_golden(name: &str, extra_flags: &[&str]) {
    let want = std::fs::read_to_string(golden_dir().join(format!("{name}.stdout")))
        .unwrap_or_else(|e| panic!("missing golden for {name}: {e}"));
    let got = run_example(name, extra_flags);
    if got != want {
        let diff = diff_lines(&want, &got);
        panic!("output mismatch for {name}:\n{diff}");
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

macro_rules! golden {
    ($name:ident) => {
        #[test]
        fn $name() {
            assert_golden(stringify!($name), &[]);
        }
    };
    ($name:ident, stdlib) => {
        #[test]
        fn $name() {
            assert_golden(stringify!($name), &["--experimental-std-lib"]);
        }
    };
}

// Core language — must pass for parity
golden!(hello);
golden!(basic);
golden!(factorial);
golden!(fibonacci);
golden!(fizzbuzz);
golden!(collatz);
golden!(gcd);
golden!(power);
golden!(primes);
golden!(leap_year);
golden!(temperature);
golden!(triangle);
golden!(roman);
golden!(grades);
golden!(age);
golden!(shapes);
golden!(tco);
golden!(conrad_fib);
golden!(doc_comment);
golden!(program_type);

// Type system / generics / inference — critical
golden!(awesome_inference);
golden!(generic_test);
golden!(enum_equality_test);
golden!(match_patterns_test);
golden!(trying_out_tuples);
golden!(trying_out_generic_structs_and_enums);
golden!(all_language_features);

// Stdlib-gated
golden!(string_split, stdlib);

// bench is timing-sensitive; just check it runs without error
#[test]
fn bench_runs() {
    run_example("bench", &[]);
}
