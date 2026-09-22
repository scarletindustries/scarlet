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

/// `al run` `<src_dir>/<name>.scrl` and diff stdout against
/// `<golden_dir>/<name>.stdout`.
fn assert_golden_in(src_dir: &Path, golden_dir: &Path, name: &str) {
    let got = run_file(&src_dir.join(format!("{name}.scrl")), name);
    let golden = golden_dir.join(format!("{name}.stdout"));
    let want = std::fs::read_to_string(&golden)
        .unwrap_or_else(|e| panic!("missing golden for {name}: {e}"));
    if got != want {
        let diff = diff_lines(&want, &got);
        panic!("output mismatch for {name}:\n{diff}");
    }
}

fn assert_example_checks(name: &str) {
    assert_checks(&examples_dir(), name);
}

fn assert_checks(src_dir: &Path, name: &str) {
    let out = run_al("check", &src_dir.join(format!("{name}.scrl")));
    if !out.success {
        panic!(
            "al check {name} exited {:?}\nstdout:\n{}\nstderr:\n{}",
            out.code, out.stdout, out.stderr
        );
    }
}

/// Every `.scrl` in a source dir is either wired into the suite or listed as
/// untested, and every golden belongs to a wired program. Subdirectories are
/// skipped: `examples/lib/` holds modules that exist to be imported, and
/// `golden/core_ir/` belongs to `core_ir.rs`.
fn assert_dir_wired(src_dir: &Path, golden_dir: &Path, wired: &[String], goldens: &[String]) {
    for entry in std::fs::read_dir(src_dir).expect("source dir") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|s| s.to_str()) != Some("scrl") {
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

// Each name is both the `.scrl` file's stem and the generated test's name, so it
// must be a valid Rust identifier and unique across all four lists.
//
//   examples  — examples/<n>.scrl          run, diff against golden/<n>.stdout
//   programs  — tests/programs/<n>.scrl    run, diff against golden/programs/<n>.stdout
//   checks    — examples/<n>.scrl          `al check` only, output is not deterministic
//   untested  — examples/<n>.scrl          deliberately outside the suite
//
// `suite_is_exhaustive` closes the loop: a new `.scrl` in either source dir with
// no entry here fails, and so does a golden left behind by a deleted program.
macro_rules! suite {
    (
        examples: [ $( $(#[$em:meta])* $example:ident ),* $(,)? ],
        programs: [ $( $(#[$pm:meta])* $program:ident ),* $(,)? ],
        checks: [ $($check:ident),* $(,)? ],
        untested: [ $($untested:literal),* $(,)? ],
    ) => {
        $(
            #[test]
            $(#[$em])*
            fn $example() {
                assert_golden_in(&examples_dir(), &golden_dir(), stringify!($example));
            }
        )*

        $(
            #[test]
            $(#[$pm])*
            fn $program() {
                assert_golden_in(&programs_dir(), &programs_golden_dir(), stringify!($program));
            }
        )*

        /// Every program with a golden still type-checks while its run waits
        /// for the VM, so a front-end regression cannot hide behind the
        /// ignore.
        #[test]
        fn every_golden_program_checks() {
            $( assert_checks(&examples_dir(), stringify!($example)); )*
            $( assert_checks(&programs_dir(), stringify!($program)); )*
        }

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
                examples.push(format!("{}.scrl", stringify!($example)));
                example_goldens.push(format!("{}.stdout", stringify!($example)));
            )*
            $( examples.push(format!("{}.scrl", stringify!($check))); )*
            $( examples.push($untested.to_string()); )*
            assert_dir_wired(&examples_dir(), &golden_dir(), &examples, &example_goldens);

            let mut programs: Vec<String> = Vec::new();
            let mut program_goldens: Vec<String> = Vec::new();
            $(
                programs.push(format!("{}.scrl", stringify!($program)));
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
    // scarlet_core's `idempotent_on_examples`. None may emit a warning: `run_file`
    // asserts the child wrote zero bytes to stderr.
    examples: [
        // Language core.
        hello,
        control_flow,
        pattern_matching,
        data_types,
        generics,
        closures,
        // Named tco.scrl: scarlet/internal.scrl's `stack_depth` doc points at it.
        #[ignore = "needs the VM"]
        tco,
        errors,
        // Stdlib surface.
        collections,
        strings,
        #[ignore = "needs the VM"]
        numbers,
        money,
        wire_format,
        // Effects. Both bind a loopback listener on port 0, serve it
        // in-process, then close it, which wakes the parked acceptors with
        // Ok(None) so the program exits. Both print facts *about* the
        // kernel-assigned port, never the port itself. A sandbox that denies
        // bind(2) fails these, as it already fails
        // `vm_io::tcp_connect_and_vectored_echo`.
        //
        // `http_client` does that twice, the second time against a hand-rolled
        // connection driver, which is the only coverage of scarlet/http/body's
        // socket-bound half — sans-IO http_parse.scrl cannot reach it.
        #[ignore = "needs the VM"]
        sockets,
        #[ignore = "needs the VM"]
        http_client,
        // Multi-file: imports examples/lib/units.scrl and lib/report/table.scrl,
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

    // TIER B — crates/scarlet/tests/programs/: internal regression programs, one
    // subsystem per file. They pin compiler behaviour, so they lean adversarial
    // and print PASS/FAIL where a bare value would not say what the right
    // answer was.
    programs: [
        // Type system: HM inference, generalization, and monomorphisation.
        inference,
        generics_adversarial,
        // Pattern matching, equality, and the shapes values come in.
        #[ignore = "needs the VM"]
        exhaustive_match,
        tuples_and_records,
        #[ignore = "needs the VM"]
        enum_equality,
        // Field punning on constructor calls: `f(now:, self:)` desugars to
        // `f(now: now, self: self)` at parse time.
        field_punning,
        // Evaluation: tail calls in constant stack, closure capture, and core
        // semantics.
        #[ignore = "needs the VM"]
        tco_and_closures,
        semantics,
        // Numeric edges: i64 wrapping, boxed ints, float canonicalization,
        // exact decimals.
        #[ignore = "needs the VM"]
        numerics,
        // Bitwise edges: the sign bit, shift counts at and past the 64-bit
        // width, negative counts, and arithmetic (not logical) right shift.
        #[ignore = "needs the VM"]
        bitwise,
        // Hex (`0x`) and binary (`0b`) integer literals: magnitude parse,
        // i64 range, separators, match-pattern identity with decimal.
        hex_literals,
        // The deterministic slice of the effectful stdlib. Everything is pinned
        // as a derived fact, never a clock reading or an env value.
        #[ignore = "needs the VM"]
        effects,
        // Subjects: a worker pool stopped and restarted; pins the native
        // park/resume + frame-slot contract (each local owns its slot).
        #[ignore = "needs the VM"]
        subject_pool_restart,
        // Subjects: rounds of spawned callers through the pool; pins the
        // native bridge-shim contract (the frame base survives a value-stack
        // growth mid-body: the shim returns the moved base with its result).
        #[ignore = "needs the VM"]
        subject_pool_rounds,
        // Subjects: send/receive ordering, park/wake, timeouts, owner death.
        // Cross-sender interleavings are asserted as aggregates only.
        #[ignore = "needs the VM"]
        messages,
        // Monitors: notices after and before the end, wrapping into the
        // receiver's type, demonitor, and request/reply against a dead server.
        #[ignore = "needs the VM"]
        monitors,
        // Ports: stdio round trip through cat, exit codes, env, the
        // terminate-on-close schedule, spawn failure, owner-death cleanup.
        #[ignore = "needs the VM"]
        ports,
        // Kill and links: Killed notices, cascades over links in both
        // directions stopping at an unlinked boundary, normal exits not
        // spreading, self-kill. (Crashes write to stderr: tests/vm_exits.rs.)
        #[ignore = "needs the VM"]
        exits,
        // Supervision: restart at a stable address, policies, one-for-all /
        // rest-for-one stop order and restart sets, Ask shutdown, nested
        // supervisors, keyed and unkeyed factories, introspection, and the
        // tree dying with the process that declared it. (Crashes and budget
        // exhaustion write to stderr: tests/vm_supervision.rs.)
        #[ignore = "needs the VM"]
        supervisors,
        // HTTP/1.1 surface. Locks the native scanners behind scarlet/http/h1 to the
        // sans-IO contract the Scarlet reference parser defined.
        #[ignore = "needs the VM"]
        http_parse,
        // The HTTP CLIENT: response-head parsing, response body framing, URL
        // parsing, and the whole request/response path driven over an
        // in-memory transport — the reach the `Io` shape was chosen for.
        #[ignore = "needs the VM"]
        http_response,
        // Backpassing: `x <- f(args)` desugars to a trailing callback.
        backpassing,
        // The pipe operator: `x |> f(args)` desugars to `f(x, args)`.
        pipe,
        // JSON: the SIMD parse, on-demand reads off the tape, typed decoding
        // with accumulated paths, and the three presence states — absent, null
        // and present — that a partial update turns on. Also pins the
        // adversarial answers: 1e400, a lone surrogate, invalid UTF-8, the
        // 64-bit boundary, duplicate keys and deep nesting.
        #[ignore = "needs the VM"]
        json,
        // scarlet/json/decode's own shape: a forty-member record written one
        // flat line per member, independent members accumulating their failures
        // past the old five-member ceiling, three-way presence under that
        // accumulation, one_of over 2 vs 2.0, and the two places where
        // accumulation is deliberately given up — `fail`, and `then`'s
        // dependent continuation.
        #[ignore = "needs the VM"]
        decoders,
        // base64, SHA-1, and the OS CSPRNG: the RFC 4648 and FIPS 180-1
        // vectors, the SHA-1 block/length padding edges, base64's rejection
        // of every non-canonical spelling, the RFC 6455 §1.3 handshake
        // example the two combine to produce, and the single-run CSPRNG
        // checks (length, two draws differ, not a uniform fill). Not a
        // quality test, and not the JIT — see native_backend.rs.
        crypto,
    ],

    // No golden, because there is no fixed output: `http_server` ends in an
    // unbounded accept loop, and `processes` prints from racing processes whose
    // order and fan-out depend on the machine. They must still type check.
    checks: [
        http_server,
        processes,
        // A supervised application (a chat service): the reference for the
        // shape of a `process.root` program. Serves for ever, like http_server.
        supervision,
        // A single-room chat: SSE responses served out of per-connection
        // mailboxes, tabs rejoining a restarted worker. Serves for ever.
        chat,
    ],

    // Perf infrastructure driven from outside this file (scripts/bench*.sh, and
    // `vm_exec::bench_typed_output_is_pinned`), plus the scratch pair.
    untested: [
        "bench_heavy.scrl",
        "bench_list_1x.scrl",
        "bench_list_2x.scrl",
        "bench_list_4x.scrl",
        "bench_map.scrl",
        "bench_service.scrl",
        "bench_typed.scrl",
        "a.scrl",
        "b.scrl",
    ],
}

// The timing-free "it still runs" check the bench scripts depend on.
#[test]
fn bench_runs() {
    run_file(&examples_dir().join("bench.scrl"), "bench");
}
