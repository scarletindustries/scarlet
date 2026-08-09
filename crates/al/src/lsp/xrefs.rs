use std::collections::HashMap;

use crate::bytecode;
use crate::reference;
use crate::span::Span;

/// One dependent-file occurrence of a cross-module definition. Carries its own
/// URI so it survives a later analysis rooted at a different file.
#[derive(Clone)]
pub(super) struct Xref {
    pub(super) uri: String,
    pub(super) span: Span,
    pub(super) kind: reference::ReferenceKind,
}

/// Workspace-wide reverse edges the session's per-entry reference graph cannot
/// retain. A position query re-roots compilation at the queried file, and a
/// file's importers are never in its own import closure, so their references
/// into it would vanish. Every analysis records the entry file's cross-module
/// uses here, keyed by target [`reference::DefId`] (stable across re-roots: the
/// session interner is append-only). `by_file` lets a re-analysis of one file
/// replace exactly its own contribution.
#[derive(Default)]
pub(super) struct WorkspaceXrefs {
    by_def: HashMap<reference::DefId, Vec<Xref>>,
    by_file: HashMap<String, Vec<reference::DefId>>,
}

impl WorkspaceXrefs {
    /// Replace `uri`'s entire contribution with `found`.
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
/// edges. Kept together so neither can outlive the other.
pub(super) struct RootState {
    pub(super) session: bytecode::IncrementalSession,
    pub(super) xrefs: WorkspaceXrefs,
}

impl RootState {
    pub(super) fn new() -> Self {
        Self {
            session: bytecode::IncrementalSession::new(&crate::STDLIB),
            xrefs: WorkspaceXrefs::default(),
        }
    }
}
