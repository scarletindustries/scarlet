//! What a long-lived `Build` checks again after an edit, over a 3-module chain
//! A→B→C. A module is checked again when its own source changes, or when an
//! import's interface does. Editing a body that keeps the interface checks
//! only that module.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use scarlet::build::{self, Analysis, Build, Entry};
use scarlet::check::{Role, UnusedBindings};
use scarlet::module::ModuleKey;
use scarlet::reference::ReferenceGraph;
use scarlet::sources::{DiskSources, Sources, Text};

mod common;
use common::{Project, cursor};

const A_SRC: &str = "import ./b\npub fn main() {\n\tprintln(b::b())\n}\n";
const B_SRC: &str = "import ./c\npub fn b() Int { c::val() + 1 }\n";
const C_SRC: &str = "pub fn val() Int { 1 }\n";

/// The disk, with editor buffers in front of it.
#[derive(Default)]
struct Editor {
    buffers: HashMap<PathBuf, Text>,
    disk: DiskSources,
}

impl Sources for Editor {
    fn read(&mut self, path: &Path) -> std::io::Result<Text> {
        match self.buffers.get(path) {
            Some(t) => Ok(t.clone()),
            None => self.disk.read(path),
        }
    }
}

/// A long-lived build, and where it reads from.
#[derive(Default)]
struct Session {
    build: Build,
    sources: Editor,
}

impl Session {
    /// Check `src` as the entry of `p`.
    fn analyse(&mut self, p: &Project, src: &str) -> Analysis {
        let parsed = build::parse(src);
        self.build.check(
            &mut self.sources,
            Entry {
                source: scarlet::source::SourceFile::new(ModuleKey::main(), None, src),
                ast: &parsed.ast,
                doc: parsed.doc,
                parse: parsed.diagnostics,
                base_dir: Some(&p.dir),
                role: Role::Entry,
                unused: UnusedBindings::Report,
            },
        )
    }

    /// Check `src` as the entry of `p`, returning how many modules were
    /// checked and whether everything came out clean.
    fn check(&mut self, p: &Project, src: &str) -> (usize, bool) {
        let a = self.analyse(p, src);
        let clean = !scarlet::diagnostic::has_errors(&a.diagnostics());
        (a.counts().checks, clean)
    }
}

fn fresh(tag: &str) -> (Project, Session) {
    let p = Project::new(tag);
    p.write("c.sca", C_SRC);
    p.write("b.sca", B_SRC);
    p.write("a.sca", A_SRC);
    let mut s = Session::default();
    let (_, clean) = s.check(&p, A_SRC);
    assert!(clean, "the chain checks");
    (p, s)
}

#[test]
fn only_what_changed_is_checked_again() {
    let (p, mut b) = fresh("3mod");

    assert_eq!(b.check(&p, A_SRC), (0, true), "nothing changed");

    // A new body with the same interface: only c is checked again.
    p.write("c.sca", "pub fn val() Int { 2 }\n");
    assert_eq!(b.check(&p, A_SRC), (1, true), "c's body changed");

    // A new public function changes c's interface, so b is checked again too.
    // b's own interface doesn't change, so the entry isn't.
    p.write("c.sca", "pub fn val() Int { 2 }\npub fn more() Int { 3 }\n");
    assert_eq!(b.check(&p, A_SRC), (2, true), "c's interface changed");

    p.write("b.sca", "import ./c\npub fn b() Int { c::val() + 100 }\n");
    assert_eq!(b.check(&p, A_SRC), (1, true), "b's body changed");

    // A type error in c is reported, though nothing else needs checking.
    p.write(
        "c.sca",
        "pub fn val() Int { 'nope' }\npub fn more() Int { 3 }\n",
    );
    assert_eq!(b.check(&p, A_SRC), (1, false), "c broke");

    p.write("c.sca", "pub fn val() Int { 3 }\npub fn more() Int { 3 }\n");
    assert_eq!(b.check(&p, A_SRC), (1, true), "c is fixed");
}

/// A module whose text hasn't changed is neither parsed nor checked again,
/// the stdlib included: only what changed is read.
#[test]
fn an_unchanged_module_is_not_parsed_again() {
    let (p, mut b) = fresh("noparse");
    let a = b.analyse(&p, A_SRC);
    assert_eq!(a.counts().checks, 0);
    assert_eq!(
        a.counts().parses,
        0,
        "nothing changed, so nothing was parsed"
    );

    p.write("c.sca", "pub fn val() Int { 9 }\n");
    let a = b.analyse(&p, A_SRC);
    assert_eq!(a.counts().parses, 1, "only c was parsed");
}

#[test]
fn an_editor_buffer_wins_over_the_file() {
    let (p, mut b) = fresh("overlay");
    let c = p.dir.join("c.sca");

    b.sources
        .buffers
        .insert(c.clone(), Text::new("pub fn val() Int { 'oops' }\n"));
    let (_, clean) = b.check(&p, A_SRC);
    assert!(!clean, "the unsaved buffer is checked, not the clean file");

    b.sources.buffers.remove(&c);
    let (n, clean) = b.check(&p, A_SRC);
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

    let a = b.analyse(&p, A_SRC);
    let g = ReferenceGraph::of_modules(a.modules());
    let m = g.module_id_by_key(&main).expect("main is in the graph");
    let def = g.def_id_at(m, l, c).expect("b::b() resolves");
    let path = g.module_path(def.module).expect("b's module");
    assert_eq!(path.last().map(String::as_str), Some("b"));
    assert!(!g.references_to(def).is_empty(), "the entry uses b");

    p.write("b.sca", "import ./c\npub fn b() Int { c::val() + 41 }\n");
    let a = b.analyse(&p, A_SRC);
    let g = ReferenceGraph::of_modules(a.modules());
    let after = g.def_id_at(m, l, c).expect("b::b() still resolves");
    assert_eq!(after, def, "b's definition kept its identity");
    assert!(!g.references_to(after).is_empty(), "the entry still uses b");

    let entry = "import ./b\npub fn main() {\n\tprintln(b::gone())\n}\n";
    p.write("b.sca", "import ./c\npub fn gone() Int { c::val() }\n");
    p.write("a.sca", entry);
    let a = b.analyse(&p, entry);
    let g = ReferenceGraph::of_modules(a.modules());
    assert!(
        g.references_to(def).is_empty(),
        "the old b has no uses left"
    );
}

/// A check is fresh only while every interface it read is unchanged, even
/// one it reached through another module's types. The entry reads a field of
/// a type from `c`, which it doesn't import, so when the field is renamed it
/// is checked again and fails.
#[test]
fn a_change_two_imports_away_reaches_the_entry() {
    let p = Project::new("transitive");
    p.write(
        "c.sca",
        "pub type T {\n\tT(n Int)\n}\npub fn make() T { T(n: 1) }\n",
    );
    p.write("b.sca", "import ./c\npub fn t() c::T { c::make() }\n");
    let entry = "import ./b\npub fn main() {\n\tprintln(b::t().n + 1)\n}\n";
    let mut s = Session::default();
    assert!(s.check(&p, entry).1, "the field exists");

    p.write(
        "c.sca",
        "pub type T {\n\tT(m Int)\n}\npub fn make() T { T(m: 1) }\n",
    );
    let a = s.analyse(&p, entry);
    assert!(
        a.diagnostics().iter().any(|d| d.message.contains("'n'")),
        "the entry reads a field that is gone: {:?}",
        a.diagnostics()
    );
}
