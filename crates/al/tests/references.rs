//! End-to-end coverage for the workspace reference graph exposed through
//! `IncrementalSession`: cross-module goto-definition, workspace
//! find-references, rename via reverse edges, document/workspace symbols,
//! the `al/*` stdlib import path (alias tracking, cross-module goto-def into
//! a precompiled stdlib declaration, and the unused-import hint),
//! unused-import / dead private-definition hints, and incremental stability
//! of the reverse-edge index (edit B → refs from A still resolve; invalidate
//! → stale reverse edges are dropped).

use al::bytecode::IncrementalSession;
use al::module;
use al::reference::EntityKind;
use al::span::Span;

mod common;
use common::{Project, SessionQueryExt, checked_with, cursor, parse};

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

    // use in entry (A) -> definition in B.
    let (l, c) = cursor(ENTRY, "bridge", 1, 1);
    let (m, span) = s
        .definition("main", l, c)
        .expect("b.bridge resolves to a definition");
    assert_eq!(m.last().map(String::as_str), Some("b"));
    assert!(span.start_line >= 0 && span.start_column >= 0);

    // use in entry (A) -> definition in C (C is two hops: A imports C, B
    // imports C; the occurrence in A still resolves to C's declaration).
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

    // rename is the reverse-edge closure: identical to find-references and
    // spanning every module that mentions the symbol.
    let edits = s.rename(defid);
    assert_eq!(edits, s.references(defid), "rename == references closure");

    let distinct: std::collections::BTreeSet<String> =
        edits.iter().map(|(m, _)| m.join("/")).collect();
    assert!(
        distinct.len() >= 2,
        "rename must rewrite across modules, touched {distinct:?}"
    );
    // Every edit span must be non-degenerate (a real identifier range).
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

    // documentSymbol for module C lists its declaration.
    let doc = s.document_symbols("./c");
    assert!(
        doc.iter()
            .any(|sym| sym.name == "shared" && sym.kind == EntityKind::Function),
        "documentSymbol(./c) missing `shared`: {:?}",
        doc.iter().map(|x| &x.name).collect::<Vec<_>>()
    );

    // documentSymbol for the entry surfaces the import module-aliases,
    // including the `al/array` stdlib import alias.
    let main = s.document_symbols("main");
    assert!(
        main.iter()
            .any(|sym| sym.name == "array" && sym.kind == EntityKind::ModuleAlias),
        "entry documentSymbol missing the `al/array` import alias: {:?}",
        main.iter().map(|x| (&x.name, x.kind)).collect::<Vec<_>>()
    );

    // workspace/symbol substring search finds the user symbol and is
    // case-insensitive.
    assert!(
        s.workspace_symbols("SHAR")
            .iter()
            .any(|sym| sym.name == "shared"),
        "workspace_symbols is not a case-insensitive substring match"
    );
    // Precision: an unrelated query returns nothing.
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

    // Cross-module goto-def into the stdlib: the cursor on `array.length`
    // resolves to the real `length` declaration in `al/array` (`src/std/al/
    // array.al`, `pub fn length` — 0-based line 34, the identifier span).
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
        (34, 7, 13),
        "must land on the real `length` declaration span, got {span:?}"
    );
    // The synthesised stdlib definition and the entry's qualified occurrence
    // share one canonical DefId, so find-references closes over both.
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

    // A *used* stdlib import is not reported unused.
    let used = unused_msgs(&s);
    assert!(
        !used.iter().any(|m| m == "unused import `array`"),
        "a used stdlib import was wrongly reported unused: {used:?}"
    );

    // The unused-import reachability rule reaches stdlib imports: an import
    // whose alias has no recorded qualified/unqualified use is a Hint.
    let p2 = Project::new("stdlib_unused");
    let unused = "import al/array\nprintln(1)\n";
    p2.write("a.al", unused);
    let s2 = checked_with(&p2, unused);
    let hints = unused_msgs(&s2);
    assert!(
        hints.iter().any(|m| m == "unused import `array`"),
        "stdlib import not run through the unused-import rule: {hints:?}"
    );
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

    assert!(
        msgs.iter().any(|m| m.contains("unused import `c`")),
        "missing unused-import hint: {msgs:?}"
    );
    assert!(
        msgs.iter().any(|m| m == "unused function `deadpriv`"),
        "missing dead private-def hint: {msgs:?}"
    );
    // `println` is a used builtin and must never be reported.
    assert!(
        !msgs.iter().any(|m| m.contains("println")),
        "false-positive on a used builtin: {msgs:?}"
    );
}

#[test]
fn incremental_edit_b_keeps_refs_then_invalidate_drops_reverse_edges() {
    let p = project();

    // --- initial: b.bridge resolves from the entry and B declares it.
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

    // --- edit B's body only (bridge kept): refs into B from A still resolve.
    p.write(
        "b.al",
        "import ./c\npub fn bridge() Int { c.shared() + 100 }\n",
    );
    let r = s.check(&parse(ENTRY), Some(&p.dir));
    assert!(r.success, "after B edit: {:?}", r.diagnostics);
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

    // --- invalidate: B drops `bridge` and the entry stops referencing it.
    // The wholesale graph rebuild must not leave a dangling reverse edge for
    // the now-gone definition.
    p.write("b.al", "pub fn other() Int { 7 }\n");
    let entry2 = "import ./b\nprintln(b.other())\n";
    p.write("a.al", entry2);
    let r = s.check(&parse(entry2), Some(&p.dir));
    assert!(r.success, "after B invalidation: {:?}", r.diagnostics);
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
    // Local binders (`let`, params, match/destructure) are recorded in the
    // graph as `EntityKind::Value` so goto-def / find-refs / rename resolve on
    // them — but they must NOT flood the documentSymbol outline or the
    // workspace/symbol picker, which list a module's structural declarations
    // only. The filter touches only the symbol projection: resolution on the
    // locals themselves is unaffected.
    let p = Project::new("symsurface");
    let entry =
        "fn calc(base Int) Int {\n\tscaled = base * 2\n\tscaled + 1\n}\nx = calc(10)\nprintln(x)\n";
    p.write("a.al", entry);
    let s = checked_with(&p, entry);

    // documentSymbol lists the top-level fn …
    let doc = s.document_symbols("main");
    assert!(
        doc.iter()
            .any(|sym| sym.name == "calc" && sym.kind == EntityKind::Function),
        "documentSymbol(main) missing top-level `calc`: {:?}",
        doc.iter().map(|x| (&x.name, x.kind)).collect::<Vec<_>>()
    );
    // … but NOT the local binder, the param, or any other `Value`.
    assert!(
        !doc.iter().any(|sym| sym.kind == EntityKind::Value),
        "documentSymbol must not surface local `Value` binders: {:?}",
        doc.iter().map(|x| (&x.name, x.kind)).collect::<Vec<_>>()
    );
    assert!(
        !doc.iter()
            .any(|sym| sym.name == "scaled" || sym.name == "base"),
        "documentSymbol leaked a local binder/param: {:?}",
        doc.iter().map(|x| &x.name).collect::<Vec<_>>()
    );

    // workspace/symbol over a local binder's name returns nothing.
    assert!(
        s.workspace_symbols("scaled").is_empty(),
        "workspace_symbols surfaced a local binder: {:?}",
        s.workspace_symbols("scaled")
            .iter()
            .map(|x| &x.name)
            .collect::<Vec<_>>()
    );

    // Resolution on the local is UNAFFECTED — the filter never touches the
    // forward index / `definitions()` scan. goto-def on the use of `scaled`
    // (its 2nd occurrence) lands on the binder, prepare_rename yields its
    // `Value` DefId, and find-refs closes over the binder and the use.
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

// ---------------------------------------------------------------------------
// Aliased selective import `{X as Y}` — rename must keep X's and Y's rename
// classes separate.
// ---------------------------------------------------------------------------

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
    // `import ./lib.{original as alias}` then uses of `alias`. Renaming the
    // imported `original` must rewrite only spans that spell `original` (its
    // declaration in lib + the `original` part of the import). Rewriting the
    // `alias` binder or any `alias` use would yield `{renamed as renamed}` and
    // `renamed()` — name capture that no longer compiles.
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
    // It must still rewrite the real declaration and the `original` half of the
    // import (two distinct `original` sites).
    assert_eq!(
        spans.len(),
        2,
        "rename of `original` should cover its declaration and the import's \
         `original` token only: {spans:?}"
    );
}

#[test]
fn rename_local_alias_does_not_touch_imported_symbol() {
    // The reverse direction: renaming the local alias `alias` (clicking a use
    // of it) must rewrite only `alias` occurrences, all in the entry module —
    // never the imported `original` or its declaration in lib.
    let p = alias_project();
    let s = checked_with(&p, ALIAS_MAIN);

    // 2nd occurrence of `alias` is the first use (`x = alias()`).
    let (l, c) = cursor(ALIAS_MAIN, "alias", 2, 1);

    // goto-def on the alias use still chains through to the imported symbol's
    // real declaration in lib (the rename class is separate, but navigation is
    // not), so the alias remains a useful local handle on `original`.
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
    // The alias binder plus its two uses.
    assert_eq!(
        spans.len(),
        3,
        "alias rename should cover its binder and both uses: {spans:?}"
    );
}

// Two plain qualified imports where only the second is used: the unused first
// import must still be flagged. The `d.helper()` qualified use resolves into
// module `d` only, so it must not keep the genuinely-unused `import ./c` alive
// (the regression: `has_real_use` formerly kept any qualified import alive on
// any cross-module qualified use, suppressing the `c` hint).
#[test]
fn unused_one_of_two_plain_qualified_imports_is_flagged() {
    let p = Project::new("twoimp");
    p.write("c.al", "pub fn shared() Int { 42 }\n");
    p.write("d.al", "pub fn helper() Int { 7 }\n");
    let entry = "import ./c\nimport ./d\nx = d.helper()\nprintln(x)\n";
    p.write("a.al", entry);

    let s = checked_with(&p, entry);
    let msgs = unused_msgs(&s);

    assert!(
        msgs.iter().any(|m| m == "unused import `c`"),
        "the unused `./c` import must be flagged even though `./d` is used: {msgs:?}"
    );
    assert!(
        !msgs.iter().any(|m| m == "unused import `d`"),
        "the used `./d` import must not be flagged: {msgs:?}"
    );
}

// ---------------------------------------------------------------------------
// Session navigation (goto-def / prepare_rename / find-refs / rename) on the
// Constructor / Type / Constant / Field entity kinds. The query layer was
// previously exercised only for Function / Value / ModuleAlias; these are the
// remaining four `EntityKind`s. Each is driven from a real *use* site:
//   - a constructor at a VALUE position (`chosen = Red`) — the match-pattern
//     position is covered by `constructor_in_match_pattern_is_a_graph_reference`;
//   - a type at an annotation use (`c Color`);
//   - a constant inside an expression (`LIMIT + 1`);
//   - a record field projection (`bx.label`).
// ---------------------------------------------------------------------------

const NAV_SRC: &str = "type Color { Red Green Blue }\n\
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
/// always the declaration; `use_nth` (1-based) selects the *use* site to click.
/// Asserts goto-def lands exactly on the declaring identifier, prepare_rename
/// reports `expected` as the entity kind, and find-references (== rename)
/// closes over the declaration and the use with every span pinned to the
/// identifier and confined to the entry module.
fn assert_nav(
    s: &IncrementalSession,
    src: &str,
    needle: &str,
    use_nth: usize,
    expected: EntityKind,
) {
    let (decl_l, decl_c) = cursor(src, needle, 1, 0);
    let (ul, uc) = cursor(src, needle, use_nth, 1);

    // goto-def from the use site resolves to the declaring identifier.
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

    // prepare_rename yields the DefId tagged with the entity kind and the span
    // of the clicked identifier.
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

    // find-references closes over the declaration plus the use; every span is
    // an identifier-precise range inside the entry module.
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

    // rename is exactly the find-references closure.
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

    // Constructor at a value position (`chosen = Red`, the 2nd `Red`): decl is
    // the variant in `type Color { Red ... }`.
    assert_nav(&s, NAV_SRC, "Red", 2, EntityKind::Constructor);

    // Type at an annotation use (`c Color`, the 2nd `Color`).
    assert_nav(&s, NAV_SRC, "Color", 2, EntityKind::Type);

    // Constant in an expression (`LIMIT + 1`, the 2nd `LIMIT`).
    assert_nav(&s, NAV_SRC, "LIMIT", 2, EntityKind::Constant);

    // Record field projection (`bx.label`, the 3rd `label`): the 1st is the
    // field declaration, the 2nd is the construction label `Box(label: ...)`
    // (not recorded as a graph reference), the 3rd is the access.
    assert_nav(&s, NAV_SRC, "label", 3, EntityKind::Field);
}

#[test]
fn constructor_in_match_pattern_is_a_graph_reference() {
    // A constructor used in a match arm pattern must be recorded as a graph
    // reference so find-references / rename reach it. `Left` here appears ONLY
    // in the type declaration and the match arm — never at a value position —
    // so this pins `type_ctor_pattern`'s `record_value_use` call directly.
    let p = Project::new("ctor_pattern_ref");
    let src = "type Side { Left Right }\n\
               fn f(x Side) Int { match x { Left -> 1 Right -> 2 } }\n\
               println(f(Right))\n";
    p.write("a.al", src);
    let s = checked_with(&p, src);

    // 2nd `Left` is the match-arm pattern occurrence.
    assert_nav(&s, src, "Left", 2, EntityKind::Constructor);
}

// --- Embedded-stdlib type fidelity through the session -------------------------

// A program that imports al/http/h1 and matches its `Parsed` enum exhaustively
// must check clean through `IncrementalSession` (the LSP's check path for
// non-stdlib files) exactly like it does through `al check`: both resolve the
// import against the same precompiled stdlib. A mismatch here means the
// session's hydration of the embedded blob lost or mangled a declaration.
const HTTP_ENTRY: &str = "import al/binary\n\
import al/http/h1.{Done, NeedMore, Bad}\n\
r = match h1.parse_request(binary.from_string('GET / HTTP/1.1\\r\\n\\r\\n'), 0) {\n\
\tDone(_, _, version, _, _) -> version\n\
\tNeedMore -> 0 - 1\n\
\tBad(s) -> s\n\
}\n\
println(r)\n";

#[test]
fn session_checks_http_h1_program_clean() {
    let p = Project::new("http_session");
    p.write("a.al", HTTP_ENTRY);
    // parse_request : fn(Binary, Int) Parsed comes from the embedded stdlib.
    checked_with(&p, HTTP_ENTRY);
}

// Same session, two checks: a trivial entry first, then the al/http/h1
// program. The stdlib types hydrated for the second entry must not be
// corrupted by arena truncation between checks.
#[test]
fn session_recheck_keeps_stdlib_types_intact() {
    let p = Project::new("http_session_recheck");
    p.write("a.al", HTTP_ENTRY);

    // First check: a trivial program (no http imports at all).
    let mut s = checked_with(&p, "println(1)\n");

    // Second check: the http program. This is the LSP's reality — one shared
    // session checks many different entry files in sequence.
    let r2 = s.check(&parse(HTTP_ENTRY), Some(&p.dir));
    assert!(
        r2.success,
        "re-check of an al/http/h1 program in a reused session failed: {:?}",
        r2.diagnostics
    );
}

// The LSP-workspace reality that broke in the field: one entry hydrates
// al/http (examples/http_hello.al), a later entry in the same session imports
// al/http/h1 directly. h1's hydration must still resolve `Parsed` /
// `Framing` / `Header` to real enum types — not collapse them to a builtin —
// even though al/http and al/http/headers were hydrated by an earlier check.
const HTTP_HELLO_ENTRY: &str = "import al/http\n\
http.serve('127.0.0.1', 8080, fn(_req) http.text('hi')) or e -> println(e)\n";

#[test]
fn session_hydrates_h1_after_http_in_earlier_check() {
    let p = Project::new("http_session_order");
    p.write("a.al", HTTP_HELLO_ENTRY);
    p.write("b.al", HTTP_ENTRY);

    // First check: a program that imports al/http (hydrates http + headers).
    let mut s = checked_with(&p, HTTP_HELLO_ENTRY);

    // Second check: a program that imports al/http/h1 directly.
    let r2 = s.check(&parse(HTTP_ENTRY), Some(&p.dir));
    assert!(
        r2.success,
        "al/http/h1 program checked after an al/http program in the same \
         session must still see Parsed as an enum: {:?}",
        r2.diagnostics
    );
}

// The shadowed-stdlib-type bug: an entry file declaring a type
// whose name collides with a seeded stdlib type — calculator.al's
// `type Parsed = Result(...)` vs al/http/h1's `Parsed` enum — overwrote the
// seeded `type_info` entry IN PLACE (IndexMap::insert keeps the existing,
// pre-watermark index), so the next check's truncate-by-length rollback could
// not restore it: every later check in the session saw the (truncated, now
// garbage) alias instead of the enum, breaking exhaustiveness and hover for
// that stdlib type. The env's overwrite journal is what fixes this; this test
// pins it.
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

    // Check 1: the shadowing entry itself is fine.
    let mut s = checked_with(&p, SHADOWING_ENTRY);

    // Check 2: a different entry that uses the REAL al/http/h1.Parsed must see
    // the enum (3 variants, exhaustive match), not the dead alias.
    let r2 = s.check(&parse(HTTP_ENTRY), Some(&p.dir));
    assert!(
        r2.success,
        "h1.Parsed corrupted by an earlier entry's shadowing type alias: {:?}",
        r2.diagnostics
    );

    // And back: re-checking the shadowing entry still works (the journal
    // restore is itself rolled back cleanly).
    let r3 = s.check(&parse(SHADOWING_ENTRY), Some(&p.dir));
    assert!(
        r3.success,
        "re-check of shadowing entry failed: {:?}",
        r3.diagnostics
    );
}
