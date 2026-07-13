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

/// Every `.al` in a source dir is either wired into the suite or listed as
/// deliberately untested, and every golden in the matching golden dir belongs
/// to a wired program. Subdirectories are skipped: `examples/lib/` holds
/// modules that exist to be imported, and `golden/core_ir/` belongs to
/// `core_ir.rs`.
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

// The whole suite, in one place. Each name is both the `.al` file's stem and
// the generated test's name, so a name must be a valid Rust identifier and
// unique across all four lists.
//
//   examples  — examples/<n>.al          run, diff against golden/<n>.stdout
//   programs  — tests/programs/<n>.al    run, diff against golden/programs/<n>.stdout
//   checks    — examples/<n>.al          `al check` only, output is not deterministic
//   untested  — examples/<n>.al          deliberately outside the suite
//
// `suite_is_exhaustive` closes the loop: a new `.al` that appears in either
// source dir without an entry here fails, and so does a golden left behind by
// a deleted program.
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
    // TIER A — examples/: showcase programs, one theme per file. Read top to
    // bottom, they teach the language. Every one is also format-idempotency
    // checked by al_core's `idempotent_on_examples`, and — because `run_file`
    // asserts the child wrote *zero* bytes to stderr — none of them may emit a
    // warning or a diagnostic.
    examples: [
        // Language core.
        hello,
        control_flow,
        pattern_matching,
        data_types,
        generics,
        closures,
        // Named tco.al, not tail_recursion.al: al/internal.al's `stack_depth`
        // doc comment points at examples/tco.al by name.
        tco,
        errors,
        // Stdlib surface.
        collections,
        strings,
        numbers,
        money,
        wire_format,
        // Effects. `sockets` and `http_client` are the effectful examples with
        // a fixed output: each binds a loopback listener on port 0, serves it
        // in-process, drives requests through it from the main process, then
        // closes the listener — which wakes the parked acceptors with
        // NotConnected, so every process finishes and the program exits. Both
        // print facts *about* the kernel-assigned port, never the port itself.
        // They need loopback TCP: a sandbox that denies bind(2) fails these
        // tests, exactly as it already fails `vm_io::tcp_connect_and_vectored_echo`.
        //
        // `http_client` runs that shape twice: once against http.serve_on, then
        // once against a hand-rolled connection driver. That second listener is
        // what covers al/http/body's socket-bound half — content_length and the
        // five drains — which tests/programs/http_parse.al, being sans-IO by
        // design, cannot reach.
        sockets,
        http_client,
        // Multi-file program: imports examples/lib/units.al and
        // examples/lib/report/table.al, which in turn imports `../units` — a
        // relative path resolves against the importing file's own directory.
        modules,
        // Algorithms, then the capstone: a lexer, parser and evaluator built
        // only from what the examples above teach. Read last.
        life,
        interpreter,
        // Benchmarks that scripts/bench*.sh also drive. Both are deterministic,
        // so they are goldened like any other example; `assert_golden_in`
        // asserts exit-0 and an empty stderr on top of the output diff.
        bench,
        bench_list,
    ],

    // TIER B — crates/al/tests/programs/: internal regression programs, one
    // subsystem per file. Not showcase code; they exist to pin compiler
    // behaviour, so they lean adversarial and print PASS/FAIL where a bare
    // value would not say what the right answer was.
    programs: [
        // Type system: HM inference, generalization, and monomorphisation.
        inference,
        generics_adversarial,
        // Pattern matching, equality, and the shapes values come in.
        exhaustive_match,
        tuples_and_records,
        enum_equality,
        // Evaluation: tail calls in constant stack, closure capture, and the
        // core semantics a program relies on without naming them.
        tco_and_closures,
        semantics,
        // Numeric edges: i64 wrapping, boxed ints, float canonicalization,
        // exact decimals.
        numerics,
        // The deterministic slice of the effectful stdlib: al/io's typed
        // IoError paths, al/time's monotonic invariants, al/process's
        // argv/env — all pinned as derived facts, never as a clock reading or
        // an env value.
        effects,
        // HTTP/1.1 protocol surface: parsing, framing, smuggling rejects,
        // header lookup, serialization. Locks the native (VM-builtin) scanners
        // behind al/http/h1 to the sans-IO contract the AL reference parser
        // defined.
        http_parse,
    ],

    // Servers and timing demos: no golden file, because there is no fixed
    // output to diff — `http_server` ends in an unbounded accept loop that
    // never returns, and `processes` prints from racing processes, whose order
    // is not fixed and whose spawn_per_core fan-out is as wide as the machine
    // has cores. They must still type check.
    checks: [
        http_server,
        processes,
    ],

    // Perf infrastructure driven from outside this file — scripts/bench*.sh,
    // and `vm_exec::bench_typed_output_is_pinned` for bench_typed — plus the
    // scratch pair. Deliberately outside the suite.
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

// bench.al is goldened above, which already asserts it runs clean. This keeps
// the timing-free "it still runs" check the bench scripts depend on.
#[test]
fn bench_runs() {
    run_file(&examples_dir().join("bench.al"), "bench");
}
