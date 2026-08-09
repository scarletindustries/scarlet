//! End-to-end coverage for the workspace reference graph exposed through
//! `IncrementalSession`: goto-definition, find-references, rename, symbols,
//! the `al/*` stdlib import path, unused-import / dead-code hints, and
//! incremental stability of the reverse-edge index.

use al::bytecode::IncrementalSession;
use al::module;
use al::reference::EntityKind;
use al::span::Span;

mod common;
use common::{
    Project, SessionQueryExt, assert_has_msg, assert_has_sym, assert_msg_eq, assert_no_msg,
    checked_with, cursor, parse, sym_names,
};

const C_SRC: &str = "pub fn shared() Int { 42 }\n";
const B_SRC: &str = "import ./c\npub fn bridge() Int { c.shared() + 1 }\n";
// Entry uses a def in B, a def in C (also used by B), and a stdlib def.
const ENTRY: &str = "import ./b\n\
import ./c\n\
import al/array\n\
x = b.bridge()\n\
y = c.shared()\n\
z = array.length([1, 2, 3])\n\
println(x + y + z)\n";

fn project() -> Project {
    let p = Project::new("xmod");
    p.write("c.al", C_SRC);
    p.write("b.al", B_SRC);
    p.write("a.al", ENTRY);
    p
}

fn checked(p: &Project) -> IncrementalSession {
    checked_with(p, ENTRY)
}

/// Unused-entity diagnostics for the entry module: asserts every one is a
/// Hint and returns their messages.
fn unused_msgs(s: &IncrementalSession) -> Vec<String> {
    let hints = s
        .reference_graph()
        .unused_diagnostics_for(&module::main_module());
    assert!(
        hints
            .iter()
            .all(|d| d.severity == al::diagnostic::Severity::Hint),
        "unused diagnostics must be Hints: {hints:?}"
    );
    hints.into_iter().map(|d| d.message).collect()
}

#[test]
fn cross_module_goto_def_use_in_a_def_in_b() {
    let p = project();
    let s = checked(&p);

    let (l, c) = cursor(ENTRY, "bridge", 1, 1);
    let (m, span) = s
        .definition("main", l, c)
        .expect("b.bridge resolves to a definition");
    assert_eq!(m.last().map(String::as_str), Some("b"));
    assert!(span.start_line >= 0 && span.start_column >= 0);

    // C is reached two ways (A imports C, B imports C); A's occurrence still
    // resolves to C's declaration.
    let (l, c) = cursor(ENTRY, "shared", 1, 1);
    let (m, _) = s
        .definition("main", l, c)
        .expect("c.shared resolves to a definition");
    assert_eq!(m.last().map(String::as_str), Some("c"));

    // goto-def on a declaration name resolves to itself (B's `bridge`).
    let (l, c) = cursor(B_SRC, "bridge", 1, 1);
    let (m, _) = s
        .definition("./b", l, c)
        .expect("declaration resolves to itself");
    assert_eq!(m.last().map(String::as_str), Some("b"));
}

#[test]
fn find_references_across_modules() {
    let p = project();
    let s = checked(&p);

    // Resolve `shared`'s canonical DefId from the entry's use site.
    let (l, c) = cursor(ENTRY, "shared", 1, 1);
    let (defid, _) = s
        .prepare_rename("main", l, c)
        .expect("prepare_rename yields shared's DefId");
    assert_eq!(defid.entity, EntityKind::Function);

    let refs = s.references(defid);
    let mods: Vec<String> = refs.iter().map(|(m, _)| m.join("/")).collect();

    // shared is declared in C and used qualified from both B and the entry.
    assert!(
        mods.iter().any(|m| m.ends_with("c")),
        "declaration module C missing from {mods:?}"
    );
    assert!(
        mods.iter().any(|m| m == "main"),
        "entry use missing from {mods:?}"
    );
    assert!(
        mods.iter().any(|m| m.ends_with("b")),
        "cross-module use in B missing from {mods:?}"
    );
    assert!(
        refs.len() >= 3,
        "expected decl + 2 uses, got {}: {mods:?}",
        refs.len()
    );
}

#[test]
fn workspace_rename_uses_reverse_edges() {
    let p = project();
    let s = checked(&p);

    let (l, c) = cursor(ENTRY, "shared", 1, 1);
    let (defid, _) = s.prepare_rename("main", l, c).expect("DefId for shared");

    let edits = s.rename(defid);
    assert_eq!(edits, s.references(defid), "rename == references closure");

    let distinct: std::collections::BTreeSet<String> =
        edits.iter().map(|(m, _)| m.join("/")).collect();
    assert!(
        distinct.len() >= 2,
        "rename must rewrite across modules, touched {distinct:?}"
    );
    for (m, sp) in &edits {
        assert!(
            sp.end_line > sp.start_line || sp.end_column > sp.start_column,
            "degenerate rename span in {m:?}: {sp:?}"
        );
    }
}

#[test]
fn document_and_workspace_symbols() {
    let p = project();
    let s = checked(&p);

    assert_has_sym(&s.document_symbols("./c"), "shared", EntityKind::Function);

    // The entry surfaces its import aliases, stdlib ones included.
    let main = s.document_symbols("main");
    assert_has_sym(&main, "array", EntityKind::ModuleAlias);

    // Substring search, case-insensitive.
    assert_has_sym(&s.workspace_symbols("SHAR"), "shared", EntityKind::Function);
    assert!(
        s.workspace_symbols("definitely_no_such_symbol").is_empty(),
        "workspace_symbols matched a non-existent name"
    );
}

#[test]
fn stdlib_import_path_is_tracked() {
    // The `al/array` import binds a `ModuleAlias` in the entry module, a
    // qualified use through it resolves goto-def *into* the precompiled stdlib
    // declaration, and the unused-import reachability rule reaches stdlib
    // imports too.
    let p = Project::new("stdlib");
    let entry = "import al/array\nz = array.length([1, 2, 3])\nprintln(z)\n";
    p.write("a.al", entry);
    let s = checked_with(&p, entry);

    let main = s.document_symbols("main");
    let alias = main
        .iter()
        .find(|sym| sym.name == "array")
        .expect("al/array import alias is tracked as a ModuleAlias");
    assert_eq!(alias.kind, EntityKind::ModuleAlias);
    assert_eq!(
        alias.module,
        module::main_module(),
        "the alias binding is owned by the importing (entry) module"
    );

    // Goto-def must land on the real `pub fn length` in src/std/al/array.al
    // (0-based line 49).
    let (l, c) = cursor(entry, "length", 1, 1);
    let (m, span) = s
        .definition("main", l, c)
        .expect("array.length resolves into the al/array stdlib module");
    assert_eq!(
        m,
        vec!["al".to_string(), "array".to_string()],
        "stdlib goto-def must land in the al/array module, got {m:?}"
    );
    assert_eq!(
        (span.start_line, span.start_column, span.end_column),
        (49, 7, 13),
        "must land on the real `length` declaration span, got {span:?}"
    );
    // One canonical DefId spans the synthesised stdlib definition and the
    // entry's occurrence.
    let (defid, _) = s
        .prepare_rename("main", l, c)
        .expect("a DefId for the stdlib `length`");
    assert_eq!(defid.entity, EntityKind::Function);
    let ref_mods: Vec<String> = s
        .references(defid)
        .iter()
        .map(|(m, _)| m.join("/"))
        .collect();
    assert!(
        ref_mods.iter().any(|m| m == "al/array"),
        "references missing the al/array declaration: {ref_mods:?}"
    );
    assert!(
        ref_mods.iter().any(|m| m == "main"),
        "references missing the entry use site: {ref_mods:?}"
    );

    assert_no_msg(&unused_msgs(&s), "unused import `array`");

    // The unused-import rule reaches stdlib imports too.
    let p2 = Project::new("stdlib_unused");
    let unused = "import al/array\nprintln(1)\n";
    p2.write("a.al", unused);
    let s2 = checked_with(&p2, unused);
    assert_msg_eq(&unused_msgs(&s2), "unused import `array`");
}

#[test]
fn unused_import_and_dead_private_def_hints() {
    let p = Project::new("unused");
    p.write("c.al", C_SRC);
    // `import ./c` is never used; `deadpriv` is a private, uncalled fn.
    let entry = "import ./c\nfn deadpriv() Int { 0 }\nprintln(1)\n";
    p.write("a.al", entry);

    let s = checked_with(&p, entry);
    let msgs = unused_msgs(&s);

    assert_has_msg(&msgs, "unused import `c`");
    assert_msg_eq(&msgs, "unused function `deadpriv`");
    // `println` is a used builtin and must never be reported.
    assert_no_msg(&msgs, "println");
}

#[test]
fn incremental_edit_b_keeps_refs_then_invalidate_drops_reverse_edges() {
    let p = project();

    let mut s = checked(&p);
    let (bl, bc) = cursor(ENTRY, "bridge", 1, 1);
    let (m0, _) = s.definition("main", bl, bc).expect("bridge resolves");
    assert_eq!(m0.last().map(String::as_str), Some("b"));
    let (defid0, _) = s.prepare_rename("main", bl, bc).expect("bridge DefId");
    assert!(
        !s.reference_graph().references_to(defid0).is_empty(),
        "entry use should create a reverse edge into B"
    );
    let n0 = s.compile_count();

    // Edit B's body only, keeping `bridge`.
    p.write(
        "b.al",
        "import ./c\npub fn bridge() Int { c.shared() + 100 }\n",
    );
    let r = s.check(&parse(ENTRY), Some(&p.dir));
    assert!(r.success(), "after B edit: {:?}", r.diagnostics);
    assert!(s.compile_count() > n0, "B must have recompiled");
    let (m1, _) = s
        .definition("main", bl, bc)
        .expect("bridge still resolves after B edit");
    assert_eq!(
        m1.last().map(String::as_str),
        Some("b"),
        "edit B: refs into B from A still resolve"
    );
    let (defid1, _) = s
        .prepare_rename("main", bl, bc)
        .expect("bridge DefId after edit");
    let refs1: Vec<String> = s
        .references(defid1)
        .iter()
        .map(|(m, _)| m.join("/"))
        .collect();
    assert!(
        refs1.iter().any(|m| m == "main"),
        "entry use lost after B edit: {refs1:?}"
    );

    // B drops `bridge`: the graph rebuild must leave no dangling reverse edge.
    p.write("b.al", "pub fn other() Int { 7 }\n");
    let entry2 = "import ./b\nprintln(b.other())\n";
    p.write("a.al", entry2);
    let r = s.check(&parse(entry2), Some(&p.dir));
    assert!(r.success(), "after B invalidation: {:?}", r.diagnostics);
    assert!(
        s.reference_graph().references_to(defid0).is_empty(),
        "stale reverse edges for the removed `bridge` must be dropped"
    );
    assert!(
        s.reference_graph().references_to(defid1).is_empty(),
        "no reverse edge may dangle after invalidation"
    );
}

#[test]
fn local_bindings_excluded_from_symbol_surfaces_but_stay_resolvable() {
    // Local binders are `EntityKind::Value` so navigation resolves on them,
    // but the symbol surfaces list structural declarations only.
    let p = Project::new("symsurface");
    let entry =
        "fn calc(base Int) Int {\n\tscaled = base * 2\n\tscaled + 1\n}\nx = calc(10)\nprintln(x)\n";
    p.write("a.al", entry);
    let s = checked_with(&p, entry);

    let doc = s.document_symbols("main");
    assert_has_sym(&doc, "calc", EntityKind::Function);
    assert!(
        !doc.iter().any(|sym| sym.kind == EntityKind::Value),
        "documentSymbol must not surface local `Value` binders: {:?}",
        sym_names(&doc)
    );
    assert!(
        !doc.iter().any(|s| s.name == "scaled" || s.name == "base"),
        "documentSymbol leaked a local binder/param: {:?}",
        sym_names(&doc)
    );

    let ws = s.workspace_symbols("scaled");
    assert!(
        ws.is_empty(),
        "workspace_symbols surfaced a local binder: {:?}",
        sym_names(&ws)
    );

    // Resolution on the local is unaffected: the filter never touches the
    // forward index.
    let (l, c) = cursor(entry, "scaled", 2, 1);
    let (m, _) = s
        .definition("main", l, c)
        .expect("goto-def on a local use still resolves to its binder");
    assert_eq!(m.last().map(String::as_str), Some("main"));
    let (defid, _) = s
        .prepare_rename("main", l, c)
        .expect("a local binder is still resolvable for rename");
    assert_eq!(defid.entity, EntityKind::Value);
    assert!(
        s.references(defid).len() >= 2,
        "find-refs over a local must include its binder and its use"
    );
}

const ALIAS_LIB: &str = "pub fn original() Int { 42 }\n";
const ALIAS_MAIN: &str = "import ./lib.{original as alias}\n\
x = alias()\n\
y = alias() + 1\n\
println(x + y)\n";

fn alias_project() -> Project {
    let p = Project::new("alias_rename");
    p.write("lib.al", ALIAS_LIB);
    p.write("a.al", ALIAS_MAIN);
    p
}

/// The exact source text covered by a single-line identifier `span`.
fn span_text(src: &str, sp: &Span) -> String {
    let line = src.lines().nth(sp.start_line as usize).unwrap_or("");
    line.chars()
        .skip(sp.start_column as usize)
        .take((sp.end_column - sp.start_column) as usize)
        .collect()
}

#[test]
fn rename_imported_symbol_does_not_capture_local_alias() {
    // Renaming the imported `original` must rewrite only spans that spell
    // `original`. Touching the `alias` binder or its uses would produce
    // `{renamed as renamed}` — name capture that no longer compiles.
    let p = alias_project();
    let s = checked_with(&p, ALIAS_MAIN);

    let (l, c) = cursor(ALIAS_MAIN, "original", 1, 1);
    let (defid, _) = s
        .prepare_rename("main", l, c)
        .expect("the `original` token resolves to lib's `original`");
    assert_eq!(defid.entity, EntityKind::Function);

    let spans = s.rename(defid);
    for (m, sp) in &spans {
        let src = if m.last().map(String::as_str) == Some("lib") {
            ALIAS_LIB
        } else {
            ALIAS_MAIN
        };
        let text = span_text(src, sp);
        assert_eq!(
            text, "original",
            "rename of `original` must never rewrite `{text}` (at {m:?} {sp:?}); \
             touching the alias binder or its uses captures the name"
        );
    }
    assert_eq!(
        spans.len(),
        2,
        "rename of `original` should cover its declaration and the import's \
         `original` token only: {spans:?}"
    );
}

#[test]
fn rename_local_alias_does_not_touch_imported_symbol() {
    // Renaming the local alias must rewrite only `alias` occurrences, all in
    // the entry module.
    let p = alias_project();
    let s = checked_with(&p, ALIAS_MAIN);

    // 2nd occurrence of `alias` is the first use (`x = alias()`).
    let (l, c) = cursor(ALIAS_MAIN, "alias", 2, 1);

    // Navigation is not split the way the rename class is: goto-def still
    // chains through to `original` in lib.
    let (gm, _) = s
        .definition("main", l, c)
        .expect("goto-def on the alias use resolves");
    assert_eq!(
        gm.last().map(String::as_str),
        Some("lib"),
        "goto-def on the alias must chain to the imported `original` in lib, got {gm:?}"
    );

    let (defid, _) = s
        .prepare_rename("main", l, c)
        .expect("a use of the local alias is resolvable for rename");

    let spans = s.rename(defid);
    for (m, sp) in &spans {
        assert_eq!(
            m.last().map(String::as_str),
            Some("main"),
            "alias rename escaped the entry module into {m:?}"
        );
        let text = span_text(ALIAS_MAIN, sp);
        assert_eq!(
            text, "alias",
            "rename of the alias must only rewrite `alias`, got `{text}` at {sp:?}"
        );
    }
    assert_eq!(
        spans.len(),
        3,
        "alias rename should cover its binder and both uses: {spans:?}"
    );
}

// `d.helper()` resolves into module `d` only, so it must not keep the unused
// `import ./c` alive.
#[test]
fn unused_one_of_two_plain_qualified_imports_is_flagged() {
    let p = Project::new("twoimp");
    p.write("c.al", "pub fn shared() Int { 42 }\n");
    p.write("d.al", "pub fn helper() Int { 7 }\n");
    let entry = "import ./c\nimport ./d\nx = d.helper()\nprintln(x)\n";
    p.write("a.al", entry);

    let s = checked_with(&p, entry);
    let msgs = unused_msgs(&s);

    assert_msg_eq(&msgs, "unused import `c`");
    assert_no_msg(&msgs, "unused import `d`");
}

// Session navigation over the Constructor / Type / Constant / Field entity
// kinds, each driven from a real use site.

const NAV_SRC: &str = "type Color {\n\tRed\n\tGreen\n\tBlue\n}\n\
type Box { label String }\n\
const LIMIT = 3\n\
fn pick(c Color) Int { if c == Green { 1 } else { 0 } }\n\
chosen = Red\n\
bx = Box(label: 'hi')\n\
shown = bx.label\n\
total = LIMIT + 1\n\
println(pick(chosen))\n\
println(shown)\n\
println(total)\n";

/// Drive goto-def + prepare_rename + find-references + rename for one entity
/// through the session query API. The 1st occurrence of `needle` in `src` is
/// always the declaration; `use_nth` (1-based) selects the use site to click.
fn assert_nav(
    s: &IncrementalSession,
    src: &str,
    needle: &str,
    use_nth: usize,
    expected: EntityKind,
) {
    let (decl_l, decl_c) = cursor(src, needle, 1, 0);
    let (ul, uc) = cursor(src, needle, use_nth, 1);

    let (decl_mod, def_span) = s
        .definition("main", ul, uc)
        .unwrap_or_else(|| panic!("goto-def on `{needle}` use returned nothing"));
    assert_eq!(
        (def_span.start_line, def_span.start_column),
        (decl_l, decl_c),
        "goto-def on `{needle}` must land on its declaration"
    );
    assert_eq!(
        span_text(src, &def_span),
        needle,
        "goto-def landed off the `{needle}` identifier: {def_span:?}"
    );

    let (defid, range) = s
        .prepare_rename("main", ul, uc)
        .unwrap_or_else(|| panic!("prepare_rename on `{needle}` use returned nothing"));
    assert_eq!(
        defid.entity, expected,
        "wrong entity kind resolving `{needle}`"
    );
    assert_eq!(
        span_text(src, &range),
        needle,
        "prepare_rename range must cover the `{needle}` identifier: {range:?}"
    );

    let refs = s.references(defid);
    assert!(
        refs.len() >= 2,
        "find-refs on `{needle}` must include its declaration and use, got {refs:?}"
    );
    for (m, sp) in &refs {
        assert_eq!(
            m, &decl_mod,
            "`{needle}` ref escaped the entry module: {m:?}"
        );
        assert_eq!(
            span_text(src, sp),
            needle,
            "`{needle}` ref span is not pinned to the identifier: {sp:?}"
        );
    }
    let (use_l, use_c) = cursor(src, needle, use_nth, 0);
    assert!(
        refs.iter()
            .any(|(_, sp)| (sp.start_line, sp.start_column) == (decl_l, decl_c)),
        "find-refs on `{needle}` is missing the declaration site: {refs:?}"
    );
    assert!(
        refs.iter()
            .any(|(_, sp)| (sp.start_line, sp.start_column) == (use_l, use_c)),
        "find-refs on `{needle}` is missing the use site: {refs:?}"
    );

    assert_eq!(
        s.rename(defid),
        refs,
        "rename on `{needle}` must equal the references closure"
    );
}

#[test]
fn navigation_on_constructor_type_constant_and_field() {
    let p = Project::new("nav_entities");
    p.write("a.al", NAV_SRC);
    let s = checked_with(&p, NAV_SRC);

    // Value position `chosen = Red`.
    assert_nav(&s, NAV_SRC, "Red", 2, EntityKind::Constructor);
    // Annotation use `c Color`.
    assert_nav(&s, NAV_SRC, "Color", 2, EntityKind::Type);
    // Expression use `LIMIT + 1`.
    assert_nav(&s, NAV_SRC, "LIMIT", 2, EntityKind::Constant);
    // 3rd `label` is `bx.label`; the 2nd is the construction label
    // `Box(label: ...)`, which is not a graph reference.
    assert_nav(&s, NAV_SRC, "label", 3, EntityKind::Field);
}

#[test]
fn constructor_in_match_pattern_is_a_graph_reference() {
    // `Left` appears only in the type declaration and the match arm, never at
    // a value position, so this pins `type_ctor_pattern`'s `record_value_use`.
    let p = Project::new("ctor_pattern_ref");
    let src = "type Side {\n\tLeft\n\tRight\n}\n\
               fn f(x Side) Int { match x { Left -> 1 Right -> 2 } }\n\
               println(f(Right))\n";
    p.write("a.al", src);
    let s = checked_with(&p, src);

    // 2nd `Left` is the match-arm pattern occurrence.
    assert_nav(&s, src, "Left", 2, EntityKind::Constructor);
}

// A program matching al/http/h1's `Parsed` enum exhaustively must check clean
// through `IncrementalSession` exactly as through `al check`. A failure means
// session hydration of the embedded stdlib blob mangled a declaration.
const HTTP_ENTRY: &str = "import al/binary\n\
import al/http/h1.{Done, NeedMore, Bad}\n\
r = match h1.parse_request(binary.from_string('GET / HTTP/1.1\\r\\n\\r\\n'), 0) {\n\
\tDone(_, _, _, _, _, consumed) -> consumed\n\
\tNeedMore -> 0 - 1\n\
\tBad(s) -> s\n\
}\n\
println(r)\n";

#[test]
fn session_checks_http_h1_program_clean() {
    let p = Project::new("http_session");
    p.write("a.al", HTTP_ENTRY);
    checked_with(&p, HTTP_ENTRY);
}

// Two checks in one session: arena truncation between them must not corrupt
// the stdlib types hydrated for the second.
#[test]
fn session_recheck_keeps_stdlib_types_intact() {
    let p = Project::new("http_session_recheck");
    p.write("a.al", HTTP_ENTRY);

    let mut s = checked_with(&p, "println(1)\n");
    let r2 = s.check(&parse(HTTP_ENTRY), Some(&p.dir));
    assert!(
        r2.success(),
        "re-check of an al/http/h1 program in a reused session failed: {:?}",
        r2.diagnostics
    );
}

// One entry hydrates al/http, a later entry in the same session imports
// al/http/h1 directly. h1's hydration must still resolve `Parsed` / `Framing` /
// `Header` to real enum types rather than collapsing them to a builtin.
const HTTP_HELLO_ENTRY: &str = "import al/http\n\
match http.serve('127.0.0.1', 8080, fn(_req) http.text('hi')) {\n\
\tOk(_) -> Nil\n\
\tErr(e) -> println(e)\n\
}\n";

#[test]
fn session_hydrates_h1_after_http_in_earlier_check() {
    let p = Project::new("http_session_order");
    p.write("a.al", HTTP_HELLO_ENTRY);
    p.write("b.al", HTTP_ENTRY);

    // Hydrates http + headers, then h1 directly.
    let mut s = checked_with(&p, HTTP_HELLO_ENTRY);
    let r2 = s.check(&parse(HTTP_ENTRY), Some(&p.dir));
    assert!(
        r2.success(),
        "al/http/h1 program checked after an al/http program in the same \
         session must still see Parsed as an enum: {:?}",
        r2.diagnostics
    );
}

// An entry type whose name collides with a seeded stdlib type overwrites the
// `type_info` entry in place (`IndexMap::insert` keeps the pre-watermark
// index), so truncate-by-length rollback cannot restore it. The env's
// overwrite journal is what fixes that; this pins it.
const SHADOWING_ENTRY: &str = "type Parsed = Result(Int, String)\n\
fn f(x Int) Parsed {\n\
\tOk(x)\n\
}\n\
println(f(1) or 0)\n";

#[test]
fn entry_type_shadowing_stdlib_name_is_undone_on_next_check() {
    let p = Project::new("type_shadow");
    p.write("a.al", SHADOWING_ENTRY);
    p.write("b.al", HTTP_ENTRY);

    let mut s = checked_with(&p, SHADOWING_ENTRY);

    // An entry using the real al/http/h1.Parsed must see the enum, not the
    // dead alias.
    let r2 = s.check(&parse(HTTP_ENTRY), Some(&p.dir));
    assert!(
        r2.success(),
        "h1.Parsed corrupted by an earlier entry's shadowing type alias: {:?}",
        r2.diagnostics
    );

    // And back: the journal restore is itself rolled back cleanly.
    let r3 = s.check(&parse(SHADOWING_ENTRY), Some(&p.dir));
    assert!(
        r3.success(),
        "re-check of shadowing entry failed: {:?}",
        r3.diagnostics
    );
}

/// The unused-import rule keys off the `Qualified` *member* occurrence, not the
/// `Qualifier`. Find-all-references does list the qualifier — see
/// `lsp_handlers::find_refs_on_module_alias_lists_the_qualifier`.
#[test]
fn qualified_member_use_keeps_the_import_live_but_a_bare_import_still_warns() {
    let p = Project::new("qualifier_liveness");
    p.write("c.al", C_SRC);
    p.write("d.al", C_SRC);
    let entry = "import ./c\nimport ./d\nprintln(c.shared())\n";
    p.write("a.al", entry);

    let msgs = unused_msgs(&checked_with(&p, entry));
    assert_no_msg(&msgs, "unused import `c`");
    assert_msg_eq(&msgs, "unused import `d`");
}

/// A local binding shadows an import's qualifier. Scope must be consulted
/// before `imported_qualifiers`, or `b.x` on a parameter named `b` is rejected
/// as "Module './b' has no member 'x'".
#[test]
fn a_local_shadows_an_import_qualifier() {
    let p = Project::new("shadow_qualifier");
    p.write("b.al", "pub fn add(a, b) {\n\ta + b\n}\n");
    p.write(
        "a.al",
        "import ./b\n\
         \n\
         type Point {\n\
         \tPoint(x Int, y Int)\n\
         }\n\
         \n\
         fn f(b Point) Int {\n\
         \tb.x\n\
         }\n\
         \n\
         println(f(Point(41, 1)) + b.add(1, 0))\n",
    );
    // `b.x` reads the parameter's field; `b.add` still reaches the module.
    common::run_project_outputs(&p, "run", "a.al", "42\n");
}

/// `Config(name: 'x')` names the constructor, never the type, so reachability
/// only reaches the type through the structural constructor edge.
#[test]
fn a_used_constructor_keeps_its_type_alive() {
    let p = Project::new("unused_ctor_type");
    let entry = "type Config {\n\tname String\n}\n\npub const config = Config(name: 'x')\nprintln(config.name)\n";
    p.write("a.al", entry);
    let s = checked_with(&p, entry);
    assert_no_msg(&unused_msgs(&s), "unused type `Config`");
}

/// The edge is structural, not a use: a type nothing constructs is still dead.
#[test]
fn a_type_nobody_constructs_is_still_unused() {
    let p = Project::new("unused_ghost_type");
    let entry = "type Ghost {\n\tn Int\n}\n\nprintln(1)\n";
    p.write("a.al", entry);
    let s = checked_with(&p, entry);
    assert_has_msg(&unused_msgs(&s), "unused type `Ghost`");
}

/// A multi-constructor type is alive when *any* of its constructors is used.
#[test]
fn one_used_constructor_is_enough_to_keep_the_type() {
    let p = Project::new("unused_one_ctor");
    let entry = "type Color {\n\tRed\n\tGreen\n}\n\nc = Red\nprintln(c)\n";
    p.write("a.al", entry);
    let s = checked_with(&p, entry);
    assert_no_msg(&unused_msgs(&s), "unused type `Color`");
}

/// `import ./lib.{helper}` whose item is never mentioned again: the binding
/// token in the import list is not a use, so the import is reported unused.
/// Adding a real call unflags it, so the check is live rather than disabled.
#[test]
fn unused_selective_import_item_binding_token_is_not_a_use() {
    let p = Project::new("selective_unused");
    p.write("lib.al", "pub fn helper() Int { 7 }\n");
    let entry = "import ./lib.{helper}\nprintln(1)\n";
    p.write("a.al", entry);
    assert_msg_eq(
        &unused_msgs(&checked_with(&p, entry)),
        "unused import `lib`",
    );

    let p2 = Project::new("selective_used");
    p2.write("lib.al", "pub fn helper() Int { 7 }\n");
    let used = "import ./lib.{helper}\nprintln(helper())\n";
    p2.write("a.al", used);
    assert_no_msg(&unused_msgs(&checked_with(&p2, used)), "unused import");
}

/// The `{helper}` binding token is still a reference site, so rename resolves
/// from it and every rewritten span spells `helper`.
#[test]
fn rename_selective_import_item_rewrites_the_binding_token() {
    let p = Project::new("selective_rename");
    let lib = "pub fn helper() Int { 7 }\n";
    p.write("lib.al", lib);
    let entry = "import ./lib.{helper}\nx = helper()\nprintln(x)\n";
    p.write("a.al", entry);
    let s = checked_with(&p, entry);

    // Cursor on the binding token inside the import list.
    let (l, c) = cursor(entry, "helper", 1, 1);
    let (defid, _) = s
        .prepare_rename("main", l, c)
        .expect("the `{helper}` token resolves to lib's `helper`");
    assert_eq!(defid.entity, EntityKind::Function);

    let spans = s.rename(defid);
    for (m, sp) in &spans {
        let src = if m.last().map(String::as_str) == Some("lib") {
            lib
        } else {
            entry
        };
        assert_eq!(span_text(src, sp), "helper", "at {m:?} {sp:?}");
    }
    assert_eq!(
        spans.len(),
        3,
        "rename must cover the declaration, the import token and the use: {spans:?}"
    );
}
