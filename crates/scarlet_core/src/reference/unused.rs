//! Finds unused imports and dead code in the [`ReferenceGraph`]. It works out
//! what is reachable across the workspace and produces `Hint` diagnostics for
//! the entry module. It never changes the graph.

use std::collections::{HashMap, HashSet, VecDeque};

use std::fmt;

use crate::diagnostic::{Diagnostic, DiagnosticCode, Problem, Severity};
use crate::module::ModulePath;

use super::{DefId, Definition, EntityKind, ModuleId, ReferenceGraph, ReferenceKind};

/// A definition nothing reachable uses: its kind and name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnusedDef {
    entity: EntityKind,
    name: String,
}

impl Problem for UnusedDef {
    fn code(&self) -> DiagnosticCode {
        DiagnosticCode::UnusedBinding
    }

    fn severity(&self) -> Severity {
        Severity::Hint
    }
}

impl fmt::Display for UnusedDef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.entity {
            EntityKind::ModuleAlias => write!(f, "unused import `{}`", self.name),
            other => write!(f, "unused {} `{}`", other.noun(), self.name),
        }
    }
}

impl ReferenceGraph {
    /// The roots that reachability starts from, which are every `pub`
    /// definition. The caller outside the LSP adds the entry module's
    /// top-level definitions to this set before walking.
    fn roots(&self) -> impl Iterator<Item = DefId> + '_ {
        self.all_defs().filter(|d| d.is_pub).map(|d| d.defid)
    }

    /// The edges between definitions. An edge `(a, b)` means definition `a`
    /// contains an occurrence that resolves to definition `b`.
    fn edges(&self) -> impl Iterator<Item = (DefId, DefId)> + '_ {
        self.modules.values().flat_map(|mr| {
            mr.occurrences
                .iter()
                .filter_map(|occ| occ.owner.map(|o| (o, occ.reference.target)))
        })
    }

    /// The set of definitions reachable from `roots()` plus `extra_roots`,
    /// walking [`edges`](Self::edges). A definition that only refers to itself
    /// (recursive, but otherwise unused) is *not* made reachable by its own
    /// edge, so it is rightly reported as dead.
    fn reachable(&self, extra_roots: Vec<DefId>) -> HashSet<DefId> {
        let mut adj: HashMap<DefId, Vec<DefId>> = HashMap::new();
        for (from, to) in self.edges() {
            adj.entry(from).or_default().push(to);
        }
        // A constructor keeps its type alive, because `Config(name: 'x')`
        // mentions the constructor and never the type. This edge comes from the
        // declaration, so it must not move into `edges()`, which is built from
        // occurrences. If it did, find-references on the type would list the
        // constructor's declaration.
        for mr in self.modules.values() {
            for d in mr.definitions.values() {
                if let Some(ty) = d.ctor_of() {
                    adj.entry(d.defid).or_default().push(ty);
                }
            }
        }
        let mut seen: HashSet<DefId> = HashSet::new();
        let mut queue: VecDeque<DefId> = VecDeque::new();
        for r in self.roots().chain(extra_roots) {
            if seen.insert(r) {
                queue.push_back(r);
            }
        }
        while let Some(d) = queue.pop_front() {
            if let Some(next) = adj.get(&d) {
                for &n in next {
                    if seen.insert(n) {
                        queue.push_back(n);
                    }
                }
            }
        }
        seen
    }

    /// What the entry module's top-level body refers to, meaning the *use*
    /// occurrences that sit in no definition. That code runs whenever the
    /// program runs, so anything it names is a live root.
    ///
    /// The filter on kind matters. The code that fills the graph also records,
    /// with `owner == None`, a `Definition` occurrence of every top-level
    /// declaration itself, plus import and binding tokens. Rooting on
    /// `owner.is_none()` alone would make every top-level definition its own
    /// root, so no private definition could ever be reported dead.
    fn entry_toplevel_roots(&self, entry: ModuleId) -> Vec<DefId> {
        match self.modules.get(&entry) {
            Some(mr) => mr
                .occurrences
                .iter()
                .filter_map(|occ| {
                    (occ.owner.is_none() && occ.reference.kind.is_use_site())
                        .then_some(occ.reference.target)
                })
                .collect(),
            None => Vec::new(),
        }
    }

    /// Everything reachable under the full dead-code rule for roots, which is
    /// every `pub` definition in the workspace plus everything the entry
    /// module's top-level body refers to.
    fn reachable_from_entry(&self, entry: ModuleId) -> HashSet<DefId> {
        self.reachable(self.entry_toplevel_roots(entry))
    }

    /// Whether `def` is used anywhere as a real reference, qualified or not. An
    /// import declaration's own `Import`, `Alias` and `Definition` occurrences
    /// never count as a use.
    ///
    /// A `ModuleAlias` is used through a qualifier. In `import a/b` followed by
    /// `b.foo()`, the member targets the def in the other module, but the
    /// qualifier `b` records a [`ReferenceKind::Qualifier`] occurrence that
    /// targets this alias. Because that occurrence names the alias itself, a
    /// second alias of the same module isn't kept alive by the first one's use.
    fn has_real_use(&self, def: &Definition) -> bool {
        let refs = self.references_to(def.defid);
        let direct = refs.iter().any(|r| r.kind.is_use_site());
        if direct || def.entity() != EntityKind::ModuleAlias {
            return direct;
        }
        let decl_span = def.decl_span();
        refs.iter()
            .any(|r| r.kind == ReferenceKind::Qualifier && !r.span.within(&decl_span))
    }

    /// The `Hint` diagnostics for the entry module, which are its unused private
    /// definitions and unused imports, decided by what the workspace reaches.
    ///
    /// Only definitions in `entry` are reported, so prelude, `@vm` and stdlib
    /// definitions are never flagged. The output is sorted by source position
    /// so `publishDiagnostics` always gets it in the same order.
    fn unused_diagnostics(&self, entry: ModuleId) -> Vec<Diagnostic<UnusedDef>> {
        let Some(mr) = self.modules.get(&entry) else {
            return Vec::new();
        };
        let reachable = self.reachable_from_entry(entry);
        let mut out: Vec<Diagnostic<UnusedDef>> = Vec::new();

        for def in mr.definitions.values() {
            if def.entity() == EntityKind::ModuleAlias {
                if !self.has_real_use(def) {
                    out.push(Diagnostic::new(
                        def.span(),
                        UnusedDef {
                            entity: EntityKind::ModuleAlias,
                            name: def.name.clone(),
                        },
                    ));
                }
                continue;
            }
            // The unused-binding check handles `Value` binders, and a
            // `Constructor` or `Field` is covered by the `Type` it belongs to.
            if def.is_pub
                || !matches!(
                    def.entity(),
                    EntityKind::Function | EntityKind::Constant | EntityKind::Type
                )
                || reachable.contains(&def.defid)
            {
                continue;
            }
            out.push(Diagnostic::new(
                def.span(),
                UnusedDef {
                    entity: def.entity(),
                    name: def.name.clone(),
                },
            ));
        }

        out.sort_by_key(|d| (d.span.start_line, d.span.start_column));
        out
    }

    /// [`unused_diagnostics`](Self::unused_diagnostics), keyed by module path.
    /// It is empty if `entry` was never interned into this graph.
    pub fn unused_diagnostics_for(&self, entry: &ModulePath) -> Vec<Diagnostic<UnusedDef>> {
        match self.module_id(entry) {
            Some(id) => self.unused_diagnostics(id),
            None => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests;
