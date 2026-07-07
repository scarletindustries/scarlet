use al::reference::EntityKind;

mod common;
use common::{Project, SessionQueryExt, checked_with, cursor, run_al, run_outputs};

const UTIL_SRC: &str =
    "pub fn quote(s String) String { '\"' + s + '\"' }\npub fn empty() String { '' }\n";

/// `al run` the project file `entry` and assert it succeeds with exactly
/// `expected` on stdout.
fn run_project_outputs(proj: &Project, entry: &str, expected: &str) {
    let r = run_al("run", &proj.dir.join(entry));
    assert!(r.success, "stdout: {}\nstderr: {}", r.stdout, r.stderr);
    assert_eq!(r.stdout, expected, "stderr: {}", r.stderr);
}

/// `al <cmd>` the project file `entry` and assert it fails with a diagnostic
/// containing at least one of `msgs`.
fn project_rejects(proj: &Project, cmd: &str, entry: &str, msgs: &[&str]) {
    let r = run_al(cmd, &proj.dir.join(entry));
    assert!(
        !r.success,
        "expected `al {cmd}` to reject {entry}\nstdout: {}\nstderr: {}",
        r.stdout, r.stderr
    );
    let combined = r.combined();
    assert!(
        msgs.iter().any(|m| combined.contains(m)),
        "expected one of {msgs:?}, got: {combined}"
    );
}

#[test]
fn relative_qualified() {
    let proj = Project::new("rel_qual");
    proj.write("util.al", UTIL_SRC);
    proj.write("main.al", "import ./util\nprintln(util.quote('hi'))\n");
    run_project_outputs(&proj, "main.al", "\"hi\"\n");
}

#[test]
fn relative_selective_and_alias() {
    let proj = Project::new("rel_sel");
    proj.write("util.al", UTIL_SRC);
    proj.write(
        "main.al",
        "import ./util as u\nimport ./util.{quote as q, empty}\nprintln(u.empty())\nprintln(q('x'))\nprintln(empty())\n",
    );
    run_project_outputs(&proj, "main.al", "\n\"x\"\n\n");
}

#[test]
fn aliased_type_import_unifies_with_canonical() {
    // Regression: `import mod.{T as X}` must hydrate an annotation of `X` to the
    // type's *canonical* nominal name, not the local alias. A value of the type
    // carries the canonical name, so an alias-named annotation would never unify
    // with it and a valid program would be wrongly rejected with a spurious
    // "Type mismatch: expected 'X', got 'T'".
    let proj = Project::new("alias_type");
    proj.write("lib.al", "pub type Color {\n\tRed\n\tGreen\n\tBlue\n}\n");
    proj.write(
        "main.al",
        "import ./lib.{Color as C, Red}\nfn id(c C) C { c }\nprintln(id(Red))\n",
    );
    run_project_outputs(&proj, "main.al", "Red\n");
}

#[test]
fn relative_import() {
    let proj = Project::new("rel_imp");
    proj.write("helper.al", "pub fn greet() String { 'hello' }\n");
    proj.write("main.al", "import ./helper\nprintln(helper.greet())\n");
    run_project_outputs(&proj, "main.al", "hello\n");
}

#[test]
fn private_is_not_importable() {
    let proj = Project::new("priv");
    proj.write("helper.al", "fn secret() { 'x' }\npub fn ok() { 'y' }\n");

    proj.write(
        "main_sel.al",
        "import ./helper.{secret}\nprintln(secret())\n",
    );
    project_rejects(&proj, "run", "main_sel.al", &["private"]);

    proj.write(
        "main_qual.al",
        "import ./helper\nprintln(helper.secret())\n",
    );
    project_rejects(&proj, "run", "main_qual.al", &["private"]);
}

#[test]
fn opaque_type_hides_constructors() {
    let proj = Project::new("opaque");
    proj.write(
        "id.al",
        "pub opaque type Id { Id(n Int) }\n\
         pub fn make(n Int) Id { Id(n) }\n\
         pub fn get(i Id) Int { match i { Id(n) -> n } }\n",
    );

    // The type and smart-constructor functions are importable.
    proj.write(
        "ok.al",
        "import ./id.{Id, make, get}\n\
         fn use(i Id) Int { get(i) }\n\
         println(use(make(42)))\n",
    );
    run_project_outputs(&proj, "ok.al", "42\n");

    // The constructor is not importable by name.
    proj.write("bad_sel.al", "import ./id.{Id}\nx = Id(1)\n");
    project_rejects(&proj, "check", "bad_sel.al", &["private", "opaque"]);

    // The constructor is not reachable via module qualifier.
    proj.write("bad_qual.al", "import ./id\nx = id.Id(1)\n");
    project_rejects(&proj, "check", "bad_qual.al", &["private", "opaque"]);
}

#[test]
fn external_type_allowed_in_user_code() {
    let proj = Project::new("ext");
    proj.write("handle.al", "pub type Handle\n");
    proj.write(
        "main.al",
        "import ./handle.{Handle}\nfn id(h Handle) Handle { h }\n",
    );
    let r = run_al("check", &proj.dir.join("main.al"));
    assert!(r.success, "out={} err={}", r.stdout, r.stderr);
}

#[test]
fn unknown_module() {
    let proj = Project::new("unknown_mod");
    proj.write("main.al", "import al/nope\n");
    project_rejects(&proj, "run", "main.al", &["not found", "Unknown module"]);
}

#[test]
fn stdlib_net_socket_type() {
    run_outputs(
        "import al/net/socket.{Socket}\nfn id(s Socket) Socket { s }\nprintln('ok')\n",
        "ok\n",
    );
}

#[test]
fn cycle_detection() {
    let proj = Project::new("cycle");
    proj.write("a.al", "import ./b\npub fn fa() { 1 }\n");
    proj.write("b.al", "import ./a\npub fn fb() { 2 }\n");
    // Case-insensitive on purpose: accepts "cycle"/"Cycle"/"Circular import" etc.
    let r = run_al("run", &proj.dir.join("a.al"));
    assert!(!r.success);
    let combined = r.combined();
    assert!(
        combined.to_lowercase().contains("cycle") || combined.to_lowercase().contains("circular"),
        "got: {combined}"
    );
}

#[test]
fn query_api_cross_module_goto_def_and_symbols() {
    let proj = Project::new("qapi_xmod");
    proj.write("util.al", UTIL_SRC);
    let entry = "import ./util\nprintln(util.quote('hi'))\n";
    proj.write("main.al", entry);
    let s = checked_with(&proj, entry);

    // `util.quote(..)` in the entry resolves to its declaration in util.al.
    let (l, c) = cursor(entry, "quote", 1, 0);
    let (m, span) = s
        .definition("main", l, c)
        .expect("util.quote resolves cross-module");
    assert_eq!(m.last().map(String::as_str), Some("util"));
    assert!(span.end_column > span.start_column, "real decl span");

    // documentSymbol over util.al lists both exported functions.
    let mut names: Vec<String> = s
        .document_symbols("./util")
        .into_iter()
        .filter(|x| x.kind == EntityKind::Function)
        .map(|x| x.name)
        .collect();
    names.sort();
    assert_eq!(names, vec!["empty".to_string(), "quote".to_string()]);

    // workspace/symbol finds it and rename mirrors the reverse-edge closure.
    assert!(
        s.workspace_symbols("quote")
            .iter()
            .any(|x| x.name == "quote")
    );
    let (defid, _) = s.prepare_rename("main", l, c).expect("quote DefId");
    assert_eq!(s.rename(defid), s.references(defid));
    assert!(
        s.references(defid)
            .iter()
            .any(|(mp, _)| mp.join("/") == "main"),
        "the entry's use of util.quote must be in the rename set"
    );
}

#[test]
fn query_api_alias_and_selective_imports_resolve() {
    let proj = Project::new("qapi_alias");
    proj.write("util.al", UTIL_SRC);
    let entry =
        "import ./util as u\nimport ./util.{quote as q}\nprintln(u.empty())\nprintln(q('x'))\n";
    proj.write("main.al", entry);
    let s = checked_with(&proj, entry);

    // Aliased qualified use `u.empty()` resolves into util.al.
    let (l, c) = cursor(entry, "empty", 1, 0);
    let (m, _) = s.definition("main", l, c).expect("u.empty resolves");
    assert_eq!(m.last().map(String::as_str), Some("util"));

    // The selective-import binder `q` (use site `q('x')`) resolves to the
    // same `quote` declaration the qualified path would.
    let (lq, cq) = cursor(entry, "q('x')", 1, 0);
    let (mq, sq) = s.definition("main", lq, cq).expect("q resolves to quote");
    assert_eq!(mq.last().map(String::as_str), Some("util"));
    let quote_decl = cursor(UTIL_SRC, "quote", 1, 0);
    assert_eq!(
        (sq.start_line, sq.start_column),
        quote_decl,
        "selective-import use must point at quote's real declaration"
    );

    // The `as u` module alias is a ModuleAlias definition in the entry.
    assert!(
        s.document_symbols("main")
            .iter()
            .any(|x| x.name == "u" && x.kind == EntityKind::ModuleAlias),
        "module alias `u` not tracked"
    );
}

// An imported module's top level may contain *only* declarations. This mirrors
// `relative_qualified` exactly except the dependency carries a bare top-level
// side-effecting statement (`println(99)`), so the failure isolates the
// imported-module rule — the entry module's own top-level `println(lib.ok())`
// is fine, only the import is rejected.
#[test]
fn module_top_level_executable_code_is_error() {
    let proj = Project::new("mod_toplevel_exec");
    proj.write("lib.al", "pub fn ok() Int { 1 }\nprintln(99)\n");
    proj.write("main.al", "import ./lib\nprintln(lib.ok())\n");
    project_rejects(
        &proj,
        "run",
        "main.al",
        &["Modules may only contain declarations at the top level"],
    );
}

// A selective import naming a member the module does not export is rejected by
// the import-resolution path, naming the module key and the missing member.
#[test]
fn selective_import_unknown_member_is_error() {
    let proj = Project::new("mod_sel_unknown");
    proj.write("lib.al", "pub fn ok() Int { 1 }\n");
    proj.write("main.al", "import ./lib.{nope}\nprintln(99)\n");
    project_rejects(
        &proj,
        "run",
        "main.al",
        &["Module './lib' has no member 'nope'"],
    );
}

// A qualified `module.member` access naming an unexported member is rejected by
// the qualified-lookup path — a distinct compiler site from the selective
// import above — with the same module-key + member message.
#[test]
fn qualified_import_unknown_member_is_error() {
    let proj = Project::new("mod_qual_unknown");
    proj.write("lib.al", "pub fn ok() Int { 1 }\n");
    proj.write("main.al", "import ./lib\nlib.nope()\n");
    project_rejects(
        &proj,
        "run",
        "main.al",
        &["Module './lib' has no member 'nope'"],
    );
}
