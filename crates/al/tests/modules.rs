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

    // The type and smart-constructor functions are importable.
    proj.write(
        "ok.al",
        "import ./id.{Id, make, get}\n\
         fn use(i Id) Int { get(i) }\n\
         println(use(make(42)))\n",
    );
    run_project_outputs(&proj, "run", "ok.al", "42\n");

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

    // `util.quote(..)` in the entry resolves to its declaration in util.al.
    let (l, c) = cursor(entry, "quote", 1, 0);
    let (m, span) = s
        .definition("main", l, c)
        .expect("util.quote resolves cross-module");
    assert_eq!(m.last().map(String::as_str), Some("util"));
    assert!(span.end_column > span.start_column, "real decl span");

    // documentSymbol over util.al lists both exported functions.
    let util_path = proj.dir.join("util.al");
    let mut names: Vec<String> = s
        .document_symbols(&util_path.to_string_lossy())
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

/// A lambda's body is walked while the import qualifier is still in scope, but
/// elaborated after a same-named top-level `let` has entered the value env.
/// The qualified/field decision belongs to the check walk, which recorded it; an
/// elaborator that re-probed the live env would decide `util.empty()` is a field
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

// ── Module identity ────────────────────────────────────────────────────────
//
// A module IS the file it resolved to. These pin that, because the compiler
// used to key its module cache on the import *as written*: `./b` from any
// directory keyed as `"b"`, so the first `b.al` loaded won program-wide.
// Nothing failed loudly — the wrong module was simply used.

/// `sub/mid.al` imports `./b`, which must be `sub/b.al`, not the root's.
/// This printed `ROOT ROOT` before the fix.
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
    project_rejects(&proj, "check", "main.al", &["Unknown module"]);
}

/// A type defined in `sub/b.al` and one in `b.al` are different types, even
/// though both modules are spelled `./b`. Sharing a cache entry would have
/// unified them.
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
