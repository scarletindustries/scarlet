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

/// Runs `<src_dir>/<name>.sca` and diffs its stdout against
/// `<golden_dir>/<name>.stdout`.
fn assert_golden_in(src_dir: &Path, golden_dir: &Path, name: &str) {
    let got = run_file(&src_dir.join(format!("{name}.sca")), name);
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
    let out = run_al("check", &src_dir.join(format!("{name}.sca")));
    if !out.success {
        panic!(
            "al check {name} exited {:?}\nstdout:\n{}\nstderr:\n{}",
            out.code, out.stdout, out.stderr
        );
    }
}

/// Checks that every `.sca` in a source dir is wired into the suite or listed
/// as untested, and that every golden belongs to a wired program. We skip
/// subdirectories, because `examples/lib/` holds modules that exist to be
/// imported and `golden/core_ir/` belongs to `core_ir.rs`.
fn assert_dir_wired(src_dir: &Path, golden_dir: &Path, wired: &[String], goldens: &[String]) {
    for entry in std::fs::read_dir(src_dir).expect("source dir") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|s| s.to_str()) != Some("sca") {
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

// Each name is both the `.sca` file's stem and the generated test's name, so it
// must be a valid Rust identifier and unique across all four lists.
//
//   examples  — examples/<n>.sca          run, diff against golden/<n>.stdout
//   programs  — tests/programs/<n>.sca    run, diff against golden/programs/<n>.stdout
//   checks    — examples/<n>.sca          `al check` only, output is not deterministic
//   untested  — examples/<n>.sca          deliberately outside the suite
//
// `suite_is_exhaustive` closes the loop. It fails on a new `.sca` in either
// source dir with no entry here, and on a golden left behind by a deleted program.
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

        /// Type-checks every program with a golden, even one whose run waits
        /// for the VM, so a front-end regression can't hide behind the ignore.
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
                examples.push(format!("{}.sca", stringify!($example)));
                example_goldens.push(format!("{}.stdout", stringify!($example)));
            )*
            $( examples.push(format!("{}.sca", stringify!($check))); )*
            $( examples.push($untested.to_string()); )*
            assert_dir_wired(&examples_dir(), &golden_dir(), &examples, &example_goldens);

            let mut programs: Vec<String> = Vec::new();
            let mut program_goldens: Vec<String> = Vec::new();
            $(
                programs.push(format!("{}.sca", stringify!($program)));
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
    // Tier A is examples/. These show off the language, one theme per file,
    // ordered to teach it top to bottom. scarlet_core's `idempotent_on_examples`
    // also checks that formatting them changes nothing. None may emit a warning,
    // because `run_file` asserts the child wrote nothing to stderr.
    examples: [
        // Language core.
        hello,
        control_flow,
        pattern_matching,
        data_types,
        generics,
        closures,
        // Keep the name tco.sca, because scarlet/internal.sca's `stack_depth`
        // doc points at it.
        tco,
        errors,
        // Stdlib surface.
        collections,
        strings,
        numbers,
        money,
        wire_format,
        // Effects. Both bind a loopback listener on port 0, serve it
        // in-process, then close it. Closing wakes the parked acceptors with
        // Ok(None) so the program exits. Both print facts *about* the port the
        // kernel picked, never the port itself. A sandbox that denies bind(2)
        // fails these, as it already fails `vm_io::tcp_connect_and_vectored_echo`.
        //
        // `http_client` does that twice, the second time against a hand-rolled
        // connection driver. That is the only test of scarlet/http/body's
        // socket-bound half, because the sans-IO http_parse.sca can't reach it.
        #[ignore = "needs the VM"]
        sockets,
        #[ignore = "needs the VM"]
        http_client,
        // This one spans several files. It imports examples/lib/units.sca and
        // lib/report/table.sca, which imports `../units` relative to its own
        // directory.
        modules,
        // The capstone: a lexer, parser and evaluator built only from what the
        // examples above teach. Read last.
        interpreter,
        // scripts/bench*.sh also drives these benchmarks. Their output is
        // deterministic, so they have goldens like any other example.
        bench,
        bench_list,
    ],

    // Tier B is crates/scarlet/tests/programs/. These are internal regression
    // programs, one subsystem per file. They pin compiler behaviour, so they
    // lean adversarial, and print PASS/FAIL where a bare value wouldn't say
    // what the right answer was.
    programs: [
        // Type system: HM inference, generalisation and monomorphisation.
        inference,
        generics_adversarial,
        // Pattern matching, equality and the shapes values come in.
        exhaustive_match,
        tuples_and_records,
        enum_equality,
        // Field punning on constructor calls. The parser desugars
        // `f(now:, self:)` to `f(now: now, self: self)`.
        field_punning,
        // Evaluation: tail calls in constant stack, closure capture and core
        // semantics.
        tco_and_closures,
        semantics,
        // Numeric edges: i64 wrapping, boxed ints, float canonicalisation and
        // exact decimals.
        numerics,
        // Bitwise edges: the sign bit, shift counts at and past the 64-bit
        // width, negative counts, and arithmetic (not logical) right shift.
        bitwise,
        // Hex (`0x`) and binary (`0b`) integer literals: how the magnitude
        // parses, the i64 range, separators, and matching the same patterns as
        // decimal.
        hex_literals,
        // The deterministic part of the effectful stdlib. It pins only facts
        // derived from readings, never a clock reading or an env value itself.
        #[ignore = "needs the VM"]
        effects,
        // Subjects: a worker pool stopped and restarted. This pins the native
        // park/resume and frame-slot contract, where each local owns its slot.
        #[ignore = "needs the VM"]
        subject_pool_restart,
        // Subjects: rounds of spawned callers through the pool. This pins the
        // native bridge-shim contract, where the frame base survives the value
        // stack growing mid-body because the shim returns the moved base with
        // its result.
        #[ignore = "needs the VM"]
        subject_pool_rounds,
        // Subjects: send/receive ordering, park/wake, timeouts and owner death.
        // We only assert totals over interleavings across senders.
        #[ignore = "needs the VM"]
        messages,
        // Monitors: notices after and before the end, wrapping into the
        // receiver's type, demonitor and request/reply against a dead server.
        #[ignore = "needs the VM"]
        monitors,
        // Ports: a stdio round trip through cat, exit codes, env, the
        // terminate-on-close schedule, spawn failure and cleanup on owner death.
        #[ignore = "needs the VM"]
        ports,
        // Kill and links: Killed notices, cascades over links in both
        // directions that stop at an unlinked boundary, normal exits that don't
        // spread, and self-kill. Crashes write to stderr, so tests/vm_exits.rs
        // covers them.
        #[ignore = "needs the VM"]
        exits,
        // Supervision: restart at a stable address, policies, one-for-all and
        // rest-for-one stop order and restart sets, Ask shutdown, nested
        // supervisors, keyed and unkeyed factories, introspection, and the tree
        // dying with the process that declared it. Crashes and an exhausted
        // budget write to stderr, so tests/vm_supervision.rs covers them.
        #[ignore = "needs the VM"]
        supervisors,
        // HTTP/1.1. This holds the native scanners behind scarlet/http/h1 to the
        // sans-IO contract the Scarlet reference parser defined.
        http_parse,
        // The HTTP client: response-head parsing, response body framing, URL
        // parsing, and the whole request/response path driven over an
        // in-memory transport. Reaching that path is why `Io` has the shape it has.
        #[ignore = "needs the VM"]
        http_response,
        // Backpassing: `x <- f(args)` desugars to a trailing callback.
        backpassing,
        // The pipe operator: `x |> f(args)` desugars to `f(x, args)`.
        pipe,
        // JSON: the SIMD parse, on-demand reads off the tape, typed decoding
        // with accumulated paths, and the three presence states (absent, null
        // and present) that a partial update turns on. It also pins the answers
        // to 1e400, a lone surrogate, invalid UTF-8, the 64-bit boundary,
        // duplicate keys and deep nesting.
        json,
        // scarlet/json/decode itself: a forty-member record written one flat
        // line per member, independent members collecting their failures past
        // the old five-member ceiling, three-way presence while collecting,
        // one_of over 2 vs 2.0, and the two places that deliberately stop
        // collecting, which are `fail` and `then`'s dependent continuation.
        decoders,
        // base64, SHA-1 and the OS CSPRNG: the RFC 4648 and FIPS 180-1
        // vectors, the SHA-1 block and length padding edges, base64 rejecting
        // every non-canonical spelling, the RFC 6455 §1.3 handshake example the
        // two combine to produce, and the CSPRNG checks one run can make (the
        // length, two draws differ, not a uniform fill). It doesn't test
        // randomness quality, or the JIT, which native_backend.rs covers.
        crypto,
    ],

    // These have no golden because their output isn't fixed. `http_server`
    // ends in an endless accept loop, and `processes` prints from racing
    // processes whose order and fan-out depend on the machine. They must still
    // type check.
    checks: [
        http_server,
        processes,
        // A supervised chat service. It shows how a `process.root` program
        // should look, and serves for ever, like http_server.
        supervision,
        // A single-room chat. It serves SSE responses out of per-connection
        // mailboxes, and tabs rejoin a restarted worker. It serves for ever.
        chat,
        // The Game of Life, drawn live over the whole terminal from a random
        // seed. It runs for ever.
        life,
    ],

    // Perf programs that scripts/bench*.sh and
    // `vm_exec::bench_typed_output_is_pinned` drive, plus the scratch pair.
    untested: [
        "bench_heavy.sca",
        "bench_list_1x.sca",
        "bench_list_2x.sca",
        "bench_list_4x.sca",
        "bench_map.sca",
        "bench_service.sca",
        "bench_typed.sca",
        "a.sca",
        "b.sca",
    ],
}

/// People read these to learn how Scarlet is written, so they stay exactly as
/// `scarlet fmt` writes them, as the stdlib does.
#[test]
fn every_example_and_program_is_formatted() {
    let mut unformatted: Vec<String> = Vec::new();
    for dir in [examples_dir(), programs_dir()] {
        let mut entries: Vec<PathBuf> = walk(&dir);
        entries.sort();
        for path in entries {
            let src = std::fs::read_to_string(&path).expect("a source file");
            let same = matches!(
                scarlet::formatter::format(&src),
                scarlet::formatter::FormatResult::Formatted { output } if output == src
            );
            if !same {
                unformatted.push(path.display().to_string());
            }
        }
    }
    assert!(
        unformatted.is_empty(),
        "not formatted; run `scarlet fmt examples crates/scarlet/tests/programs`: {unformatted:?}"
    );
}

/// Every `.sca` under `dir`, at any depth.
fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(walk(&path));
        } else if path.extension().is_some_and(|e| e == "sca") {
            out.push(path);
        }
    }
    out
}

// The bench scripts depend on this check that bench.sca still runs. It
// measures no timings.
#[test]
fn bench_runs() {
    run_file(&examples_dir().join("bench.sca"), "bench");
}
