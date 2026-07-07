//! Handler-layer coverage for the LSP server. The reference-graph rewrite's
//! regressions all hid because the existing `references.rs` suite drives the
//! `IncrementalSession` query API directly, never the `LspServer` handlers the
//! editor actually calls. These tests close that gap: they boot a real
//! `LspServer`, open a document through the same path `textDocument/didOpen`
//! takes, and assert on the JSON each query *returns* — the seam the handlers
//! were refactored to expose (`hover_response` / `definition_response` /
//! `references_response` / `rename_response` / `prepare_rename_response`).

use al::lsp::{LspServer, new_server};
use serde_json::{Value as Json, json};

mod common;
use common::{Project, cursor};

/// `file://`-form URI for `name` inside `p`, matching the non-percent-decoded
/// shape the server's own path<->uri round-trip produces for repo-local files.
fn uri_of(p: &Project, name: &str) -> String {
    format!("file://{}", p.dir.join(name).display())
}

/// `textDocument`/`position` params for a position-based request.
fn pos(uri: &str, line: i32, col: i32) -> Json {
    json!({
        "textDocument": { "uri": uri },
        "position": { "line": line, "character": col },
    })
}

/// Boot a server rooted at `p`, open `name` (with contents `src`) as a client
/// tab, and hand back the server plus the document URI ready to query.
fn open(p: &Project, name: &str, src: &str) -> (LspServer, String) {
    let mut s = new_server();
    s.add_workspace_root(p.dir.clone());
    let uri = uri_of(p, name);
    s.open_document(&uri, src);
    (s, uri)
}

/// Boot a server over a fresh single-file project (`src` written as `a.al`) and
/// open it. The `Project` is returned so its temp dir outlives the server.
fn open_single(tag: &str, src: &str) -> (Project, LspServer, String) {
    let p = Project::new(tag);
    p.write("a.al", src);
    let (s, uri) = open(&p, "a.al", src);
    (p, s, uri)
}

const SRC: &str = "pub fn greet() Int { 7 }\nx = greet()\nprintln(x)\n";

/// The refactored handlers compute and return their response JSON; the thin
/// wrappers only forward it to the wire. This drives every position query
/// through the seam and checks the happy-path shapes plus the "no result"
/// convention (`Json::Null`, wire-equivalent to the old `send_null_response`).
#[test]
fn seam_round_trips_position_queries() {
    let (_p, mut s, uri) = open_single("seam", SRC);

    // hover on the declaration name -> a markdown block naming the symbol.
    let (l, c) = cursor(SRC, "greet", 1, 1);
    let hov = s.hover_response(&pos(&uri, l, c));
    let md = hov["contents"]["value"]
        .as_str()
        .expect("hover returns markdown contents");
    assert!(md.contains("greet"), "hover should name the symbol: {md:?}");

    // goto-def on the *use* of greet -> the declaration's location (line 0).
    let (l, c) = cursor(SRC, "greet", 2, 1);
    let def = s.definition_response(&pos(&uri, l, c));
    assert_eq!(
        def["uri"].as_str(),
        Some(uri.as_str()),
        "definition resolves in the same file: {def:?}"
    );
    assert_eq!(def["range"]["start"]["line"].as_i64(), Some(0));

    // find-refs on the declaration -> a JSON *array* (the decl plus its one
    // use), never null when a definition resolves there.
    let (l, c) = cursor(SRC, "greet", 1, 1);
    let refs = s.references_response(&pos(&uri, l, c));
    let arr = refs.as_array().expect("references is an array");
    assert!(arr.len() >= 2, "expected decl + use, got {arr:?}");

    // "no result" answers null: both an empty position payload and a position
    // sitting on nothing collapse to Json::Null.
    assert!(s.definition_response(&json!({})).is_null());
    assert!(s.hover_response(&pos(&uri, 99, 99)).is_null());

    // the session accessor is wired for an open non-stdlib document (Bug 1
    // joins the inferred type onto hover through it).
    assert!(s.session_for(&uri).is_some());

    // rename / prepareRename are reachable through the seam and return the
    // declared shapes; their *behaviour* is pinned by the Bug-4 tests.
    let _ = s.prepare_rename_response(&pos(&uri, 0, 7));
    let _ = s.rename_response(&json!({
        "textDocument": { "uri": uri },
        "position": { "line": 0, "character": 7 },
        "newName": "greet2",
    }));
}

const TYPED: &str =
    "pub fn answer() Int {\n  base = 42\n  base\n}\npub fn run() Int {\n  answer()\n}\n";

/// Bug 1: the hover handler must surface the *inferred type*, not only the
/// entity kind. Pre-fix `hover_response` rendered just `name` + `*kind*` and
/// the type-join path (`session_for(uri)` -> `IncrementalSession::hover`) was
/// orphaned — no caller. The markdown must now read `name : <Type>` at a local
/// binder, at a *use* of it, and carry the function type at a call site.
#[test]
fn hover_markdown_includes_inferred_type() {
    let (_p, mut s, uri) = open_single("hovertype", TYPED);

    let md = |s: &mut LspServer, l: i32, c: i32| -> String {
        s.hover_response(&pos(&uri, l, c))["contents"]["value"]
            .as_str()
            .unwrap_or("")
            .to_string()
    };

    // local binder `base = 42` -> the joined type, not just the kind.
    let (l, c) = cursor(TYPED, "base", 1, 1);
    let at_binder = md(&mut s, l, c);
    assert!(
        at_binder.contains("base Int"),
        "binder hover must show the inferred type: {at_binder:?}"
    );

    // a later *use* of `base` -> still typed.
    let (l, c) = cursor(TYPED, "base", 2, 1);
    let at_use = md(&mut s, l, c);
    assert!(
        at_use.contains("base Int"),
        "use-site hover must show the inferred type: {at_use:?}"
    );

    // a call of the top-level fn -> its function type joins in (independent of
    // the local-def emission, so this pins the type join even on graph defs).
    let (l, c) = cursor(TYPED, "answer", 2, 1);
    let at_call = md(&mut s, l, c);
    assert!(
        at_call.contains("answer fn() Int"),
        "call-site hover must show the function type: {at_call:?}"
    );
}

// ============================================================================
// Bug 4 — rename refused on clean user defs.
//
// `analyze_text` only ran the session check (which builds the workspace graph
// and latches `entry_uri`) inside its `if !has_errors` block, so a buffer that
// carried a parse error never produced a session at all: `ensure_entry`
// returned false and *every* position query — hover, goto-def, find-refs and,
// most visibly, rename — short-circuited to null before the graph was ever
// consulted. The cruel part is that the symbol being renamed is itself
// perfectly well-formed; the user is merely mid-edit somewhere else in the
// file (an unterminated expression, a half-typed line), and the editor reports
// "The element can't be renamed" for a clean, untouched top-level fn or local.
//
// Each test below opens a buffer whose declarations are clean but which ends in
// a syntax error, and asserts the clean def is still renamable — fail-before
// (null), pass-after. A genuinely error-free buffer was always renamable, so
// it would not distinguish the fix.
// ============================================================================

/// `helper` (decl + two call sites on lines 0–2) is well-formed; the trailing
/// line is a deliberate parse error standing in for an in-progress edit. The
/// recovering parser drops only that line, so the graph still spells `helper`.
const BUG4_FN_SRC: &str =
    "fn helper() Int { 1 }\nx = helper()\ny = helper()\nprintln(x + y)\nbroken @#$\n";

#[test]
fn bug4_prepare_rename_top_level_fn_yields_range_and_placeholder() {
    let (_p, mut s, uri) = open_single("b4_prepare_fn", BUG4_FN_SRC);

    // Cursor inside the `helper` declaration name.
    let (l, c) = cursor(BUG4_FN_SRC, "helper", 1, 1);
    let resp = s.prepare_rename_response(&pos(&uri, l, c));

    assert!(
        resp.get("range").is_some() && resp.get("placeholder").is_some(),
        "prepareRename on a well-formed top-level fn must return \
         {{range, placeholder}} even when the buffer has a syntax error \
         elsewhere, got {resp:?}"
    );
    assert_eq!(
        resp["placeholder"],
        json!("helper"),
        "placeholder must be the current name"
    );
    assert_eq!(resp["range"]["start"]["line"].as_i64(), Some(0));
    assert!(
        resp["range"]["end"]["character"].as_i64() > resp["range"]["start"]["character"].as_i64(),
        "range must be a non-degenerate identifier span: {resp:?}"
    );
}

#[test]
fn bug4_prepare_rename_local_binding_yields_range() {
    // Needs bug2 (locals recorded as graph definitions). Cursor on a *use* of a
    // function-local binding; prepareRename must follow it to the binder — and
    // still must, with an unrelated parse error sitting at the end of the file.
    let src = "pub fn run() Int {\n  total = 41\n  total + 1\n}\nprintln(run())\nbroken @#$\n";
    let (_p, mut s, uri) = open_single("b4_prepare_local", src);

    // Second occurrence of `total` is its use in `total + 1`.
    let (l, c) = cursor(src, "total", 2, 1);
    let resp = s.prepare_rename_response(&pos(&uri, l, c));

    assert!(
        resp.get("range").is_some(),
        "prepareRename on a local binding use must return a range, got {resp:?}"
    );
    assert_eq!(
        resp["placeholder"],
        json!("total"),
        "placeholder must be the local's name"
    );
}

#[test]
fn bug4_rename_top_level_fn_rewrites_def_and_all_refs() {
    let (_p, mut s, uri) = open_single("b4_rename_fn", BUG4_FN_SRC);

    let (l, c) = cursor(BUG4_FN_SRC, "helper", 1, 1);
    let mut params = pos(&uri, l, c);
    params["newName"] = json!("renamed");

    let resp = s
        .rename_response(&params)
        .expect("rename of a user-defined fn must be allowed, not refused");
    let changes = resp
        .get("changes")
        .and_then(Json::as_object)
        .expect("a WorkspaceEdit with a `changes` map");
    let edits = changes
        .get(&uri)
        .and_then(Json::as_array)
        .expect("edits keyed by the open file's uri");

    // Declaration + the two call sites = three rewrites, every one -> "renamed".
    assert_eq!(
        edits.len(),
        3,
        "rename must rewrite the declaration and both uses, got {edits:?}"
    );
    assert!(
        edits.iter().all(|e| e["newText"] == json!("renamed")),
        "every edit must substitute the new name: {edits:?}"
    );
    let lines: std::collections::BTreeSet<i64> = edits
        .iter()
        .filter_map(|e| e["range"]["start"]["line"].as_i64())
        .collect();
    assert_eq!(
        lines,
        [0, 1, 2].into_iter().collect(),
        "decl line + both use lines must each be rewritten, got {lines:?}"
    );
}

// ============================================================================
// Bug 2 — goto-def / find-refs dead on locals.
//
// Before the fix the four local-binder sites (`let` binding, `or`-receiver,
// fn parameter, pattern binder) recorded their *use* occurrences but never an
// `add_definition` for the binder, so `ReferenceGraph::definition(target)`
// returned `None` and `definition_at` / `references_to` (which both resolve
// through it) answered nothing. These pin the handler-layer behaviour for a
// local variable, a function parameter, find-references, and shadowing.
// ============================================================================

const BUG2_SRC: &str = "pub fn add(lhs Int, rhs Int) Int {\n\
\x20 lhs + rhs\n\
}\n\
pub fn run() Int {\n\
\x20 total = add(1, 2)\n\
\x20 total + 3\n\
}\n\
pub fn shadow() Int {\n\
\x20 x = 1\n\
\x20 f = fn(x Int) x + 1\n\
\x20 x + f(2)\n\
}\n\
println(run() + shadow())\n";

fn start(result: &Json) -> (i64, i64) {
    let s = &result["range"]["start"];
    (
        s["line"]
            .as_i64()
            .unwrap_or_else(|| panic!("expected range.start.line, got {result}")),
        s["character"]
            .as_i64()
            .unwrap_or_else(|| panic!("expected range.start.character, got {result}")),
    )
}

/// Open `BUG2_SRC` in a project named `tag`, put the cursor on the `use_nth`
/// occurrence of `needle`, and assert goto-def resolves to the `binder_nth`
/// occurrence (the binder's identifier span) in the same file.
fn assert_goto_def_lands(tag: &str, needle: &str, use_nth: usize, binder_nth: usize) {
    let (_p, mut s, uri) = open_single(tag, BUG2_SRC);

    let (l, c) = cursor(BUG2_SRC, needle, use_nth, 1);
    let def = s.definition_response(&pos(&uri, l, c));
    assert!(
        !def.is_null(),
        "goto-def on a `{needle}` use must resolve, got null"
    );
    assert_eq!(
        def["uri"].as_str(),
        Some(uri.as_str()),
        "the binder is in the same file"
    );
    let binder = cursor(BUG2_SRC, needle, binder_nth, 0);
    assert_eq!(
        start(&def),
        (binder.0 as i64, binder.1 as i64),
        "goto-def must land exactly on the binder's identifier span"
    );
}

#[test]
fn bug2_goto_def_local_var_use_lands_on_binder() {
    // The `total` in `total + 3` must jump to the `total = add(1, 2)` binder.
    assert_goto_def_lands("b2_local_def", "total", 2, 1);
}

#[test]
fn bug2_goto_def_param_use_lands_on_parameter() {
    // The `lhs` in `lhs + rhs` must jump to the `lhs` parameter of `add`.
    assert_goto_def_lands("b2_param_def", "lhs", 2, 1);
}

#[test]
fn bug2_find_refs_on_local_binder_includes_uses() {
    let (_p, mut s, uri) = open_single("b2_local_refs", BUG2_SRC);

    // Cursor on the `total` binder; find-refs must surface the use below it.
    let (l, c) = cursor(BUG2_SRC, "total", 1, 1);
    let refs = s.references_response(&json!({
        "textDocument": { "uri": uri },
        "position": { "line": l, "character": c },
        "context": { "includeDeclaration": true },
    }));
    let arr = refs
        .as_array()
        .unwrap_or_else(|| panic!("references must be an array, got {refs}"));

    let use_line = cursor(BUG2_SRC, "total", 2, 0).0 as i64;
    assert!(
        arr.iter().any(|r| start(r).0 == use_line),
        "find-refs on a local binder must include its use at line {use_line}, got {refs}"
    );
    let binder_line = cursor(BUG2_SRC, "total", 1, 0).0 as i64;
    assert!(
        arr.iter().any(|r| start(r).0 == binder_line),
        "with includeDeclaration the binder itself is listed, got {refs}"
    );
}

#[test]
fn bug2_shadowed_binding_resolves_inner_then_outer() {
    let (_p, mut s, uri) = open_single("b2_shadow", BUG2_SRC);

    // `shadow`: outer `x = 1`, then `f = fn(x Int) x + 1` whose body's `x` is
    // the lambda parameter, then `x + f(2)` whose `x` is the outer binding.
    // `x` is a single-column identifier, so the cursor sits *on* it (`into` 0).
    let (il, ic) = cursor(BUG2_SRC, "x", 3, 0); // `x + 1` inside the lambda
    let (ol, oc) = cursor(BUG2_SRC, "x", 4, 0); // `x` in `x + f(2)`
    let inner = s.definition_response(&pos(&uri, il, ic));
    let outer = s.definition_response(&pos(&uri, ol, oc));
    assert!(
        !inner.is_null() && !outer.is_null(),
        "both `x` uses must resolve (inner={inner}, outer={outer})"
    );

    let param_line = cursor(BUG2_SRC, "x", 2, 0).0 as i64; // `fn(x Int)`
    let outer_line = cursor(BUG2_SRC, "x", 1, 0).0 as i64; // `x = 1`
    assert_eq!(
        start(&inner).0,
        param_line,
        "the lambda-body `x` resolves to the lambda parameter, got {inner}"
    );
    assert_eq!(
        start(&outer).0,
        outer_line,
        "the trailing `x` resolves to the outer binding, got {outer}"
    );
    assert_ne!(
        start(&inner).0,
        start(&outer).0,
        "an inner shadow and its outer must be distinct definitions"
    );
}

// ============================================================================
// Bug 3 — every token of an `import a/b` was clickable, all to no effect.
//
// Only the final module-name segment should be a goto-def target, and it must
// open the *imported* module: the sibling file for `import ./x`, nothing for an
// embedded `al/*` (there is no on-disk file to open — never fabricate one). The
// `import` keyword and any non-final path segment resolve to nothing.
// ============================================================================

const BUG3_HELPER: &str = "pub fn greet() String {\n\x20 'hi'\n}\n";
const BUG3_MAIN: &str = "import ./helper\n\
import al/string\n\
x = helper.greet()\n\
y = string.length(x)\n\
println(y)\n";

/// Boot a server over a project carrying both `BUG3_HELPER` (as `helper.al`)
/// and `BUG3_MAIN` (as `main.al`), open `main.al`, and return the server + its
/// URI ready to query the import declarations.
fn bug3_project() -> (Project, LspServer, String) {
    let p = Project::new("b3_import");
    p.write("helper.al", BUG3_HELPER);
    p.write("main.al", BUG3_MAIN);
    let (s, uri) = open(&p, "main.al", BUG3_MAIN);
    (p, s, uri)
}

#[test]
fn bug3_goto_def_on_import_segment_opens_the_module() {
    let (_p, mut s, uri) = bug3_project();

    // `import ./helper` — clicking the `helper` segment opens helper.al, NOT
    // the importing line itself.
    let (l, c) = cursor(BUG3_MAIN, "helper", 1, 1); // 1st `helper` = the path
    let def = s.definition_response(&pos(&uri, l, c));
    let target = def["uri"]
        .as_str()
        .unwrap_or_else(|| panic!("goto-def on an import segment must open a file, got {def}"));
    assert!(
        target.ends_with("helper.al"),
        "`import ./helper` must jump to helper.al, got {target}"
    );

    // `import al/string` is embedded stdlib: there is no editable file, so the
    // segment resolves to nothing rather than to a fabricated location.
    let (l, c) = cursor(BUG3_MAIN, "string", 1, 1); // 1st `string` = the path
    let def = s.definition_response(&pos(&uri, l, c));
    assert!(
        def.is_null(),
        "goto-def on an embedded `al/*` import segment must be null, got {def}"
    );
}

#[test]
fn bug3_import_keyword_and_non_final_segment_are_not_clickable() {
    let (_p, mut s, uri) = bug3_project();

    // `import a/b` is not a rename target anywhere. Bug 3 narrowed the `Import`
    // *occurrence* to the final segment only, but the alias `Definition` still
    // spans the whole declaration — so the `import` keyword, the `./` and `/`
    // separators, and any non-final segment fall solely under that wide span.
    // Pre-fix `prepareRename` there returned the entire-line range and `rename`
    // executed, silently overwriting `import a/b` with the new name and
    // corrupting the file; only `definition_response` had the compensating
    // `ModuleAlias` suppression, so this very test (def-only) shipped green
    // while the rename surface stayed broken. Renaming would also orphan every
    // qualified `q.member` use (it targets the remote member, not the alias).
    // So at each such position all three handlers must decline: goto-def null,
    // prepareRename null, and rename never an edit (null, or refused with Err).
    let refused = |s: &mut LspServer, l: i32, c: i32| {
        assert!(
            s.definition_response(&pos(&uri, l, c)).is_null(),
            "goto-def must be null at import-decl position ({l},{c})"
        );
        assert!(
            s.prepare_rename_response(&pos(&uri, l, c)).is_null(),
            "prepareRename must refuse (null) at import-decl position ({l},{c}), \
             not offer the whole-declaration span"
        );
        let mut params = pos(&uri, l, c);
        params["newName"] = json!("renamed");
        if let Ok(edit) = s.rename_response(&params) {
            assert!(
                edit.is_null(),
                "rename at import-decl position ({l},{c}) must not emit a \
                 WorkspaceEdit (it would overwrite the `import` line), got {edit}"
            );
        }
    };

    // `import ./helper`: the keyword, then the `./` separator chars.
    let (l, c) = cursor(BUG3_MAIN, "import", 1, 1);
    refused(&mut s, l, c);
    let (l, c) = cursor(BUG3_MAIN, "./helper", 1, 0); // the `.`
    refused(&mut s, l, c);
    let (l, c) = cursor(BUG3_MAIN, "/helper", 1, 0); // the `/`
    refused(&mut s, l, c);

    // `import al/string`: the keyword, the non-final `al` segment, the `/`.
    let (l, c) = cursor(BUG3_MAIN, "import", 2, 1);
    refused(&mut s, l, c);
    let (l, c) = cursor(BUG3_MAIN, "al/string", 1, 0); // the `al`
    refused(&mut s, l, c);
    let (l, c) = cursor(BUG3_MAIN, "/string", 1, 0); // the `/`
    refused(&mut s, l, c);

    // The *final* segment is the one navigable token (it opens the imported
    // file, asserted in `bug3_goto_def_on_import_segment_opens_the_module`) —
    // but navigable is not renamable: you rename the file, not the import. Both
    // the resolvable sibling (`helper`) and the embedded-stdlib (`string`)
    // final segments must still refuse prepareRename and rename.
    for (l, c) in [
        cursor(BUG3_MAIN, "helper", 1, 1),
        cursor(BUG3_MAIN, "string", 1, 1),
    ] {
        assert!(
            s.prepare_rename_response(&pos(&uri, l, c)).is_null(),
            "prepareRename must refuse the final import segment ({l},{c})"
        );
        let mut params = pos(&uri, l, c);
        params["newName"] = json!("renamed");
        if let Ok(edit) = s.rename_response(&params) {
            assert!(
                edit.is_null(),
                "rename must not edit the final import segment ({l},{c}), got {edit}"
            );
        }
    }
}

// ============================================================================
// Symbol surface — local bindings excluded from documentSymbol / workspace
// symbol, yet still resolvable.
//
// Bug 2 is the first code to record `EntityKind::Value` binders (locals,
// parameters, pattern binders) as graph definitions, so goto-def / find-refs /
// rename now work on them. But the two symbol *projections* — `documentSymbol`
// (the editor outline) and `workspace/symbol` (the Cmd-T picker) — must stay a
// list of a module's structural declarations; a flood of every local in every
// function body is noise. Both handler loops therefore filter on
// `Definition::is_symbol_listable`. Dropping that one filter keeps every other
// test in the suite green — they all probe the result with `.any()` / `.find()`
// — while wrecking the outline, exactly the silent-untested-regression class
// these handler-layer tests exist to catch. The two below assert at the handler
// layer that the structural decls are listed, the `Value` binders are not, and
// that the exclusion is *projection-only*: a local kept out of the symbol
// surface is still a resolvable definition (find-refs / rename on locals are
// pinned by the Bug-2 / Bug-4 tests; this guards that the symbol filter never
// reached into resolution).
// ============================================================================

const SYMSURFACE_SRC: &str = "pub fn scale(factor Int) Int {\n\
\x20 doubled = factor * 2\n\
\x20 doubled + 1\n\
}\n\
pub fn run() Int {\n\
\x20 scale(10)\n\
}\n\
println(run())\n";

#[test]
fn document_symbol_lists_top_level_fns_not_locals() {
    let (_p, mut s, uri) = open_single("symsurface_doc", SYMSURFACE_SRC);

    let resp = s.document_symbol_response(&json!({ "textDocument": { "uri": uri } }));
    let syms = resp
        .as_array()
        .unwrap_or_else(|| panic!("documentSymbol must be an array, got {resp}"));
    let names: Vec<&str> = syms.iter().filter_map(|x| x["name"].as_str()).collect();

    // Both top-level fns are listed, as `SymbolKind::Function` (12).
    for fname in ["scale", "run"] {
        assert!(
            syms.iter()
                .any(|x| x["name"] == json!(fname) && x["kind"] == json!(12)),
            "documentSymbol must list top-level fn `{fname}` (kind 12), got {names:?}"
        );
    }

    // Neither the local binder nor the parameter leaks in — not by name, and
    // not as the `SymbolKind::Value` (13) every `EntityKind::Value` maps to.
    for local in ["factor", "doubled"] {
        assert!(
            !names.contains(&local),
            "documentSymbol leaked the local `{local}`: {names:?}"
        );
    }
    assert!(
        !syms.iter().any(|x| x["kind"] == json!(13)),
        "documentSymbol must not surface any `Value` (kind 13) binder: {syms:?}"
    );
}

#[test]
fn workspace_symbol_query_skips_locals_keeps_decls() {
    let (_p, mut s, uri) = open_single("symsurface_ws", SYMSURFACE_SRC);

    let ws = |s: &mut LspServer, q: &str| -> Vec<String> {
        let resp = s.workspace_symbol_response(&json!({ "query": q }));
        resp.as_array()
            .unwrap_or_else(|| panic!("workspace/symbol must be an array, got {resp}"))
            .iter()
            .filter_map(|x| x["name"].as_str().map(str::to_string))
            .collect()
    };

    // A query matching a top-level fn surfaces it …
    assert!(
        ws(&mut s, "scale").iter().any(|n| n == "scale"),
        "workspace/symbol must surface the top-level fn `scale`"
    );
    // … but a query matching a local binder or a parameter returns nothing.
    assert!(
        ws(&mut s, "doubled").is_empty(),
        "workspace/symbol must not surface the local `doubled`: {:?}",
        ws(&mut s, "doubled")
    );
    assert!(
        ws(&mut s, "factor").is_empty(),
        "workspace/symbol must not surface the parameter `factor`: {:?}",
        ws(&mut s, "factor")
    );

    // Projection-only: the local kept out of the symbol surface is still a
    // resolvable definition — goto-def on its use lands on the binder span.
    let (l, c) = cursor(SYMSURFACE_SRC, "doubled", 2, 1); // use in `doubled + 1`
    let def = s.definition_response(&pos(&uri, l, c));
    assert!(
        !def.is_null(),
        "goto-def on a local excluded from the symbol surface must still resolve"
    );
    let binder = cursor(SYMSURFACE_SRC, "doubled", 1, 0);
    assert_eq!(
        start(&def),
        (binder.0 as i64, binder.1 as i64),
        "goto-def must still land on the local's binder span: {def}"
    );
}

// ============================================================================
// Cross-module handler surface — goto-def into another file, rename across two.
//
// Every handler test above is single-file, so two seams stay unexercised at the
// layer the editor actually calls:
//   * `uri_for`'s *cross-module* branch — a `def.defid.module` that is a
//     different on-disk module, mapped to its file via
//     `reference::rename::module_uri` (the same-file tests only hit the
//     `*path == req_mod` -> request-URI branch, and Bug 3 hits the distinct
//     import-segment path via `import_target_at`); and
//   * `workspace_edit_json` shaping a `changes` map keyed by 2+ file URIs (the
//     Bug-4 rename asserts a single-key map).
// `references.rs` pins both at the `IncrementalSession` layer; these pin the
// `LspServer` handler JSON. A two-file project: `lib.al` declares `greet`,
// `main.al` imports it and calls `lib.greet()`.
// ============================================================================

const XMOD_LIB: &str = "pub fn greet() Int { 7 }\n";
const XMOD_MAIN: &str = "import ./lib\nx = lib.greet()\nprintln(x)\n";

/// Boot a server over a project carrying `lib.al` + `main.al`, open both as
/// client tabs (lib first, so `main.al` is the last-analysed entry), and return
/// the server plus both URIs ready to query the cross-module `lib.greet()` use.
fn xmod_project() -> (Project, LspServer, String, String) {
    let p = Project::new("xmod_handler");
    p.write("lib.al", XMOD_LIB);
    p.write("main.al", XMOD_MAIN);
    let mut s = new_server();
    s.add_workspace_root(p.dir.clone());
    let lib_uri = uri_of(&p, "lib.al");
    let main_uri = uri_of(&p, "main.al");
    s.open_document(&lib_uri, XMOD_LIB);
    s.open_document(&main_uri, XMOD_MAIN);
    (p, s, main_uri, lib_uri)
}

/// The single location in `arr` whose `uri` is `want`, as its `range.start`
/// `(line, character)`. Panics unless exactly one location matches.
fn location_in(arr: &[Json], want: &str) -> (i64, i64) {
    let hits: Vec<&Json> = arr
        .iter()
        .filter(|r| r["uri"].as_str() == Some(want))
        .collect();
    assert_eq!(hits.len(), 1, "exactly one location in {want}, got {arr:?}");
    start(hits[0])
}

/// First-query portion shared by every cross-module find-references test:
/// queries references on `greet` from `query_uri` (either the qualified use in
/// main.al or the declaration in lib.al) with `includeDeclaration: true` and
/// asserts the result is a Location[] whose URIs span exactly lib.al and
/// main.al. Returns the locations plus the queried (line, character) so
/// `assert_refs_span_lib_and_main` can layer on the exact-span assertions and
/// the `includeDeclaration: false` re-query.
fn assert_refs_uris_span_lib_and_main(
    s: &mut LspServer,
    query_uri: &str,
    main_uri: &str,
    lib_uri: &str,
) -> (Vec<Json>, i32, i32) {
    let query_src = if query_uri == main_uri {
        XMOD_MAIN
    } else {
        XMOD_LIB
    };
    let (l, c) = cursor(query_src, "greet", 1, 1);
    let refs = s.references_response(&json!({
        "textDocument": { "uri": query_uri },
        "position": { "line": l, "character": c },
        "context": { "includeDeclaration": true },
    }));
    let arr = refs
        .as_array()
        .unwrap_or_else(|| panic!("references must be an array, got {refs}"));

    let uris: std::collections::BTreeSet<&str> =
        arr.iter().filter_map(|r| r["uri"].as_str()).collect();
    assert_eq!(
        uris,
        [lib_uri, main_uri].into_iter().collect(),
        "find-refs must span exactly lib.al and main.al, got {uris:?}"
    );
    (arr.clone(), l, c)
}

/// Full shared assertion block for the cross-module find-references tests that
/// pin exact spans. On top of the URI-set check above, asserts:
///   * the result holds exactly two locations — the declaration's identifier
///     in lib.al (line 0) and the `greet` token of `lib.greet()` in main.al
///     (line 1), never the `lib` qualifier;
///   * re-querying with `includeDeclaration: false` drops the declaration,
///     leaving only the main.al use — an array, never empty/null.
fn assert_refs_span_lib_and_main(
    s: &mut LspServer,
    query_uri: &str,
    main_uri: &str,
    lib_uri: &str,
) {
    let (arr, l, c) = assert_refs_uris_span_lib_and_main(s, query_uri, main_uri, lib_uri);

    assert_eq!(
        arr.len(),
        2,
        "expected the use in main.al + the decl in lib.al, got {arr:?}"
    );

    let lib_decl = cursor(XMOD_LIB, "greet", 1, 0);
    assert_eq!(
        location_in(&arr, lib_uri),
        (lib_decl.0 as i64, lib_decl.1 as i64),
        "the lib.al location must cover greet's declaration span: {arr:?}"
    );
    let main_use = cursor(XMOD_MAIN, "greet", 1, 0);
    assert_eq!(
        location_in(&arr, main_uri),
        (main_use.0 as i64, main_use.1 as i64),
        "the main.al location must cover the `greet` of `lib.greet()`: {arr:?}"
    );

    let no_decl = s.references_response(&json!({
        "textDocument": { "uri": query_uri },
        "position": { "line": l, "character": c },
        "context": { "includeDeclaration": false },
    }));
    let arr2 = no_decl
        .as_array()
        .unwrap_or_else(|| panic!("references must be an array, got {no_decl}"));
    assert_eq!(
        arr2.len(),
        1,
        "without the declaration only the main.al use remains: {arr2:?}"
    );
    assert_eq!(
        arr2[0]["uri"].as_str(),
        Some(main_uri),
        "the surviving location is the use in main.al, the declaration is gone: {arr2:?}"
    );
    assert_eq!(
        start(&arr2[0]),
        (main_use.0 as i64, main_use.1 as i64),
        "and it still covers the `greet` of `lib.greet()`: {arr2:?}"
    );
}

/// Shared assertion block for the cross-module rename tests. The WorkspaceEdit
/// `changes` map must be keyed by exactly lib.al and main.al, each holding one
/// edit — the declaration's identifier in lib.al (line 0) and the `greet` token
/// of `lib.greet()` in main.al (line 1), never the `lib` qualifier — and every
/// edit must substitute exactly `new_name`.
fn assert_rename_spans_lib_and_main(resp: &Json, main_uri: &str, lib_uri: &str, new_name: &str) {
    let changes = resp
        .get("changes")
        .and_then(Json::as_object)
        .expect("a WorkspaceEdit with a `changes` map");

    let keys: std::collections::BTreeSet<&str> = changes.keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [lib_uri, main_uri].into_iter().collect(),
        "changes must be keyed by lib.al and main.al exactly, got {keys:?}"
    );

    let lib_edits = changes
        .get(lib_uri)
        .and_then(Json::as_array)
        .expect("edits for lib.al");
    assert_eq!(
        lib_edits.len(),
        1,
        "lib.al holds only the declaration rewrite: {lib_edits:?}"
    );
    let lib_decl = cursor(XMOD_LIB, "greet", 1, 0);
    assert_eq!(
        start(&lib_edits[0]),
        (lib_decl.0 as i64, lib_decl.1 as i64),
        "lib.al edit must cover greet's declaration span: {lib_edits:?}"
    );

    let main_edits = changes
        .get(main_uri)
        .and_then(Json::as_array)
        .expect("edits for main.al");
    assert_eq!(
        main_edits.len(),
        1,
        "main.al holds only the use rewrite: {main_edits:?}"
    );
    let main_use = cursor(XMOD_MAIN, "greet", 1, 0);
    assert_eq!(
        start(&main_edits[0]),
        (main_use.0 as i64, main_use.1 as i64),
        "main.al edit must cover the `greet` token of `lib.greet()`: {main_edits:?}"
    );

    for (uri, edits) in changes {
        for e in edits.as_array().expect("each file's edits is an array") {
            assert_eq!(
                e["newText"],
                json!(new_name),
                "every edit must rewrite to the new name, got {e} in {uri}"
            );
        }
    }
}

#[test]
fn cross_module_goto_def_returns_the_other_files_uri() {
    let (_p, mut s, main_uri, lib_uri) = xmod_project();

    // Click the `greet` of `lib.greet()` in main.al (a qualified member use).
    // goto-def must cross the module boundary and land on the declaration in
    // lib.al — a *different* file's URI than the one the request came from.
    let (l, c) = cursor(XMOD_MAIN, "greet", 1, 1);
    let def = s.definition_response(&pos(&main_uri, l, c));
    assert!(
        !def.is_null(),
        "cross-module goto-def on `lib.greet()` must resolve, got null"
    );

    let target = def["uri"]
        .as_str()
        .unwrap_or_else(|| panic!("goto-def must carry a uri, got {def}"));
    assert!(
        target.ends_with("lib.al"),
        "cross-module goto-def must open lib.al, got {target}"
    );
    assert_eq!(
        target, lib_uri,
        "the returned uri must be lib.al's exact uri (uri_for's module_uri branch)"
    );
    assert_ne!(
        target, main_uri,
        "the definition lives in another file, not the requesting one"
    );

    // It lands exactly on `greet`'s declaring identifier: line 0, the `g` of
    // `pub fn greet` (column 7).
    let decl = cursor(XMOD_LIB, "greet", 1, 0);
    assert_eq!(
        start(&def),
        (decl.0 as i64, decl.1 as i64),
        "goto-def must land on greet's declaration span in lib.al, got {def}"
    );
}

#[test]
fn cross_module_rename_rewrites_decl_and_use_across_two_files() {
    let (_p, mut s, main_uri, lib_uri) = xmod_project();

    // Rename driven from the qualified use in main.al. The WorkspaceEdit must
    // rewrite both the declaration in lib.al and the use in main.al, so its
    // `changes` map is keyed by two distinct file URIs.
    let (l, c) = cursor(XMOD_MAIN, "greet", 1, 1);
    let mut params = pos(&main_uri, l, c);
    params["newName"] = json!("salute");

    let resp = s
        .rename_response(&params)
        .expect("a cross-module rename of a user fn must be allowed, not refused");
    assert_rename_spans_lib_and_main(&resp, &main_uri, &lib_uri, "salute");
}

/// The third cross-module handler seam: `references_response` shaping a flat
/// `Location[]` whose entries span 2+ file URIs. goto-def returns a single
/// location and rename a `changes` map; find-refs is the only handler emitting
/// an array of locations, with its own dedup, `include_decl` arm, and
/// per-reference `uri_for` loop. Driven from the qualified use `lib.greet()` in
/// main.al, the use is same-module (request-URI branch) while the declaration's
/// module is lib.al — a *different* file — so the `include_decl` arm routes it
/// through `uri_for`'s cross-module branch and the result must carry both files'
/// URIs. `references.rs::find_references_across_modules` pins this at the
/// `IncrementalSession` layer; nothing pinned the `LspServer` JSON.
#[test]
fn cross_module_find_references_spans_decl_and_use_across_files() {
    let (_p, mut s, main_uri, lib_uri) = xmod_project();

    // Cursor on the `greet` token of `lib.greet()` in main.al: the use is
    // same-module (request-URI branch) while the declaration is reached
    // cross-module through the `include_decl` arm.
    assert_refs_span_lib_and_main(&mut s, &main_uri, &main_uri, &lib_uri);
}

// ============================================================================
// Find-references / rename driven from a *library* file's own declaration.
//
// `cross_module_find_references_spans_decl_and_use_across_files` drives the
// query from the *use* site in the importer (main.al), where the importer is
// the analysed entry so its reverse edge into lib.al is live. The mirror case —
// the user clicks the declaration in the imported file (lib.al) and asks "who
// calls this?" — was broken: a position query re-roots compilation to lib.al,
// so lib.al becomes the entry (module `main`) and main.al, which imports lib.al
// (not vice-versa), falls outside lib.al's import closure. main.al's reverse
// edge to `greet` was only ever transient to main.al's own analysis and was
// never persisted, so find-references from lib.al's declaration returned only
// the declaration itself and rename rewrote only lib.al — silently leaving
// every caller untouched.
// ============================================================================

#[test]
fn find_references_from_library_declaration_includes_dependent_callers() {
    let (_p, mut s, main_uri, lib_uri) = xmod_project();

    // Cursor on greet's DECLARATION in lib.al (`pub fn greet`, line 0). Pre-fix
    // this returned only the lib.al declaration, dropping main.al's caller.
    assert_refs_span_lib_and_main(&mut s, &lib_uri, &main_uri, &lib_uri);
}

#[test]
fn rename_from_library_declaration_rewrites_dependent_callers() {
    let (_p, mut s, main_uri, lib_uri) = xmod_project();

    // Rename driven from greet's DECLARATION in lib.al. The WorkspaceEdit must
    // rewrite both the declaration in lib.al and the call site in main.al.
    let (l, c) = cursor(XMOD_LIB, "greet", 1, 1);
    let mut params = pos(&lib_uri, l, c);
    params["newName"] = json!("salute");

    let resp = s
        .rename_response(&params)
        .expect("a rename driven from a library declaration must be allowed");
    assert_rename_spans_lib_and_main(&resp, &main_uri, &lib_uri, "salute");
}

#[test]
fn find_references_from_library_declaration_when_only_library_is_open() {
    // Robustness: the caller (main.al) is never opened as a client tab — only
    // the one-time workspace scan analyses it. The dependent-file reverse edge
    // must still be recorded then, so find-references driven from lib.al's
    // declaration surfaces main.al's call even though lib.al is the only open
    // (and therefore last-analysed entry) buffer. This pins that the fix is not
    // order-dependent on an importer being the entry at query time.
    let p = Project::new("xmod_lib_only");
    p.write("lib.al", XMOD_LIB);
    p.write("main.al", XMOD_MAIN);
    let mut s = new_server();
    s.add_workspace_root(p.dir.clone());
    let lib_uri = uri_of(&p, "lib.al");
    let main_uri = uri_of(&p, "main.al");
    s.open_document(&lib_uri, XMOD_LIB);

    // Only the URI set is pinned here: what matters is that the never-opened
    // main.al caller shows up at all. Exact spans and the includeDeclaration
    // arm are covered by the tests above, where both files are open.
    assert_refs_uris_span_lib_and_main(&mut s, &lib_uri, &main_uri, &lib_uri);
}
