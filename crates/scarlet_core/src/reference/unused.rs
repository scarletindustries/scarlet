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
mod tests {
    use super::super::{
        Definition, DefinitionKind, ModuleReferences, Reference, ReferenceKind as K, add_def,
        add_ref, def, main_graph, mp, stub_kind,
    };
    use super::*;
    use crate::diagnostic::Severity;

    #[track_caller]
    fn sole_unused(g: &ReferenceGraph, m: ModuleId) -> Diagnostic<UnusedDef> {
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
        assert_eq!(d.severity(), Severity::Hint);
        assert_eq!(d.message(), "unused function `dead`");
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
        assert_eq!(d.message(), "unused function `dead`");
        assert_eq!(d.span, dead.span);
    }

    #[test]
    fn unused_diag_self_recursive_only_is_flagged() {
        let (mut g, m, mut mr) = main_graph();
        let spin = add_def(&mut mr, m, "spin", 1, EntityKind::Function, false);
        // `spin` only refers to itself, and an edge to itself never makes it reachable.
        add_ref(&mut mr, Some(spin), (2, 4, 8), K::Unqualified, spin);
        g.insert_module(mr);
        let g = g.finish();

        assert_eq!(sole_unused(&g, m).message(), "unused function `spin`");
    }

    #[test]
    fn unused_diag_only_reports_entry_module() {
        let (mut g, entry, mut em) = main_graph();
        let lib = g.intern_module(&mp(&["lib"]));

        add_def(&mut em, entry, "go", 1, EntityKind::Function, true);
        g.insert_module(em);

        // Prelude/@vm/library defs live here; they must never surface.
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
        assert_eq!(d.message(), "unused import `io`");
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

        assert_eq!(sole_unused(&g, m).message(), "unused import `util`");
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
        let msgs: Vec<String> = d.iter().map(|x| x.message()).collect();
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

    /// `Compiler::emit_def` records a `K::Definition` occurrence of every
    /// top-level declaration itself, with `owner == None`. Those must not count
    /// as roots from the entry body, or every definition would be its own root.
    #[test]
    fn unused_diag_definition_self_occurrence_does_not_root_dead_code() {
        let (mut g, m, mut mr) = main_graph();

        let api = add_def(&mut mr, m, "api", 1, EntityKind::Function, true);
        let used = add_def(&mut mr, m, "used", 3, EntityKind::Function, false);
        let dead = add_def(&mut mr, m, "dead", 5, EntityKind::Function, false);

        for d in [api, used, dead] {
            mr.add_reference(None, Reference::new(d.span, K::Definition, d));
        }
        // pub `api` calls private `used`; nobody calls `dead`.
        add_ref(&mut mr, Some(api), (2, 4, 8), K::Unqualified, used);
        g.insert_module(mr);
        let g = g.finish();

        let d = sole_unused(&g, m);
        assert_eq!(d.message(), "unused function `dead`");
        assert_eq!(d.span, dead.span);
    }

    /// A plain `import a/b` alias, built the way the compiler builds it: the
    /// `Definition`, its occurrence of itself, and the `Import` occurrence
    /// whose target is in the imported module.
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

    /// With `import a/b` and `b.foo()`, the `Qualifier` occurrence on `b` is
    /// what keeps the import alive. Dropping the use must flag it again.
    #[test]
    fn unused_diag_plain_qualified_import_recovers_use() {
        let build = |with_use: bool| {
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
            lib_mr.add_reference(None, Reference::new(foo.span, K::Definition, foo));
            g.insert_module(lib_mr);

            // main: `import a/b` then `pub fn run() { b.foo() }`.
            let alias = add_plain_import(&mut mr, m, "b", 1, lib);
            let run = add_def(&mut mr, m, "run", 3, EntityKind::Function, true);
            mr.add_reference(None, Reference::new(run.span, K::Definition, run));
            if with_use {
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

        // Drop the use, to show the check still works and isn't just switched
        // off.
        let (g, m) = build(false);
        assert_eq!(sole_unused(&g, m).message(), "unused import `b`");
    }

    /// Two plain qualified imports, with one used. The used one's `Qualifier`
    /// occurrence names its own alias, so it can't hide the other one.
    #[test]
    fn unused_diag_one_of_two_plain_qualified_imports_still_flagged() {
        let (mut g, m, mut mr) = main_graph();
        let lib_b = g.intern_module(&mp(&["a", "b"]));
        let lib_c = g.intern_module(&mp(&["a", "c"]));

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
        add_ref(&mut mr, Some(run), (3, 16, 17), K::Qualifier, alias_c);
        add_ref(&mut mr, Some(run), (3, 18, 24), K::Qualified, used);
        g.insert_module(mr);
        let g = g.finish();

        assert_eq!(
            sole_unused(&g, m).message(),
            "unused import `b`",
            "the unused `a/b` import must be flagged while the used `a/c` is not"
        );
    }

    /// Two aliases of the SAME module, with only `bb` used, so only `b` is
    /// flagged. Going by the target module can't tell the two apart, so the
    /// edge that keeps an alias alive has to name the alias.
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
            d.message(),
            "unused import `b`",
            "only the alias the use actually went through stays alive"
        );
        assert_eq!(d.span, alias_b.span);
    }

    /// An import used only as a constructor *pattern*'s qualifier records the
    /// same occurrences as the expression path, so it is not flagged.
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
