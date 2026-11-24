//! Turns a rename into edits, and checks that prepare-rename is safe.
//!
//! `scarlet_core` has no `serde_json`, so this produces a plain
//! [`WorkspaceEdit`] and the LSP layer in the `scarlet` crate turns it into
//! JSON for the wire.
//!
//! This relies on three things being true:
//! * Every [`Reference::span`] covers exactly the identifier token that spells
//!   the target's name, so a rename only swaps the text in each span.
//! * [`ReferenceKind::Import`] spans the module path, not a symbol name, so a
//!   symbol rename never rewrites one.
//! * The declaration site is always rewritten, even when no
//!   [`ReferenceKind::Definition`] occurrence was recorded. Identical edits are
//!   merged, so the two can't both apply.

use std::collections::BTreeMap;
use std::fmt;

use crate::module::ModulePath;
use crate::span::Span;
use crate::token;

use super::uri::{ModuleUriError, module_uri};
use super::{DefId, EntityKind, ModuleId, ReferenceGraph, ReferenceKind};

/// One text replacement in one file. `span` uses the compiler's coordinates,
/// which count from 0 and exclude the end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextEdit {
    pub span: Span,
    pub new_text: String,
}

/// A rename across files, keyed by file URI. Every container is ordered, so the
/// output is always the same.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspaceEdit {
    pub changes: BTreeMap<String, Vec<TextEdit>>,
}

/// The result of `textDocument/prepareRename`.
///
/// `def` is the hit's target as-is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedRename {
    def: DefId,
    pub range: Span,
    pub placeholder: String,
}

/// Why a resolved definition is refused for rename.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotRenameableReason {
    /// It is defined in the standard library, the prelude or `@vm`.
    Stdlib,
    /// The cursor is on an `import a/b` path, which would be a file rename,
    /// not a symbol rename.
    ModulePath,
    /// The target is a `ModuleAlias`. Renaming it would rewrite the import
    /// line, but every qualified `q.member` use targets the member in the other
    /// module, not the alias, so those uses would be left pointing at nothing.
    ModuleAlias,
}

/// Why a rename / prepare-rename was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenameError {
    /// There is nothing to rename under the cursor, or no such definition.
    NotFound,
    /// The symbol exists but must not be rewritten.
    NotRenameable(NotRenameableReason),
    /// The new name asked for is not a valid identifier.
    InvalidName(String),
    /// A module with occurrences in it has no file URI, so the rename would
    /// only partly apply. Each entry says why.
    Unresolvable(Vec<(ModulePath, ModuleUriError)>),
}

/// The message the LSP shows in its `ResponseError`.
impl fmt::Display for RenameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RenameError::NotFound => write!(f, "there's nothing here that can be renamed"),
            RenameError::NotRenameable(NotRenameableReason::Stdlib) => {
                write!(
                    f,
                    "you can't rename something defined in the standard library"
                )
            }
            RenameError::NotRenameable(NotRenameableReason::ModulePath) => {
                write!(
                    f,
                    "you can't rename a module path here, so rename the file instead"
                )
            }
            RenameError::NotRenameable(NotRenameableReason::ModuleAlias) => {
                write!(f, "you can't rename an import")
            }
            RenameError::InvalidName(n) => write!(f, "`{n}` is not a valid identifier"),
            RenameError::Unresolvable(mods) => {
                write!(f, "can't find the source file for ")?;
                for (i, (m, why)) in mods.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{} ({why})", m.join("/"))?;
                }
                Ok(())
            }
        }
    }
}

/// Check a proposed new name. This must match the scanner's identifier grammar.
fn is_valid_identifier(name: &str) -> bool {
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

    /// For `textDocument/prepareRename`, check that the position names a symbol
    /// that can be renamed, and return the exact range of its identifier.
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

    /// Turn renaming `def` to `new_name` into a `WorkspaceEdit`, resolving each
    /// [`ModuleId`] to a URI with `uri_of`.
    fn rename_with<F>(
        &self,
        def: DefId,
        new_name: &str,
        uri_of: F,
    ) -> Result<WorkspaceEdit, RenameError>
    where
        F: Fn(ModuleId) -> Result<String, ModuleUriError>,
    {
        if !is_valid_identifier(new_name) {
            return Err(RenameError::InvalidName(new_name.to_string()));
        }

        let def_record = self.definition(def).ok_or(RenameError::NotFound)?;

        self.reject_if_not_renameable(def)?;

        // Rename every DefId spelt by the same declaring token together.
        // `type Config { .. }` mints both a Type and its implicit Constructor
        // at one span, so renaming either must rewrite both. The constructors
        // of a type with several have their own spans and stay separate.
        let mut class: Vec<DefId> = vec![def];
        match def.entity {
            EntityKind::Type => {
                if let Some(mr) = self.module_refs(def.module) {
                    class.extend(
                        mr.definitions()
                            .filter(|d| d.ctor_of() == Some(def) && d.defid.span == def.span)
                            .map(|d| d.defid),
                    );
                }
            }
            EntityKind::Constructor => {
                if let Some(ty) = def_record.ctor_of()
                    && ty.span == def.span
                {
                    class.push(ty);
                }
            }
            _ => {}
        }

        // The declaration's own name span goes in once, because everything
        // renamed together shares that token. `Import` occurrences span the
        // module path, not the symbol name, so we skip them.
        let mut sites: Vec<(ModuleId, Span)> = vec![(def.module, def_record.span())];
        for d in &class {
            for r in self.references_to(*d) {
                if r.kind == ReferenceKind::Import {
                    continue;
                }
                sites.push((r.module, r.span));
            }
        }

        // Refuse the whole rename if we can't find any one module, because a
        // partial rewrite is worse than none.
        let mut uris: BTreeMap<ModuleId, Option<String>> = BTreeMap::new();
        let mut unresolved: Vec<(ModuleId, ModuleUriError)> = Vec::new();
        let mut changes: BTreeMap<String, Vec<TextEdit>> = BTreeMap::new();
        for (m, span) in sites {
            let uri = uris.entry(m).or_insert_with(|| match uri_of(m) {
                Ok(u) => Some(u),
                Err(e) => {
                    unresolved.push((m, e));
                    None
                }
            });
            if let Some(uri) = uri {
                changes.entry(uri.clone()).or_default().push(TextEdit {
                    span,
                    new_text: new_name.to_string(),
                });
            }
        }
        if !unresolved.is_empty() {
            // Name a module even when it has no interned path, because dropping
            // it would hide the very failure we're reporting.
            let mods = unresolved
                .into_iter()
                .map(|(m, e)| {
                    let path = self
                        .module_path(m)
                        .cloned()
                        .unwrap_or_else(|| vec![format!("<module#{}>", m.0)]);
                    (path, e)
                })
                .collect();
            return Err(RenameError::Unresolvable(mods));
        }

        for edits in changes.values_mut() {
            edits.sort_by_key(|e| (e.span.start_line, e.span.start_column));
            edits.dedup();
        }

        Ok(WorkspaceEdit { changes })
    }

    /// Turn renaming `def` to `new_name` into a `WorkspaceEdit`. `entry` gives
    /// one module a known URI. That is the open buffer or bare entry module,
    /// which resolution can't find on purpose.
    pub fn rename(
        &self,
        def: DefId,
        new_name: &str,
        entry: Option<(ModuleId, &str)>,
    ) -> Result<WorkspaceEdit, RenameError> {
        self.rename_with(def, new_name, |m| {
            if let Some((eid, euri)) = entry
                && m == eid
            {
                return Ok(euri.to_string());
            }
            module_uri(self, m)
        })
    }
}

#[cfg(test)]
mod tests;
