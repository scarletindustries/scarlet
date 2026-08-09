use al::reference::EntityKind;

mod common;
use common::{
    Project, SessionQueryExt, checked_with, cursor, project_rejects, run_al, run_project_outputs,
};

const UTIL_SRC: &str =
    "pub fn quote(s String) String { '\"' + s + '\"' }\npub fn empty() String { '' }\n";

#[test]
fn relative_qualified() {
    let proj = Project::new("rel_qual");
    proj.write("util.al", UTIL_SRC);
    proj.write("main.al", "import ./util\nprintln(util.quote('hi'))\n");
    run_project_outputs(&proj, "run", "main.al", "\"hi\"\n");
}

#[test]
fn relative_selective_and_alias() {
    let proj = Project::new("rel_sel");
    proj.write("util.al", UTIL_SRC);
    proj.write(
        "main.al",
        "import ./util as u\nimport ./util.{quote as q, empty}\nprintln(u.empty())\nprintln(q('x'))\nprintln(empty())\n",
    );
    run_project_outputs(&proj, "run", "main.al", "\n\"x\"\n\n");
}

#[test]
fn aliased_type_import_unifies_with_canonical() {
    // `import mod.{T as X}` must hydrate an annotation of `X` to the type's
    // canonical nominal name. Values carry the canonical name, so an
    // alias-named annotation would never unify with one.
    let proj = Project::new("alias_type");
    proj.write("lib.al", "pub type Color {\n\tRed\n\tGreen\n\tBlue\n}\n");
    proj.write(
        "main.al",
        "import ./lib.{Color as C, Red}\nfn id(c C) C { c }\nprintln(id(Red))\n",
    );
    run_project_outputs(&proj, "run", "main.al", "Red\n");
}

#[test]
fn relative_import() {
    let proj = Project::new("rel_imp");
    proj.write("helper.al", "pub fn greet() String { 'hello' }\n");
    proj.write("main.al", "import ./helper\nprintln(helper.greet())\n");
    run_project_outputs(&proj, "run", "main.al", "hello\n");
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

    proj.write(
        "ok.al",
        "import ./id.{Id, make, get}\n\
         fn use(i Id) Int { get(i) }\n\
         println(use(make(42)))\n",
    );
    run_project_outputs(&proj, "run", "ok.al", "42\n");

    proj.write("bad_sel.al", "import ./id.{Id}\nx = Id(1)\n");
    project_rejects(&proj, "check", "bad_sel.al", &["private", "opaque"]);

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
    project_rejects(&proj, "run", "main.al", &["no such stdlib module al/nope"]);
}

run_case! {
    stdlib_net_socket_type: (
        "import al/net/socket.{Socket}\nfn id(s Socket) Socket { s }\nprintln('ok')\n",
        "ok\n",
    ),
}

#[test]
fn cycle_detection() {
    let proj = Project::new("cycle");
    proj.write("a.al", "import ./b\npub fn fa() { 1 }\n");
    proj.write("b.al", "import ./a\npub fn fb() { 2 }\n");
    project_rejects(&proj, "run", "a.al", &["cycle", "circular"]);
}

#[test]
fn query_api_cross_module_goto_def_and_symbols() {
    let proj = Project::new("qapi_xmod");
    proj.write("util.al", UTIL_SRC);
    let entry = "import ./util\nprintln(util.quote('hi'))\n";
    proj.write("main.al", entry);
    let s = checked_with(&proj, entry);

    let (l, c) = cursor(entry, "quote", 1, 0);
    let (m, span) = s
        .definition("main", l, c)
        .expect("util.quote resolves cross-module");
    assert_eq!(m.last().map(String::as_str), Some("util"));
    assert!(span.end_column > span.start_column, "real decl span");

    let util_path = proj.dir.join("util.al");
    let mut names: Vec<String> = s
        .document_symbols(&util_path.to_string_lossy())
        .into_iter()
        .filter(|x| x.kind == EntityKind::Function)
        .map(|x| x.name)
        .collect();
    names.sort();
    assert_eq!(names, vec!["empty".to_string(), "quote".to_string()]);

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

    let (l, c) = cursor(entry, "empty", 1, 0);
    let (m, _) = s.definition("main", l, c).expect("u.empty resolves");
    assert_eq!(m.last().map(String::as_str), Some("util"));

    // The selective-import binder `q` resolves to the same `quote`
    // declaration the qualified path would.
    let (lq, cq) = cursor(entry, "q('x')", 1, 0);
    let (mq, sq) = s.definition("main", lq, cq).expect("q resolves to quote");
    assert_eq!(mq.last().map(String::as_str), Some("util"));
    let quote_decl = cursor(UTIL_SRC, "quote", 1, 0);
    assert_eq!(
        (sq.start_line, sq.start_column),
        quote_decl,
        "selective-import use must point at quote's real declaration"
    );

    assert!(
        s.document_symbols("main")
            .iter()
            .any(|x| x.name == "u" && x.kind == EntityKind::ModuleAlias),
        "module alias `u` not tracked"
    );
}

// An imported module's top level may contain only declarations. The entry's
// own top-level `println` is fine; only the import is rejected.
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

// Rejected by the import-resolution path.
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

// Rejected by the qualified-lookup path, a distinct compiler site from the
// selective import above, with the same message.
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

/// A lambda's body is walked while the import qualifier is in scope but
/// elaborated after a same-named top-level `let` entered the value env. An
/// elaborator that re-probed the live env would read `util.empty()` as a field
/// access, enter an expression the walk never entered, and abort.
#[test]
fn lambda_body_keeps_the_walks_qualifier_verdict() {
    let proj = Project::new("qual_pinned");
    proj.write("util.al", "pub fn empty() String { 'E' }\n");
    proj.write(
        "main.al",
        "import ./util\nf = fn() { util.empty() }\nutil = 5\nprintln(f())\nprintln(util)\n",
    );
    run_project_outputs(&proj, "run", "main.al", "E\n5\n");
}

/// The other side of the same coin: the shadowing `let` binds a value with a
/// field of the member's name, so after it `one.go` really is a field read while
/// inside the earlier-walked lambda it is still module `one`'s `go`.
#[test]
fn shadowed_qualifier_is_a_field_read_only_after_the_bind() {
    let proj = Project::new("qual_shadow");
    proj.write("one.al", "pub const go = 7\n");
    proj.write(
        "main.al",
        "import ./one\ntype Box {\n\tgo Int\n}\ng = fn() { one.go }\none = Box(9)\nprintln(g())\nprintln(one.go)\n",
    );
    run_project_outputs(&proj, "run", "main.al", "7\n9\n");
}

// A module is the file it resolved to, never the import as written. Keying the
// module cache on the spelling made the first `b.al` loaded win program-wide,
// and nothing failed loudly — the wrong module was simply used.

/// `sub/mid.al` imports `./b`, which must be `sub/b.al`, not the root's.
#[test]
fn same_named_modules_in_different_directories_are_distinct() {
    let proj = Project::new("mod_identity");
    std::fs::create_dir_all(proj.dir.join("sub")).unwrap();
    proj.write("b.al", "pub fn who() String {\n\t'ROOT'\n}\n");
    proj.write("sub/b.al", "pub fn who() String {\n\t'SUB'\n}\n");
    proj.write(
        "sub/mid.al",
        "import ./b\n\npub fn go() String {\n\tb.who()\n}\n",
    );
    proj.write(
        "main.al",
        "import ./b\nimport ./sub/mid\n\nprintln(b.who())\nprintln(mid.go())\n",
    );
    run_project_outputs(&proj, "run", "main.al", "ROOT\nSUB\n");
}

/// The same file reached by two different spellings (`./b` from the root and
/// `../b` from `sub/`) is ONE module: it must compile once and share state.
#[test]
fn one_file_reached_two_ways_is_one_module() {
    let proj = Project::new("mod_identity_alias");
    std::fs::create_dir_all(proj.dir.join("sub")).unwrap();
    proj.write("b.al", "pub fn who() String {\n\t'ROOT'\n}\n");
    proj.write(
        "sub/mid.al",
        "import ../b\n\npub fn go() String {\n\tb.who()\n}\n",
    );
    proj.write(
        "main.al",
        "import ./b\nimport ./sub/mid\n\nprintln(b.who())\nprintln(mid.go())\n",
    );
    run_project_outputs(&proj, "run", "main.al", "ROOT\nROOT\n");
}

/// A missing relative import must not be satisfied by a same-named file in
/// another directory. This is the shape the LSP silently accepted.
#[test]
fn a_module_in_another_directory_does_not_satisfy_a_relative_import() {
    let proj = Project::new("mod_identity_missing");
    std::fs::create_dir_all(proj.dir.join("sub")).unwrap();
    proj.write("sub/b.al", "pub fn who() String {\n\t'SUB'\n}\n");
    // `./b` from the root: `sub/b.al` must NOT satisfy it.
    proj.write("main.al", "import ./b\n\nprintln(b.who())\n");
    project_rejects(&proj, "check", "main.al", &["file not found"]);
}

/// A type in `sub/b.al` and one in `b.al` are different types even though both
/// modules are spelled `./b`. A shared cache entry would unify them.
#[test]
fn same_named_modules_do_not_share_types() {
    let proj = Project::new("mod_identity_types");
    std::fs::create_dir_all(proj.dir.join("sub")).unwrap();
    proj.write(
        "b.al",
        "pub type T {\n\tT(n Int)\n}\n\npub fn make() T {\n\tT(1)\n}\n\npub fn get(t T) Int {\n\tt.n\n}\n",
    );
    proj.write(
        "sub/b.al",
        "pub type T {\n\tT(s String)\n}\n\npub fn make() T {\n\tT('x')\n}\n\npub fn get(t T) String {\n\tt.s\n}\n",
    );
    proj.write(
        "sub/mid.al",
        "import ./b\n\npub fn go() String {\n\tb.get(b.make())\n}\n",
    );
    proj.write(
        "main.al",
        "import ./b\nimport ./sub/mid\n\nprintln(b.get(b.make()))\nprintln(mid.go())\n",
    );
    run_project_outputs(&proj, "run", "main.al", "1\nx\n");
}

// `match e { io.NotFound(path) -> … }` reaches a constructor through its own
// module, so a program need not import the name to match on it and two modules
// exporting a `NotFound` cannot collide.

const COLOR_SRC: &str = "pub type Color {\n\tRed\n\tGreen(shade Int)\n}\n";

#[test]
fn a_qualified_constructor_pattern_matches() {
    let proj = Project::new("qual_pat");
    proj.write("color.al", COLOR_SRC);
    proj.write(
        "main.al",
        "import ./color\n\nfn v(c Color) Int {\n\tmatch c {\n\t\tcolor.Red -> 0\n\t\tcolor.Green(s) -> s\n\t}\n}\nprintln(v(color.Green(3)))\nprintln(v(color.Red))\n",
    );
    run_project_outputs(&proj, "run", "main.al", "3\n0\n");
}

/// The imported name and the qualified spelling denote the same constructor,
/// so exhaustiveness counts them together.
#[test]
fn qualified_and_imported_constructors_are_the_same_constructor() {
    let proj = Project::new("qual_pat_mixed");
    proj.write("color.al", COLOR_SRC);
    proj.write(
        "main.al",
        "import ./color.{Red}\nimport ./color\n\nfn v(c Color) Int {\n\tmatch c {\n\t\tRed -> 0\n\t\tcolor.Green(s) -> s\n\t}\n}\nprintln(v(color.Green(9)))\n",
    );
    run_project_outputs(&proj, "run", "main.al", "9\n");
}

/// Exhaustiveness resolves a constructor against the scrutinee's own variants,
/// so a qualified arm counts as covering.
#[test]
fn a_qualified_pattern_is_seen_by_exhaustiveness() {
    let proj = Project::new("qual_pat_exh");
    proj.write("color.al", COLOR_SRC);
    proj.write(
        "main.al",
        "import ./color\n\nfn v(c Color) Int {\n\tmatch c {\n\t\tcolor.Red -> 0\n\t}\n}\nprintln(v(color.Red))\n",
    );
    project_rejects(&proj, "check", "main.al", &["not exhaustive", "Green"]);
}

/// Labelled arguments and `..` work through a qualifier, as they do bare.
#[test]
fn a_qualified_pattern_takes_labels_and_rest() {
    let proj = Project::new("qual_pat_args");
    proj.write("color.al", COLOR_SRC);
    proj.write(
        "lab.al",
        "import ./color\n\nfn v(c Color) Int {\n\tmatch c {\n\t\tcolor.Red -> 0\n\t\tcolor.Green(shade: s) -> s\n\t}\n}\nprintln(v(color.Green(5)))\n",
    );
    run_project_outputs(&proj, "run", "lab.al", "5\n");
    proj.write(
        "rest.al",
        "import ./color\n\nfn v(c Color) Int {\n\tmatch c {\n\t\tcolor.Red -> 0\n\t\tcolor.Green(..) -> 1\n\t}\n}\nprintln(v(color.Green(5)))\n",
    );
    run_project_outputs(&proj, "run", "rest.al", "1\n");
}

/// An `opaque` type's constructor is not reachable by qualifier, exactly as it
/// is not reachable as an expression (`id.Id(1)`).
#[test]
fn a_qualified_pattern_cannot_reach_an_opaque_constructor() {
    let proj = Project::new("qual_pat_opaque");
    proj.write(
        "id.al",
        "pub opaque type Id {\n\tId(n Int)\n}\n\npub fn make(n Int) Id {\n\tId(n)\n}\n",
    );
    proj.write(
        "main.al",
        "import ./id\n\nfn get(i Id) Int {\n\tmatch i {\n\t\tid.Id(n) -> n\n\t}\n}\nprintln(get(id.make(7)))\n",
    );
    project_rejects(&proj, "check", "main.al", &["private"]);
}

/// Every failure must produce a diagnostic. A silent `None` leaves the module
/// error-free, mints its clean-module proof, and aborts the elaborator on a
/// program `al check` accepted.
#[test]
fn a_bad_qualified_pattern_is_a_diagnostic_not_a_crash() {
    let proj = Project::new("qual_pat_bad");
    proj.write("color.al", COLOR_SRC);
    proj.write(
        "unknown_qual.al",
        "import ./color\n\nfn v(c Color) Int {\n\tmatch c {\n\t\tnope.Red -> 0\n\t\telse -> 1\n\t}\n}\nprintln(v(color.Red))\n",
    );
    project_rejects(
        &proj,
        "check",
        "unknown_qual.al",
        &["Unknown module qualifier"],
    );

    proj.write(
        "unknown_member.al",
        "import ./color\n\nfn v(c Color) Int {\n\tmatch c {\n\t\tcolor.Purple -> 0\n\t\telse -> 1\n\t}\n}\nprintln(v(color.Red))\n",
    );
    project_rejects(&proj, "check", "unknown_member.al", &["has no constructor"]);
}
