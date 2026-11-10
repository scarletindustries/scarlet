//! Reference-graph queries over a [`Build`]'s analysis, for tests that check a program
//! and then ask what its names resolve to. The entry is checked as `main`, the
//! way `scarlet check` checks a file, and its imports under their real keys.

use scarlet::build::{self, Build, Counts, Entry};
use scarlet::check::{Role, UnusedBindings};
use scarlet::diagnostic::{self, Diagnostic};
use scarlet::module::{self, ModulePath};
use scarlet::reference::{DefId, Definition, EntityKind, ModuleId, ReferenceGraph};
use scarlet::sources::DiskSources;
use scarlet::span::Span;

use super::Project;

/// A build that has checked an entry, and the reference graph of everything it
/// holds.
pub struct Checked {
    build: Build,
    sources: DiskSources,
    graph: ReferenceGraph,
    dir: std::path::PathBuf,
}

/// What one check of the entry reported, its imports included.
pub struct CheckResult {
    pub diagnostics: Vec<Diagnostic>,
    pub counts: Counts,
}

impl CheckResult {
    pub fn success(&self) -> bool {
        !diagnostic::has_errors(&self.diagnostics)
    }
}

impl Checked {
    pub fn new(p: &Project) -> Self {
        Checked {
            build: Build::default(),
            sources: DiskSources::new(),
            graph: ReferenceGraph::of_modules([]),
            dir: p.dir.clone(),
        }
    }

    /// Checks `entry` as the program `main` in this build, then rebuilds the
    /// graph.
    pub fn check(&mut self, entry: &str) -> CheckResult {
        let parsed = build::parse(entry);
        assert!(
            parsed.diagnostics.is_empty(),
            "parse errors: {:?}\n---\n{entry}",
            parsed.diagnostics
        );
        let analysis = self.build.check(
            &mut self.sources,
            Entry {
                source: scarlet::source::SourceFile::new(module::ModuleKey::main(), None, entry),
                ast: &parsed.ast,
                doc: parsed.doc,
                parse: parsed.diagnostics,
                base_dir: Some(&self.dir),
                role: Role::Entry,
                unused: UnusedBindings::Report,
            },
        );
        self.graph = ReferenceGraph::of_modules(analysis.modules());
        CheckResult {
            diagnostics: analysis.diagnostics(),
            counts: analysis.counts(),
        }
    }

    pub fn reference_graph(&self) -> &ReferenceGraph {
        &self.graph
    }
}

/// Makes a fresh build, checks `entry` against project `p`, and asserts it
/// succeeds.
pub fn checked_with(p: &Project, entry: &str) -> Checked {
    let mut s = Checked::new(p);
    let r = s.check(entry);
    assert!(r.success(), "check failed: {:?}", r.diagnostics);
    s
}

/// A document/workspace symbol projected from a graph `Definition`.
#[derive(Debug, Clone)]
pub struct SymbolInfo {
    pub name: String,
    pub kind: EntityKind,
    pub module: ModulePath,
    pub span: Span,
}

/// Lists the (name, kind) pairs, for the failure message of an `assert!` over symbols.
pub fn sym_names(syms: &[SymbolInfo]) -> Vec<(&str, EntityKind)> {
    syms.iter().map(|s| (s.name.as_str(), s.kind)).collect()
}

/// Asserts `syms` includes a `name` of `kind`, and prints every (name, kind) if not.
pub fn assert_has_sym(syms: &[SymbolInfo], name: &str, kind: EntityKind) {
    assert!(
        syms.iter().any(|s| s.name == name && s.kind == kind),
        "missing symbol `{name}` ({kind:?}) in {:?}",
        sym_names(syms)
    );
}

/// Assert at least one of `msgs` contains `needle`.
pub fn assert_has_msg(msgs: &[String], needle: &str) {
    assert!(
        msgs.iter().any(|m| m.contains(needle)),
        "expected a message containing `{needle}`: {msgs:?}"
    );
}

/// Assert at least one of `msgs` is exactly `exact` (not a substring match).
pub fn assert_msg_eq(msgs: &[String], exact: &str) {
    assert!(
        msgs.iter().any(|m| m == exact),
        "expected a message == `{exact}`: {msgs:?}"
    );
}

/// Assert no message in `msgs` contains `needle`.
pub fn assert_no_msg(msgs: &[String], needle: &str) {
    assert!(
        !msgs.iter().any(|m| m.contains(needle)),
        "unexpected message containing `{needle}`: {msgs:?}"
    );
}

fn module_for(g: &ReferenceGraph, module_or_path: &str) -> Option<ModuleId> {
    // 1. Try the canonical key exactly. Like the LSP boundary, this looks a key up
    // and never mints one.
    if let Some(id) = g.module_id_by_key(&module::ModuleKey::from_lookup_str(module_or_path)) {
        return Some(id);
    }
    // 2. Try a path to the source file.
    let p = std::path::Path::new(module_or_path);
    if p.is_file()
        && let Ok(mp) = module::file_module_path(p)
        && let Some(id) = g.module_id_by_key(&module::ModuleKey::of(&mp))
    {
        return Some(id);
    }
    // 3. For test convenience only, try a written name (`./c`, `b`). A module's
    // name is NOT its identity, so this matches the last path segment and panics
    // if several modules match rather than silently picking one.
    let want = module_or_path.rsplit('/').next().unwrap_or(module_or_path);
    let hits: Vec<ModuleId> = g
        .modules()
        .filter(|(_, path)| path.last().map(String::as_str) == Some(want))
        .map(|(id, _)| id)
        .collect();
    match hits.as_slice() {
        [one] => return Some(*one),
        [] => {}
        many => panic!(
            "module name {module_or_path:?} is ambiguous across {} modules; pass the source path instead",
            many.len()
        ),
    }
    g.module_id_by_key(&module::ModuleKey::main())
}

fn symbol_of(g: &ReferenceGraph, d: &Definition) -> Option<SymbolInfo> {
    if !d.is_symbol_listable() {
        return None;
    }
    Some(SymbolInfo {
        name: d.name.clone(),
        kind: d.entity(),
        module: g.module_path(d.defid.module)?.clone(),
        span: d.span(),
    })
}

/// String-keyed LSP-style queries answered from the reference graph. The
/// language server queries the graph directly with resolved `ModuleId`s. These
/// key by module path and fall back to the entry `main`.
pub trait GraphQueryExt {
    fn definition(&self, module_or_path: &str, line: i32, col: i32) -> Option<(ModulePath, Span)>;
    fn prepare_rename(&self, module_or_path: &str, line: i32, col: i32) -> Option<(DefId, Span)>;
    fn references(&self, def: DefId) -> Vec<(ModulePath, Span)>;
    fn rename(&self, def: DefId) -> Vec<(ModulePath, Span)>;
    fn document_symbols(&self, module_or_path: &str) -> Vec<SymbolInfo>;
    fn workspace_symbols(&self, query: &str) -> Vec<SymbolInfo>;
}

impl GraphQueryExt for Checked {
    /// Goto-definition: finds the declaration the occurrence under the cursor names.
    fn definition(&self, module_or_path: &str, line: i32, col: i32) -> Option<(ModulePath, Span)> {
        let g = self.reference_graph();
        let m = module_for(g, module_or_path)?;
        let id = g.def_id_at(m, line, col)?;
        Some((g.module_path(id.module)?.clone(), id.span))
    }

    /// The alias-preserving `DefId` under the cursor plus the span of the
    /// smallest enclosing occurrence.
    fn prepare_rename(&self, module_or_path: &str, line: i32, col: i32) -> Option<(DefId, Span)> {
        let g = self.reference_graph();
        let m = module_for(g, module_or_path)?;
        let id = g.def_id_at(m, line, col)?;
        let cursor = g
            .module_refs(m)
            .and_then(|mr| {
                mr.occurrences()
                    .iter()
                    .map(|o| o.reference)
                    .filter(|r| r.span.contains(line, col))
                    .min_by_key(|r| r.span.width())
                    .map(|r| r.span)
            })
            .unwrap_or(id.span);
        Some((id, cursor))
    }

    /// Find-references: returns the declaration plus every occurrence in the
    /// workspace, without listing the declaration's own occurrence twice.
    fn references(&self, def: DefId) -> Vec<(ModulePath, Span)> {
        let g = self.reference_graph();
        let mut out: Vec<(ModulePath, Span)> = Vec::new();
        if let Some(p) = g.module_path(def.module) {
            out.push((p.clone(), def.span));
        }
        for rr in g.references_to(def) {
            if (rr.module, rr.span.start_line, rr.span.start_column)
                == (def.module, def.span.start_line, def.span.start_column)
            {
                continue;
            }
            if let Some(p) = g.module_path(rr.module) {
                out.push((p.clone(), rr.span));
            }
        }
        out
    }

    /// Returns every span a rename rewrites, which is the same as find-references.
    fn rename(&self, def: DefId) -> Vec<(ModulePath, Span)> {
        self.references(def)
    }

    /// Every symbol-listable definition declared in the module.
    fn document_symbols(&self, module_or_path: &str) -> Vec<SymbolInfo> {
        let g = self.reference_graph();
        match module_for(g, module_or_path) {
            Some(m) => g.defs_in(m).filter_map(|d| symbol_of(g, d)).collect(),
            None => Vec::new(),
        }
    }

    /// Case-insensitive substring search over every workspace declaration.
    fn workspace_symbols(&self, query: &str) -> Vec<SymbolInfo> {
        let g = self.reference_graph();
        let needle = query.to_lowercase();
        g.all_defs()
            .filter(|d| needle.is_empty() || d.name.to_lowercase().contains(needle.as_str()))
            .filter_map(|d| symbol_of(g, d))
            .collect()
    }
}
