use super::{ReferenceKind as K, *};
use std::collections::HashSet;

impl ModuleInterner {
    fn len(&self) -> usize {
        self.paths.len()
    }
}

impl ReferenceGraphBuilder {
    /// [`Self::insert`] on freshly built refs.
    pub(super) fn insert_module(&mut self, refs: ModuleReferences) {
        self.insert(Rc::new(refs));
    }
}

pub(super) fn mp(parts: &[&str]) -> ModulePath {
    parts.iter().map(|s| s.to_string()).collect()
}

pub(super) fn def(module: ModuleId, line: i32, c0: i32, c1: i32, kind: EntityKind) -> DefId {
    DefId::new(module, crate::span::Span::single_line(line, c0, c1), kind)
}

/// The default payload for `defid`'s entity, for tests.
pub(super) fn stub_kind(defid: DefId) -> DefinitionKind {
    match defid.entity {
        EntityKind::Value => DefinitionKind::Value,
        EntityKind::Function => DefinitionKind::Function {
            param_names: Vec::new(),
        },
        EntityKind::Constant => DefinitionKind::Constant,
        EntityKind::Constructor => DefinitionKind::Constructor {
            ctor_of: None,
            param_names: Vec::new(),
        },
        EntityKind::Type => DefinitionKind::Type,
        EntityKind::ModuleAlias => DefinitionKind::ModuleAlias {
            decl_span: defid.span,
            imports_module: None,
        },
        EntityKind::Field => DefinitionKind::Field,
    }
}

pub(super) fn add_ref(
    mr: &mut ModuleReferences,
    owner: Option<DefId>,
    (line, c0, c1): (i32, i32, i32),
    kind: ReferenceKind,
    target: DefId,
) {
    mr.add_reference(
        owner,
        Reference::new(crate::span::Span::single_line(line, c0, c1), kind, target),
    );
}

pub(super) fn main_graph() -> (ReferenceGraphBuilder, ModuleId, ModuleReferences) {
    let mut g = ReferenceGraphBuilder::new();
    let m = g.intern_module(&mp(&["main"]));
    let mr = ModuleReferences::new(m);
    (g, m, mr)
}

pub(super) fn add_def(
    mr: &mut ModuleReferences,
    m: ModuleId,
    name: &str,
    line: i32,
    kind: EntityKind,
    is_pub: bool,
) -> DefId {
    let d = def(m, line, 3, 3 + name.len() as i32, kind);
    mr.add_definition(Definition::new(
        d.module,
        d.span,
        name,
        None,
        is_pub,
        stub_kind(d),
    ));
    d
}

#[test]
fn interner_is_stable_and_bijective() {
    let mut it = ModuleInterner::default();
    let a = it.intern(&mp(&["main"]));
    let b = it.intern(&mp(&["scarlet", "array"]));
    let a2 = it.intern(&mp(&["main"]));

    assert_eq!(a, a2, "same path re-interns to the same id");
    assert_ne!(a, b);
    assert_eq!(it.len(), 2, "distinct paths get distinct ids");
    assert_eq!(it.path(a), Some(&mp(&["main"])));
    assert_eq!(it.path(b), Some(&mp(&["scarlet", "array"])));
    assert_eq!(it.lookup(&mp(&["scarlet", "array"])), Some(b));
    assert_eq!(
        it.lookup_key(&crate::module::ModuleKey::for_stdlib(&mp(&[
            "scarlet", "array"
        ]))),
        Some(b)
    );
    assert_eq!(it.lookup(&mp(&["nope"])), None);
    assert_eq!(it.path(ModuleId(99)), None);
}

#[test]
fn defid_hash_eq_consistent() {
    let m = ModuleId(3);
    let d1 = def(m, 1, 0, 4, EntityKind::Function);
    let d2 = def(m, 1, 0, 4, EntityKind::Function);
    let d3 = def(m, 1, 0, 4, EntityKind::Type); // entity differs
    let d4 = def(m, 2, 0, 4, EntityKind::Function); // span differs

    assert_eq!(d1, d2);
    assert_ne!(d1, d3);
    assert_ne!(d1, d4);

    let mut set: HashSet<DefId> = HashSet::new();
    set.insert(d1);
    assert!(set.contains(&d2), "equal DefIds hash to the same bucket");
    assert!(!set.contains(&d3));
    assert!(!set.contains(&d4));
}

#[test]
fn span_containment_is_half_open() {
    let s = Span::single_line(5, 2, 6); // columns [2, 6) on line 5
    assert!(!s.contains(5, 1));
    assert!(s.contains(5, 2));
    assert!(s.contains(5, 5));
    assert!(!s.contains(5, 6), "end column is exclusive");
    assert!(!s.contains(4, 3));

    let p = Span::point(7, 9);
    assert!(p.contains(7, 9));
    assert!(!p.contains(7, 10));
}

#[test]
fn span_width_orders_tightest_first() {
    let narrow = Span::single_line(1, 0, 3);
    let wide = Span::single_line(1, 0, 30);
    let multiline = Span {
        start_line: 1,
        start_column: 0,
        end_line: 3,
        end_column: 0,
    };
    assert!(narrow.width() < wide.width());
    assert!(wide.width() < multiline.width());
}

#[test]
fn module_refs_intra_module_reverse_index() {
    let m = ModuleId(0);
    let mut mr = ModuleReferences::new(m);

    let foo = def(m, 1, 3, 6, EntityKind::Function);
    mr.add_definition(Definition::new(
        foo.module,
        foo.span,
        "foo",
        Some("the foo".into()),
        true,
        stub_kind(foo),
    ));

    // two uses of foo inside some other (owner) definition
    let owner = def(m, 10, 3, 7, EntityKind::Function);
    add_ref(&mut mr, Some(owner), (11, 4, 7), K::Unqualified, foo);
    add_ref(&mut mr, Some(owner), (12, 4, 7), K::Unqualified, foo);

    assert_eq!(mr.defs_named("foo"), &[foo]);
    assert_eq!(mr.definition(foo).map(|d| d.name.as_str()), Some("foo"));
    assert_eq!(mr.occurrences().len(), 2);
}

#[test]
fn resolve_position_prefers_tightest_then_falls_back_to_def_name() {
    let m = ModuleId(0);
    let mut mr = ModuleReferences::new(m);

    let target = def(m, 1, 0, 3, EntityKind::Function);
    mr.add_definition(Definition::new(
        target.module,
        target.span,
        "fn",
        None,
        false,
        stub_kind(target),
    ));

    // A wide occurrence and a tighter one over the same point.
    let wide = def(m, 9, 0, 1, EntityKind::Value);
    add_ref(&mut mr, None, (20, 0, 40), K::Unqualified, wide);
    add_ref(&mut mr, None, (20, 10, 14), K::Unqualified, target);

    assert_eq!(mr.resolve_position(20, 12), Some(target));
    assert_eq!(mr.resolve_position(20, 2), Some(wide));
    assert_eq!(mr.resolve_position(1, 1), Some(target));
    assert_eq!(mr.resolve_position(99, 0), None);
}

fn two_module_graph() -> (ReferenceGraph, ModuleId, ModuleId, DefId) {
    let mut g = ReferenceGraphBuilder::new();
    let lib = g.intern_module(&mp(&["lib"]));
    let app = g.intern_module(&mp(&["app"]));

    // lib defines `helper` (pub) at line 1.
    let helper = def(lib, 1, 3, 9, EntityKind::Function);
    let mut lib_mr = ModuleReferences::new(lib);
    lib_mr.add_definition(Definition::new(
        helper.module,
        helper.span,
        "helper",
        Some("doc".into()),
        true,
        stub_kind(helper),
    ));
    g.insert_module(lib_mr);

    // app uses `helper` twice (qualified), inside app's `run`.
    let run = def(app, 1, 3, 6, EntityKind::Function);
    let mut app_mr = ModuleReferences::new(app);
    app_mr.add_definition(Definition::new(
        run.module,
        run.span,
        "run",
        None,
        true,
        stub_kind(run),
    ));
    add_ref(&mut app_mr, Some(run), (2, 4, 14), K::Qualified, helper);
    add_ref(&mut app_mr, Some(run), (3, 4, 14), K::Qualified, helper);
    g.insert_module(app_mr);

    (g.finish(), lib, app, helper)
}

#[test]
fn graph_cross_module_goto_def_and_find_refs() {
    let (g, _lib, app, helper) = two_module_graph();

    // goto-def from a use in `app` lands on the `lib` definition.
    let d = g
        .definition_at(app, 2, 8)
        .expect("position resolves to a definition");
    assert_eq!(d.name, "helper");
    assert_eq!(d.defid, helper);
    assert_eq!(d.doc.as_deref(), Some("doc"));

    // hover surfaces the same definition record.
    assert_eq!(g.definition_at(app, 3, 8).map(|d| d.defid), Some(helper));

    // find-references returns both occurrences, both attributed to `app`.
    let refs = g.references_to(helper);
    assert_eq!(refs.len(), 2);
    assert!(refs.iter().all(|r| r.module == app));
    assert!(refs.iter().all(|r| r.kind == K::Qualified));
}

#[test]
fn graph_symbol_queries() {
    let (g, lib, _app, helper) = two_module_graph();

    let lib_syms: Vec<&str> = g.defs_in(lib).map(|d| d.name.as_str()).collect();
    assert_eq!(lib_syms, vec!["helper"]);

    let mut all: Vec<&str> = g.all_defs().map(|d| d.name.as_str()).collect();
    all.sort_unstable();
    assert_eq!(all, vec!["helper", "run"]);

    assert_eq!(
        g.definition(helper).map(|d| d.name.as_str()),
        Some("helper")
    );
}

#[test]
fn omitting_a_referencing_module_leaves_no_dangling_reverse_edges() {
    // Nothing removes a module. A fresh graph is built from the live set,
    // so evicting a module means leaving it out. Build from `lib` alone and
    // no reverse edge into `helper` can survive.
    let mut g = ReferenceGraphBuilder::new();
    let lib = g.intern_module(&mp(&["lib"]));
    let helper = def(lib, 1, 3, 9, EntityKind::Function);
    let mut lib_mr = ModuleReferences::new(lib);
    lib_mr.add_definition(Definition::new(
        helper.module,
        helper.span,
        "helper",
        Some("doc".into()),
        true,
        stub_kind(helper),
    ));
    g.insert(Rc::new(lib_mr));
    let g = g.finish();

    assert_eq!(
        g.references_to(helper).len(),
        0,
        "no reverse edge may dangle when the referencing module is omitted"
    );
    // `helper`'s own definition still resolves from lib.
    assert_eq!(
        g.definition(helper).map(|d| d.name.as_str()),
        Some("helper")
    );
}

#[test]
fn value_binders_are_excluded_from_symbol_surfaces() {
    // Local binders are `EntityKind::Value` definitions so goto-def and
    // rename resolve on them, but they must not reach the documentSymbol
    // outline. `Definition::is_symbol_listable` is the one place that
    // decides this.
    let (mut g, m, mut mr) = main_graph();
    let func = add_def(&mut mr, m, "compute", 1, EntityKind::Function, true);
    let local = add_def(&mut mr, m, "tmp", 2, EntityKind::Value, false);
    g.insert_module(mr);
    let g = g.finish();

    // Only the local `Value` is unlistable.
    assert!(
        g.definition(func)
            .expect("fn def present")
            .is_symbol_listable()
    );
    assert!(
        !g.definition(local)
            .expect("value def present")
            .is_symbol_listable()
    );

    // The underlying iteration stays unfiltered, so resolution and
    // reachability still walk the `Value` binder.
    let raw: Vec<&str> = g.defs_in(m).map(|d| d.name.as_str()).collect();
    assert!(raw.contains(&"compute") && raw.contains(&"tmp"), "{raw:?}");

    // The symbol projection keeps the function and drops the local.
    let surfaced: Vec<&str> = g
        .defs_in(m)
        .filter(|d| d.is_symbol_listable())
        .map(|d| d.name.as_str())
        .collect();
    assert_eq!(surfaced, vec!["compute"]);
}
