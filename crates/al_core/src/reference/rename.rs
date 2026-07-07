//! Rename projection + prepare-rename safety.
//!
//! This is the correctness-critical seam: given a [`DefId`] and a new name,
//! project the workspace reverse edges into an LSP `WorkspaceEdit` keyed by
//! file URI, and given a cursor position decide whether a rename is even legal
//! (rejecting the standard library, `@vm`/external intrinsics, the prelude,
//! and module-path positions).
//!
//! Layering: `al_core` has no `serde_json`, so the projection produces a pure
//! [`WorkspaceEdit`] of [`TextEdit`]s; the LSP layer (in the `al` crate)
//! shapes it into the wire JSON. All of the *logic* — which spans to rewrite,
//! which positions are renameable, how a [`ModuleId`] becomes a URI — lives
//! here so it is testable without an editor.
//!
//! Span/URI invariants:
//! * Every recorded [`Reference::span`] is exactly the identifier token that
//!   spells the target's name (see [`ReferenceKind`]). The projection is a
//!   pure span -> new-text rewrite and relies on that contract.
//! * [`ReferenceKind::Import`] spans the *module path*, not a symbol name, so
//!   it is never rewritten by a symbol rename (and renaming *at* such a
//!   position is rejected up front).
//! * The declaration site is always rewritten, whether or not a populator
//!   recorded an explicit [`ReferenceKind::Definition`] occurrence for it;
//!   identical edits are de-duplicated so the two paths can't double-apply.

use std::collections::BTreeMap;
use std::path::Path;

use crate::module::ModulePath;
use crate::span::Span;
use crate::token;

use super::uri::module_uri;
use super::{DefId, EntityKind, ModuleId, ReferenceGraph, ReferenceKind};

/// A single text replacement within one file. `span` is in the compiler's
/// 0-based, end-exclusive coordinate space (what the LSP already consumes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextEdit {
    pub span: Span,
    pub new_text: String,
}

/// A cross-file rename, keyed by file URI. `BTreeMap`/sorted vecs make the
/// output deterministic (stable tests, stable client behaviour).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspaceEdit {
    pub changes: BTreeMap<String, Vec<TextEdit>>,
}

/// Result of `textDocument/prepareRename`: the canonical definition being
/// renamed, the exact source range of the identifier under the cursor (what
/// the editor highlights), and the current name as the rename placeholder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedRename {
    pub def: DefId,
    pub range: Span,
    pub placeholder: String,
}

/// Why a resolved definition is refused for rename.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotRenameableReason {
    /// Defined in the precompiled standard library / prelude / `@vm`.
    Stdlib,
    /// Cursor sits on the module-path segment of an `import a/b` — that is a
    /// file rename, not a symbol rename.
    ModulePath,
    /// Target is a `ModuleAlias` (an `import` declaration binding). Renaming
    /// it would rewrite the import line while every qualified `q.member` use
    /// targets the remote member, not the alias, so callers would be orphaned.
    ModuleAlias,
}

/// Why a rename / prepare-rename was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenameError {
    /// Nothing renameable under the cursor / no such definition.
    NotFound,
    /// The symbol exists but must not be rewritten.
    NotRenameable(NotRenameableReason),
    /// The requested new name is not a legal identifier.
    InvalidName(String),
    /// One or more modules holding occurrences could not be mapped to a file
    /// URI, so applying the rename would silently corrupt or partially apply.
    /// Refusing is safer than a partial rewrite.
    Unresolvable(Vec<ModulePath>),
}

impl RenameError {
    /// Human-readable message for the LSP `ResponseError`.
    pub fn message(&self) -> String {
        match self {
            RenameError::NotFound => "no renameable symbol at this position".to_string(),
            RenameError::NotRenameable(NotRenameableReason::Stdlib) => {
                "cannot rename a symbol defined in the standard library".to_string()
            }
            RenameError::NotRenameable(NotRenameableReason::ModulePath) => {
                "cannot rename a module path; rename the file instead".to_string()
            }
            RenameError::NotRenameable(NotRenameableReason::ModuleAlias) => {
                "cannot rename an import declaration".to_string()
            }
            RenameError::InvalidName(n) => format!("`{n}` is not a valid identifier"),
            RenameError::Unresolvable(mods) => {
                let list = mods
                    .iter()
                    .map(|m| m.join("/"))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("cannot locate the source file for: {list}")
            }
        }
    }
}

/// Validate a proposed new name against al's identifier grammar (mirrors the
/// scanner: leading `[A-Za-z_]`, rest `[A-Za-z0-9_]`) and reject keywords.
pub(crate) fn is_valid_identifier(name: &str) -> bool {
    let mut bytes = name.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    if !token::is_name_start(first) {
        return false;
    }
    if !bytes.all(token::is_name_continue) {
        return false;
    }
    token::match_keyword(name).is_none()
}

impl ReferenceGraph {
    fn reject_if_not_renameable(&self, def: DefId) -> Result<(), RenameError> {
        if def.entity == EntityKind::ModuleAlias {
            return Err(RenameError::NotRenameable(NotRenameableReason::ModuleAlias));
        }
        if let Some(path) = self.module_path(def.module)
            && crate::module::is_stdlib(path)
        {
            return Err(RenameError::NotRenameable(NotRenameableReason::Stdlib));
        }
        Ok(())
    }

    /// `textDocument/prepareRename`: validate that the position names a
    /// renameable symbol and return the precise identifier range.
    pub fn prepare_rename(
        &self,
        module: ModuleId,
        line: i32,
        col: i32,
    ) -> Result<PreparedRename, RenameError> {
        let hit = self
            .module_refs(module)
            .and_then(|mr| mr.cursor_hit(line, col))
            .ok_or(RenameError::NotFound)?;

        // A module-path position is an import path, not a symbol — renaming it
        // would be a module/file rename, which is out of scope.
        if hit.kind == ReferenceKind::Import {
            return Err(RenameError::NotRenameable(NotRenameableReason::ModulePath));
        }

        let def = self.definition(hit.target).ok_or(RenameError::NotFound)?;

        self.reject_if_not_renameable(hit.target)?;

        Ok(PreparedRename {
            def: hit.target,
            range: hit.range,
            placeholder: def.name.clone(),
        })
    }

    /// Project `def -> new_name` into a `WorkspaceEdit`, resolving every
    /// occurrence's [`ModuleId`] to a URI through `uri_of`. The generic form;
    /// [`rename`](Self::rename) wraps it with the spec's `module::resolve`
    /// translation plus an entry-module override.
    pub(crate) fn rename_with<F>(
        &self,
        def: DefId,
        new_name: &str,
        uri_of: F,
    ) -> Result<WorkspaceEdit, RenameError>
    where
        F: Fn(ModuleId) -> Option<String>,
    {
        if !is_valid_identifier(new_name) {
            return Err(RenameError::InvalidName(new_name.to_string()));
        }

        let def_record = self.definition(def).ok_or(RenameError::NotFound)?;

        self.reject_if_not_renameable(def)?;

        // (module, span) pairs to rewrite. Always include the declaration's
        // own name span; reverse edges supply every use. `Import` occurrences
        // span the module path, never the symbol name, so they are skipped.
        let mut sites: Vec<(ModuleId, Span)> = vec![(def.module, def_record.span())];
        for r in self.references_to(def) {
            if r.kind == ReferenceKind::Import {
                continue;
            }
            sites.push((r.module, r.span));
        }

        // Resolve every involved module to a URI up front; refuse the whole
        // rename if any can't be located (a partial rewrite is worse than no
        // rewrite).
        let mut resolved: BTreeMap<ModuleId, Option<String>> = BTreeMap::new();
        for (m, _) in &sites {
            resolved.entry(*m).or_insert_with(|| uri_of(*m));
        }
        let unresolved: Vec<ModuleId> = resolved
            .iter()
            .filter_map(|(m, u)| u.is_none().then_some(*m))
            .collect();
        if !unresolved.is_empty() {
            let mods = unresolved
                .into_iter()
                .filter_map(|m| self.module_path(m).cloned())
                .collect();
            return Err(RenameError::Unresolvable(mods));
        }

        // Past the check above every entry is Some, so the build loop can
        // index unconditionally.
        let uris: BTreeMap<ModuleId, String> = resolved
            .into_iter()
            .filter_map(|(m, u)| u.map(|u| (m, u)))
            .collect();

        let mut changes: BTreeMap<String, Vec<TextEdit>> = BTreeMap::new();
        for (m, span) in sites {
            changes.entry(uris[&m].clone()).or_default().push(TextEdit {
                span,
                new_text: new_name.to_string(),
            });
        }
        for edits in changes.values_mut() {
            edits.sort_by_key(|e| (e.span.start_line, e.span.start_column));
            edits.dedup();
        }

        Ok(WorkspaceEdit { changes })
    }

    /// Project `def -> new_name` into a `WorkspaceEdit`, translating each
    /// module to a URI via [`crate::module::resolve`] relative to `base_dir`.
    /// `entry` lets the caller pin one module (the open buffer / bare entry
    /// module, which `resolve` deliberately can't locate) to a known URI.
    pub fn rename(
        &self,
        def: DefId,
        new_name: &str,
        base_dir: Option<&Path>,
        entry: Option<(ModuleId, &str)>,
    ) -> Result<WorkspaceEdit, RenameError> {
        self.rename_with(def, new_name, |m| {
            if let Some((eid, euri)) = entry
                && m == eid
            {
                return Some(euri.to_string());
            }
            module_uri(self, m, base_dir)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{ReferenceKind as K, *};
    use crate::reference::{Definition, ModuleReferences, Reference, add_ref, def, mp};
    use crate::span::range_span;
    use std::collections::HashMap;

    /// lib defines `helper` (pub); app imports it and uses it twice plus its
    /// own private `run`. Returns the graph and the key ids.
    fn workspace() -> (ReferenceGraph, ModuleId, ModuleId, DefId) {
        let mut g = ReferenceGraph::new();
        let lib = g.intern_module(&mp(&["sub", "lib"]));
        let app = g.intern_module(&mp(&["main"]));

        let helper = def(lib, 0, 3, 9, EntityKind::Function);
        let mut lib_mr = ModuleReferences::new(lib);
        lib_mr.add_definition(Definition::new(helper, "helper", None, true));
        // Populator-recorded Definition occurrence at the decl site.
        lib_mr.add_reference(
            Some(helper),
            Reference::new(helper.span, K::Definition, helper),
        );
        g.insert_module(lib_mr);

        let run = def(app, 0, 3, 6, EntityKind::Function);
        let mut app_mr = ModuleReferences::new(app);
        app_mr.add_definition(Definition::new(run, "run", None, false));
        // import path occurrence (must NOT be rewritten by a symbol rename)
        add_ref(&mut app_mr, None, (0, 7, 14), K::Import, helper);
        // two qualified uses of helper
        add_ref(&mut app_mr, Some(run), (2, 8, 14), K::Qualified, helper);
        add_ref(&mut app_mr, Some(run), (3, 8, 14), K::Qualified, helper);
        g.insert_module(app_mr);

        (g, lib, app, helper)
    }

    fn resolver<'a>(
        map: &'a HashMap<ModuleId, &'a str>,
    ) -> impl Fn(ModuleId) -> Option<String> + 'a {
        move |m| map.get(&m).map(|s| s.to_string())
    }

    #[test]
    fn cross_module_rename_keyed_by_uri() {
        let (g, lib, app, helper) = workspace();
        let mut map = HashMap::new();
        map.insert(lib, "file:///proj/sub/lib.al");
        map.insert(app, "file:///proj/main.al");

        let we = g
            .rename_with(helper, "renamed", resolver(&map))
            .expect("rename ok");

        // Two files touched.
        assert_eq!(we.changes.len(), 2);

        // lib: just the declaration (decl span + the Definition occurrence
        // dedupe to one edit).
        let lib_edits = &we.changes["file:///proj/sub/lib.al"];
        assert_eq!(lib_edits.len(), 1);
        assert_eq!(lib_edits[0].span, range_span(0, 3, 9));
        assert_eq!(lib_edits[0].new_text, "renamed");

        // app: the two qualified uses, in source order. The Import occurrence
        // is NOT rewritten.
        let app_edits = &we.changes["file:///proj/main.al"];
        assert_eq!(app_edits.len(), 2);
        assert_eq!(app_edits[0].span, range_span(2, 8, 14));
        assert_eq!(app_edits[1].span, range_span(3, 8, 14));
        assert!(app_edits.iter().all(|e| e.new_text == "renamed"));
        assert!(
            !app_edits.iter().any(|e| e.span == range_span(0, 7, 14)),
            "import-path span must never be rewritten"
        );
    }

    #[test]
    fn declaration_rewritten_without_explicit_definition_occurrence() {
        // No Definition occurrence recorded; the decl must still be renamed.
        let mut g = ReferenceGraph::new();
        let m = g.intern_module(&mp(&["main"]));
        let foo = def(m, 0, 3, 6, EntityKind::Function);
        let mut mr = ModuleReferences::new(m);
        mr.add_definition(Definition::new(foo, "foo", None, false));
        add_ref(&mut mr, Some(foo), (5, 4, 7), K::Unqualified, foo);
        g.insert_module(mr);

        let mut map = HashMap::new();
        map.insert(m, "file:///main.al");
        let we = g.rename_with(foo, "bar", resolver(&map)).expect("ok");
        let edits = &we.changes["file:///main.al"];
        assert_eq!(edits.len(), 2, "declaration + the one use");
        assert_eq!(edits[0].span, range_span(0, 3, 6));
        assert_eq!(edits[1].span, range_span(5, 4, 7));
    }

    #[test]
    fn prepare_rename_returns_precise_range_and_placeholder() {
        let (g, _lib, app, helper) = workspace();
        // Cursor inside the first qualified use at app line 2, col 10.
        let p = g.prepare_rename(app, 2, 10).expect("prepare ok");
        assert_eq!(p.def, helper);
        assert_eq!(p.range, range_span(2, 8, 14));
        assert_eq!(p.placeholder, "helper");
    }

    #[test]
    fn prepare_rename_rejects_stdlib() {
        let mut g = ReferenceGraph::new();
        let std_mod = g.intern_module(&mp(&["al", "array"]));
        let user = g.intern_module(&mp(&["main"]));
        let map_fn = def(std_mod, 0, 4, 7, EntityKind::Function);
        let mut std_mr = ModuleReferences::new(std_mod);
        std_mr.add_definition(Definition::new(map_fn, "map", None, true));
        g.insert_module(std_mr);

        let mut user_mr = ModuleReferences::new(user);
        add_ref(&mut user_mr, None, (1, 2, 5), K::Qualified, map_fn);
        g.insert_module(user_mr);

        // prepareRename at the use of a stdlib symbol -> rejected.
        let err = g.prepare_rename(user, 1, 3).unwrap_err();
        assert_eq!(err, RenameError::NotRenameable(NotRenameableReason::Stdlib));
        // and rename() itself refuses too.
        let err2 = g.rename(map_fn, "renamed", None, None).unwrap_err();
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
        let mut g = ReferenceGraph::new();
        let m = g.intern_module(&mp(&["main"]));
        let alias = def(m, 0, 7, 10, EntityKind::ModuleAlias);
        let mut mr = ModuleReferences::new(m);
        mr.add_definition(Definition::new(alias, "lib", None, false));
        g.insert_module(mr);

        // Cursor on the alias binding — the wide `ModuleAlias` definition span
        // resolves here, and core must refuse it (previously the LSP layer had
        // to remember to post-filter this in three places).
        assert_eq!(
            g.prepare_rename(m, 0, 8).unwrap_err(),
            RenameError::NotRenameable(NotRenameableReason::ModuleAlias)
        );
        assert_eq!(
            g.rename(alias, "renamed", None, Some((m, "file:///main.al")))
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
        map.insert(lib, "file:///lib.al");
        map.insert(app, "file:///main.al");
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
        // Only resolve lib, not app -> the app uses can't be located.
        let mut map = HashMap::new();
        map.insert(lib, "file:///lib.al");
        let err = g
            .rename_with(helper, "renamed", resolver(&map))
            .unwrap_err();
        match err {
            RenameError::Unresolvable(mods) => {
                assert_eq!(mods, vec![mp(&["main"])]);
            }
            other => panic!("expected Unresolvable, got {other:?}"),
        }
    }

    #[test]
    fn rename_unknown_def_is_not_found() {
        let (g, _lib, _app, _helper) = workspace();
        let ghost = def(ModuleId(999), 0, 0, 1, EntityKind::Function);
        assert_eq!(
            g.rename_with(ghost, "x", |_| Some("file:///x".to_string())),
            Err(RenameError::NotFound)
        );
    }

    #[test]
    fn rename_via_module_resolve_with_entry_override() {
        // The entry module is the bare ["main"] path, which module::resolve
        // deliberately cannot locate; the caller pins it via `entry`.
        let (g, _lib, app, _helper) = workspace();
        let run = def(app, 0, 3, 6, EntityKind::Function);
        // run is private to app, only referenced at its declaration.
        let we = g
            .rename(run, "go", None, Some((app, "file:///proj/main.al")))
            .expect("entry-pinned rename ok");
        assert_eq!(we.changes.len(), 1);
        let edits = &we.changes["file:///proj/main.al"];
        assert_eq!(edits[0].span, range_span(0, 3, 6));
        assert_eq!(edits[0].new_text, "go");
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
}
