use scarlet::reference::EntityKind;

mod common;
use common::{
    Project, SessionQueryExt, checked_with, cursor, project_rejects, run_al, run_project_outputs,
};

const UTIL_SRC: &str =
    "pub fn quote(s String) String { '\"' + s + '\"' }\npub fn empty() String { '' }\n";

#[test]
fn relative_qualified() {
    let proj = Project::new("rel_qual");
    proj.write("util.scrl", UTIL_SRC);
    proj.write(
        "main.scrl",
        "import ./util\n\npub fn main() {\n\tprintln(util.quote('hi'))\n}\n",
    );
    run_project_outputs(&proj, "run", "main.scrl", "\"hi\"\n");
}

#[test]
fn labelled_args_on_a_qualified_call_take_declared_order() {
    // A qualified callee resolves through `resolve_qualified`, so its parameter
    // labels come off the imported module's scheme rather than the local env —
    // a separate path from the bare-name one covered in `type_semantics`. Two
    // same-typed parameters and an order-revealing body, so source order and
    // declared order give different answers while both type-check.
    let proj = Project::new("qual_labels");
    proj.write("math.scrl", "pub fn diff(a Int, b Int) Int { a - b }\n");
    proj.write(
        "main.scrl",
        "import ./math\nimport ./math as m\n\npub fn main() {\n\
         \tprintln(math.diff(b: 2, a: 1))\n\
         \tprintln(m.diff(b: 10, a: 4))\n\
         }\n",
    );
    run_project_outputs(&proj, "run", "main.scrl", "-1\n-6\n");
}

#[test]
fn aliased_module_type_unifies_with_canonical() {
    // `l.Color` under `import ./lib as l` must hydrate to the type's canonical
    // nominal name. Values carry the canonical name, so an annotation spelled
    // through the alias would never unify with one.
    let proj = Project::new("alias_type");
    proj.write("lib.scrl", "pub type Color {\n\tRed\n\tGreen\n\tBlue\n}\n");
    proj.write(
        "main.scrl",
        "import ./lib as l\n\nfn id(c l.Color) l.Color { c }\n\npub fn main() {\n\tprintln(id(l.Red))\n}\n",
    );
    run_project_outputs(&proj, "run", "main.scrl", "Red\n");
}

#[test]
fn relative_import() {
    let proj = Project::new("rel_imp");
    proj.write("helper.scrl", "pub fn greet() String { 'hello' }\n");
    proj.write(
        "main.scrl",
        "import ./helper\n\npub fn main() {\n\tprintln(helper.greet())\n}\n",
    );
    run_project_outputs(&proj, "run", "main.scrl", "hello\n");
}

#[test]
fn private_is_not_importable() {
    let proj = Project::new("priv");
    proj.write("helper.scrl", "fn secret() { 'x' }\npub fn ok() { 'y' }\n");

    proj.write(
        "main_qual.scrl",
        "import ./helper\n\npub fn main() {\n\tprintln(helper.secret())\n}\n",
    );
    project_rejects(&proj, "run", "main_qual.scrl", &["private"]);
}

#[test]
fn opaque_type_hides_constructors() {
    let proj = Project::new("opaque");
    proj.write(
        "id.scrl",
        "pub opaque type Id { Id(n Int) }\n\
         pub fn make(n Int) Id { Id(n) }\n\
         pub fn get(i Id) Int { match i { Id(n) -> n } }\n",
    );

    proj.write(
        "ok.scrl",
        "import ./id\n\
         fn use(i id.Id) Int { id.get(i) }\n\
         pub fn main() {\n\
         \tprintln(use(id.make(42)))\n\
         }\n",
    );
    run_project_outputs(&proj, "run", "ok.scrl", "42\n");

    proj.write(
        "bad_qual.scrl",
        "import ./id\n\npub fn main() {\n\t_ = id.Id(1)\n}\n",
    );
    project_rejects(&proj, "check", "bad_qual.scrl", &["private", "opaque"]);
}

#[test]
fn external_type_allowed_in_user_code() {
    let proj = Project::new("ext");
    proj.write("handle.scrl", "pub type Handle\n");
    proj.write(
        "main.scrl",
        "import ./handle\nfn id(h handle.Handle) handle.Handle { h }\n",
    );
    let r = run_al("check", &proj.dir.join("main.scrl"));
    assert!(r.success, "out={} err={}", r.stdout, r.stderr);
}

#[test]
fn unknown_module() {
    let proj = Project::new("unknown_mod");
    proj.write("main.scrl", "import scarlet/nope\n");
    project_rejects(
        &proj,
        "run",
        "main.scrl",
        &["no such stdlib module scarlet/nope"],
    );
}

run_case! {
    stdlib_net_socket_type: (
        "import scarlet/net/socket\n\nfn id(s socket.Socket) socket.Socket { s }\n\npub fn main() {\n\tprintln('ok')\n}\n",
        "ok\n",
    ),
}

#[test]
fn cycle_detection() {
    let proj = Project::new("cycle");
    proj.write("a.scrl", "import ./b\npub fn fa() { 1 }\n");
    proj.write("b.scrl", "import ./a\npub fn fb() { 2 }\n");
    project_rejects(&proj, "run", "a.scrl", &["cycle", "circular"]);
}

#[test]
fn query_api_cross_module_goto_def_and_symbols() {
    let proj = Project::new("qapi_xmod");
    proj.write("util.scrl", UTIL_SRC);
    let entry = "import ./util\n\npub fn main() {\n\tprintln(util.quote('hi'))\n}\n";
    proj.write("main.scrl", entry);
    let s = checked_with(&proj, entry);

    let (l, c) = cursor(entry, "quote", 1, 0);
    let (m, span) = s
        .definition("main", l, c)
        .expect("util.quote resolves cross-module");
    assert_eq!(m.last().map(String::as_str), Some("util"));
    assert!(span.end_column > span.start_column, "real decl span");

    let util_path = proj.dir.join("util.scrl");
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
fn query_api_alias_imports_resolve() {
    let proj = Project::new("qapi_alias");
    proj.write("util.scrl", UTIL_SRC);
    let entry =
        "import ./util as u\n\npub fn main() {\n\tprintln(u.empty())\n\tprintln(u.quote('x'))\n}\n";
    proj.write("main.scrl", entry);
    let s = checked_with(&proj, entry);

    let (l, c) = cursor(entry, "empty", 1, 0);
    let (m, _) = s.definition("main", l, c).expect("u.empty resolves");
    assert_eq!(m.last().map(String::as_str), Some("util"));

    // A member reached through the alias points at its real declaration.
    let (lq, cq) = cursor(entry, "quote('x')", 1, 0);
    let (mq, sq) = s.definition("main", lq, cq).expect("u.quote resolves");
    assert_eq!(mq.last().map(String::as_str), Some("util"));
    let quote_decl = cursor(UTIL_SRC, "quote", 1, 0);
    assert_eq!(
        (sq.start_line, sq.start_column),
        quote_decl,
        "u.quote must point at quote's real declaration"
    );

    assert!(
        s.document_symbols("main")
            .iter()
            .any(|x| x.name == "u" && x.kind == EntityKind::ModuleAlias),
        "module alias `u` not tracked"
    );
}

// An imported module's top level may contain only declarations, and says so
// in the module's own words rather than the entry file's `pub fn main()` hint.
// The entry is a well-formed program; only the import is rejected.
#[test]
fn module_top_level_executable_code_is_error() {
    let proj = Project::new("mod_toplevel_exec");
    proj.write("lib.scrl", "pub fn ok() Int { 1 }\nprintln(99)\n");
    proj.write(
        "main.scrl",
        "import ./lib\n\npub fn main() {\n\tprintln(lib.ok())\n}\n",
    );
    project_rejects(
        &proj,
        "run",
        "main.scrl",
        &["Modules may only contain declarations at the top level"],
    );
}

// Rejected by the qualified-lookup path.
#[test]
fn qualified_import_unknown_member_is_error() {
    let proj = Project::new("mod_qual_unknown");
    proj.write("lib.scrl", "pub fn ok() Int { 1 }\n");
    proj.write(
        "main.scrl",
        "import ./lib\n\npub fn main() {\n\tlib.nope()\n}\n",
    );
    project_rejects(
        &proj,
        "run",
        "main.scrl",
        &["Module './lib' has no member 'nope'"],
    );
}

/// A lambda's body is walked while the import qualifier is in scope but
/// elaborated after a same-named `let` later in the block entered the value
/// env. An elaborator that re-probed the live env would read `util.empty()` as
/// a field access, enter an expression the walk never entered, and abort.
#[test]
fn lambda_body_keeps_the_walks_qualifier_verdict() {
    let proj = Project::new("qual_pinned");
    proj.write("util.scrl", "pub fn empty() String { 'E' }\n");
    proj.write(
        "main.scrl",
        "import ./util\n\npub fn main() {\n\tf = fn() { util.empty() }\n\tutil = 5\n\tprintln(f())\n\tprintln(util)\n}\n",
    );
    run_project_outputs(&proj, "run", "main.scrl", "E\n5\n");
}

/// The other side of the same coin: the shadowing `let` binds a value with a
/// field of the member's name, so after it `one.go` really is a field read while
/// inside the earlier-walked lambda it is still module `one`'s `go`.
#[test]
fn shadowed_qualifier_is_a_field_read_only_after_the_bind() {
    let proj = Project::new("qual_shadow");
    proj.write("one.scrl", "pub const go = 7\n");
    proj.write(
        "main.scrl",
        "import ./one\n\ntype Box {\n\tgo Int\n}\n\npub fn main() {\n\tg = fn() { one.go }\n\tone = Box(9)\n\tprintln(g())\n\tprintln(one.go)\n}\n",
    );
    run_project_outputs(&proj, "run", "main.scrl", "7\n9\n");
}

// A module is the file it resolved to, never the import as written. Keying the
// module cache on the spelling made the first `b.scrl` loaded win program-wide,
// and nothing failed loudly — the wrong module was simply used.

/// `sub/mid.scrl` imports `./b`, which must be `sub/b.scrl`, not the root's.
#[test]
fn same_named_modules_in_different_directories_are_distinct() {
    let proj = Project::new("mod_identity");
    std::fs::create_dir_all(proj.dir.join("sub")).unwrap();
    proj.write("b.scrl", "pub fn who() String {\n\t'ROOT'\n}\n");
    proj.write("sub/b.scrl", "pub fn who() String {\n\t'SUB'\n}\n");
    proj.write(
        "sub/mid.scrl",
        "import ./b\n\npub fn go() String {\n\tb.who()\n}\n",
    );
    proj.write(
        "main.scrl",
        "import ./b\nimport ./sub/mid\n\npub fn main() {\n\tprintln(b.who())\n\tprintln(mid.go())\n}\n",
    );
    run_project_outputs(&proj, "run", "main.scrl", "ROOT\nSUB\n");
}

/// The same file reached by two different spellings (`./b` from the root and
/// `../b` from `sub/`) is ONE module: it must compile once and share state.
#[test]
fn one_file_reached_two_ways_is_one_module() {
    let proj = Project::new("mod_identity_alias");
    std::fs::create_dir_all(proj.dir.join("sub")).unwrap();
    proj.write("b.scrl", "pub fn who() String {\n\t'ROOT'\n}\n");
    proj.write(
        "sub/mid.scrl",
        "import ../b\n\npub fn go() String {\n\tb.who()\n}\n",
    );
    proj.write(
        "main.scrl",
        "import ./b\nimport ./sub/mid\n\npub fn main() {\n\tprintln(b.who())\n\tprintln(mid.go())\n}\n",
    );
    run_project_outputs(&proj, "run", "main.scrl", "ROOT\nROOT\n");
}

/// A missing relative import must not be satisfied by a same-named file in
/// another directory. This is the shape the LSP silently accepted.
#[test]
fn a_module_in_another_directory_does_not_satisfy_a_relative_import() {
    let proj = Project::new("mod_identity_missing");
    std::fs::create_dir_all(proj.dir.join("sub")).unwrap();
    proj.write("sub/b.scrl", "pub fn who() String {\n\t'SUB'\n}\n");
    // `./b` from the root: `sub/b.scrl` must NOT satisfy it.
    proj.write(
        "main.scrl",
        "import ./b\n\npub fn main() {\n\tprintln(b.who())\n}\n",
    );
    project_rejects(&proj, "check", "main.scrl", &["file not found"]);
}

/// A type in `sub/b.scrl` and one in `b.scrl` are different types even though both
/// modules are spelled `./b`. A shared cache entry would unify them.
#[test]
fn same_named_modules_do_not_share_types() {
    let proj = Project::new("mod_identity_types");
    std::fs::create_dir_all(proj.dir.join("sub")).unwrap();
    proj.write(
        "b.scrl",
        "pub type T {\n\tT(n Int)\n}\n\npub fn make() T {\n\tT(1)\n}\n\npub fn get(t T) Int {\n\tt.n\n}\n",
    );
    proj.write(
        "sub/b.scrl",
        "pub type T {\n\tT(s String)\n}\n\npub fn make() T {\n\tT('x')\n}\n\npub fn get(t T) String {\n\tt.s\n}\n",
    );
    proj.write(
        "sub/mid.scrl",
        "import ./b\n\npub fn go() String {\n\tb.get(b.make())\n}\n",
    );
    proj.write(
        "main.scrl",
        "import ./b\nimport ./sub/mid\n\npub fn main() {\n\tprintln(b.get(b.make()))\n\tprintln(mid.go())\n}\n",
    );
    run_project_outputs(&proj, "run", "main.scrl", "1\nx\n");
}

// `match e { io.NotFound(path) -> … }` reaches a constructor through its own
// module, so a program need not import the name to match on it and two modules
// exporting a `NotFound` cannot collide.

const COLOR_SRC: &str = "pub type Color {\n\tRed\n\tGreen(shade Int)\n}\n";

#[test]
fn a_qualified_constructor_pattern_matches() {
    let proj = Project::new("qual_pat");
    proj.write("color.scrl", COLOR_SRC);
    proj.write(
        "main.scrl",
        "import ./color\n\nfn v(c color.Color) Int {\n\tmatch c {\n\t\tcolor.Red -> 0\n\t\tcolor.Green(s) -> s\n\t}\n}\n\npub fn main() {\n\tprintln(v(color.Green(3)))\n\tprintln(v(color.Red))\n}\n",
    );
    run_project_outputs(&proj, "run", "main.scrl", "3\n0\n");
}

/// Two qualifiers for one module denote the same constructors, so
/// exhaustiveness counts an arm through each of them together.
#[test]
fn constructors_through_two_qualifiers_are_the_same_constructor() {
    let proj = Project::new("qual_pat_mixed");
    proj.write("color.scrl", COLOR_SRC);
    proj.write(
        "main.scrl",
        "import ./color\nimport ./color as c\n\nfn v(k color.Color) Int {\n\tmatch k {\n\t\tc.Red -> 0\n\t\tcolor.Green(s) -> s\n\t}\n}\n\npub fn main() {\n\tprintln(v(color.Green(9)))\n}\n",
    );
    run_project_outputs(&proj, "run", "main.scrl", "9\n");
}

/// Exhaustiveness resolves a constructor against the scrutinee's own variants,
/// so a qualified arm counts as covering.
#[test]
fn a_qualified_pattern_is_seen_by_exhaustiveness() {
    let proj = Project::new("qual_pat_exh");
    proj.write("color.scrl", COLOR_SRC);
    proj.write(
        "main.scrl",
        "import ./color\n\nfn v(c color.Color) Int {\n\tmatch c {\n\t\tcolor.Red -> 0\n\t}\n}\n\npub fn main() {\n\tprintln(v(color.Red))\n}\n",
    );
    project_rejects(&proj, "check", "main.scrl", &["not exhaustive", "Green"]);
}

/// The irrefutability check on a destructuring binding lowers its pattern
/// through the same path as a match, so a qualified head must resolve there
/// too, whether the value is built here or returned by the declaring module.
#[test]
fn a_qualified_constructor_destructures_irrefutably() {
    let proj = Project::new("qual_ctor_destructure");
    proj.write(
        "pair.scrl",
        "pub type Pair {\n\tPair(a Int, b Int)\n}\n\npub fn make(x Int, y Int) Pair {\n\tPair(x, y)\n}\n",
    );
    proj.write(
        "main.scrl",
        "import ./pair\n\npub fn main() {\n\tpair.Pair(a, b) = pair.Pair(1, 2)\n\tprintln(a + b)\n\tpair.Pair(c, d) = pair.make(3, 4)\n\tprintln(c + d)\n}\n",
    );
    run_project_outputs(&proj, "run", "main.scrl", "3\n7\n");
}

/// `Color`, plus a constructor function in the declaring module, so a
/// scrutinee can cross the module boundary.
const COLOR_MAKE_SRC: &str =
    "pub type Color {\n\tRed\n\tGreen(shade Int)\n}\n\npub fn make(n Int) Color {\n\tGreen(n)\n}\n";

/// Three variants, so a catch-all arm stays reachable alongside an arm that
/// covers only one of them.
const HUE_SRC: &str = "pub type Hue {\n\tRed\n\tGreen(shade Int)\n\tBlue\n}\n\npub fn make(n Int) Hue {\n\tGreen(n)\n}\n";

/// A match with a catch-all arm is not exhaustive-by-heads, so it lowers to
/// the test ladder rather than the tag switch. The ladder must test for the
/// variant the qualified head resolves to, for a value returned by the
/// declaring module and for one built here.
///
/// The tests above omit a catch-all, so they all take the switch and none of
/// them can see this.
#[test]
fn a_qualified_arm_matches_beside_a_catch_all() {
    let proj = Project::new("qual_ctor_catchall");
    proj.write("color.scrl", COLOR_MAKE_SRC);
    proj.write(
        "main.scrl",
        "import ./color\n\npub fn main() {\n\tmatch color.make(9) {\n\t\tcolor.Green(s) -> println(s)\n\t\t_ -> println(0)\n\t}\n\tmatch color.Green(4) {\n\t\tcolor.Green(s) -> println(s)\n\t\t_ -> println(0)\n\t}\n}\n",
    );
    run_project_outputs(&proj, "run", "main.scrl", "9\n4\n");
}

/// A bare binding arm is a catch-all too, and takes the same ladder. This is
/// the shape that broke RESP3 pub/sub: `other -> ...` after a module's head.
#[test]
fn a_qualified_arm_matches_beside_a_bare_binding_catch_all() {
    let proj = Project::new("qual_ctor_binding_catchall");
    proj.write("color.scrl", COLOR_MAKE_SRC);
    proj.write(
        "main.scrl",
        "import ./color\n\npub fn main() {\n\tx = color.make(7)\n\tmatch x {\n\t\tcolor.Green(s) -> println(s)\n\t\tother -> println(other)\n\t}\n}\n",
    );
    run_project_outputs(&proj, "run", "main.scrl", "7\n");
}

/// A qualified head nested inside a tuple pattern reaches the ladder through a
/// different lowering path than a top-level head, and resolves the same way.
#[test]
fn a_qualified_head_matches_nested_beside_a_catch_all() {
    let proj = Project::new("qual_ctor_nested_catchall");
    proj.write("hue.scrl", HUE_SRC);
    proj.write(
        "main.scrl",
        "import ./hue\n\npub fn main() {\n\tn = (hue.make(9), 1)\n\tmatch n {\n\t\t(hue.Green(s), 1) -> println(s)\n\t\t_ -> println(0)\n\t}\n}\n",
    );
    run_project_outputs(&proj, "run", "main.scrl", "9\n");
}

/// A qualified head as one alternative of an or-pattern. `Hue`'s third variant
/// keeps the catch-all reachable, so the match still takes the ladder.
#[test]
fn a_qualified_head_matches_in_an_or_pattern_beside_a_catch_all() {
    let proj = Project::new("qual_ctor_or_catchall");
    proj.write("hue.scrl", HUE_SRC);
    proj.write(
        "main.scrl",
        "import ./hue\n\npub fn main() {\n\to = hue.make(4)\n\tmatch o {\n\t\thue.Green(_s) | hue.Red -> println(1)\n\t\t_ -> println(0)\n\t}\n}\n",
    );
    run_project_outputs(&proj, "run", "main.scrl", "1\n");
}

/// Resolving the head through the declaration must not make a dead arm look
/// live: a catch-all placed first still shadows the qualified arm after it.
#[test]
fn a_catch_all_before_a_qualified_arm_is_still_unreachable() {
    let proj = Project::new("qual_ctor_catchall_first");
    proj.write("hue.scrl", HUE_SRC);
    proj.write(
        "main.scrl",
        "import ./hue\n\npub fn main() {\n\tc = hue.make(3)\n\tmatch c {\n\t\t_ -> println(0)\n\t\thue.Green(_s) -> println(1)\n\t}\n}\n",
    );
    project_rejects(&proj, "check", "main.scrl", &["unreachable"]);
}

/// A module alias is a spelling, not an identity. A value built through it is
/// the variant the module declares: it prints under that name and is equal to
/// the same variant built any other way.
#[test]
fn a_module_alias_does_not_change_a_constructed_value_identity() {
    let proj = Project::new("alias_ctor_value_identity");
    proj.write("color.scrl", COLOR_MAKE_SRC);
    proj.write(
        "main.scrl",
        "import ./color\nimport ./color as c\n\npub fn main() {\n\tprintln(c.Green(9))\n\tprintln(c.Green(9) == color.Green(9))\n\tprintln(c.Green(9) == color.make(9))\n}\n",
    );
    run_project_outputs(&proj, "run", "main.scrl", "Green(9)\nTrue\nTrue\n");
}

/// A stdlib constructor's `variant_name` is written into the static blob by
/// `crates/scarlet/build.rs`, not by the resolver, so it is a second copy of
/// the same decision, and the tests above all use user modules.
///
/// `binary.from_int_ascii` dispatches on its `Radix` argument by name inside
/// the VM, so a `Hex` reached through an alias must still be the `Radix` the
/// builtin recognises.
#[test]
fn a_stdlib_constructor_through_an_alias_reaches_a_vm_builtin() {
    let proj = Project::new("alias_ctor_stdlib");
    proj.write(
        "main.scrl",
        "import scarlet/binary as b\n\npub fn main() {\n\tprintln(b.from_int_ascii(255, b.Hex))\n}\n",
    );
    run_project_outputs(&proj, "run", "main.scrl", "<<102, 102>>\n");
}

/// Labelled arguments and `..` work through a qualifier, as they do bare.
#[test]
fn a_qualified_pattern_takes_labels_and_rest() {
    let proj = Project::new("qual_pat_args");
    proj.write("color.scrl", COLOR_SRC);
    proj.write(
        "lab.scrl",
        "import ./color\n\nfn v(c color.Color) Int {\n\tmatch c {\n\t\tcolor.Red -> 0\n\t\tcolor.Green(shade: s) -> s\n\t}\n}\n\npub fn main() {\n\tprintln(v(color.Green(5)))\n}\n",
    );
    run_project_outputs(&proj, "run", "lab.scrl", "5\n");
    proj.write(
        "rest.scrl",
        "import ./color\n\nfn v(c color.Color) Int {\n\tmatch c {\n\t\tcolor.Red -> 0\n\t\tcolor.Green(..) -> 1\n\t}\n}\n\npub fn main() {\n\tprintln(v(color.Green(5)))\n}\n",
    );
    run_project_outputs(&proj, "run", "rest.scrl", "1\n");
}

/// An `opaque` type's constructor is not reachable by qualifier, exactly as it
/// is not reachable as an expression (`id.Id(1)`).
#[test]
fn a_qualified_pattern_cannot_reach_an_opaque_constructor() {
    let proj = Project::new("qual_pat_opaque");
    proj.write(
        "id.scrl",
        "pub opaque type Id {\n\tId(n Int)\n}\n\npub fn make(n Int) Id {\n\tId(n)\n}\n",
    );
    proj.write(
        "main.scrl",
        "import ./id\n\nfn get(i Id) Int {\n\tmatch i {\n\t\tid.Id(n) -> n\n\t}\n}\n\npub fn main() {\n\tprintln(get(id.make(7)))\n}\n",
    );
    project_rejects(&proj, "check", "main.scrl", &["private"]);
}

/// Every failure must produce a diagnostic. A silent `None` leaves the module
/// error-free, mints its clean-module proof, and aborts the elaborator on a
/// program `al check` accepted.
#[test]
fn a_bad_qualified_pattern_is_a_diagnostic_not_a_crash() {
    let proj = Project::new("qual_pat_bad");
    proj.write("color.scrl", COLOR_SRC);
    proj.write(
        "unknown_qual.scrl",
        "import ./color\n\nfn v(c color.Color) Int {\n\tmatch c {\n\t\tnope.Red -> 0\n\t\t_ -> 1\n\t}\n}\n\npub fn main() {\n\tprintln(v(color.Red))\n}\n",
    );
    project_rejects(
        &proj,
        "check",
        "unknown_qual.scrl",
        &["Unknown module qualifier"],
    );

    proj.write(
        "unknown_member.scrl",
        "import ./color\n\nfn v(c color.Color) Int {\n\tmatch c {\n\t\tcolor.Purple -> 0\n\t\t_ -> 1\n\t}\n}\n\npub fn main() {\n\tprintln(v(color.Red))\n}\n",
    );
    project_rejects(
        &proj,
        "check",
        "unknown_member.scrl",
        &["has no constructor"],
    );
}
