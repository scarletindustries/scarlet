//! What a long-lived `Build` checks again after an edit, over a 3-module chain
//! A→B→C. A module is checked again when its own source changes, or when an
//! import's interface does. Editing a body that keeps the interface checks
//! only that module.

use scarlet::build::{self, Build, Entry};
use scarlet::check::{Role, UnusedBindings};
use scarlet::module::ModuleKey;
use scarlet::reference::ReferenceGraph;

mod common;
use common::{Project, cursor};

const A_SRC: &str = "import ./b\npub fn main() {\n\tprintln(b::b())\n}\n";
const B_SRC: &str = "import ./c\npub fn b() Int { c::val() + 1 }\n";
const C_SRC: &str = "pub fn val() Int { 1 }\n";

/// Check `src` as the entry of `p`, returning how many modules were checked
/// and whether everything came out clean.
fn check(b: &mut Build, p: &Project, src: &str) -> (usize, bool) {
    let before = b.checks();
    let parsed = build::parse(src);
    let reach = b.check_entry(Entry {
        key: ModuleKey::main(),
        ast: &parsed.ast,
        doc: parsed.doc,
        parse: parsed.diagnostics,
        hash: build::source_hash(src),
        base_dir: Some(&p.dir),
        role: Role::Entry,
        unused: UnusedBindings::Report,
    });
    let clean = !scarlet::diagnostic::has_errors(&b.diagnostics(&reach));
    (b.checks() - before, clean)
}

fn fresh(tag: &str) -> (Project, Build) {
    let p = Project::new(tag);
    p.write("c.sca", C_SRC);
    p.write("b.sca", B_SRC);
    p.write("a.sca", A_SRC);
    let mut b = Build::default();
    let (_, clean) = check(&mut b, &p, A_SRC);
    assert!(clean, "the chain checks");
    (p, b)
}

#[test]
fn only_what_changed_is_checked_again() {
    let (p, mut b) = fresh("3mod");

    assert_eq!(check(&mut b, &p, A_SRC), (0, true), "nothing changed");

    // A new body with the same interface: only c is checked again.
    p.write("c.sca", "pub fn val() Int { 2 }\n");
    assert_eq!(check(&mut b, &p, A_SRC), (1, true), "c's body changed");

    // A new public function changes c's interface, so b is checked again too.
    // b's own interface doesn't change, so the entry isn't.
    p.write("c.sca", "pub fn val() Int { 2 }\npub fn more() Int { 3 }\n");
    assert_eq!(check(&mut b, &p, A_SRC), (2, true), "c's interface changed");

    p.write("b.sca", "import ./c\npub fn b() Int { c::val() + 100 }\n");
    assert_eq!(check(&mut b, &p, A_SRC), (1, true), "b's body changed");

    // A type error in c is reported, though nothing else needs checking.
    p.write(
        "c.sca",
        "pub fn val() Int { 'nope' }\npub fn more() Int { 3 }\n",
    );
    assert_eq!(check(&mut b, &p, A_SRC), (1, false), "c broke");

    p.write("c.sca", "pub fn val() Int { 3 }\npub fn more() Int { 3 }\n");
    assert_eq!(check(&mut b, &p, A_SRC), (1, true), "c is fixed");
}

/// A module whose text hasn't changed is neither parsed nor checked again,
/// the stdlib included: only what changed is read.
#[test]
fn an_unchanged_module_is_not_parsed_again() {
    let (p, mut b) = fresh("noparse");
    let parses = b.parses();
    assert_eq!(check(&mut b, &p, A_SRC), (0, true));
    assert_eq!(b.parses(), parses, "nothing changed, so nothing was parsed");

    p.write("c.sca", "pub fn val() Int { 9 }\n");
    check(&mut b, &p, A_SRC);
    assert_eq!(b.parses(), parses + 1, "only c was parsed");
}

#[test]
fn an_editor_buffer_wins_over_the_file() {
    let (p, mut b) = fresh("overlay");
    let c = p.dir.join("c.sca");

    b.set_overlay(c.clone(), "pub fn val() Int { 'oops' }\n".to_string());
    let (_, clean) = check(&mut b, &p, A_SRC);
    assert!(!clean, "the unsaved buffer is checked, not the clean file");

    b.clear_overlay(&c);
    let (n, clean) = check(&mut b, &p, A_SRC);
    assert!(clean, "the file is read again once the buffer is gone");
    assert_eq!(n, 1, "only c is checked again");
}

/// The graph is built from every module's own check, so a use in the entry
/// reaches a definition two imports away, and an edit leaves no stale edge.
#[test]
fn the_graph_follows_the_chain_across_an_edit() {
    let (p, mut b) = fresh("graph");
    let main = ModuleKey::main();
    let (l, c) = cursor(A_SRC, "b()", 1, 0);

    let g = ReferenceGraph::of_build(&b);
    let m = g.module_id_by_key(&main).expect("main is in the graph");
    let def = g.def_id_at(m, l, c).expect("b::b() resolves");
    let path = g.module_path(def.module).expect("b's module");
    assert_eq!(path.last().map(String::as_str), Some("b"));
    assert!(!g.references_to(def).is_empty(), "the entry uses b");

    p.write("b.sca", "import ./c\npub fn b() Int { c::val() + 41 }\n");
    check(&mut b, &p, A_SRC);
    let g = ReferenceGraph::of_build(&b);
    let after = g.def_id_at(m, l, c).expect("b::b() still resolves");
    assert_eq!(after, def, "b's definition kept its identity");
    assert!(!g.references_to(after).is_empty(), "the entry still uses b");

    let entry = "import ./b\npub fn main() {\n\tprintln(b::gone())\n}\n";
    p.write("b.sca", "import ./c\npub fn gone() Int { c::val() }\n");
    p.write("a.sca", entry);
    check(&mut b, &p, entry);
    let g = ReferenceGraph::of_build(&b);
    assert!(
        g.references_to(def).is_empty(),
        "the old b has no uses left"
    );
}
