use std::path::{Path, PathBuf};

mod common;
use common::{diff_lines, run_al};

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
    assert!(
        out.stderr.is_empty(),
        "example {name} wrote to stderr:\n{}",
        out.stderr
    );
    out.stdout
}

/// `al run` `<src_dir>/<name>.al` and diff stdout against
/// `<golden_dir>/<name>.stdout`.
fn assert_golden_in(src_dir: &Path, golden_dir: &Path, name: &str) {
    let got = run_file(&src_dir.join(format!("{name}.al")), name);
    let golden = golden_dir.join(format!("{name}.stdout"));
    let want = std::fs::read_to_string(&golden)
        .unwrap_or_else(|e| panic!("missing golden for {name}: {e}"));
    if got != want {
        let diff = diff_lines(&want, &got);
        panic!("output mismatch for {name}:\n{diff}");
    }
}

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

/// Every `.al` in a source dir is either wired into the suite or listed as
/// untested, and every golden belongs to a wired program. Subdirectories are
/// skipped: `examples/lib/` holds modules that exist to be imported, and
/// `golden/core_ir/` belongs to `core_ir.rs`.
fn assert_dir_wired(src_dir: &Path, golden_dir: &Path, wired: &[String], goldens: &[String]) {
    for entry in std::fs::read_dir(src_dir).expect("source dir") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|s| s.to_str()) != Some("al") {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            wired.contains(&name),
            "{} is in no test: give it a golden, a check, or an `untested` entry \
             in golden_examples.rs",
            path.display()
        );
    }
    for entry in std::fs::read_dir(golden_dir).expect("golden dir") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|s| s.to_str()) != Some("stdout") {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            goldens.contains(&name),
            "orphan golden {}: no program in golden_examples.rs produces it",
            path.display()
        );
    }
}

// Each name is both the `.al` file's stem and the generated test's name, so it
// must be a valid Rust identifier and unique across all four lists.
//
//   examples  — examples/<n>.al          run, diff against golden/<n>.stdout
//   programs  — tests/programs/<n>.al    run, diff against golden/programs/<n>.stdout
//   checks    — examples/<n>.al          `al check` only, output is not deterministic
//   untested  — examples/<n>.al          deliberately outside the suite
//
// `suite_is_exhaustive` closes the loop: a new `.al` in either source dir with
// no entry here fails, and so does a golden left behind by a deleted program.
macro_rules! suite {
    (
        examples: [ $($example:ident),* $(,)? ],
        programs: [ $($program:ident),* $(,)? ],
        checks: [ $($check:ident),* $(,)? ],
        untested: [ $($untested:literal),* $(,)? ],
    ) => {
        $(
            #[test]
            fn $example() {
                assert_golden_in(&examples_dir(), &golden_dir(), stringify!($example));
            }
        )*

        $(
            #[test]
            fn $program() {
                assert_golden_in(&programs_dir(), &programs_golden_dir(), stringify!($program));
            }
        )*

        $(
            #[test]
            fn $check() {
                assert_example_checks(stringify!($check));
            }
        )*

        #[test]
        fn suite_is_exhaustive() {
            let mut examples: Vec<String> = Vec::new();
            let mut example_goldens: Vec<String> = Vec::new();
            $(
                examples.push(format!("{}.al", stringify!($example)));
                example_goldens.push(format!("{}.stdout", stringify!($example)));
            )*
            $( examples.push(format!("{}.al", stringify!($check))); )*
            $( examples.push($untested.to_string()); )*
            assert_dir_wired(&examples_dir(), &golden_dir(), &examples, &example_goldens);

            let mut programs: Vec<String> = Vec::new();
            let mut program_goldens: Vec<String> = Vec::new();
            $(
                programs.push(format!("{}.al", stringify!($program)));
                program_goldens.push(format!("{}.stdout", stringify!($program)));
            )*
            assert_dir_wired(
                &programs_dir(),
                &programs_golden_dir(),
                &programs,
                &program_goldens,
            );
        }
    };
}

suite! {
    // TIER A — examples/: showcase programs, one theme per file, ordered to
    // teach the language top to bottom. Also format-idempotency checked by
    // al_core's `idempotent_on_examples`. None may emit a warning: `run_file`
    // asserts the child wrote zero bytes to stderr.
    examples: [
        // Language core.
        hello,
        control_flow,
        pattern_matching,
        data_types,
        generics,
        closures,
        // Named tco.al: al/internal.al's `stack_depth` doc points at it.
        tco,
        errors,
        // Stdlib surface.
        collections,
        strings,
        numbers,
        money,
        wire_format,
        // Effects. Both bind a loopback listener on port 0, serve it
        // in-process, then close it, which wakes the parked acceptors with
        // NotConnected so the program exits. Both print facts *about* the
        // kernel-assigned port, never the port itself. A sandbox that denies
        // bind(2) fails these, as it already fails
        // `vm_io::tcp_connect_and_vectored_echo`.
        //
        // `http_client` does that twice, the second time against a hand-rolled
        // connection driver, which is the only coverage of al/http/body's
        // socket-bound half — sans-IO http_parse.al cannot reach it.
        sockets,
        http_client,
        // Multi-file: imports examples/lib/units.al and lib/report/table.al,
        // which imports `../units` relative to its own directory.
        modules,
        // Algorithms, then the capstone: a lexer, parser and evaluator built
        // only from what the examples above teach. Read last.
        life,
        interpreter,
        // Benchmarks scripts/bench*.sh also drives. Deterministic, so goldened
        // like any other example.
        bench,
        bench_list,
    ],

    // TIER B — crates/al/tests/programs/: internal regression programs, one
    // subsystem per file. They pin compiler behaviour, so they lean adversarial
    // and print PASS/FAIL where a bare value would not say what the right
    // answer was.
    programs: [
        // Type system: HM inference, generalization, and monomorphisation.
        inference,
        generics_adversarial,
        // Pattern matching, equality, and the shapes values come in.
        exhaustive_match,
        tuples_and_records,
        enum_equality,
        // Evaluation: tail calls in constant stack, closure capture, and core
        // semantics.
        tco_and_closures,
        semantics,
        // Numeric edges: i64 wrapping, boxed ints, float canonicalization,
        // exact decimals.
        numerics,
        // The deterministic slice of the effectful stdlib. Everything is pinned
        // as a derived fact, never a clock reading or an env value.
        effects,
        // HTTP/1.1 surface. Locks the native scanners behind al/http/h1 to the
        // sans-IO contract the AL reference parser defined.
        http_parse,
    ],

    // No golden, because there is no fixed output: `http_server` ends in an
    // unbounded accept loop, and `processes` prints from racing processes whose
    // order and fan-out depend on the machine. They must still type check.
    checks: [
        http_server,
        processes,
    ],

    // Perf infrastructure driven from outside this file (scripts/bench*.sh, and
    // `vm_exec::bench_typed_output_is_pinned`), plus the scratch pair.
    untested: [
        "bench_heavy.al",
        "bench_list_1x.al",
        "bench_list_2x.al",
        "bench_list_4x.al",
        "bench_map.al",
        "bench_service.al",
        "bench_typed.al",
        "a.al",
        "b.al",
    ],
}

// The timing-free "it still runs" check the bench scripts depend on.
#[test]
fn bench_runs() {
    run_file(&examples_dir().join("bench.al"), "bench");
}
