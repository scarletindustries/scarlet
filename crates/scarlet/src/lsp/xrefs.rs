use std::collections::HashMap;

use crate::bytecode;
use crate::reference;
use crate::span::Span;

/// Cross-edit identity of a definition: its defining file, name, and entity
/// kind. A [`reference::DefId`] is unusable as this key twice over: its span
/// shifts with any edit above it in the defining file, and its `ModuleId`
/// depends on how the analysis was rooted (the same file is `./lib` when
/// imported but the bare entry module when queried directly, and an edit
/// flips which mapping answers). The file URI is the identity both sides can
/// always compute. This key survives every edit short of the rename itself,
/// which rewrites the edges anyway.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct StableDefId {
    file: String,
    name: String,
    entity: reference::EntityKind,
}

impl StableDefId {
    /// Build the key for `def` as seen from `request_uri`'s graph. `None` when
    /// the defining module has no file (an embedded stdlib module), which
    /// needs no reverse edges: nothing on disk imports it by path.
    pub(super) fn of(
        graph: &reference::ReferenceGraph,
        request_uri: &str,
        def: &reference::Definition,
    ) -> Option<Self> {
        let file = super::wire::uri_for(graph, request_uri, def.defid.module)?;
        Some(StableDefId {
            file,
            name: def.name.clone(),
            entity: def.defid.entity,
        })
    }
}

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
/// uses here, keyed by [`StableDefId`], which survives edits to the defining
/// file (a `DefId`'s span does not). `by_file` lets a re-analysis of one file
/// replace exactly its own contribution.
#[derive(Default)]
pub(super) struct WorkspaceXrefs {
    by_def: HashMap<StableDefId, Vec<Xref>>,
    by_file: HashMap<String, Vec<StableDefId>>,
}

impl WorkspaceXrefs {
    /// Replace `uri`'s entire contribution with `found`.
    pub(super) fn refresh(
        &mut self,
        uri: &str,
        found: Vec<(StableDefId, Span, reference::ReferenceKind)>,
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
        let mut touched: Vec<StableDefId> = Vec::new();
        for (target, span, kind) in found {
            self.by_def.entry(target.clone()).or_default().push(Xref {
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
    pub(super) fn callers(&self, def: &StableDefId) -> &[Xref] {
        self.by_def.get(def).map_or(&[], Vec::as_slice)
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
            session: bytecode::IncrementalSession::new(),
            xrefs: WorkspaceXrefs::default(),
        }
    }

    /// A root for the in-repo stdlib source tree itself: the session compiles
    /// `scarlet/...` modules from the `.scrl` files under `stdlib_root` rather than
    /// the copy embedded in the binary, which may be stale against the files
    /// being edited, so stdlib sources get the same hover / goto-def /
    /// references fidelity as user code.
    pub(super) fn new_stdlib(stdlib_root: std::path::PathBuf) -> Self {
        Self {
            session: bytecode::IncrementalSession::new_from_source(stdlib_root),
            xrefs: WorkspaceXrefs::default(),
        }
    }
}
