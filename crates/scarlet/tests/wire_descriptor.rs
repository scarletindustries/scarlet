//! These test the wire descriptor at the two seams a unit test can't reach:
//! a long-lived `Build`, which is what the editor checks with, and the REPL
//! as a subprocess. A refusal raised only when a program is emitted would be
//! invisible to an editor while still failing `scarlet run`, which is the
//! split these exist to prevent. Nothing here runs a wire op. The round trips
//! are in `wire_handles.rs` and `wire_closures.rs`.

use scarlet::build::{self, Build};
use scarlet::compile::{self, Compiled};

mod common;

/// Check `src` the way an editor does: in a build that outlives one check,
/// without running it.
fn check(b: &mut Build, src: &str) -> Compiled {
    let parsed = build::parse(src);
    assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
    let mut opts = compile::Options::program(None);
    opts.run = false;
    compile::compile(
        b,
        &mut scarlet::sources::DiskSources::new(),
        src,
        parsed,
        &opts,
    )
}

/// A record with a `fn` field. Closures can cross the wire, so it checks clean,
/// and it's the control that shows no refusal fires on one.
const HANDLER: &str = "import scarlet/wire\n\
                       type Handler {\n\
                       \tHandler(name String, run fn(Int) Int)\n\
                       }\n\
                       pub fn main() {\n\
                       \tprintln(wire::encode(Handler('h', fn(x) { x + 1 })))\n\
                       }\n";

/// A record over a user-declared bodiless type, which no VM table backs. A type
/// with no values is a node no value reaches, so it checks clean, and this is
/// the control that shows it. `send` is never called, because nothing can
/// construct a `Native`, and that's fine on a path that never runs.
const NATIVE: &str = "import scarlet/wire\n\
                      pub type Native\n\
                      type Handler {\n\
                      \tHandler(name String, raw Native)\n\
                      }\n\
                      fn send(h Handler) Binary {\n\
                      \twire::encode(h)\n\
                      }\n\
                      pub fn main() {\n\
                      \t_ = send\n\
                      }\n";

/// A generic function that encodes its parameter. This is the one refusal, and
/// it's about inference rather than a value, so every refusal test here goes
/// through this fixture.
const GENERIC: &str = "import scarlet/wire\n\
                       fn send(xs Array(a)) Binary {\n\
                       \twire::encode(xs)\n\
                       }\n\
                       pub fn main() {\n\
                       \t_ = send\n\
                       }\n";

/// A record whose field is a stdlib type this program never names or imports,
/// because `Port`'s stream is a `scarlet/net/socket.Connection`. On this path the
/// stdlib is the precompiled blob, so `Connection`'s declaration is answered
/// from the by-id registry the session seeds at construction, not from
/// anything `import scarlet/os/port` brought in.
const PORT: &str = "import scarlet/os/port\n\
                    import scarlet/wire\n\
                    fn send(p port::Port) Binary {\n\
                    \twire::encode(p)\n\
                    }\n\
                    pub fn main() {\n\
                    \t_ = send\n\
                    }\n";

const EVENT: &str = "import scarlet/wire\n\
                     type Event {\n\
                     \tSaid(who String)\n\
                     \tLeft(who String)\n\
                     }\n\
                     pub fn main() {\n\
                     \tprintln(wire::encode(Left('a')))\n\
                     }\n";

fn messages(r: &Compiled) -> String {
    r.diagnostics()
        .iter()
        .map(|d| d.message().clone())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The editor path checks without running. A refusal
/// has to arrive here or an editor shows a clean file that `al run` rejects.
#[test]
#[ignore = "waits for the wire redesign: check stopped refusing wire types in #50"]
fn a_refusal_is_reported_on_the_session_check_path() {
    let mut s = Build::default();
    let r = check(&mut s, GENERIC);
    assert!(!r.success(), "an unknown element type must be refused");
    assert!(
        messages(&r).contains("the type is still polymorphic here"),
        "got: {}",
        messages(&r)
    );
}

/// A field of a type no value has is a node no value reaches, and the record
/// around it checks clean.
#[test]
fn a_bodiless_field_checks_clean_on_the_session_path() {
    let mut s = Build::default();
    let r = check(&mut s, NATIVE);
    assert!(r.success(), "{:?}", r.diagnostics());
}

/// This is the other half, and it catches a refusal that fires on everything.
/// An encodable type must still check clean on the same path.
#[test]
fn an_encodable_type_still_checks_clean_on_the_session_path() {
    let mut s = Build::default();
    let r = check(&mut s, EVENT);
    assert!(r.success(), "{:?}", r.diagnostics());
}

/// A `fn` field is encodable too, so the same buffer checks clean on the same
/// path.
#[test]
fn a_fn_field_checks_clean_on_the_session_path() {
    let mut s = Build::default();
    let r = check(&mut s, HANDLER);
    assert!(r.success(), "{:?}", r.diagnostics());
}

/// A stdlib type reached only through a field of an imported type is
/// described from the seeded registry, and the record checks clean.
#[test]
fn a_stdlib_type_reached_only_through_a_field_checks_clean_on_the_session_path() {
    let mut s = Build::default();
    let r = check(&mut s, PORT);
    assert!(r.success(), "{:?}", r.diagnostics());
}

/// An editor checks the same buffer again and again in one build. The
/// descriptor is a constant of the program being built, so nothing carries
/// from one check to the next. We check that by running the cycle rather than
/// arguing it.
#[test]
#[ignore = "waits for the wire redesign: check stopped refusing wire types in #50"]
fn a_session_re_checks_a_wire_call_across_an_edit() {
    let mut s = Build::default();
    for _ in 0..2 {
        let bad = check(&mut s, GENERIC);
        assert!(!bad.success());
        let good = check(&mut s, HANDLER);
        assert!(good.success(), "{:?}", good.diagnostics());
    }
}

/// The REPL submits one line at a time to its own session. `wire.decode` with
/// nothing to fix its payload is the refusal a REPL user hits first, and it
/// must be a diagnostic there rather than an internal error.
#[test]
#[ignore = "needs the VM"]
fn the_repl_reports_an_unconstrained_decode() {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let bin = env!("CARGO_BIN_EXE_scarlet");
    let mut child = Command::new(bin)
        .arg("repl")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn al repl");
    {
        let mut stdin = child.stdin.take().expect("stdin");
        stdin
            .write_all(b"import scarlet/wire\nwire::decode(<<1>>)\n")
            .expect("write stdin");
    }
    let out = common::wait_or_kill(child, common::CHILD_TIMEOUT_SECS);
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        all.contains("the type `wire::decode` produces here is not known"),
        "got: {all}"
    );
}

/// A `Subject` three levels down a record. The handle is an identity node and
/// the whole record checks clean on the session path, where a descriptor built
/// during elaboration has to be reachable without emission.
const SUBJECT_DEEP: &str = "import scarlet/process\n\
                            import scarlet/wire\n\
                            pub type Inner {\n\
                            \tInner(reply process::Subject(String))\n\
                            }\n\
                            pub type Middle {\n\
                            \tMiddle(inner Inner)\n\
                            }\n\
                            pub type Outer {\n\
                            \tOuter(mid Middle)\n\
                            }\n\
                            pub fn main() {\n\
                            \tprintln(wire::encode(Outer(Middle(Inner(process::subject())))))\n\
                            }\n";

#[test]
fn a_handle_three_levels_down_checks_clean_on_the_session_path() {
    let mut s = Build::default();
    let r = check(&mut s, SUBJECT_DEEP);
    assert!(r.success(), "{:?}", r.diagnostics());
}

/// The refusal's PATH has to survive to the LSP too.
///
/// `a_refusal_is_reported_on_the_session_check_path` above pins that *a*
/// refusal arrives; this pins that the useful half arrives with it. An editor
/// showing "cannot encode" on a nine-position shape, with the chain dropped
/// somewhere between the builder and the diagnostic, is the failure that
/// would otherwise show up only when someone tried to use it. The type at
/// the bottom is the unknown one, which is the one refusal. The shape is positional
/// because a `Data` node's arguments are walked before its fields, so
/// `Outer(a)` refuses at the argument with no path.
#[test]
#[ignore = "waits for the wire redesign: check stopped refusing wire types in #50"]
fn the_refusal_path_survives_to_the_session_check_path() {
    let mut s = Build::default();
    let r = check(
        &mut s,
        "import scarlet/map\n\
         import scarlet/wire\n\
         fn send(o (Int, map::Map(String, Array(a)))) Binary {\n\
         \twire::encode(o)\n\
         }\n\
         pub fn main() {\n\
         \t_ = send\n\
         }\n",
    );
    assert!(
        !r.success(),
        "an unknown type three levels down must be refused"
    );
    assert!(
        messages(&r).contains("[1] -> [value] -> [element]"),
        "got: {}",
        messages(&r)
    );
}
