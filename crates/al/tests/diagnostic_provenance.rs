//! Diagnostic source-file provenance: a type error raised while checking an
//! *imported* module carries a span into that module's text, so it must be
//! stamped with the module's key and rendered against the module's file — not
//! caret-sliced out of the entry file's (unrelated) text.

mod common;
use common::{Project, module_key};

use std::path::PathBuf;

use al::{ast, bytecode, diagnostic, module, parser, scanner};

/// The resolver the CLI uses: a stamped `ModuleKey` is an on-disk module (the
/// key is its canonical path, `.al` stripped) or an embedded stdlib module.
fn resolve(key: &module::ModuleKey) -> Option<(PathBuf, String)> {
    let path: module::ModulePath = key.as_str().split('/').map(str::to_string).collect();
    match module::resolve_canonical(&path).ok()?.source {
        module::ModuleSource::File(p) => {
            let text = std::fs::read_to_string(&p).ok()?;
            Some((p, text))
        }
        module::ModuleSource::Embedded(s) => Some((PathBuf::from(key.as_str()), s.to_string())),
    }
}

fn check_project(p: &Project, entry_src: &str) -> bytecode::CompileResult {
    let mut sc = scanner::new_scanner(entry_src.to_string());
    let mut ps = parser::new_parser(&mut sc);
    let parsed = ps.parse_program();
    assert!(
        parsed.diagnostics.is_empty(),
        "entry failed to parse: {:?}",
        parsed.diagnostics
    );
    let expr = ast::Expression::BlockExpression(parsed.ast);
    bytecode::check(&expr, Some(&p.dir), Some(&al::STDLIB))
}

#[test]
fn imported_module_type_error_is_stamped_with_that_module() {
    let p = Project::new("diagprov");
    p.write("lib.al", "pub fn broken() Int {\n  \"not an int\"\n}\n");
    let entry_src = "import ./lib\n_x = lib.broken\n";

    let result = check_project(&p, entry_src);
    assert!(!result.success, "expected the lib type error to surface");

    let lib_key = module_key(&p.dir, "lib.al");
    let stamped = result
        .diagnostics
        .iter()
        .find(|d| d.severity == diagnostic::Severity::Error)
        .expect("no error diagnostic");
    assert_eq!(
        stamped.source.as_ref(),
        Some(&lib_key),
        "type error in lib.al not stamped with lib's key: {:?}",
        result.diagnostics
    );
}

#[test]
fn imported_module_type_error_renders_against_that_modules_text() {
    let p = Project::new("diagprovrender");
    p.write("lib.al", "pub fn broken() Int {\n  \"not an int\"\n}\n");
    let entry_src = "import ./lib\n_x = lib.broken\n";

    let result = check_project(&p, entry_src);
    assert!(!result.success, "expected the lib type error to surface");

    let out = diagnostic::render_diagnostics(&result.diagnostics, entry_src, "main.al", &resolve);

    // Header names the imported module's file, not the entry.
    assert!(
        out.contains("lib.al:"),
        "diagnostic location does not name lib.al:\n{out}"
    );
    // The snippet is sliced from lib.al's text...
    assert!(
        out.contains("\"not an int\""),
        "snippet not taken from lib.al's text:\n{out}"
    );
    // ...and not from the entry file, whose line 2 sits at the same span.
    assert!(
        !out.contains("_x = lib.broken"),
        "snippet wrongly sliced from the entry file's text:\n{out}"
    );
}

#[test]
fn unresolvable_source_prints_location_but_never_a_snippet() {
    let p = Project::new("diagprovgone");
    p.write("lib.al", "pub fn broken() Int {\n  \"not an int\"\n}\n");
    let entry_src = "import ./lib\n_x = lib.broken\n";

    let result = check_project(&p, entry_src);
    assert!(!result.success, "expected the lib type error to surface");

    // A resolver with no answer: header + location only, no caret into the
    // entry file's text.
    let out = diagnostic::render_diagnostics(&result.diagnostics, entry_src, "main.al", &|_| None);
    assert!(
        !out.contains("_x = lib.broken") && !out.contains("import ./lib"),
        "snippet sliced from the entry file for a foreign diagnostic:\n{out}"
    );
    let lib_key = module_key(&p.dir, "lib.al");
    assert!(
        out.contains(lib_key.as_str()),
        "location line does not name the foreign module:\n{out}"
    );
}
