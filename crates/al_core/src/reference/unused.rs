//! Unused-import / dead-code analysis over the [`ReferenceGraph`]: workspace
//! reachability and the `Hint` diagnostics it produces for the entry module.
//! Read-only over the graph.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::diagnostic::{Diagnostic, DiagnosticCode};
use crate::module::ModulePath;

use super::{DefId, Definition, EntityKind, ModuleId, ReferenceGraph, ReferenceKind};

impl ReferenceGraph {
    /// Reachability roots: every `pub` definition. The non-LSP consumer adds
    /// the entry module's top-level definitions to this set before walking.
    fn roots(&self) -> impl Iterator<Item = DefId> + '_ {
        self.all_defs().filter(|d| d.is_pub).map(|d| d.defid)
    }

    /// Definition→definition edges: an edge `(a, b)` means definition `a`
    /// contains an occurrence that resolves to definition `b`.
    fn edges(&self) -> impl Iterator<Item = (DefId, DefId)> + '_ {
        self.modules.values().flat_map(|mr| {
            mr.occurrences
                .iter()
                .filter_map(|occ| occ.owner.map(|o| (o, occ.reference.target)))
        })
    }

    /// The set of definitions reachable from `roots()` plus `extra_roots`,
    /// walking [`edges`](Self::edges). A definition only self-referenced
    /// (recursive but otherwise unused) is *not* made reachable by its own
    /// edge, so it is correctly reported as dead.
    fn reachable(&self, extra_roots: Vec<DefId>) -> HashSet<DefId> {
        let mut adj: HashMap<DefId, Vec<DefId>> = HashMap::new();
        for (from, to) in self.edges() {
            adj.entry(from).or_default().push(to);
        }
        // A constructor keeps its type alive: `Config(name: 'x')` mentions the
        // constructor, never the type. This edge is structural and must not
        // move into `edges()`, which is built from occurrences — find-references
        // on the type would then list the constructor's declaration.
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

    /// Targets of the entry module's executed top-level body — *use*
    /// occurrences with no enclosing definition. That code runs whenever the
    /// program runs, so anything it names is a live root.
    ///
    /// The kind filter is load-bearing. The populator also records, with
    /// `owner == None`, a `Definition` self-occurrence for every top-level
    /// declaration plus import/binding tokens. Rooting on `owner.is_none()`
    /// alone would make every top-level definition its own root, so no private
    /// definition could ever be reported dead.
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

    /// The reachable set under the full dead-code root rule: every `pub`
    /// definition workspace-wide plus everything the entry module's top-level
    /// body references.
    pub fn reachable_from_entry(&self, entry: ModuleId) -> HashSet<DefId> {
        self.reachable(self.entry_toplevel_roots(entry))
    }

    /// Whether `def` is used as a real (qualified/unqualified) reference
    /// anywhere. An import declaration's own `Import`/`Alias`/`Definition`
    /// occurrences never count as a use.
    ///
    /// A `ModuleAlias` needs two extra edges, because neither way of using one
    /// records a use-site occurrence pointing at the alias:
    ///
    /// * `import a/b` then `b.foo()`: the member targets the *remote* def, but
    ///   the qualifier `b` records a [`ReferenceKind::Qualifier`] occurrence
    ///   targeting this alias. Naming the alias itself is what stops a second
    ///   alias of the same module from being kept alive by its sibling's use.
    /// * `import a/b.{item}` then `item()`: the binding token is an
    ///   [`ReferenceKind::ImportItem`] occurrence targeting the remote `item`,
    ///   so a use site of that target is a genuine use. Containment in
    ///   [`Definition::decl_span`] attributes each token to *this* import.
    fn has_real_use(&self, def: &Definition) -> bool {
        let refs = self.references_to(def.defid);
        let direct = refs.iter().any(|r| r.kind.is_use_site());
        if direct || def.entity() != EntityKind::ModuleAlias {
            return direct;
        }
        let decl_span = def.decl_span();
        if refs
            .iter()
            .any(|r| r.kind == ReferenceKind::Qualifier && !r.span.within(&decl_span))
        {
            return true;
        }
        // Targets bound by this import: its `ImportItem` tokens, plus the local
        // def minted for the `Y` of an `{X as Y}` item (an `Alias` occurrence
        // inside the declaration).
        let Some(mr) = self.modules.get(&def.defid.module) else {
            return false;
        };
        let imported: HashSet<DefId> = mr
            .occurrences()
            .iter()
            .map(|o| o.reference)
            .filter(|r| {
                matches!(r.kind, ReferenceKind::ImportItem | ReferenceKind::Alias)
                    && r.target != def.defid
                    && r.span.within(&decl_span)
            })
            .map(|r| r.target)
            .collect();
        !imported.is_empty()
            && mr
                .occurrences()
                .iter()
                .map(|o| o.reference)
                .any(|r| imported.contains(&r.target) && r.kind.is_use_site())
    }

    /// `Hint` diagnostics for the entry module: unused private definitions and
    /// unused imports, decided by workspace reachability.
    ///
    /// Only definitions owned by `entry` are reported, so prelude / `@vm` /
    /// stdlib definitions are never flagged. Output is sorted by source
    /// position so it flows stably through `publishDiagnostics`.
    pub fn unused_diagnostics(&self, entry: ModuleId) -> Vec<Diagnostic> {
        let Some(mr) = self.modules.get(&entry) else {
            return Vec::new();
        };
        let reachable = self.reachable_from_entry(entry);
        let mut out: Vec<Diagnostic> = Vec::new();

        for def in mr.definitions.values() {
            if def.entity() == EntityKind::ModuleAlias {
                if !self.has_real_use(def) {
                    out.push(Diagnostic::hint(
                        def.span(),
                        DiagnosticCode::UnusedBinding,
                        format!("unused import `{}`", def.name),
                    ));
                }
                continue;
            }
            // `Value` binders are the unused-binding check's job, and
            // `Constructor`/`Field` are covered via their owning `Type`.
            if def.is_pub
                || !matches!(
                    def.entity(),
                    EntityKind::Function | EntityKind::Constant | EntityKind::Type
                )
                || reachable.contains(&def.defid)
            {
                continue;
            }
            out.push(Diagnostic::hint(
                def.span(),
                DiagnosticCode::UnusedBinding,
                format!("unused {} `{}`", def.entity().noun(), def.name),
            ));
        }

        out.sort_by_key(|d| (d.span.start_line, d.span.start_column));
        out
    }

    /// [`unused_diagnostics`](Self::unused_diagnostics) keyed by module path;
    /// empty if `entry` was never interned into this graph.
    pub fn unused_diagnostics_for(&self, entry: &ModulePath) -> Vec<Diagnostic> {
        match self.module_id(entry) {
            Some(id) => self.unused_diagnostics(id),
            None => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{
        Definition, DefinitionKind, ModuleReferences, Reference, ReferenceKind as K, add_def,
        add_ref, def, main_graph, mp, stub_kind,
    };
    use super::*;
    use crate::diagnostic::Severity;
    use crate::span::Span;

    #[track_caller]
    fn sole_unused(g: &ReferenceGraph, m: ModuleId) -> Diagnostic {
        let mut d = g.unused_diagnostics(m);
        assert_eq!(d.len(), 1, "expected exactly one unused diagnostic: {d:?}");
        d.pop().unwrap()
    }

    #[test]
    fn edges_only_emitted_for_owned_occurrences() {
        let (mut g, m, mut mr) = main_graph();

        let a = def(m, 1, 0, 1, EntityKind::Function);
        let b = def(m, 2, 0, 1, EntityKind::Function);
        mr.add_definition(Definition::new(
            a.module,
            a.span,
            "a",
            None,
            true,
            stub_kind(a),
        ));
        mr.add_definition(Definition::new(
            b.module,
            b.span,
            "b",
            None,
            false,
            stub_kind(b),
        ));

        // Owned by `a` -> contributes an a->b edge.
        add_ref(&mut mr, Some(a), (1, 5, 6), K::Unqualified, b);
        // Unowned top-level occurrence -> no edge.
        add_ref(&mut mr, None, (9, 0, 1), K::Unqualified, b);
        g.insert_module(mr);
        let g = g.finish();

        let edges: Vec<(DefId, DefId)> = g.edges().collect();
        assert_eq!(edges, vec![(a, b)]);

        assert!(g.reachable(Vec::new()).contains(&b));
    }

    #[test]
    fn reachable_closes_transitively_and_skips_self_only_recursion() {
        let (mut g, m, mut mr) = main_graph();

        // pub `a` -> private `b` -> private `deep`: a multi-hop chain whose
        // middle node `b` is not itself a root.
        let a = add_def(&mut mr, m, "a", 1, EntityKind::Function, true);
        let b = add_def(&mut mr, m, "b", 3, EntityKind::Function, false);
        let deep = add_def(&mut mr, m, "deep", 5, EntityKind::Function, false);
        // private `loopy`, only self-recursive.
        let loopy = add_def(&mut mr, m, "loopy", 7, EntityKind::Function, false);

        add_ref(&mut mr, Some(a), (2, 4, 5), K::Unqualified, b);
        add_ref(&mut mr, Some(b), (4, 4, 8), K::Unqualified, deep);
        add_ref(&mut mr, Some(loopy), (8, 4, 9), K::Unqualified, loopy);
        g.insert_module(mr);
        let g = g.finish();

        let r = g.reachable(Vec::new());
        assert!(
            r.contains(&deep),
            "transitively reachable through a -> b -> deep"
        );
        assert!(
            !r.contains(&loopy),
            "a self-only-recursive private def is not rooted by its own edge"
        );
    }

    #[test]
    fn unused_diag_flags_private_fn_keeps_pub_and_used() {
        let (mut g, m, mut mr) = main_graph();
        let api = add_def(&mut mr, m, "api", 1, EntityKind::Function, true);
        let used = add_def(&mut mr, m, "used", 3, EntityKind::Function, false);
        let dead = add_def(&mut mr, m, "dead", 5, EntityKind::Function, false);
        // pub `api` calls private `used`; nobody calls `dead`.
        add_ref(&mut mr, Some(api), (2, 4, 8), K::Unqualified, used);
        g.insert_module(mr);
        let g = g.finish();

        let d = sole_unused(&g, m);
        assert_eq!(d.severity, Severity::Hint);
        assert_eq!(d.message, "unused function `dead`");
        assert_eq!(d.span, dead.span);
    }

    #[test]
    fn unused_diag_entry_toplevel_body_keeps_def_live() {
        let (mut g, m, mut mr) = main_graph();
        let helper = add_def(&mut mr, m, "helper", 1, EntityKind::Function, false);
        let dead = add_def(&mut mr, m, "dead", 4, EntityKind::Function, false);
        // `helper` is only called by the executed top-level body (owner None).
        add_ref(&mut mr, None, (9, 0, 6), K::Unqualified, helper);
        g.insert_module(mr);
        let g = g.finish();

        let d = sole_unused(&g, m);
        assert_eq!(d.message, "unused function `dead`");
        assert_eq!(d.span, dead.span);
    }

    #[test]
    fn unused_diag_self_recursive_only_is_flagged() {
        let (mut g, m, mut mr) = main_graph();
        let spin = add_def(&mut mr, m, "spin", 1, EntityKind::Function, false);
        // `spin` references only itself — a self edge never makes it reachable.
        add_ref(&mut mr, Some(spin), (2, 4, 8), K::Unqualified, spin);
        g.insert_module(mr);
        let g = g.finish();

        assert_eq!(sole_unused(&g, m).message, "unused function `spin`");
    }

    #[test]
    fn unused_diag_only_reports_entry_module() {
        let (mut g, entry, mut em) = main_graph();
        let lib = g.intern_module(&mp(&["lib"]));

        add_def(&mut em, entry, "go", 1, EntityKind::Function, true);
        g.insert_module(em);

        // A private, unreachable def in another module must NOT surface when
        // diagnosing the entry module (prelude/@vm/library defs live here).
        let mut lm = ModuleReferences::new(lib);
        add_def(&mut lm, lib, "secret", 1, EntityKind::Function, false);
        g.insert_module(lm);
        let g = g.finish();

        assert!(g.unused_diagnostics(entry).is_empty());
    }

    #[test]
    fn unused_diag_unused_and_used_module_alias_import() {
        let (mut g, m, mut mr) = main_graph();
        let unused_imp = add_def(&mut mr, m, "io", 1, EntityKind::ModuleAlias, false);
        let used_imp = add_def(&mut mr, m, "fmt", 2, EntityKind::ModuleAlias, false);
        // `fmt` is used through a qualified access; `io` never is.
        add_ref(&mut mr, None, (5, 4, 12), K::Qualified, used_imp);
        g.insert_module(mr);
        let g = g.finish();

        let d = sole_unused(&g, m);
        assert_eq!(d.message, "unused import `io`");
        assert_eq!(d.span, unused_imp.span);
    }

    #[test]
    fn unused_diag_import_with_only_decl_occurrences_is_flagged() {
        let (mut g, m, mut mr) = main_graph();
        let imp = add_def(&mut mr, m, "util", 1, EntityKind::ModuleAlias, false);
        // The import declaration's own occurrences are not "uses".
        add_ref(&mut mr, None, (1, 7, 11), K::Import, imp);
        add_ref(&mut mr, None, (1, 7, 11), K::Definition, imp);
        g.insert_module(mr);
        let g = g.finish();

        assert_eq!(sole_unused(&g, m).message, "unused import `util`");
    }

    #[test]
    fn unused_diag_unqualified_import_item_used_keeps_import_live() {
        // `import a/b.{used}` then `pub fn main() { used() }`. The binding
        // token targets the *remote* `used`, and so does the call, so the
        // import counts as used. The `Import` occurrence covers only the final
        // module-name segment, too narrow to contain the binding — modelled
        // here to prove that narrowing does not regress detection.
        let build = |with_use: bool| {
            let (mut g, m, _) = main_graph();
            let lib = g.intern_module(&mp(&["a", "b"]));
            let mut mr = ModuleReferences::new(m);
            let alias = def(m, 1, 0, 18, EntityKind::ModuleAlias);
            mr.add_definition(Definition::new(
                alias.module,
                alias.span,
                "b",
                None,
                false,
                stub_kind(alias),
            ));
            let remote_used = def(lib, 1, 3, 7, EntityKind::Function);
            add_ref(&mut mr, None, (1, 9, 10), K::Import, alias);
            add_ref(&mut mr, None, (1, 12, 16), K::ImportItem, remote_used);
            let main = add_def(&mut mr, m, "main", 3, EntityKind::Function, true);
            if with_use {
                add_ref(
                    &mut mr,
                    Some(main),
                    (3, 16, 20),
                    K::Unqualified,
                    remote_used,
                );
            }
            g.insert_module(mr);
            (g.finish(), m)
        };

        let (g, m) = build(true);
        assert!(
            g.unused_diagnostics(m).is_empty(),
            "an import whose unqualified item is used must not be flagged"
        );

        // Drop the use: the check is still live, not just disabled.
        let (g, m) = build(false);
        assert_eq!(sole_unused(&g, m).message, "unused import `b`");
    }

    #[test]
    fn unused_diag_aliased_selective_import_item_used_keeps_import_live() {
        // `import ./util as u.{empty}` then `println(empty())`. The parser
        // emits `as u` before `.{empty}`, so the alias's `defid.span` is just
        // the `u` identifier and the item binding sits after it. Containment
        // uses `decl_span`, the whole statement, which is why it still counts.
        //
        //   import ./util as u.{empty}
        //   0      ^9   ^13 ^17^20   ^25
        let build = |with_use: bool| {
            let (mut g, m, _) = main_graph();
            let lib = g.intern_module(&mp(&["util"]));

            let alias = def(m, 1, 17, 18, EntityKind::ModuleAlias);
            let mut mr = ModuleReferences::new(m);
            let alias_def = Definition::new(
                alias.module,
                alias.span,
                "u",
                None,
                false,
                DefinitionKind::ModuleAlias {
                    decl_span: Span::single_line(1, 0, 26),
                    imports_module: Some(lib),
                },
            );
            mr.add_definition(alias_def);
            let remote_empty = def(lib, 2, 7, 12, EntityKind::Function);
            add_ref(
                &mut mr,
                None,
                (1, 9, 13),
                K::Import,
                def(lib, 1, 9, 13, EntityKind::ModuleAlias),
            );
            add_ref(&mut mr, None, (1, 17, 18), K::Alias, alias);
            add_ref(&mut mr, None, (1, 20, 25), K::ImportItem, remote_empty);
            if with_use {
                add_ref(&mut mr, None, (2, 8, 13), K::Unqualified, remote_empty);
            }
            g.insert_module(mr);
            (g.finish(), m, alias)
        };

        let (g, m, _) = build(true);
        assert!(
            g.unused_diagnostics(m).is_empty(),
            "a used item from an aliased selective import must not be flagged"
        );

        // Drop the use: the widened boundary did not just disable the check.
        let (g, m, alias) = build(false);
        let d = sole_unused(&g, m);
        assert_eq!(d.message, "unused import `u`");
        assert_eq!(d.span, alias.span);
    }

    #[test]
    fn unused_diag_constant_and_type_wording_and_ordering() {
        let (mut g, m, mut mr) = main_graph();
        // Declared out of source order to prove the output is position-sorted.
        add_def(&mut mr, m, "Color", 9, EntityKind::Type, false);
        add_def(&mut mr, m, "MAX", 2, EntityKind::Constant, false);
        g.insert_module(mr);
        let g = g.finish();

        let d = g.unused_diagnostics(m);
        let msgs: Vec<&str> = d.iter().map(|x| x.message.as_str()).collect();
        assert_eq!(msgs, vec!["unused constant `MAX`", "unused type `Color`"]);
    }

    #[test]
    fn unused_diag_value_binders_are_not_reported() {
        let (mut g, m, mut mr) = main_graph();
        // `Value` is the unused-binding checker's job, never a dead-code hint.
        add_def(&mut mr, m, "x", 1, EntityKind::Value, false);
        g.insert_module(mr);
        let g = g.finish();
        assert!(g.unused_diagnostics(m).is_empty());
    }

    #[test]
    fn unused_diag_cross_module_pub_keeps_private_live() {
        let (mut g, entry, mut em) = main_graph();
        let lib = g.intern_module(&mp(&["lib"]));

        // lib: pub `run` -> private `helper`.
        let mut lm = ModuleReferences::new(lib);
        let run = add_def(&mut lm, lib, "run", 1, EntityKind::Function, true);
        let helper = add_def(&mut lm, lib, "helper", 3, EntityKind::Function, false);
        add_ref(&mut lm, Some(run), (2, 4, 10), K::Unqualified, helper);
        g.insert_module(lm);

        // entry top-level body calls `lib.run` qualified.
        add_ref(&mut em, None, (5, 0, 7), K::Qualified, run);
        g.insert_module(em);
        let g = g.finish();

        assert!(g.unused_diagnostics(entry).is_empty());
        // `helper` is reachable via the pub `run` root, not dead.
        assert!(g.reachable_from_entry(entry).contains(&helper));
    }

    #[test]
    fn unused_diag_for_path_and_unknown_entry() {
        let (mut g, m, mut mr) = main_graph();
        add_def(&mut mr, m, "dead", 1, EntityKind::Function, false);
        g.insert_module(mr);
        // Interned but never populated (checked below): empty.
        let bare = g.intern_module(&mp(&["bare"]));
        let g = g.finish();

        assert_eq!(g.unused_diagnostics_for(&mp(&["main"])).len(), 1);
        // Never-interned path: empty, no panic.
        assert!(g.unused_diagnostics_for(&mp(&["ghost"])).is_empty());
        assert!(g.unused_diagnostics(bare).is_empty());
    }

    /// Mirrors production `Compiler::emit_def`, which records a
    /// `K::Definition` self-occurrence (`owner == None`,
    /// `target == the def`) for every top-level fn/const/type. Such
    /// self-occurrences must NOT be treated as entry-body reachability roots
    /// (the `entry_toplevel_roots` kind filter) — otherwise every top-level
    /// definition roots itself and no dead private code is ever reported.
    #[test]
    fn unused_diag_definition_self_occurrence_does_not_root_dead_code() {
        let (mut g, m, mut mr) = main_graph();

        let api = add_def(&mut mr, m, "api", 1, EntityKind::Function, true);
        let used = add_def(&mut mr, m, "used", 3, EntityKind::Function, false);
        let dead = add_def(&mut mr, m, "dead", 5, EntityKind::Function, false);

        // emit_def's self-occurrence for every top-level def (owner None).
        for d in [api, used, dead] {
            mr.add_reference(None, Reference::new(d.span, K::Definition, d));
        }
        // pub `api` calls private `used`; nobody calls `dead`.
        add_ref(&mut mr, Some(api), (2, 4, 8), K::Unqualified, used);
        g.insert_module(mr);
        let g = g.finish();

        let d = sole_unused(&g, m);
        assert_eq!(d.message, "unused function `dead`");
        assert_eq!(d.span, dead.span);
    }

    /// Registers a plain `import a/b` alias named `name` in `mr` at `line`,
    /// with the production shape: the `Definition` (with `imports_module`
    /// stored) plus its `Definition` self-occurrence and the `Import`
    /// occurrence whose target is owned by the *imported* module.
    fn add_plain_import(
        mr: &mut ModuleReferences,
        m: ModuleId,
        name: &str,
        line: i32,
        imports: ModuleId,
    ) -> DefId {
        let alias = def(m, line, 7, 7 + name.len() as i32, EntityKind::ModuleAlias);
        mr.add_definition(Definition::new(
            alias.module,
            alias.span,
            name,
            None,
            false,
            DefinitionKind::ModuleAlias {
                decl_span: alias.span,
                imports_module: Some(imports),
            },
        ));
        mr.add_reference(None, Reference::new(alias.span, K::Definition, alias));
        add_ref(
            mr,
            None,
            (line, 7, 8),
            K::Import,
            def(imports, line, 7, 8, EntityKind::ModuleAlias),
        );
        alias
    }

    /// Production shape of `import a/b` + `b.foo()`: the call records a
    /// `Qualified` occurrence whose target is the *remote* `foo` def (owned by
    /// `a/b`) plus a `Qualifier` occurrence on the `b` identifier targeting
    /// *this alias*. The `Qualifier` edge is what keeps the import alive; the
    /// import must not be reported as unused, and dropping the use must flag
    /// it again.
    #[test]
    fn unused_diag_plain_qualified_import_recovers_use() {
        let build = |with_use: bool| {
            let (mut g, m, mut mr) = main_graph();
            let lib = g.intern_module(&mp(&["a", "b"]));

            // `a/b` defines pub `foo` (+ its emit_def self-occurrence).
            let foo = def(lib, 1, 3, 6, EntityKind::Function);
            let mut lib_mr = ModuleReferences::new(lib);
            lib_mr.add_definition(Definition::new(
                foo.module,
                foo.span,
                "foo",
                None,
                true,
                stub_kind(foo),
            ));
            lib_mr.add_reference(None, Reference::new(foo.span, K::Definition, foo));
            g.insert_module(lib_mr);

            // main: `import a/b` then `pub fn run() { b.foo() }`.
            let alias = add_plain_import(&mut mr, m, "b", 1, lib);
            let run = add_def(&mut mr, m, "run", 3, EntityKind::Function, true);
            mr.add_reference(None, Reference::new(run.span, K::Definition, run));
            if with_use {
                // `b.foo()` — the `Qualified` member occurrence targets the
                // remote `foo`; the `Qualifier` occurrence targets the alias.
                add_ref(&mut mr, Some(run), (3, 16, 17), K::Qualifier, alias);
                add_ref(&mut mr, Some(run), (3, 18, 21), K::Qualified, foo);
            }
            g.insert_module(mr);
            (g.finish(), m)
        };
        let (g, m) = build(true);

        assert!(
            g.unused_diagnostics(m).is_empty(),
            "a plain qualified import whose member is used must not be flagged"
        );

        // Drop the use: the import is genuinely unused again and IS flagged
        // (the check is still live, not just disabled).
        let (g, m) = build(false);
        assert_eq!(sole_unused(&g, m).message, "unused import `b`");
    }

    /// Two plain qualified imports where only one is used: the unused one must
    /// still be flagged. The used import's `Qualifier` occurrence targets that
    /// alias alone, so it can never mask the genuinely-unused sibling.
    #[test]
    fn unused_diag_one_of_two_plain_qualified_imports_still_flagged() {
        let (mut g, m, mut mr) = main_graph();
        let lib_b = g.intern_module(&mp(&["a", "b"]));
        let lib_c = g.intern_module(&mp(&["a", "c"]));

        // `a/c` defines pub `used`; `a/b` defines pub `foo` (never used here).
        let used = def(lib_c, 1, 3, 7, EntityKind::Function);
        let mut c_mr = ModuleReferences::new(lib_c);
        c_mr.add_definition(Definition::new(
            used.module,
            used.span,
            "used",
            None,
            true,
            stub_kind(used),
        ));
        g.insert_module(c_mr);
        let foo = def(lib_b, 1, 3, 6, EntityKind::Function);
        let mut b_mr = ModuleReferences::new(lib_b);
        b_mr.add_definition(Definition::new(
            foo.module,
            foo.span,
            "foo",
            None,
            true,
            stub_kind(foo),
        ));
        g.insert_module(b_mr);

        // main:
        //   import a/b   (line 1, unused)
        //   import a/c   (line 2, used via c.used())
        //   pub fn run() { c.used() }
        add_plain_import(&mut mr, m, "b", 1, lib_b);
        let alias_c = add_plain_import(&mut mr, m, "c", 2, lib_c);
        let run = add_def(&mut mr, m, "run", 3, EntityKind::Function, true);
        // `c.used()` — `Qualifier` on the `c` alias + `Qualified` into `a/c`.
        add_ref(&mut mr, Some(run), (3, 16, 17), K::Qualifier, alias_c);
        add_ref(&mut mr, Some(run), (3, 18, 24), K::Qualified, used);
        g.insert_module(mr);
        let g = g.finish();

        assert_eq!(
            sole_unused(&g, m).message,
            "unused import `b`",
            "the unused `a/b` import must be flagged while the used `a/c` is not"
        );
    }

    /// Two aliases importing the SAME module (`import a/b` + `import a/b as
    /// bb`) where only `bb` is used: exactly the untouched `b` is flagged.
    /// Attribution by *target module* cannot tell the two apart — the
    /// pre-`Qualifier`-edge heuristic kept any alias alive whenever *some*
    /// qualified use resolved into its imported module, so `bb.foo()` wrongly
    /// suppressed `import a/b`'s hint. The direct alias edge does not.
    #[test]
    fn unused_diag_two_aliases_of_same_module_only_unused_one_flagged() {
        let (mut g, m, mut mr) = main_graph();
        let lib = g.intern_module(&mp(&["a", "b"]));

        let foo = def(lib, 1, 3, 6, EntityKind::Function);
        let mut lib_mr = ModuleReferences::new(lib);
        lib_mr.add_definition(Definition::new(
            foo.module,
            foo.span,
            "foo",
            None,
            true,
            stub_kind(foo),
        ));
        g.insert_module(lib_mr);

        // main:
        //   import a/b        (line 1, unused)
        //   import a/b as bb  (line 2, used via bb.foo())
        //   pub fn run() { bb.foo() }
        let alias_b = add_plain_import(&mut mr, m, "b", 1, lib);
        let alias_bb = add_plain_import(&mut mr, m, "bb", 2, lib);
        let run = add_def(&mut mr, m, "run", 4, EntityKind::Function, true);
        add_ref(&mut mr, Some(run), (4, 16, 18), K::Qualifier, alias_bb);
        add_ref(&mut mr, Some(run), (4, 19, 22), K::Qualified, foo);
        g.insert_module(mr);
        let g = g.finish();

        let d = sole_unused(&g, m);
        assert_eq!(
            d.message, "unused import `b`",
            "only the alias the use actually went through stays alive"
        );
        assert_eq!(d.span, alias_b.span);
    }

    /// An import used only as the qualifier of a constructor *pattern*
    /// (`case x { b.Ctor(..) -> ... }`): the pattern records a `Qualified`
    /// member occurrence on the remote constructor plus a `Qualifier`
    /// occurrence on the alias — exactly like the expression path — so the
    /// import is not flagged.
    #[test]
    fn unused_diag_alias_used_only_from_pattern_qualifier_is_kept() {
        let (mut g, m, mut mr) = main_graph();
        let lib = g.intern_module(&mp(&["a", "b"]));

        let ctor = def(lib, 2, 2, 6, EntityKind::Constructor);
        let mut lib_mr = ModuleReferences::new(lib);
        lib_mr.add_definition(Definition::new(
            ctor.module,
            ctor.span,
            "Ctor",
            None,
            true,
            stub_kind(ctor),
        ));
        g.insert_module(lib_mr);

        // main: `import a/b` then a match arm `b.Ctor(v) -> ...` inside `run`.
        let alias = add_plain_import(&mut mr, m, "b", 1, lib);
        let run = add_def(&mut mr, m, "run", 3, EntityKind::Function, true);
        add_ref(&mut mr, Some(run), (4, 8, 9), K::Qualifier, alias);
        add_ref(&mut mr, Some(run), (4, 10, 14), K::Qualified, ctor);
        g.insert_module(mr);
        let g = g.finish();

        assert!(
            g.unused_diagnostics(m).is_empty(),
            "an import used only from a pattern qualifier must not be flagged"
        );
    }
}
