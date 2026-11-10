//! Tests for the LSP server's handlers. `references.rs` drives the reference
//! graph of a `Build` directly. These start a real `Workspace`, open a
//! document the way `textDocument/didOpen` does, and assert on the JSON each
//! `*_response` returns.

use scarlet::lsp::Workspace;
use serde_json::{Value as Json, json};

mod common;
use common::{Project, cursor};

/// The `file://` URI for `name` inside `p`, without percent-decoding, which is
/// what the server's own round trip from path to URI produces.
fn uri_of(p: &Project, name: &str) -> String {
    format!("file://{}", p.dir.join(name).display())
}

/// The `textDocument` and `position` params for a request at a position.
fn pos(uri: &str, line: i32, col: i32) -> Json {
    json!({
        "textDocument": { "uri": uri },
        "position": { "line": line, "character": col },
    })
}

/// Sends `textDocument/rename` at (`l`, `c`) with `newName` set to `to`,
/// through the seam.
fn rename_at(
    s: &mut Workspace,
    uri: &str,
    l: i32,
    c: i32,
    to: &str,
) -> Result<Json, impl std::fmt::Debug> {
    let mut params = pos(uri, l, c);
    params["newName"] = json!(to);
    s.rename_response(&params)
}

/// Sends `textDocument/references` at (`l`, `c`) with `includeDeclaration`
/// set to `include_decl`.
fn refs_at(s: &mut Workspace, uri: &str, l: i32, c: i32, include_decl: bool) -> Json {
    let mut params = pos(uri, l, c);
    params["context"] = json!({ "includeDeclaration": include_decl });
    s.references_response(&params)
}

/// Starts a workspace rooted at `p` and opens `name` as a client tab.
fn open(p: &Project, name: &str, src: &str) -> (Workspace, String) {
    let mut s = Workspace::new();
    s.add_workspace_root(p.dir.clone());
    let uri = uri_of(p, name);
    s.open_document(&uri, src);
    (s, uri)
}

/// Opens `src` as `a.sca` in a fresh project. We return the `Project` so its
/// temp dir outlives the server.
fn open_single(tag: &str, src: &str) -> (Project, Workspace, String) {
    let p = Project::new(tag);
    p.write("a.sca", src);
    let (s, uri) = open(&p, "a.sca", src);
    (p, s, uri)
}

const SRC: &str = "pub fn greet() Int { 7 }\nx = greet()\nprintln(x)\n";

/// Checks what every position query returns when it works, and that "no
/// result" is `Json::Null`.
#[test]
fn seam_round_trips_position_queries() {
    let (_p, mut s, uri) = open_single("seam", SRC);

    // Hover on the declaration name.
    let (l, c) = cursor(SRC, "greet", 1, 1);
    let hov = s.hover_response(&pos(&uri, l, c));
    let md = hov["contents"]["value"]
        .as_str()
        .expect("hover returns markdown contents");
    assert!(md.contains("greet"), "hover should name the symbol: {md:?}");

    // Goto-def on the use of greet lands on the declaration, on line 0.
    let (l, c) = cursor(SRC, "greet", 2, 1);
    let def = s.definition_response(&pos(&uri, l, c));
    assert_eq!(
        def["uri"].as_str(),
        Some(uri.as_str()),
        "definition resolves in the same file: {def:?}"
    );
    assert_eq!(def["range"]["start"]["line"].as_i64(), Some(0));

    // Find-refs on a resolved definition is an array, never null.
    let (l, c) = cursor(SRC, "greet", 1, 1);
    let refs = s.references_response(&pos(&uri, l, c));
    let arr = refs.as_array().expect("references is an array");
    assert!(arr.len() >= 2, "expected decl + use, got {arr:?}");

    // An empty payload and a position on nothing both answer null.
    assert!(s.definition_response(&json!({})).is_null());
    assert!(s.hover_response(&pos(&uri, 99, 99)).is_null());

    // Hover joins its type through the build that owns the file.
    assert!(s.build_for(&uri).is_some());

    // These are reachable through the seam. Tests further down pin how they
    // behave.
    let _ = s.prepare_rename_response(&pos(&uri, 0, 7));
    let _ = rename_at(&mut s, &uri, 0, 7, "greet2");
}

const TYPED: &str =
    "pub fn answer() Int {\n  base = 42\n  base\n}\npub fn run() Int {\n  answer()\n}\n";

/// Hover must show the inferred type, not only the kind of entity. It does at
/// a local binder, at a use of it, and as a function type at a call site.
#[test]
fn hover_markdown_includes_inferred_type() {
    let (_p, mut s, uri) = open_single("hovertype", TYPED);

    let md = |s: &mut Workspace, l: i32, c: i32| -> String {
        s.hover_response(&pos(&uri, l, c))["contents"]["value"]
            .as_str()
            .unwrap_or("")
            .to_string()
    };

    // The local binder `base = 42`.
    let (l, c) = cursor(TYPED, "base", 1, 1);
    let at_binder = md(&mut s, l, c);
    assert!(
        at_binder.contains("base Int"),
        "binder hover must show the inferred type: {at_binder:?}"
    );

    // A later use of `base`.
    let (l, c) = cursor(TYPED, "base", 2, 1);
    let at_use = md(&mut s, l, c);
    assert!(
        at_use.contains("base Int"),
        "use-site hover must show the inferred type: {at_use:?}"
    );

    // A call of the top-level fn, which pins the join on a graph definition.
    let (l, c) = cursor(TYPED, "answer", 2, 1);
    let at_call = md(&mut s, l, c);
    assert!(
        at_call.contains("answer fn() Int"),
        "call-site hover must show the function type: {at_call:?}"
    );
}

// A buffer with a parse error somewhere must still answer position queries
// about the parts that are clean. Every test below ends its source in a syntax
// error, because a buffer with no errors wouldn't show the difference.

/// `helper` is well-formed. The last line is a deliberate parse error that
/// stands in for an edit in progress. The recovering parser drops only that
/// line.
const BUG4_FN_SRC: &str =
    "fn helper() Int { 1 }\nx = helper()\ny = helper()\nprintln(x + y)\nbroken @#$\n";

#[test]
fn bug4_prepare_rename_top_level_fn_yields_range_and_placeholder() {
    let (_p, mut s, uri) = open_single("b4_prepare_fn", BUG4_FN_SRC);

    // Cursor in the `helper` declaration name.
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
    // This needs bug2, where locals are recorded as graph definitions. The
    // cursor is on a *use* of a local binding, and prepareRename must follow it
    // to the binder, even with an unrelated parse error at the end of the file.
    let src = "pub fn run() Int {\n  total = 41\n  total + 1\n}\nprintln(run())\nbroken @#$\n";
    let (_p, mut s, uri) = open_single("b4_prepare_local", src);

    // The second `total` is its use in `total + 1`.
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
    let resp = rename_at(&mut s, &uri, l, c, "renamed")
        .expect("rename of a user-defined fn must be allowed, not refused");
    let changes = resp
        .get("changes")
        .and_then(Json::as_object)
        .expect("a WorkspaceEdit with a `changes` map");
    let edits = changes
        .get(&uri)
        .and_then(Json::as_array)
        .expect("edits keyed by the open file's uri");

    // The declaration plus two call sites.
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

// Goto-def and find-refs on locals: a local variable, a function parameter,
// find-references and shadowing.

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

/// Asserts that goto-def from the `use_nth` occurrence of `needle` lands on
/// the `binder_nth` occurrence in the same file.
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

    // With the cursor on the `total` binder, find-refs must list the use below
    // it.
    let (l, c) = cursor(BUG2_SRC, "total", 1, 1);
    let refs = refs_at(&mut s, &uri, l, c, true);
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

    // `x` is one column wide, so the cursor sits on it with `into` at 0.
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

// In `import a/b` only the final segment is a goto-def target, and it opens the
// imported module. That's the sibling file for `import ./x`, and nothing for an
// embedded `scarlet/*`, which has no file on disk. Every other token resolves
// to nothing.

const BUG3_HELPER: &str = "pub fn greet() String {\n\x20 'hi'\n}\n";
const BUG3_MAIN: &str = "import ./helper\n\
import scarlet/string\n\
x = helper::greet()\n\
y = string::length(x)\n\
println(y)\n";

/// A two-file project with `main.sca` open, ready to query its imports.
fn bug3_project() -> (Project, Workspace, String) {
    let p = Project::new("b3_import");
    p.write("helper.sca", BUG3_HELPER);
    p.write("main.sca", BUG3_MAIN);
    let (s, uri) = open(&p, "main.sca", BUG3_MAIN);
    (p, s, uri)
}

#[test]
fn bug3_goto_def_on_import_segment_opens_the_module() {
    let (_p, mut s, uri) = bug3_project();

    // The `helper` segment opens helper.sca, not the importing line.
    let (l, c) = cursor(BUG3_MAIN, "helper", 1, 1); // 1st `helper` = the path
    let def = s.definition_response(&pos(&uri, l, c));
    let target = def["uri"]
        .as_str()
        .unwrap_or_else(|| panic!("goto-def on an import segment must open a file, got {def}"));
    assert!(
        target.ends_with("helper.sca"),
        "`import ./helper` must jump to helper.sca, got {target}"
    );

    // The embedded stdlib has no file to edit, so we must never make up a
    // location.
    let (l, c) = cursor(BUG3_MAIN, "string", 1, 1); // 1st `string` = the path
    let def = s.definition_response(&pos(&uri, l, c));
    assert!(
        def.is_null(),
        "goto-def on an embedded `scarlet/*` import segment must be null, got {def}"
    );
}

#[test]
fn bug3_import_keyword_and_non_final_segment_are_not_clickable() {
    let (_p, mut s, uri) = bug3_project();

    // No part of `import a/b` is a rename target. The alias `Definition` spans
    // the whole declaration, so a rename there would overwrite the `import`
    // line and orphan every qualified `q::member` use. All three handlers must
    // decline at each of these positions.
    let refused = |s: &mut Workspace, l: i32, c: i32| {
        assert!(
            s.definition_response(&pos(&uri, l, c)).is_null(),
            "goto-def must be null at import-decl position ({l},{c})"
        );
        assert!(
            s.prepare_rename_response(&pos(&uri, l, c)).is_null(),
            "prepareRename must refuse (null) at import-decl position ({l},{c}), \
             not offer the whole-declaration span"
        );
        if let Ok(edit) = rename_at(s, &uri, l, c, "renamed") {
            assert!(
                edit.is_null(),
                "rename at import-decl position ({l},{c}) must not emit a \
                 WorkspaceEdit (it would overwrite the `import` line), got {edit}"
            );
        }
    };

    // In `import ./helper`, the keyword, then the `./` separator characters.
    let (l, c) = cursor(BUG3_MAIN, "import", 1, 1);
    refused(&mut s, l, c);
    let (l, c) = cursor(BUG3_MAIN, "./helper", 1, 0); // the `.`
    refused(&mut s, l, c);
    let (l, c) = cursor(BUG3_MAIN, "/helper", 1, 0); // the `/`
    refused(&mut s, l, c);

    // In `import scarlet/string`, the keyword, the `scarlet` segment that isn't
    // last, and the `/`.
    let (l, c) = cursor(BUG3_MAIN, "import", 2, 1);
    refused(&mut s, l, c);
    let (l, c) = cursor(BUG3_MAIN, "scarlet/string", 1, 0); // the `scarlet`
    refused(&mut s, l, c);
    let (l, c) = cursor(BUG3_MAIN, "/string", 1, 0); // the `/`
    refused(&mut s, l, c);

    // You can navigate from the final segment but not rename it, because you
    // rename the file, not the import.
    for (l, c) in [
        cursor(BUG3_MAIN, "helper", 1, 1),
        cursor(BUG3_MAIN, "string", 1, 1),
    ] {
        assert!(
            s.prepare_rename_response(&pos(&uri, l, c)).is_null(),
            "prepareRename must refuse the final import segment ({l},{c})"
        );
        if let Ok(edit) = rename_at(&mut s, &uri, l, c, "renamed") {
            assert!(
                edit.is_null(),
                "rename must not edit the final import segment ({l},{c}), got {edit}"
            );
        }
    }
}

// `documentSymbol` and `workspace/symbol` must only list a module's structural
// declarations, and filter out `EntityKind::Value` binders through
// `Definition::is_symbol_listable`. Every other test in the suite checks with
// `.any()` or `.find()`, so dropping that filter would stay green there while
// wrecking the outline. The filter only hides locals from the symbol list. A
// local left out of it still resolves as a definition.

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

    // `SymbolKind::Function` is 12.
    for fname in ["scale", "run"] {
        assert!(
            syms.iter()
                .any(|x| x["name"] == json!(fname) && x["kind"] == json!(12)),
            "documentSymbol must list top-level fn `{fname}` (kind 12), got {names:?}"
        );
    }

    // Locals must not appear by name, or as `SymbolKind::Value`, which is 13.
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

    let ws = |s: &mut Workspace, q: &str| -> Vec<String> {
        let resp = s.workspace_symbol_response(&json!({ "query": q }));
        resp.as_array()
            .unwrap_or_else(|| panic!("workspace/symbol must be an array, got {resp}"))
            .iter()
            .filter_map(|x| x["name"].as_str().map(str::to_string))
            .collect()
    };

    // A top-level fn is listed…
    assert!(
        ws(&mut s, "scale").iter().any(|n| n == "scale"),
        "workspace/symbol must surface the top-level fn `scale`"
    );
    // …but a local binder or parameter isn't.
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

    // It still resolves, so goto-def on its use lands on the binder span.
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

// The handler paths that cross modules, which the single-file tests above never
// reach. These are `uri_for`'s cross-module branch, and a `changes` map keyed
// by two file URIs. `lib.sca` declares `greet`, and `main.sca` imports it and
// calls `lib::greet()`.

const XMOD_LIB: &str = "pub fn greet() Int { 7 }\n";
const XMOD_MAIN: &str = "import ./lib\nx = lib::greet()\nprintln(x)\n";

/// Opens both files as client tabs, lib first, so `main.sca` is the entry
/// analysed last.
fn xmod_project() -> (Project, Workspace, String, String) {
    let p = Project::new("xmod_handler");
    p.write("lib.sca", XMOD_LIB);
    p.write("main.sca", XMOD_MAIN);
    let mut s = Workspace::new();
    s.add_workspace_root(p.dir.clone());
    let lib_uri = uri_of(&p, "lib.sca");
    let main_uri = uri_of(&p, "main.sca");
    s.open_document(&lib_uri, XMOD_LIB);
    s.open_document(&main_uri, XMOD_MAIN);
    (p, s, main_uri, lib_uri)
}

/// The one location in `arr` whose `uri` is `want`, as its `range.start`
/// `(line, character)`. Panics unless exactly one location matches.
fn location_in(arr: &[Json], want: &str) -> (i64, i64) {
    let hits: Vec<&Json> = arr
        .iter()
        .filter(|r| r["uri"].as_str() == Some(want))
        .collect();
    assert_eq!(hits.len(), 1, "exactly one location in {want}, got {arr:?}");
    start(hits[0])
}

/// Queries references on `greet` from `query_uri` with `includeDeclaration`,
/// and asserts the URIs are exactly lib.sca and main.sca. Returns the locations
/// and the queried position so callers can add more assertions.
fn assert_refs_uris_span_lib_and_main(
    s: &mut Workspace,
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
    let refs = refs_at(s, query_uri, l, c, true);
    let arr = refs
        .as_array()
        .unwrap_or_else(|| panic!("references must be an array, got {refs}"));

    let uris: std::collections::BTreeSet<&str> =
        arr.iter().filter_map(|r| r["uri"].as_str()).collect();
    assert_eq!(
        uris,
        [lib_uri, main_uri].into_iter().collect(),
        "find-refs must span exactly lib.sca and main.sca, got {uris:?}"
    );
    (arr.clone(), l, c)
}

/// The declaration identifier in lib.sca, and the `greet` token of
/// `lib::greet()` in main.sca.
fn greet_spans() -> ((i64, i64), (i64, i64)) {
    let at = |src| {
        let (l, c) = cursor(src, "greet", 1, 0);
        (l as i64, c as i64)
    };
    (at(XMOD_LIB), at(XMOD_MAIN))
}

/// Asserts `items` holds exactly one entry whose `range.start` is `want`.
fn one_at(items: &[Json], want: (i64, i64), what: &str) {
    assert_eq!(items.len(), 1, "exactly one {what}: {items:?}");
    assert_eq!(start(&items[0]), want, "{what} span: {items:?}");
}

/// Asserts there are exactly two locations, the lib.sca declaration and the
/// main.sca use, and never the `lib` qualifier. Asking again without
/// `includeDeclaration` leaves only the use, as an array.
fn assert_refs_span_lib_and_main(
    s: &mut Workspace,
    query_uri: &str,
    main_uri: &str,
    lib_uri: &str,
) {
    let (arr, l, c) = assert_refs_uris_span_lib_and_main(s, query_uri, main_uri, lib_uri);

    assert_eq!(
        arr.len(),
        2,
        "expected the use in main.sca + the decl in lib.sca, got {arr:?}"
    );

    let (lib_decl, main_use) = greet_spans();
    assert_eq!(
        location_in(&arr, lib_uri),
        lib_decl,
        "the lib.sca location must cover greet's declaration span: {arr:?}"
    );
    assert_eq!(
        location_in(&arr, main_uri),
        main_use,
        "the main.sca location must cover the `greet` of `lib::greet()`: {arr:?}"
    );

    let no_decl = refs_at(s, query_uri, l, c, false);
    let arr2 = no_decl
        .as_array()
        .unwrap_or_else(|| panic!("references must be an array, got {no_decl}"));
    one_at(
        arr2,
        main_use,
        "main.sca use once the declaration is dropped",
    );
    assert_eq!(
        arr2[0]["uri"].as_str(),
        Some(main_uri),
        "the surviving location is the use in main.sca, the declaration is gone: {arr2:?}"
    );
}

/// Asserts the `changes` map is keyed by lib.sca and main.sca. Each has one
/// edit at `greet`, never at the `lib` qualifier, and each puts in `new_name`.
fn assert_rename_spans_lib_and_main(resp: &Json, main_uri: &str, lib_uri: &str, new_name: &str) {
    let changes = resp
        .get("changes")
        .and_then(Json::as_object)
        .expect("a WorkspaceEdit with a `changes` map");

    let keys: std::collections::BTreeSet<&str> = changes.keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [lib_uri, main_uri].into_iter().collect(),
        "changes must be keyed by lib.sca and main.sca exactly, got {keys:?}"
    );

    let (lib_decl, main_use) = greet_spans();
    let edits_for = |uri: &str| {
        changes
            .get(uri)
            .and_then(Json::as_array)
            .unwrap_or_else(|| panic!("edits for {uri}"))
    };
    one_at(edits_for(lib_uri), lib_decl, "lib.sca declaration rewrite");
    one_at(
        edits_for(main_uri),
        main_use,
        "main.sca rewrite at the `greet` of `lib::greet()`",
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

    // The `greet` of `lib::greet()` must resolve to lib.sca, a different file
    // from the one the request came from.
    let (l, c) = cursor(XMOD_MAIN, "greet", 1, 1);
    let def = s.definition_response(&pos(&main_uri, l, c));
    assert!(
        !def.is_null(),
        "cross-module goto-def on `lib::greet()` must resolve, got null"
    );

    let target = def["uri"]
        .as_str()
        .unwrap_or_else(|| panic!("goto-def must carry a uri, got {def}"));
    assert!(
        target.ends_with("lib.sca"),
        "cross-module goto-def must open lib.sca, got {target}"
    );
    assert_eq!(
        target, lib_uri,
        "the returned uri must be lib.sca's exact uri (uri_for's module_uri branch)"
    );
    assert_ne!(
        target, main_uri,
        "the definition lives in another file, not the requesting one"
    );

    // It must land on the `g` of `pub fn greet`, at line 0 column 7.
    let decl = cursor(XMOD_LIB, "greet", 1, 0);
    assert_eq!(
        start(&def),
        (decl.0 as i64, decl.1 as i64),
        "goto-def must land on greet's declaration span in lib.sca, got {def}"
    );
}

#[test]
fn cross_module_rename_rewrites_decl_and_use_across_two_files() {
    let (_p, mut s, main_uri, lib_uri) = xmod_project();

    // We start from the qualified use in main.sca, so `changes` is keyed by two
    // different file URIs.
    let (l, c) = cursor(XMOD_MAIN, "greet", 1, 1);
    let resp = rename_at(&mut s, &main_uri, l, c, "salute")
        .expect("a cross-module rename of a user fn must be allowed, not refused");
    assert_rename_spans_lib_and_main(&resp, &main_uri, &lib_uri, "salute");
}

/// `references_response` is the only handler that returns an array of
/// locations. It has its own dedup, `include_decl` arm and `uri_for` loop over
/// each reference.
#[test]
fn cross_module_find_references_spans_decl_and_use_across_files() {
    let (_p, mut s, main_uri, lib_uri) = xmod_project();

    // The use is in the same module, while the declaration is reached in
    // another module through the `include_decl` arm.
    assert_refs_span_lib_and_main(&mut s, &main_uri, &main_uri, &lib_uri);
}

// These mirror the tests above, with the query starting at the declaration in
// the imported file. main.sca imports lib.sca rather than the reverse, so
// main.sca is outside lib.sca's import closure, and its reverse edge has to come
// from main.sca's own check.

#[test]
fn find_references_from_library_declaration_includes_dependent_callers() {
    let (_p, mut s, main_uri, lib_uri) = xmod_project();

    // The cursor is on greet's declaration in lib.sca.
    assert_refs_span_lib_and_main(&mut s, &lib_uri, &main_uri, &lib_uri);
}

#[test]
fn rename_from_library_declaration_rewrites_dependent_callers() {
    let (_p, mut s, main_uri, lib_uri) = xmod_project();

    // We start from greet's declaration in lib.sca.
    let (l, c) = cursor(XMOD_LIB, "greet", 1, 1);
    let resp = rename_at(&mut s, &lib_uri, l, c, "salute")
        .expect("a rename driven from a library declaration must be allowed");
    assert_rename_spans_lib_and_main(&resp, &main_uri, &lib_uri, "salute");
}

/// A regression test for stale data. Edit lib.sca so every span in it shifts,
/// *then* rename from the moved declaration. main.sca's reverse edge was
/// recorded before the edit. If main.sca weren't checked again, its edge would
/// still point at the old span, and the rename would silently drop main.sca's
/// call site, breaking the workspace with no diagnostic.
#[test]
fn rename_after_library_edit_still_rewrites_dependent_callers() {
    let (_p, mut s, main_uri, lib_uri) = xmod_project();

    // Simulate didChange. A blank line added to the top of lib.sca moves
    // greet's declaration down one line. Only lib.sca changes.
    let edited = "\npub fn greet() Int { 7 }\n";
    s.open_document(&lib_uri, edited);

    let (l, c) = cursor(edited, "greet", 1, 1);
    let resp = rename_at(&mut s, &lib_uri, l, c, "salute")
        .expect("a rename after an edit to the library must still be allowed");
    let changes = resp
        .get("changes")
        .and_then(Json::as_object)
        .expect("a WorkspaceEdit with a `changes` map");
    assert!(
        changes.contains_key(main_uri.as_str()),
        "the dependent caller in main.sca must be rewritten after lib.sca \
         was edited; dropping it silently breaks the workspace: {changes:?}"
    );
    let main_edits = changes[main_uri.as_str()].as_array().expect("edits array");
    assert_eq!(
        main_edits.len(),
        1,
        "exactly the `greet` of `lib::greet()`: {main_edits:?}"
    );
}

#[test]
fn find_references_from_library_declaration_when_only_library_is_open() {
    // main.sca is never opened as a tab, so only the one-off workspace scan
    // checks it. The reverse edge must be recorded then.
    let p = Project::new("xmod_lib_only");
    p.write("lib.sca", XMOD_LIB);
    p.write("main.sca", XMOD_MAIN);
    let mut s = Workspace::new();
    s.add_workspace_root(p.dir.clone());
    let lib_uri = uri_of(&p, "lib.sca");
    let main_uri = uri_of(&p, "main.sca");
    s.open_document(&lib_uri, XMOD_LIB);

    // Only check the set of URIs, because what matters is that the caller we
    // never opened appears.
    assert_refs_uris_span_lib_and_main(&mut s, &lib_uri, &main_uri, &lib_uri);
}

// An in-memory edit to a dependency must refresh the diagnostics of an open
// file that imports it, or its Problems panel goes stale.
#[test]
fn editing_a_dependency_republishes_its_open_importers() {
    let (_p, mut s, main_uri, lib_uri) = xmod_project();

    // lib.sca stops exporting `greet`, so main.sca's `lib::greet()` breaks.
    let published = s.open_document(&lib_uri, "pub fn salute() Int { 7 }\n");
    let uris: Vec<&str> = published.iter().map(|(u, _)| u.as_str()).collect();
    assert!(
        uris.contains(&lib_uri.as_str()) && uris.contains(&main_uri.as_str()),
        "the edited file and its open importer are both published, got {uris:?}"
    );
    let main_diags = &published
        .iter()
        .find(|(u, _)| *u == main_uri)
        .expect("main.sca is published")
        .1;
    assert!(
        main_diags
            .iter()
            .any(|d| d["message"].as_str().is_some_and(|m| m.contains("greet"))),
        "main.sca must now report the missing `greet`: {main_diags:?}"
    );

    // A position query changes nothing, so it has nothing to publish.
    let (l, c) = cursor(XMOD_MAIN, "greet", 1, 1);
    s.hover_response(&pos(&main_uri, l, c));
}

/// An error inside an imported module is shown on that module's file, never on
/// the file that imports it.
#[test]
fn an_imported_modules_error_is_published_on_its_own_file() {
    let p = Project::new("diag_owner");
    let lib = "pub fn broken() Int {\n  'not an int'\n}\n";
    let main = "import ./lib\npub fn main() {\n  println(lib::broken())\n}\n";
    p.write("lib.sca", lib);
    p.write("main.sca", main);
    let mut s = Workspace::new();
    s.add_workspace_root(p.dir.clone());
    let main_uri = uri_of(&p, "main.sca");
    let lib_uri = uri_of(&p, "lib.sca");
    let published = s.open_document(&main_uri, main);

    let diags_of = |uri: &str| -> Vec<Json> {
        published
            .iter()
            .find(|(u, _)| u == uri)
            .map(|(_, d)| d.clone())
            .unwrap_or_else(|| panic!("{uri} is published: {published:?}"))
    };
    let errors = |d: &[Json]| d.iter().filter(|d| d["severity"] == json!(1)).count();
    assert_eq!(
        errors(&diags_of(&main_uri)),
        0,
        "main.sca has no errors of its own: {:?}",
        diags_of(&main_uri)
    );
    let lib_diags = diags_of(&lib_uri);
    assert!(
        errors(&lib_diags) >= 1,
        "lib.sca's type error is published on lib.sca: {lib_diags:?}"
    );
    assert_eq!(
        lib_diags[0]["range"]["start"]["line"].as_i64(),
        Some(1),
        "the error points into lib.sca's text: {lib_diags:?}"
    );
}

/// The `/** */` at line 0 is `b.sca`'s module doc, not `add`'s.
const MODDOC_B: &str = "/**\n * Adds numbers together.\n */\npub fn add(a, b) {\n  a + b\n}\n";

const MODDOC_A: &str = "import ./b\n\nprintln(b::add(10, 20))\n";

/// The `a.sca` and `b.sca` demo pair, opened as `a.sca`.
fn moddoc_project() -> (Project, Workspace, String) {
    let p = Project::new("moddoc");
    p.write("b.sca", MODDOC_B);
    p.write("a.sca", MODDOC_A);
    let (s, uri) = open(&p, "a.sca", MODDOC_A);
    (p, s, uri)
}

fn hover_md(s: &mut Workspace, uri: &str, l: i32, c: i32) -> String {
    s.hover_response(&pos(uri, l, c))["contents"]["value"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

#[test]
fn hover_on_module_alias_shows_the_module_doc() {
    let (_p, mut s, uri) = moddoc_project();

    // The `b` of `b::add(10, 20)`, a `Qualifier` occurrence on the import.
    let (l, c) = cursor(MODDOC_A, "b::add", 1, 0);
    let md = hover_md(&mut s, &uri, l, c);
    assert!(
        md.contains("Adds numbers together."),
        "hover on the `b` qualifier must render b.sca's module doc, got {md:?}"
    );
    assert!(md.contains("*module*"), "…and name it a module: {md:?}");

    // The `b` of the `import ./b` line itself.
    let (l, c) = cursor(MODDOC_A, "./b", 1, 2);
    let md = hover_md(&mut s, &uri, l, c);
    assert!(
        md.contains("Adds numbers together."),
        "hover on the import path segment must render the module doc, got {md:?}"
    );

    // The member `add` still hovers as the function, not as the module.
    let (l, c) = cursor(MODDOC_A, "add", 1, 0);
    let md = hover_md(&mut s, &uri, l, c);
    assert!(
        !md.contains("*module*"),
        "hover on the member must not render the module: {md:?}"
    );
}

#[test]
fn goto_def_on_module_alias_opens_the_module_file() {
    let (_p, mut s, uri) = moddoc_project();

    // The `b` of `b::add(10, 20)` opens b.sca at 0:0.
    let (l, c) = cursor(MODDOC_A, "b::add", 1, 0);
    let def = s.definition_response(&pos(&uri, l, c));
    let target = def["uri"]
        .as_str()
        .unwrap_or_else(|| panic!("goto-def on a module alias must open a file, got {def}"));
    assert!(
        target.ends_with("b.sca"),
        "`b::add(..)` must jump to b.sca, got {target}"
    );
    assert_eq!(def["range"]["start"]["line"].as_i64(), Some(0));
    assert_eq!(def["range"]["start"]["character"].as_i64(), Some(0));

    // The alias `Definition` spans the whole declaration, so jumping to the
    // `import` keyword would be a self-jump.
    let (l, c) = cursor(MODDOC_A, "import", 1, 1);
    assert!(
        s.definition_response(&pos(&uri, l, c)).is_null(),
        "goto-def on the `import` keyword must stay null"
    );
}

/// A `/** */` at line 0 is the module's doc, so the first declaration below it
/// must not inherit it.
#[test]
fn module_doc_is_not_attached_to_the_first_declaration() {
    let src = "/** Module prose. */\npub fn greet() Int { 7 }\nprintln(greet())\n";
    let (_p, mut s, uri) = open_single("moddoc_first", src);

    let (l, c) = cursor(src, "greet", 1, 1);
    let md = hover_md(&mut s, &uri, l, c);
    assert!(md.contains("greet"), "hover names the function: {md:?}");
    assert!(
        !md.contains("Module prose."),
        "the line-0 doc is the module's, not `greet`'s: {md:?}"
    );
}

/// A local binding shadows the import alias, so the `b` of `b.x` names the
/// `Point`, not the module `b`.
const SHADOW_A: &str = "import ./b\n\
type Point {\n\
\x20 Point(x Int)\n\
}\n\
pub fn get(b Point) Int {\n\
\x20 b.x\n\
}\n\
p = Point(41)\n\
println(get(p) + b::add(0, 1))\n";

#[test]
fn qualifier_occurrence_respects_a_shadowing_local() {
    let p = Project::new("shadow_qualifier");
    p.write("b.sca", MODDOC_B);
    p.write("a.sca", SHADOW_A);
    let (mut s, uri) = open(&p, "a.sca", SHADOW_A);

    // The `b` of `b.x` inside `get` is a parameter, not the module alias.
    let (l, c) = cursor(SHADOW_A, "b.x", 1, 0);
    let def = s.definition_response(&pos(&uri, l, c));
    let target = def["uri"]
        .as_str()
        .unwrap_or_else(|| panic!("goto-def on a shadowed name must resolve, got {def}"));
    assert!(
        target.ends_with("a.sca"),
        "the shadowed `b` must resolve to the local parameter in a.sca, got {target}"
    );
    let (param_l, param_c) = cursor(SHADOW_A, "b Point", 1, 0);
    assert_eq!(def["range"]["start"]["line"].as_i64(), Some(param_l as i64));
    assert_eq!(
        def["range"]["start"]["character"].as_i64(),
        Some(param_c as i64)
    );

    let md = hover_md(&mut s, &uri, l, c);
    assert!(
        !md.contains("*module*"),
        "a shadowed `b` is not a module qualifier: {md:?}"
    );

    // …while the `b` of `b::add` is the module.
    let (l, c) = cursor(SHADOW_A, "b::add", 1, 0);
    let md = hover_md(&mut s, &uri, l, c);
    assert!(
        md.contains("*module*") && md.contains("Adds numbers together."),
        "the unshadowed `b` still names module b: {md:?}"
    );
}

/// The `b` of `b::add(..)` is a reference to the import, even though it isn't
/// the use the unused-import rule looks at.
#[test]
fn find_refs_on_module_alias_lists_the_qualifier() {
    let (_p, mut s, uri) = moddoc_project();

    let (l, c) = cursor(MODDOC_A, "b::add", 1, 0);
    let refs = refs_at(&mut s, &uri, l, c, false);
    let items = refs
        .as_array()
        .unwrap_or_else(|| panic!("references on a module alias must be an array, got {refs}"));
    assert_eq!(
        items.len(),
        1,
        "the sole reference to alias `b` is the qualifier of `b::add`: {items:?}"
    );
    assert_eq!(items[0]["range"]["start"]["line"].as_i64(), Some(l as i64));
    assert_eq!(
        items[0]["range"]["start"]["character"].as_i64(),
        Some(c as i64)
    );

    // With the declaration included, the `import ./b` line joins in.
    let with_decl = refs_at(&mut s, &uri, l, c, true);
    assert_eq!(with_decl.as_array().map(Vec::len), Some(2));
}

/// `scarlet/process` is the embedded stdlib, so its module doc reaches hover
/// through that module's own check.
#[test]
fn hover_on_a_stdlib_module_alias_shows_its_module_doc() {
    let src = "import scarlet/process as s\n\ns::spawn(fn() { Nil })\n";
    let (_p, mut w, uri) = open_single("stdlib_moddoc", src);

    let (l, c) = cursor(src, "s::spawn", 1, 0);
    let md = hover_md(&mut w, &uri, l, c);
    assert!(
        md.contains("*module*"),
        "the `s` of `s::spawn(..)` names a module: {md:?}"
    );
    assert!(
        md.contains("Processes, message passing, and the supervision tree."),
        "hover must render scarlet/process's module doc: {md:?}"
    );
}

// A function's parameter names are documentation, not part of its type. They
// travel on `Definition::param_names` and only hover shows them. Putting them
// in `Type::Function` would force unification to decide whether
// `fn(path String)` equals `fn(p String)`. Constructor field labels do carry
// meaning and live in the type, and hover shows them the same way.

#[test]
fn hover_shows_a_functions_parameter_names() {
    let src = "fn greet(name String, greeting String) String {\n\tgreeting + name\n}\n\nprintln(greet('al', 'hi '))\n";
    let (_p, mut s, uri) = open_single("hover_params", src);
    let md = hover_md(&mut s, &uri, 0, 4); // the `greet` declaration
    assert!(
        md.contains("fn(name String, greeting String) String"),
        "hover must label the parameters, got:\n{md}"
    );
}

/// The parameter names of an embedded stdlib function reach hover too.
#[test]
fn hover_shows_parameter_names_across_the_stdlib_blob() {
    let src = "import scarlet/io\n\nprintln(io::read_text('x') or e -> '')\n";
    let (_p, mut s, uri) = open_single("hover_blob", src);
    let md = hover_md(&mut s, &uri, 2, 12); // inside `read_text`
    assert!(
        md.contains("fn(path String)"),
        "stdlib fn must carry its parameter name, got:\n{md}"
    );
}

/// A constructor's labels are its field labels.
#[test]
fn hover_shows_a_constructors_field_labels() {
    let src = "import scarlet/io\n\nfn f(e io::IoError) String {\n\tmatch e {\n\t\tio::NotFound(p) -> p\n\t\t_ -> ''\n\t}\n}\nprintln(1)\n";
    let (_p, mut s, uri) = open_single("hover_ctor", src);
    let md = hover_md(&mut s, &uri, 4, 6); // the `NotFound` of `io::NotFound(p)`
    assert!(
        md.contains("fn(path String) IoError"),
        "constructor must show its field label, got:\n{md}"
    );
}

/// A declaration's doc comment must cross a module boundary.
#[test]
fn hover_shows_a_declaration_doc_from_another_module() {
    let p = Project::new("hover_xmod_doc");
    p.write(
        "lib.sca",
        "/** Text helpers. */\n\n/** Join `parts` with `sep`. */\npub fn join(parts Array(String), sep String) String {\n\t'x'\n}\n",
    );
    let src = "import ./lib\n\nprintln(lib::join(['a'], ','))\n";
    p.write("main.sca", src);
    let (mut s, uri) = open(&p, "main.sca", src);
    let md = hover_md(&mut s, &uri, 2, 14); // inside `join`
    assert!(
        md.contains("fn(parts Array(String), sep String)"),
        "labels must cross the module boundary, got:\n{md}"
    );
    assert!(
        md.contains("Join `parts` with `sep`."),
        "declaration doc must cross the module boundary, got:\n{md}"
    );
}

// `textDocument/completion`. After `alias::` the imported module's public
// exports appear. A bare position lists the module's own top-level
// declarations. A `::` after a name that isn't a module gives an empty list.

/// Completion labels at (`l`, `c`), sorted as the server returns them.
fn completion_labels(s: &mut Workspace, uri: &str, l: i32, c: i32) -> Vec<String> {
    let resp = s.completion_response(&pos(uri, l, c));
    resp.as_array()
        .unwrap_or_else(|| panic!("completion must be an array, got {resp}"))
        .iter()
        .filter_map(|i| i["label"].as_str().map(str::to_string))
        .collect()
}

const COMP_STDLIB: &str = "import scarlet/string\nx = string::\n";

#[test]
fn completion_after_stdlib_alias_dot_lists_exports() {
    let (_p, mut s, uri) = open_single("comp_std", COMP_STDLIB);

    // The cursor is right after the trailing `::`. The statement is a parse
    // error the recovering parser drops, and completion must survive that.
    let labels = completion_labels(&mut s, &uri, 1, 12);
    assert!(
        labels.iter().any(|l| l == "length"),
        "`string::` must offer scarlet/string's `length`, got {labels:?}"
    );
}

const COMP_HELPER: &str = "pub fn greet() String {\n\x20 'hi'\n}\nfn hidden() Int { 1 }\n";
const COMP_MAIN: &str = "import ./helper\nz = helper::\n";

#[test]
fn completion_after_file_module_alias_dot_lists_pub_exports_only() {
    let p = Project::new("comp_file");
    p.write("helper.sca", COMP_HELPER);
    p.write("main.sca", COMP_MAIN);
    let (mut s, uri) = open(&p, "main.sca", COMP_MAIN);

    let labels = completion_labels(&mut s, &uri, 1, 12);
    assert!(
        labels.iter().any(|l| l == "greet"),
        "`helper::` must offer the pub `greet`, got {labels:?}"
    );
    assert!(
        !labels.iter().any(|l| l == "hidden"),
        "a private fn must not be offered, got {labels:?}"
    );
}

const COMP_BARE: &str = "import scarlet/string\npub fn go() Int { 7 }\nx = g\n";

#[test]
fn completion_at_bare_identifier_lists_declarations_and_aliases() {
    let (_p, mut s, uri) = open_single("comp_bare", COMP_BARE);

    let (l, c) = cursor(COMP_BARE, "x = g", 1, 5);
    let labels = completion_labels(&mut s, &uri, l, c);
    assert!(
        labels.iter().any(|l| l == "go"),
        "a bare position must offer the module's own `go`, got {labels:?}"
    );
    assert!(
        labels.iter().any(|l| l == "string"),
        "a bare position must offer the `string` import alias, got {labels:?}"
    );
}

const COMP_VALUE_DOT: &str = "x = 1\ny = x.\n";

#[test]
fn completion_after_non_module_dot_is_empty() {
    let (_p, mut s, uri) = open_single("comp_val", COMP_VALUE_DOT);

    let labels = completion_labels(&mut s, &uri, 1, 6);
    assert!(
        labels.is_empty(),
        "a dot after a plain value must offer nothing, got {labels:?}"
    );
}

/// Inside the stdlib tree, a module qualifier opens that module's source.
#[test]
fn goto_def_on_stdlib_module_qualifier_opens_its_file() {
    let std_dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../scarlet_core/src/std/scarlet");
    let http = std_dir.join("http.sca");
    let src = std::fs::read_to_string(&http).expect("the stdlib http module");
    let mut s = Workspace::new();
    let uri = format!("file://{}", http.display());
    s.open_document(&uri, &src);

    let (l, c) = cursor(&src, "error::NetError", 1, 0);
    let def = s.definition_response(&pos(&uri, l, c));
    let target = def["uri"]
        .as_str()
        .unwrap_or_else(|| panic!("goto-def on `error::` must open a file, got {def}"));
    assert!(
        target.ends_with("scarlet/net/error.sca"),
        "`error::NetError` must jump to net/error.sca, got {target}"
    );
    assert_eq!(def["range"]["start"]["line"].as_i64(), Some(0));
    assert_eq!(def["range"]["start"]["character"].as_i64(), Some(0));
}

// Edits from outside the editor reach the build through didClose and
// didChangeWatchedFiles. Nothing is rewound: the build reads the file again and
// checks what changed.

/// The diagnostics published for `uri`, or a panic naming what was published.
fn published_for<'a>(published: &'a [(String, Vec<Json>)], uri: &str) -> &'a [Json] {
    published
        .iter()
        .find(|(u, _)| u == uri)
        .map(|(_, d)| d.as_slice())
        .unwrap_or_else(|| panic!("{uri} is published: {published:?}"))
}

fn mentions(diags: &[Json], needle: &str) -> bool {
    diags
        .iter()
        .any(|d| d["message"].as_str().is_some_and(|m| m.contains(needle)))
}

#[test]
fn closing_an_edited_buffer_puts_the_file_on_disk_back() {
    let (_p, mut s, main_uri, lib_uri) = xmod_project();

    // An unsaved edit to lib.sca breaks main.sca's `lib::greet()`.
    let published = s.open_document(&lib_uri, "pub fn salute() Int { 7 }\n");
    assert!(mentions(published_for(&published, &main_uri), "greet"));

    // Closing lib.sca without saving brings back the file on disk, which still
    // exports `greet`.
    let published = s.close_document(&lib_uri);
    assert!(
        !mentions(published_for(&published, &main_uri), "greet"),
        "main.sca is checked against lib.sca's file again: {published:?}"
    );
}

#[test]
fn a_watched_change_to_a_closed_file_rechecks_its_importers() {
    let p = Project::new("watched");
    p.write("lib.sca", XMOD_LIB);
    p.write("main.sca", XMOD_MAIN);
    let (mut s, main_uri) = open(&p, "main.sca", XMOD_MAIN);
    let lib_uri = uri_of(&p, "lib.sca");

    // lib.sca changes on disk, say from a git checkout.
    p.write("lib.sca", "pub fn salute() Int { 7 }\n");
    let published = s.files_changed(&[(lib_uri.clone(), scarlet::lsp::WatchedChange::Touched)]);
    assert!(
        mentions(published_for(&published, &main_uri), "greet"),
        "main.sca must see that lib.sca no longer exports `greet`: {published:?}"
    );

    // Once lib.sca is deleted, its definitions leave the graph.
    std::fs::remove_file(p.dir.join("lib.sca")).unwrap();
    let published = s.files_changed(&[(lib_uri.clone(), scarlet::lsp::WatchedChange::Deleted)]);
    assert!(
        !published_for(&published, &main_uri).is_empty(),
        "main.sca's import of the deleted lib.sca is an error: {published:?}"
    );
    let found = s.workspace_symbol_response(&json!({ "query": "salute" }));
    assert_eq!(
        found,
        json!([]),
        "a deleted file's definitions must leave the graph"
    );
}
