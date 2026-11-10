//! An error from an imported module carries a span into that module's text, so
//! it must render against that module's file, not the entry file.

mod common;
use common::{Project, module_key};

use scarlet::build::{self, Build, Entry};
use scarlet::check::{Role, UnusedBindings};
use scarlet::{diagnostic, module};

const LIB_SRC: &str = "pub fn broken() Int {\n  \"not an int\"\n}\n";

/// The entry's second line sits at the same span as lib's error, so a snippet
/// sliced from the wrong file still looks plausible.
const ENTRY_SRC: &str = "import ./lib\npub fn probe() Int { lib::broken() }\n";

/// What checking a project's entry reported.
struct Checked {
    diagnostics: Vec<diagnostic::Diagnostic>,
}

impl Checked {
    fn success(&self) -> bool {
        !diagnostic::has_errors(&self.diagnostics)
    }
}

fn check_project(p: &Project, entry_src: &str) -> Checked {
    let parsed = build::parse(entry_src);
    assert!(
        parsed.diagnostics.is_empty(),
        "entry failed to parse: {:?}",
        parsed.diagnostics
    );
    let mut b = Build::default();
    let reach = b.check_entry(Entry {
        source: scarlet::source::SourceFile::new(module::ModuleKey::main(), None, entry_src),
        ast: &parsed.ast,
        doc: parsed.doc,
        parse: parsed.diagnostics,
        base_dir: Some(&p.dir),
        role: Role::Entry,
        unused: UnusedBindings::Report,
    });
    Checked {
        diagnostics: b.diagnostics(&reach),
    }
}

#[test]
fn imported_module_type_error_is_stamped_with_that_module() {
    let p = Project::new("diagprov");
    p.write("lib.sca", LIB_SRC);

    let result = check_project(&p, ENTRY_SRC);
    assert!(!result.success(), "expected the lib type error to surface");

    let lib_key = module_key(&p.dir, "lib.sca");
    let stamped = result
        .diagnostics
        .iter()
        .find(|d| d.severity == diagnostic::Severity::Error)
        .expect("no error diagnostic");
    assert_eq!(
        stamped.source.as_ref().map(|s| &s.key),
        Some(&lib_key),
        "type error in lib.sca not stamped with lib's key: {:?}",
        result.diagnostics
    );
}

#[test]
fn imported_module_type_error_renders_against_that_modules_text() {
    let p = Project::new("diagprovrender");
    p.write("lib.sca", LIB_SRC);

    let result = check_project(&p, ENTRY_SRC);
    assert!(!result.success(), "expected the lib type error to surface");

    let out = diagnostic::render_diagnostics(&result.diagnostics, ENTRY_SRC, "main.sca");

    assert!(
        out.contains("lib.sca:"),
        "diagnostic location does not name lib.sca:\n{out}"
    );
    assert!(
        out.contains("\"not an int\""),
        "snippet not taken from lib.sca's text:\n{out}"
    );
    assert!(
        !out.contains("pub fn probe"),
        "snippet wrongly sliced from the entry file's text:\n{out}"
    );
}

/// A diagnostic shows the text the check saw, even when the file changes on
/// disk afterwards. Nothing reads it again to print it.
#[test]
fn a_diagnostic_renders_the_text_its_check_read() {
    let p = Project::new("diagprovgone");
    p.write("lib.sca", LIB_SRC);

    let result = check_project(&p, ENTRY_SRC);
    assert!(!result.success(), "expected the lib type error to surface");
    p.write("lib.sca", "pub fn broken() Int { 1 }\n");

    let out = diagnostic::render_diagnostics(&result.diagnostics, ENTRY_SRC, "main.sca");
    assert!(
        out.contains("\"not an int\""),
        "snippet not taken from the text the check read:\n{out}"
    );
}

/// The entry's own errors show its code, like every other module's.
#[test]
fn the_entrys_errors_show_its_code() {
    let src = "pub fn main() {\n\tx Int = 'not an int'\n\tprintln(x)\n}\n";
    let out = common::run_source("check", src).combined();
    assert!(
        out.contains("x Int = 'not an int'"),
        "no snippet under the entry's error:\n{out}"
    );
}
