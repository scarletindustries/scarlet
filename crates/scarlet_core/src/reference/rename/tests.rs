use super::{ReferenceKind as K, *};
use crate::reference::tests::{add_ref, def, mp, stub_kind};
use crate::reference::{
    Definition, DefinitionKind, ModuleReferences, Reference, ReferenceGraphBuilder,
};
use std::collections::HashMap;

/// lib defines a `pub` `helper`. app imports it, uses it twice, and has
/// its own private `run`.
fn workspace() -> (ReferenceGraph, ModuleId, ModuleId, DefId) {
    let mut g = ReferenceGraphBuilder::new();
    let lib = g.intern_module(&mp(&["sub", "lib"]));
    let app = g.intern_module(&mp(&["main"]));

    let helper = def(lib, 0, 3, 9, EntityKind::Function);
    let mut lib_mr = ModuleReferences::new(lib);
    lib_mr.add_definition(Definition::new(
        helper.module,
        helper.span,
        "helper",
        None,
        true,
        stub_kind(helper),
    ));
    lib_mr.add_reference(
        Some(helper),
        Reference::new(helper.span, K::Definition, helper),
    );
    g.insert_module(lib_mr);

    let run = def(app, 0, 3, 6, EntityKind::Function);
    let mut app_mr = ModuleReferences::new(app);
    app_mr.add_definition(Definition::new(
        run.module,
        run.span,
        "run",
        None,
        false,
        stub_kind(run),
    ));
    // import path occurrence (must NOT be rewritten by a symbol rename)
    add_ref(&mut app_mr, None, (0, 7, 14), K::Import, helper);
    add_ref(&mut app_mr, Some(run), (2, 8, 14), K::Qualified, helper);
    add_ref(&mut app_mr, Some(run), (3, 8, 14), K::Qualified, helper);
    g.insert_module(app_mr);

    (g.finish(), lib, app, helper)
}

fn resolver<'a>(
    map: &'a HashMap<ModuleId, &'a str>,
) -> impl Fn(ModuleId) -> Result<String, ModuleUriError> + 'a {
    move |m| {
        map.get(&m)
            .map(|s| s.to_string())
            .ok_or(ModuleUriError::NoPath)
    }
}

#[test]
fn cross_module_rename_keyed_by_uri() {
    let (g, lib, app, helper) = workspace();
    let mut map = HashMap::new();
    map.insert(lib, "file:///proj/sub/lib.sca");
    map.insert(app, "file:///proj/main.sca");

    let we = g
        .rename_with(helper, "renamed", resolver(&map))
        .expect("rename ok");

    assert_eq!(we.changes.len(), 2);

    // The declaration span and the Definition occurrence merge into one
    // edit.
    let lib_edits = &we.changes["file:///proj/sub/lib.sca"];
    assert_eq!(lib_edits.len(), 1);
    assert_eq!(lib_edits[0].span, Span::single_line(0, 3, 9));
    assert_eq!(lib_edits[0].new_text, "renamed");

    let app_edits = &we.changes["file:///proj/main.sca"];
    assert_eq!(app_edits.len(), 2);
    assert_eq!(app_edits[0].span, Span::single_line(2, 8, 14));
    assert_eq!(app_edits[1].span, Span::single_line(3, 8, 14));
    assert!(app_edits.iter().all(|e| e.new_text == "renamed"));
    assert!(
        !app_edits
            .iter()
            .any(|e| e.span == Span::single_line(0, 7, 14)),
        "import-path span must never be rewritten"
    );
}

#[test]
fn declaration_rewritten_without_explicit_definition_occurrence() {
    // No Definition occurrence recorded; the decl must still be renamed.
    let mut g = ReferenceGraphBuilder::new();
    let m = g.intern_module(&mp(&["main"]));
    let foo = def(m, 0, 3, 6, EntityKind::Function);
    let mut mr = ModuleReferences::new(m);
    mr.add_definition(Definition::new(
        foo.module,
        foo.span,
        "foo",
        None,
        false,
        stub_kind(foo),
    ));
    add_ref(&mut mr, Some(foo), (5, 4, 7), K::Unqualified, foo);
    g.insert_module(mr);
    let g = g.finish();

    let mut map = HashMap::new();
    map.insert(m, "file:///main.sca");
    let we = g.rename_with(foo, "bar", resolver(&map)).expect("ok");
    let edits = &we.changes["file:///main.sca"];
    assert_eq!(edits.len(), 2, "declaration + the one use");
    assert_eq!(edits[0].span, Span::single_line(0, 3, 6));
    assert_eq!(edits[1].span, Span::single_line(5, 4, 7));
}

#[test]
fn prepare_rename_returns_precise_range_and_placeholder() {
    let (g, _lib, app, helper) = workspace();
    // Cursor inside the first qualified use at app line 2, col 10.
    let p = g.prepare_rename(app, 2, 10).expect("prepare ok");
    assert_eq!(p.def, helper);
    assert_eq!(p.range, Span::single_line(2, 8, 14));
    assert_eq!(p.placeholder, "helper");
}

#[test]
fn prepare_rename_rejects_stdlib() {
    let mut g = ReferenceGraphBuilder::new();
    let std_mod = g.intern_module(&mp(&["scarlet", "array"]));
    let user = g.intern_module(&mp(&["main"]));
    let map_fn = def(std_mod, 0, 4, 7, EntityKind::Function);
    let mut std_mr = ModuleReferences::new(std_mod);
    std_mr.add_definition(Definition::new(
        map_fn.module,
        map_fn.span,
        "map",
        None,
        true,
        stub_kind(map_fn),
    ));
    g.insert_module(std_mr);

    let mut user_mr = ModuleReferences::new(user);
    add_ref(&mut user_mr, None, (1, 2, 5), K::Qualified, map_fn);
    g.insert_module(user_mr);
    let g = g.finish();

    let err = g.prepare_rename(user, 1, 3).unwrap_err();
    assert_eq!(err, RenameError::NotRenameable(NotRenameableReason::Stdlib));
    let err2 = g.rename(map_fn, "renamed", None).unwrap_err();
    assert_eq!(
        err2,
        RenameError::NotRenameable(NotRenameableReason::Stdlib)
    );
}

#[test]
fn prepare_rename_rejects_module_path_position() {
    let (g, _lib, app, _helper) = workspace();
    // app line 0 cols [7,14) is the Import occurrence.
    let err = g.prepare_rename(app, 0, 9).unwrap_err();
    assert_eq!(
        err,
        RenameError::NotRenameable(NotRenameableReason::ModulePath),
        "got {err:?}"
    );
}

#[test]
fn prepare_rename_and_rename_reject_module_alias() {
    let mut g = ReferenceGraphBuilder::new();
    let m = g.intern_module(&mp(&["main"]));
    let alias = def(m, 0, 7, 10, EntityKind::ModuleAlias);
    let mut mr = ModuleReferences::new(m);
    mr.add_definition(Definition::new(
        alias.module,
        alias.span,
        "lib",
        None,
        false,
        stub_kind(alias),
    ));
    g.insert_module(mr);
    let g = g.finish();

    // Cursor on the alias binding: core must refuse, not the LSP layer.
    assert_eq!(
        g.prepare_rename(m, 0, 8).unwrap_err(),
        RenameError::NotRenameable(NotRenameableReason::ModuleAlias)
    );
    assert_eq!(
        g.rename(alias, "renamed", Some((m, "file:///main.sca")))
            .unwrap_err(),
        RenameError::NotRenameable(NotRenameableReason::ModuleAlias)
    );
}

#[test]
fn prepare_rename_not_found_off_any_symbol() {
    let (g, _lib, app, _helper) = workspace();
    assert_eq!(g.prepare_rename(app, 99, 0), Err(RenameError::NotFound));
}

#[test]
fn rename_rejects_invalid_new_names() {
    let (g, lib, app, helper) = workspace();
    let mut map = HashMap::new();
    map.insert(lib, "file:///lib.sca");
    map.insert(app, "file:///main.sca");
    for bad in ["", "1abc", "has space", "fn", "match", "a-b", "x!"] {
        let e = g.rename_with(helper, bad, resolver(&map)).unwrap_err();
        assert!(
            matches!(e, RenameError::InvalidName(_)),
            "{bad:?} should be invalid, got {e:?}"
        );
    }
    for ok in ["renamed", "_x", "a1", "snake_case", "CamelCase"] {
        assert!(
            g.rename_with(helper, ok, resolver(&map)).is_ok(),
            "{ok:?} should be valid"
        );
    }
}

#[test]
fn rename_refuses_when_a_module_uri_is_unresolvable() {
    let (g, lib, _app, helper) = workspace();
    // Only lib resolves, so the app uses cannot be located.
    let mut map = HashMap::new();
    map.insert(lib, "file:///lib.sca");
    let err = g
        .rename_with(helper, "renamed", resolver(&map))
        .unwrap_err();
    match err {
        RenameError::Unresolvable(mods) => {
            assert_eq!(mods, vec![(mp(&["main"]), ModuleUriError::NoPath)]);
        }
        other => panic!("expected Unresolvable, got {other:?}"),
    }
}

#[test]
fn rename_unknown_def_is_not_found() {
    let (g, _lib, _app, _helper) = workspace();
    let ghost = def(ModuleId(999), 0, 0, 1, EntityKind::Function);
    assert_eq!(
        g.rename_with(ghost, "x", |_| Ok("file:///x".to_string())),
        Err(RenameError::NotFound)
    );
}

#[test]
fn rename_via_module_resolve_with_entry_override() {
    // The bare ["main"] entry module cannot be resolved; `entry` pins it.
    let (g, _lib, app, _helper) = workspace();
    let run = def(app, 0, 3, 6, EntityKind::Function);
    let we = g
        .rename(run, "go", Some((app, "file:///proj/main.sca")))
        .expect("entry-pinned rename ok");
    assert_eq!(we.changes.len(), 1);
    let edits = &we.changes["file:///proj/main.sca"];
    assert_eq!(edits[0].span, Span::single_line(0, 3, 6));
    assert_eq!(edits[0].new_text, "go");
}

/// `type Config { .. }` makes a Type and an implicit Constructor at the same
/// span. This adds one use of each.
fn record_shorthand() -> (ReferenceGraph, ModuleId, DefId, DefId) {
    let mut g = ReferenceGraphBuilder::new();
    let m = g.intern_module(&mp(&["main"]));
    let ty = def(m, 0, 5, 11, EntityKind::Type);
    let ctor = def(m, 0, 5, 11, EntityKind::Constructor);
    let mut mr = ModuleReferences::new(m);
    mr.add_definition(Definition::new(
        ty.module,
        ty.span,
        "Config",
        None,
        false,
        stub_kind(ty),
    ));
    mr.add_definition(Definition::new(
        ctor.module,
        ctor.span,
        "Config",
        None,
        false,
        DefinitionKind::Constructor {
            ctor_of: Some(ty),
            param_names: Vec::new(),
        },
    ));
    add_ref(&mut mr, None, (3, 10, 16), K::Unqualified, ty);
    add_ref(&mut mr, None, (5, 8, 14), K::Unqualified, ctor);
    g.insert_module(mr);
    (g.finish(), m, ty, ctor)
}

#[test]
fn record_shorthand_rename_from_type_rewrites_ctor_uses() {
    let (g, m, ty, _ctor) = record_shorthand();
    let mut map = HashMap::new();
    map.insert(m, "file:///main.sca");
    let we = g.rename_with(ty, "Settings", resolver(&map)).expect("ok");
    let edits = &we.changes["file:///main.sca"];
    assert_eq!(edits.len(), 3, "decl + type use + ctor use, got {edits:?}");
    assert_eq!(edits[0].span, Span::single_line(0, 5, 11));
    assert_eq!(edits[1].span, Span::single_line(3, 10, 16));
    assert_eq!(edits[2].span, Span::single_line(5, 8, 14));
    assert!(edits.iter().all(|e| e.new_text == "Settings"));
}

#[test]
fn record_shorthand_rename_from_ctor_rewrites_type_uses() {
    let (g, m, _ty, ctor) = record_shorthand();
    let mut map = HashMap::new();
    map.insert(m, "file:///main.sca");
    let we = g.rename_with(ctor, "Settings", resolver(&map)).expect("ok");
    let edits = &we.changes["file:///main.sca"];
    assert_eq!(edits.len(), 3, "decl + type use + ctor use, got {edits:?}");
    assert_eq!(edits[0].span, Span::single_line(0, 5, 11));
    assert_eq!(edits[1].span, Span::single_line(3, 10, 16));
    assert_eq!(edits[2].span, Span::single_line(5, 8, 14));
}

#[test]
fn multi_constructor_type_rename_keeps_ctors_separate() {
    // In `type Shape { A B }` the constructors have their own spans, so
    // renaming the type must not rename them too, and the other way round.
    let mut g = ReferenceGraphBuilder::new();
    let m = g.intern_module(&mp(&["main"]));
    let ty = def(m, 0, 5, 10, EntityKind::Type);
    let a = def(m, 1, 2, 3, EntityKind::Constructor);
    let b = def(m, 2, 2, 3, EntityKind::Constructor);
    let mut mr = ModuleReferences::new(m);
    mr.add_definition(Definition::new(
        ty.module,
        ty.span,
        "Shape",
        None,
        false,
        stub_kind(ty),
    ));
    for (c, n) in [(a, "A"), (b, "B")] {
        mr.add_definition(Definition::new(
            c.module,
            c.span,
            n,
            None,
            false,
            DefinitionKind::Constructor {
                ctor_of: Some(ty),
                param_names: Vec::new(),
            },
        ));
    }
    add_ref(&mut mr, None, (4, 8, 9), K::Unqualified, a);
    add_ref(&mut mr, None, (6, 10, 15), K::Unqualified, ty);
    g.insert_module(mr);
    let g = g.finish();

    let mut map = HashMap::new();
    map.insert(m, "file:///main.sca");

    let we = g.rename_with(ty, "Form", resolver(&map)).expect("ok");
    let edits = &we.changes["file:///main.sca"];
    assert_eq!(edits.len(), 2, "type decl + type use only, got {edits:?}");
    assert_eq!(edits[0].span, Span::single_line(0, 5, 10));
    assert_eq!(edits[1].span, Span::single_line(6, 10, 15));

    let we = g.rename_with(a, "C", resolver(&map)).expect("ok");
    let edits = &we.changes["file:///main.sca"];
    assert_eq!(edits.len(), 2, "ctor decl + ctor use only, got {edits:?}");
    assert_eq!(edits[0].span, Span::single_line(1, 2, 3));
    assert_eq!(edits[1].span, Span::single_line(4, 8, 9));
}

#[test]
fn identifier_validation_matches_scanner_grammar() {
    assert!(is_valid_identifier("foo"));
    assert!(is_valid_identifier("_priv"));
    assert!(is_valid_identifier("a_1B"));
    assert!(!is_valid_identifier(""));
    assert!(!is_valid_identifier("9x"));
    assert!(!is_valid_identifier("a b"));
    assert!(!is_valid_identifier("a.b"));
    assert!(!is_valid_identifier("pub")); // keyword
    assert!(!is_valid_identifier("type")); // keyword
}
