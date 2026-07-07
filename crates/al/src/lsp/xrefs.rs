use std::collections::HashMap;

use crate::bytecode;
use crate::reference;
use crate::span::Span;

/// One dependent-file occurrence of a cross-module definition, captured with
/// its true file URI so it survives a later analysis that makes a *different*
/// file the compilation entry.
#[derive(Clone)]
pub(super) struct Xref {
    pub(super) uri: String,
    pub(super) span: Span,
    pub(super) kind: reference::ReferenceKind,
}

/// Workspace-wide reverse edges that the session's per-entry reference graph
/// cannot retain on its own.
///
/// A position query re-roots compilation to the queried file, so that file
/// becomes the entry module (`main`) and only its own import closure is in the
/// session graph. A file's *importers* are never in its closure (import edges
/// point the other way) and are never cached as modules, so their references
/// into it — e.g. `main.al`'s call of `lib.greet()` when the query is driven
/// from `greet`'s declaration in `lib.al` — would otherwise vanish whenever
/// `lib.al` is the entry.
///
/// After every analysis we record the entry file's cross-module uses here,
/// keyed by the canonical [`reference::DefId`] each one targets (a cached
/// module's `DefId` is stable across re-roots because the session interner is
/// append-only). `by_file` lets a re-analysis of one file replace exactly its
/// own contribution, so a removed import stops resolving — keeping the index
/// coherent with incremental edits.
#[derive(Default)]
pub(super) struct WorkspaceXrefs {
    by_def: HashMap<reference::DefId, Vec<Xref>>,
    by_file: HashMap<String, Vec<reference::DefId>>,
}

impl WorkspaceXrefs {
    /// Replace `uri`'s entire contribution with `found` (each entry a canonical
    /// target plus the occurrence's span/kind in `uri`).
    pub(super) fn refresh(
        &mut self,
        uri: &str,
        found: Vec<(reference::DefId, Span, reference::ReferenceKind)>,
    ) {
        if let Some(defs) = self.by_file.remove(uri) {
            for d in defs {
                if let Some(v) = self.by_def.get_mut(&d) {
                    v.retain(|x| x.uri != uri);
                    if v.is_empty() {
                        self.by_def.remove(&d);
                    }
                }
            }
        }
        let mut touched: Vec<reference::DefId> = Vec::new();
        for (target, span, kind) in found {
            self.by_def.entry(target).or_default().push(Xref {
                uri: uri.to_string(),
                span,
                kind,
            });
            touched.push(target);
        }
        if !touched.is_empty() {
            self.by_file.insert(uri.to_string(), touched);
        }
    }

    /// Every persisted dependent-file occurrence of `def`.
    pub(super) fn callers(&self, def: reference::DefId) -> &[Xref] {
        self.by_def.get(&def).map_or(&[], Vec::as_slice)
    }
}

/// Per-workspace-root state: the persistent compiler session (reused across
/// `didChange` so unchanged imports stay cached) and its cross-module reverse
/// edges (see [`WorkspaceXrefs`]). Kept together so a root added/removed at
/// runtime can never leave one populated without the other.
pub(super) struct RootState {
    pub(super) session: bytecode::IncrementalSession,
    pub(super) xrefs: WorkspaceXrefs,
}

impl RootState {
    pub(super) fn new() -> Self {
        Self {
            session: bytecode::IncrementalSession::new(crate::stdlib()),
            xrefs: WorkspaceXrefs::default(),
        }
    }
}
