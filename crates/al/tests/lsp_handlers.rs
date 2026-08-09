//! Handler-layer coverage for the LSP server. `references.rs` drives the
//! `IncrementalSession` query API; these boot a real `Workspace`, open a
//! document the way `textDocument/didOpen` does, and assert on the JSON each
//! `*_response` returns.

use al::lsp::Workspace;
use serde_json::{Value as Json, json};

mod common;
use common::{Project, cursor};

/// `file://`-form URI for `name` inside `p`, in the non-percent-decoded shape
/// the server's own path/uri round-trip produces.
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

/// `textDocument/rename` at (`l`, `c`) with `newName` = `to`, through the seam.
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

/// `textDocument/references` at (`l`, `c`) with `includeDeclaration` set.
fn refs_at(s: &mut Workspace, uri: &str, l: i32, c: i32, include_decl: bool) -> Json {
    let mut params = pos(uri, l, c);
    params["context"] = json!({ "includeDeclaration": include_decl });
    s.references_response(&params)
}

/// Boot a workspace rooted at `p` and open `name` as a client tab.
fn open(p: &Project, name: &str, src: &str) -> (Workspace, String) {
    let mut s = Workspace::new();
    s.add_workspace_root(p.dir.clone());
    let uri = uri_of(p, name);
    s.open_document(&uri, src);
    (s, uri)
}

/// Open `src` as `a.al` in a fresh project. The `Project` comes back so its
/// temp dir outlives the server.
fn open_single(tag: &str, src: &str) -> (Project, Workspace, String) {
    let p = Project::new(tag);
    p.write("a.al", src);
    let (s, uri) = open(&p, "a.al", src);
    (p, s, uri)
}

const SRC: &str = "pub fn greet() Int { 7 }\nx = greet()\nprintln(x)\n";

/// Every position query's happy-path shape, plus the "no result" convention of
/// `Json::Null`.
#[test]
fn seam_round_trips_position_queries() {
    let (_p, mut s, uri) = open_single("seam", SRC);

    // hover on the declaration name.
    let (l, c) = cursor(SRC, "greet", 1, 1);
    let hov = s.hover_response(&pos(&uri, l, c));
    let md = hov["contents"]["value"]
        .as_str()
        .expect("hover returns markdown contents");
    assert!(md.contains("greet"), "hover should name the symbol: {md:?}");

    // goto-def on the use of greet lands on the declaration, line 0.
    let (l, c) = cursor(SRC, "greet", 2, 1);
    let def = s.definition_response(&pos(&uri, l, c));
    assert_eq!(
        def["uri"].as_str(),
        Some(uri.as_str()),
        "definition resolves in the same file: {def:?}"
    );
    assert_eq!(def["range"]["start"]["line"].as_i64(), Some(0));

    // find-refs on a resolved definition is an array, never null.
    let (l, c) = cursor(SRC, "greet", 1, 1);
    let refs = s.references_response(&pos(&uri, l, c));
    let arr = refs.as_array().expect("references is an array");
    assert!(arr.len() >= 2, "expected decl + use, got {arr:?}");

    // An empty payload and a position on nothing both answer null.
    assert!(s.definition_response(&json!({})).is_null());
    assert!(s.hover_response(&pos(&uri, 99, 99)).is_null());

    // Hover joins its type through the session accessor.
    assert!(s.session_for(&uri).is_some());

    // Reachable through the seam; behaviour is pinned further down.
    let _ = s.prepare_rename_response(&pos(&uri, 0, 7));
    let _ = rename_at(&mut s, &uri, 0, 7, "greet2");
}

const TYPED: &str =
    "pub fn answer() Int {\n  base = 42\n  base\n}\npub fn run() Int {\n  answer()\n}\n";

/// Hover must surface the inferred type, not only the entity kind: at a local
/// binder, at a use of it, and as a function type at a call site.
#[test]
fn hover_markdown_includes_inferred_type() {
    let (_p, mut s, uri) = open_single("hovertype", TYPED);

    let md = |s: &mut Workspace, l: i32, c: i32| -> String {
        s.hover_response(&pos(&uri, l, c))["contents"]["value"]
            .as_str()
            .unwrap_or("")
            .to_string()
    };

    // local binder `base = 42`.
    let (l, c) = cursor(TYPED, "base", 1, 1);
    let at_binder = md(&mut s, l, c);
    assert!(
        at_binder.contains("base Int"),
        "binder hover must show the inferred type: {at_binder:?}"
    );

    // a later use of `base`.
    let (l, c) = cursor(TYPED, "base", 2, 1);
    let at_use = md(&mut s, l, c);
    assert!(
        at_use.contains("base Int"),
        "use-site hover must show the inferred type: {at_use:?}"
    );

    // a call of the top-level fn, which pins the join on a graph def.
    let (l, c) = cursor(TYPED, "answer", 2, 1);
    let at_call = md(&mut s, l, c);
    assert!(
        at_call.contains("answer fn() Int"),
        "call-site hover must show the function type: {at_call:?}"
    );
}

// A buffer holding a parse error somewhere must still answer position queries
// about the parts that are clean. Every test below ends its source in a syntax
// error; an error-free buffer would not distinguish the behaviour.

/// `helper` is well-formed; the trailing line is a deliberate parse error
/// standing in for an in-progress edit. The recovering parser drops only that
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

    // Declaration plus two call sites.
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

// goto-def / find-refs on locals: a local variable, a function parameter,
// find-references, and shadowing.

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

/// Assert goto-def from the `use_nth` occurrence of `needle` lands on the
/// `binder_nth` occurrence in the same file.
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

    // `x` is one column wide, so the cursor sits on it (`into` 0).
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
// imported module: the sibling file for `import ./x`, nothing for an embedded
// `al/*`, which has no on-disk file. Every other token resolves to nothing.

const BUG3_HELPER: &str = "pub fn greet() String {\n\x20 'hi'\n}\n";
const BUG3_MAIN: &str = "import ./helper\n\
import al/string\n\
x = helper.greet()\n\
y = string.length(x)\n\
println(y)\n";

/// A two-file project with `main.al` open, ready to query its imports.
fn bug3_project() -> (Project, Workspace, String) {
    let p = Project::new("b3_import");
    p.write("helper.al", BUG3_HELPER);
    p.write("main.al", BUG3_MAIN);
    let (s, uri) = open(&p, "main.al", BUG3_MAIN);
    (p, s, uri)
}

#[test]
fn bug3_goto_def_on_import_segment_opens_the_module() {
    let (_p, mut s, uri) = bug3_project();

    // The `helper` segment opens helper.al, not the importing line.
    let (l, c) = cursor(BUG3_MAIN, "helper", 1, 1); // 1st `helper` = the path
    let def = s.definition_response(&pos(&uri, l, c));
    let target = def["uri"]
        .as_str()
        .unwrap_or_else(|| panic!("goto-def on an import segment must open a file, got {def}"));
    assert!(
        target.ends_with("helper.al"),
        "`import ./helper` must jump to helper.al, got {target}"
    );

    // Embedded stdlib has no editable file, so never fabricate a location.
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

    // `import a/b` is not a rename target anywhere. The alias `Definition`
    // spans the whole declaration, so a rename there would overwrite the
    // `import` line, and every qualified `q.member` use would be orphaned. All
    // three handlers must decline at each such position.
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

    // The final segment is navigable but not renamable: you rename the file,
    // not the import.
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

// `documentSymbol` and `workspace/symbol` must list a module's structural
// declarations only, filtering `EntityKind::Value` binders out through
// `Definition::is_symbol_listable`. Every other test in the suite probes with
// `.any()` / `.find()`, so dropping that filter would stay green here while
// wrecking the outline. The exclusion is projection-only: a local kept out of
// the symbol surface is still a resolvable definition.

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

    // Neither by name nor as `SymbolKind::Value`, which is 13.
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

    // A top-level fn is surfaced …
    assert!(
        ws(&mut s, "scale").iter().any(|n| n == "scale"),
        "workspace/symbol must surface the top-level fn `scale`"
    );
    // … but a local binder or parameter is not.
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

    // Still resolvable: goto-def on its use lands on the binder span.
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

// Cross-module handler surface, which the single-file tests above never reach:
// `uri_for`'s cross-module branch, and a `changes` map keyed by two file URIs.
// `lib.al` declares `greet`; `main.al` imports it and calls `lib.greet()`.

const XMOD_LIB: &str = "pub fn greet() Int { 7 }\n";
const XMOD_MAIN: &str = "import ./lib\nx = lib.greet()\nprintln(x)\n";

/// Open both files as client tabs, lib first, so `main.al` is the
/// last-analysed entry.
fn xmod_project() -> (Project, Workspace, String, String) {
    let p = Project::new("xmod_handler");
    p.write("lib.al", XMOD_LIB);
    p.write("main.al", XMOD_MAIN);
    let mut s = Workspace::new();
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

/// Query references on `greet` from `query_uri` with `includeDeclaration` and
/// assert the URIs span exactly lib.al and main.al. Returns the locations and
/// the queried position so callers can layer on more assertions.
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
        "find-refs must span exactly lib.al and main.al, got {uris:?}"
    );
    (arr.clone(), l, c)
}

/// The declaration identifier in lib.al, and the `greet` token of
/// `lib.greet()` in main.al.
fn greet_spans() -> ((i64, i64), (i64, i64)) {
    let at = |src| {
        let (l, c) = cursor(src, "greet", 1, 0);
        (l as i64, c as i64)
    };
    (at(XMOD_LIB), at(XMOD_MAIN))
}

/// Assert `items` holds exactly one entry whose `range.start` is `want`.
fn one_at(items: &[Json], want: (i64, i64), what: &str) {
    assert_eq!(items.len(), 1, "exactly one {what}: {items:?}");
    assert_eq!(start(&items[0]), want, "{what} span: {items:?}");
}

/// Exactly two locations, the lib.al declaration and the main.al use, never the
/// `lib` qualifier. Re-querying without `includeDeclaration` leaves only the
/// use, as an array.
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
        "expected the use in main.al + the decl in lib.al, got {arr:?}"
    );

    let (lib_decl, main_use) = greet_spans();
    assert_eq!(
        location_in(&arr, lib_uri),
        lib_decl,
        "the lib.al location must cover greet's declaration span: {arr:?}"
    );
    assert_eq!(
        location_in(&arr, main_uri),
        main_use,
        "the main.al location must cover the `greet` of `lib.greet()`: {arr:?}"
    );

    let no_decl = refs_at(s, query_uri, l, c, false);
    let arr2 = no_decl
        .as_array()
        .unwrap_or_else(|| panic!("references must be an array, got {no_decl}"));
    one_at(
        arr2,
        main_use,
        "main.al use once the declaration is dropped",
    );
    assert_eq!(
        arr2[0]["uri"].as_str(),
        Some(main_uri),
        "the surviving location is the use in main.al, the declaration is gone: {arr2:?}"
    );
}

/// The `changes` map is keyed by lib.al and main.al, each with one edit at
/// `greet`, never at the `lib` qualifier, each substituting `new_name`.
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

    let (lib_decl, main_use) = greet_spans();
    let edits_for = |uri: &str| {
        changes
            .get(uri)
            .and_then(Json::as_array)
            .unwrap_or_else(|| panic!("edits for {uri}"))
    };
    one_at(edits_for(lib_uri), lib_decl, "lib.al declaration rewrite");
    one_at(
        edits_for(main_uri),
        main_use,
        "main.al rewrite at the `greet` of `lib.greet()`",
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

    // The `greet` of `lib.greet()` must resolve to lib.al, a different file
    // from the one the request came from.
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

    // Landing on the `g` of `pub fn greet`, line 0 column 7.
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

    // Driven from the qualified use in main.al, so `changes` is keyed by two
    // distinct file URIs.
    let (l, c) = cursor(XMOD_MAIN, "greet", 1, 1);
    let resp = rename_at(&mut s, &main_uri, l, c, "salute")
        .expect("a cross-module rename of a user fn must be allowed, not refused");
    assert_rename_spans_lib_and_main(&resp, &main_uri, &lib_uri, "salute");
}

/// `references_response` is the only handler emitting an array of locations,
/// with its own dedup, `include_decl` arm, and per-reference `uri_for` loop.
#[test]
fn cross_module_find_references_spans_decl_and_use_across_files() {
    let (_p, mut s, main_uri, lib_uri) = xmod_project();

    // The use is same-module while the declaration is reached cross-module
    // through the `include_decl` arm.
    assert_refs_span_lib_and_main(&mut s, &main_uri, &main_uri, &lib_uri);
}

// The mirror of the tests above: the query starts at the declaration in the
// imported file. That re-roots compilation to lib.al, and main.al imports
// lib.al rather than the reverse, so main.al falls outside lib.al's import
// closure and its reverse edge must come from persisted xrefs.

#[test]
fn find_references_from_library_declaration_includes_dependent_callers() {
    let (_p, mut s, main_uri, lib_uri) = xmod_project();

    // Cursor on greet's declaration in lib.al.
    assert_refs_span_lib_and_main(&mut s, &lib_uri, &main_uri, &lib_uri);
}

#[test]
fn rename_from_library_declaration_rewrites_dependent_callers() {
    let (_p, mut s, main_uri, lib_uri) = xmod_project();

    // Driven from greet's declaration in lib.al.
    let (l, c) = cursor(XMOD_LIB, "greet", 1, 1);
    let resp = rename_at(&mut s, &lib_uri, l, c, "salute")
        .expect("a rename driven from a library declaration must be allowed");
    assert_rename_spans_lib_and_main(&resp, &main_uri, &lib_uri, "salute");
}

#[test]
fn find_references_from_library_declaration_when_only_library_is_open() {
    // main.al is never opened as a tab, so only the one-time workspace scan
    // analyses it. The reverse edge must be recorded then, not just when an
    // importer happens to be the entry at query time.
    let p = Project::new("xmod_lib_only");
    p.write("lib.al", XMOD_LIB);
    p.write("main.al", XMOD_MAIN);
    let mut s = Workspace::new();
    s.add_workspace_root(p.dir.clone());
    let lib_uri = uri_of(&p, "lib.al");
    let main_uri = uri_of(&p, "main.al");
    s.open_document(&lib_uri, XMOD_LIB);

    // Only the URI set: what matters is that the never-opened caller appears.
    assert_refs_uris_span_lib_and_main(&mut s, &lib_uri, &main_uri, &lib_uri);
}

// A position query that re-roots the entry to another open file must stage
// that file's diagnostics for the transport layer, or its Problems panel goes
// stale after an in-memory edit to a dependency.
#[test]
fn position_query_reroot_stages_diagnostics_for_open_file() {
    let (_p, mut s, main_uri, lib_uri) = xmod_project();
    // Both open; entry is main (last opened). Nothing staged yet.
    assert!(s.take_pending_diagnostics().is_empty());

    // Hover in lib.al re-roots the entry to lib, so lib's diags stage.
    let (l, c) = cursor(XMOD_LIB, "greet", 1, 1);
    s.hover_response(&pos(&lib_uri, l, c));
    let staged = s.take_pending_diagnostics();
    assert_eq!(
        staged.iter().map(|(u, _)| u.as_str()).collect::<Vec<_>>(),
        [lib_uri.as_str()],
        "re-rooting an open file stages its diagnostics"
    );

    // A second query on the now-entry file re-roots nothing.
    s.hover_response(&pos(&lib_uri, l, c));
    assert!(s.take_pending_diagnostics().is_empty());

    // Switching entry back to the other open file stages that one in turn.
    let (l, c) = cursor(XMOD_MAIN, "greet", 1, 1);
    s.hover_response(&pos(&main_uri, l, c));
    let staged = s.take_pending_diagnostics();
    assert_eq!(
        staged.iter().map(|(u, _)| u.as_str()).collect::<Vec<_>>(),
        [main_uri.as_str()]
    );
}

/// The `/** */` at line 0 is `b.al`'s module doc, not `add`'s.
const MODDOC_B: &str = "/**\n * Adds numbers together.\n */\npub fn add(a, b) {\n  a + b\n}\n";

const MODDOC_A: &str = "import ./b\n\nprintln(b.add(10, 20))\n";

/// The `a.al` + `b.al` demo pair, opened as `a.al`.
fn moddoc_project() -> (Project, Workspace, String) {
    let p = Project::new("moddoc");
    p.write("b.al", MODDOC_B);
    p.write("a.al", MODDOC_A);
    let (s, uri) = open(&p, "a.al", MODDOC_A);
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

    // The `b` of `b.add(10, 20)`, a `Qualifier` occurrence on the import.
    let (l, c) = cursor(MODDOC_A, "b.add", 1, 0);
    let md = hover_md(&mut s, &uri, l, c);
    assert!(
        md.contains("Adds numbers together."),
        "hover on the `b` qualifier must render b.al's module doc, got {md:?}"
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

    // The `b` of `b.add(10, 20)` opens b.al at 0:0.
    let (l, c) = cursor(MODDOC_A, "b.add", 1, 0);
    let def = s.definition_response(&pos(&uri, l, c));
    let target = def["uri"]
        .as_str()
        .unwrap_or_else(|| panic!("goto-def on a module alias must open a file, got {def}"));
    assert!(
        target.ends_with("b.al"),
        "`b.add(..)` must jump to b.al, got {target}"
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
/// `Point`, not module `b`.
const SHADOW_A: &str = "import ./b\n\
type Point {\n\
\x20 Point(x Int)\n\
}\n\
pub fn get(b Point) Int {\n\
\x20 b.x\n\
}\n\
p = Point(41)\n\
println(get(p) + b.add(0, 1))\n";

#[test]
fn qualifier_occurrence_respects_a_shadowing_local() {
    let p = Project::new("shadow_qualifier");
    p.write("b.al", MODDOC_B);
    p.write("a.al", SHADOW_A);
    let (mut s, uri) = open(&p, "a.al", SHADOW_A);

    // The `b` of `b.x` inside `get` is a parameter, not the module alias.
    let (l, c) = cursor(SHADOW_A, "b.x", 1, 0);
    let def = s.definition_response(&pos(&uri, l, c));
    let target = def["uri"]
        .as_str()
        .unwrap_or_else(|| panic!("goto-def on a shadowed name must resolve, got {def}"));
    assert!(
        target.ends_with("a.al"),
        "the shadowed `b` must resolve to the local parameter in a.al, got {target}"
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

    // …while the genuinely-qualified `b.add` at top level still is one.
    let (l, c) = cursor(SHADOW_A, "b.add", 1, 0);
    let md = hover_md(&mut s, &uri, l, c);
    assert!(
        md.contains("*module*") && md.contains("Adds numbers together."),
        "the unshadowed `b` still names module b: {md:?}"
    );
}

/// The `b` of `b.add(..)` is a reference to the import, even though it is not
/// the use site the unused-import rule keys off.
#[test]
fn find_refs_on_module_alias_lists_the_qualifier() {
    let (_p, mut s, uri) = moddoc_project();

    let (l, c) = cursor(MODDOC_A, "b.add", 1, 0);
    let refs = refs_at(&mut s, &uri, l, c, false);
    let items = refs
        .as_array()
        .unwrap_or_else(|| panic!("references on a module alias must be an array, got {refs}"));
    assert_eq!(
        items.len(),
        1,
        "the sole reference to alias `b` is the qualifier of `b.add`: {items:?}"
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

/// `al/scheduler` is seeded from the static blob and never re-compiled, so its
/// module doc reaches hover only through `synth_refs_from_interface`.
#[test]
fn hover_on_a_stdlib_module_alias_shows_its_module_doc() {
    let src = "import al/scheduler as s\n\ns.spawn(fn() { Nil })\n";
    let (_p, mut w, uri) = open_single("stdlib_moddoc", src);

    let (l, c) = cursor(src, "s.spawn", 1, 0);
    let md = hover_md(&mut w, &uri, l, c);
    assert!(
        md.contains("*module*"),
        "the `s` of `s.spawn(..)` names a module: {md:?}"
    );
    assert!(
        md.contains("Lightweight processes."),
        "hover must render al/scheduler's module doc: {md:?}"
    );
}

// A function's parameter names are documentation, not part of its type: they
// ride on `Definition::param_names` and are rendered only at hover. Putting
// them in `Type::Function` would force unification to decide whether
// `fn(path String)` equals `fn(p String)`. Constructor field labels are
// semantic and do live in the type; hover mirrors them so they read the same.

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

/// The stdlib is hydrated from the precompiled blob, never compiled from
/// source, so this only passes if `SExport` carries the names.
#[test]
fn hover_shows_parameter_names_across_the_stdlib_blob() {
    let src = "import al/io\n\nprintln(io.read_text('x') or e -> '')\n";
    let (_p, mut s, uri) = open_single("hover_blob", src);
    let md = hover_md(&mut s, &uri, 2, 12); // inside `read_text`
    assert!(
        md.contains("fn(path String)"),
        "stdlib fn must carry its parameter name through the blob, got:\n{md}"
    );
}

/// A constructor's labels come from `ValueKind::Constructor.field_labels`.
#[test]
fn hover_shows_a_constructors_field_labels() {
    let src = "import al/io.{NotFound}\n\nfn f(e IoError) String {\n\tmatch e {\n\t\tNotFound(p) -> p\n\t\telse -> ''\n\t}\n}\nprintln(1)\n";
    let (_p, mut s, uri) = open_single("hover_ctor", src);
    let md = hover_md(&mut s, &uri, 4, 4); // the `NotFound` pattern
    assert!(
        md.contains("fn(path String) IoError"),
        "constructor must show its field label, got:\n{md}"
    );
}

/// A declaration's doc comment must cross a module boundary, carried by
/// `ExportedValue::doc`.
#[test]
fn hover_shows_a_declaration_doc_from_another_module() {
    let p = Project::new("hover_xmod_doc");
    p.write(
        "lib.al",
        "/** Text helpers. */\n\n/** Join `parts` with `sep`. */\npub fn join(parts Array(String), sep String) String {\n\t'x'\n}\n",
    );
    let src = "import ./lib\n\nprintln(lib.join(['a'], ','))\n";
    p.write("main.al", src);
    let (mut s, uri) = open(&p, "main.al", src);
    let md = hover_md(&mut s, &uri, 2, 12); // inside `join`
    assert!(
        md.contains("fn(parts Array(String), sep String)"),
        "labels must cross the module boundary, got:\n{md}"
    );
    assert!(
        md.contains("Join `parts` with `sep`."),
        "declaration doc must cross the module boundary, got:\n{md}"
    );
}

/// With a selective import, the importer's persisted xrefs must carry the
/// `{greet}` binding token as well as the call. Dropping the token leaves the
/// import naming a symbol that no longer exists.
#[test]
fn rename_from_library_declaration_rewrites_selective_import_token() {
    const LIB: &str = "pub fn greet() Int { 7 }\n";
    const MAIN: &str = "import ./lib.{greet}\nx = greet()\nprintln(x)\n";
    let p = Project::new("xmod_selective_rename");
    p.write("lib.al", LIB);
    p.write("main.al", MAIN);
    let (mut s, lib_uri) = open(&p, "lib.al", LIB);
    let main_uri = uri_of(&p, "main.al");

    // Driven from greet's declaration in lib.al.
    let (l, c) = cursor(LIB, "greet", 1, 1);
    let resp = rename_at(&mut s, &lib_uri, l, c, "salute")
        .expect("a rename driven from a library declaration must be allowed");
    let changes = resp
        .get("changes")
        .and_then(Json::as_object)
        .expect("a WorkspaceEdit with a `changes` map");

    let main_edits = changes
        .get(&main_uri)
        .and_then(Json::as_array)
        .unwrap_or_else(|| panic!("edits for main.al: {changes:?}"));
    let mut starts: Vec<(i64, i64)> = main_edits
        .iter()
        .map(|e| {
            (
                e["range"]["start"]["line"].as_i64().unwrap(),
                e["range"]["start"]["character"].as_i64().unwrap(),
            )
        })
        .collect();
    starts.sort_unstable();
    // The `{greet}` import token (line 0) and the `greet()` call (line 1).
    let tok = cursor(MAIN, "greet", 1, 0);
    let call = cursor(MAIN, "greet", 2, 0);
    assert_eq!(
        starts,
        vec![(tok.0 as i64, tok.1 as i64), (call.0 as i64, call.1 as i64)],
        "main.al must rewrite the import's binding token and the call: {main_edits:?}"
    );
}

// `textDocument/completion`: after `alias.` the imported module's public
// exports appear; a bare position lists the module's own top-level
// declarations; a dot after a non-module value yields the empty list.

/// Completion labels at (`l`, `c`), sorted as the server returns them.
fn completion_labels(s: &mut Workspace, uri: &str, l: i32, c: i32) -> Vec<String> {
    let resp = s.completion_response(&pos(uri, l, c));
    resp.as_array()
        .unwrap_or_else(|| panic!("completion must be an array, got {resp}"))
        .iter()
        .filter_map(|i| i["label"].as_str().map(str::to_string))
        .collect()
}

const COMP_STDLIB: &str = "import al/string\nx = string.\n";

#[test]
fn completion_after_stdlib_alias_dot_lists_exports() {
    let (_p, mut s, uri) = open_single("comp_std", COMP_STDLIB);

    // Cursor right after the trailing dot; the statement is a parse error the
    // recovering parser drops, which completion must survive.
    let labels = completion_labels(&mut s, &uri, 1, 11);
    assert!(
        labels.iter().any(|l| l == "length"),
        "`string.` must offer al/string's `length`, got {labels:?}"
    );
}

const COMP_HELPER: &str = "pub fn greet() String {\n\x20 'hi'\n}\nfn hidden() Int { 1 }\n";
const COMP_MAIN: &str = "import ./helper\nz = helper.\n";

#[test]
fn completion_after_file_module_alias_dot_lists_pub_exports_only() {
    let p = Project::new("comp_file");
    p.write("helper.al", COMP_HELPER);
    p.write("main.al", COMP_MAIN);
    let (mut s, uri) = open(&p, "main.al", COMP_MAIN);

    let labels = completion_labels(&mut s, &uri, 1, 11);
    assert!(
        labels.iter().any(|l| l == "greet"),
        "`helper.` must offer the pub `greet`, got {labels:?}"
    );
    assert!(
        !labels.iter().any(|l| l == "hidden"),
        "a private fn must not be offered, got {labels:?}"
    );
}

const COMP_BARE: &str = "import al/string\npub fn go() Int { 7 }\nx = g\n";

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
