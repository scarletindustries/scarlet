//! The wire descriptor at the two seams a unit test cannot stand over: the
//! LSP's `IncrementalSession`, and the REPL as a subprocess.
//!
//! Both matter because the descriptor is built during *elaboration*. `check`
//! truncates the pipeline before emission, so a refusal raised at emission
//! would be invisible here and in an editor while still failing `al run` —
//! the exact split this ticket exists to prevent. Nothing here runs a wire
//! op; the round trips are `wire_handles.rs` and `wire_closures.rs`.

use scarlet::bytecode::IncrementalSession;

mod common;
use common::parse;

/// A record with a `fn` field: closures cross, so it checks clean, and it is
/// the control that says no refusal fires on one.
const HANDLER: &str = "import scarlet/wire\n\
                       type Handler {\n\
                       \tHandler(name String, run fn(Int) Int)\n\
                       }\n\
                       pub fn main() {\n\
                       \tprintln(wire.encode(Handler('h', fn(x) { x + 1 })))\n\
                       }\n";

/// A record over a user-declared bodiless type, which no VM table backs: a
/// type no value has is a node no value reaches, so it checks clean, and it
/// is the control that says so. `send` is never called — nothing can
/// construct a `Native` — which is fine on a path that never runs.
const NATIVE: &str = "import scarlet/wire\n\
                      pub type Native\n\
                      type Handler {\n\
                      \tHandler(name String, raw Native)\n\
                      }\n\
                      fn send(h Handler) Binary {\n\
                      \twire.encode(h)\n\
                      }\n\
                      pub fn main() {\n\
                      \t_ = send\n\
                      }\n";

/// A generic function encoding its parameter: the one refusal, about
/// inference rather than a value, and so the fixture every refusal test here
/// is reached through.
const GENERIC: &str = "import scarlet/wire\n\
                       fn send(xs Array(a)) Binary {\n\
                       \twire.encode(xs)\n\
                       }\n\
                       pub fn main() {\n\
                       \t_ = send\n\
                       }\n";

/// A record whose field is a stdlib type this program never names or imports:
/// `Port`'s stream is a `scarlet/net/socket.Connection`. On this path the
/// stdlib is the precompiled blob, so `Connection`'s declaration is answered
/// from the by-id registry the session seeds at construction, not from
/// anything `import scarlet/os/port` brought in.
const PORT: &str = "import scarlet/os/port\n\
                    import scarlet/wire\n\
                    fn send(p port.Port) Binary {\n\
                    \twire.encode(p)\n\
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
                     \tprintln(wire.encode(Left('a')))\n\
                     }\n";

fn messages(r: &scarlet::bytecode::CompileResult) -> String {
    r.diagnostics
        .iter()
        .map(|d| d.message.clone())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The LSP path is `IncrementalSession::check`, which never emits. A refusal
/// has to arrive here or an editor shows a clean file that `al run` rejects.
#[test]
fn a_refusal_is_reported_on_the_session_check_path() {
    let mut s = IncrementalSession::new();
    let r = s.check(&parse(GENERIC), None);
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
    let mut s = IncrementalSession::new();
    let r = s.check(&parse(NATIVE), None);
    assert!(r.success(), "{:?}", r.diagnostics);
}

/// The other half, and the one that catches a refusal that fires on
/// everything: an encodable type must still check clean on the same path.
#[test]
fn an_encodable_type_still_checks_clean_on_the_session_path() {
    let mut s = IncrementalSession::new();
    let r = s.check(&parse(EVENT), None);
    assert!(r.success(), "{:?}", r.diagnostics);
}

/// A `fn` field is one of those: the same buffer checks clean on the same
/// path.
#[test]
fn a_fn_field_checks_clean_on_the_session_path() {
    let mut s = IncrementalSession::new();
    let r = s.check(&parse(HANDLER), None);
    assert!(r.success(), "{:?}", r.diagnostics);
}

/// A stdlib type reached only through a field of an imported type is
/// described from the seeded registry, and the record checks clean.
#[test]
fn a_stdlib_type_reached_only_through_a_field_checks_clean_on_the_session_path() {
    let mut s = IncrementalSession::new();
    let r = s.check(&parse(PORT), None);
    assert!(r.success(), "{:?}", r.diagnostics);
}

/// A session compiles the same buffer repeatedly, rewinding between edits.
/// The descriptor is a constant of the program being built, so nothing has to
/// be carried across a rewind — asserted by running the cycle rather than by
/// arguing it: the failure this would catch is descriptors accumulating from
/// a rewound compile and being minted against a program that no longer has
/// the call site.
#[test]
fn a_session_re_checks_a_wire_call_across_an_edit() {
    let mut s = IncrementalSession::new();
    for _ in 0..2 {
        let bad = s.check(&parse(GENERIC), None);
        assert!(!bad.success());
        let good = s.check(&parse(HANDLER), None);
        assert!(good.success(), "{:?}", good.diagnostics);
    }
}

/// The REPL submits one line at a time to its own session. `wire.decode` with
/// nothing to fix its payload is the refusal a REPL user hits first, and it
/// must be a diagnostic there rather than an internal error.
#[test]
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
            .write_all(b"import scarlet/wire\nwire.decode(<<1>>)\n")
            .expect("write stdin");
    }
    let out = common::wait_or_kill(child, common::CHILD_TIMEOUT_SECS);
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        all.contains("the type `wire.decode` produces here is not known"),
        "got: {all}"
    );
}

/// A `Subject` three levels down a record: the handle is an identity node
/// and the whole record checks clean — on the session path, where a
/// descriptor built during elaboration has to be reachable without emission.
const SUBJECT_DEEP: &str = "import scarlet/process\n\
                            import scarlet/wire\n\
                            pub type Inner {\n\
                            \tInner(reply process.Subject(String))\n\
                            }\n\
                            pub type Middle {\n\
                            \tMiddle(inner Inner)\n\
                            }\n\
                            pub type Outer {\n\
                            \tOuter(mid Middle)\n\
                            }\n\
                            pub fn main() {\n\
                            \tprintln(wire.encode(Outer(Middle(Inner(process.subject())))))\n\
                            }\n";

#[test]
fn a_handle_three_levels_down_checks_clean_on_the_session_path() {
    let mut s = IncrementalSession::new();
    let r = s.check(&parse(SUBJECT_DEEP), None);
    assert!(r.success(), "{:?}", r.diagnostics);
}

/// The refusal's PATH has to survive to the LSP too.
///
/// `a_refusal_is_reported_on_the_session_check_path` above pins that *a*
/// refusal arrives; this pins that the useful half arrives with it. An editor
/// showing "cannot encode" on a nine-position shape, with the chain dropped
/// somewhere between the builder and the diagnostic, is the failure that
/// would otherwise show up only when someone tried to use it. The type at
/// the bottom is the unknown one, the one refusal. The shape is positional
/// because a `Data` node's arguments are walked before its fields, so
/// `Outer(a)` refuses at the argument with no path.
#[test]
fn the_refusal_path_survives_to_the_session_check_path() {
    let mut s = IncrementalSession::new();
    let r = s.check(
        &parse(
            "import scarlet/wire\n\
             fn send(o (Int, Map(String, Array(a)))) Binary {\n\
             \twire.encode(o)\n\
             }\n\
             pub fn main() {\n\
             \t_ = send\n\
             }\n",
        ),
        None,
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
