//! An error from an imported module carries a span into that module's text, so
//! it must render against that module's file, not the entry file.

mod common;
use common::{Project, module_key};

use std::path::PathBuf;

use scarlet::{ast, bytecode, diagnostic, module, parser, scanner};

/// The resolver the CLI uses: a `ModuleKey` names an on-disk module (its
/// canonical path with `.scrl` stripped) or an embedded stdlib module.
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

const LIB_SRC: &str = "pub fn broken() Int {\n  \"not an int\"\n}\n";

/// The entry's second line sits at the same span as lib's error, so a snippet
/// sliced from the wrong file still looks plausible.
const ENTRY_SRC: &str = "import ./lib\npub fn probe() Int { lib.broken() }\n";

fn check_project(p: &Project, entry_src: &str) -> bytecode::CompileResult {
    let mut sc = scanner::new_scanner(entry_src.to_string());
    let ps = parser::new_parser(&mut sc);
    let parsed = ps.parse_program();
    assert!(
        parsed.diagnostics.is_empty(),
        "entry failed to parse: {:?}",
        parsed.diagnostics
    );
    let expr = ast::Expression::BlockExpression(parsed.ast);
    bytecode::check(&expr, Some(&p.dir))
}

#[test]
fn imported_module_type_error_is_stamped_with_that_module() {
    let p = Project::new("diagprov");
    p.write("lib.scrl", LIB_SRC);

    let result = check_project(&p, ENTRY_SRC);
    assert!(!result.success(), "expected the lib type error to surface");

    let lib_key = module_key(&p.dir, "lib.scrl");
    let stamped = result
        .diagnostics
        .iter()
        .find(|d| d.severity == diagnostic::Severity::Error)
        .expect("no error diagnostic");
    assert_eq!(
        stamped.source.as_ref(),
        Some(&lib_key),
        "type error in lib.scrl not stamped with lib's key: {:?}",
        result.diagnostics
    );
}

#[test]
fn imported_module_type_error_renders_against_that_modules_text() {
    let p = Project::new("diagprovrender");
    p.write("lib.scrl", LIB_SRC);

    let result = check_project(&p, ENTRY_SRC);
    assert!(!result.success(), "expected the lib type error to surface");

    let out = diagnostic::render_diagnostics(&result.diagnostics, ENTRY_SRC, "main.scrl", &resolve);

    assert!(
        out.contains("lib.scrl:"),
        "diagnostic location does not name lib.scrl:\n{out}"
    );
    assert!(
        out.contains("\"not an int\""),
        "snippet not taken from lib.scrl's text:\n{out}"
    );
    assert!(
        !out.contains("pub fn probe"),
        "snippet wrongly sliced from the entry file's text:\n{out}"
    );
}

#[test]
fn unresolvable_source_prints_location_but_never_a_snippet() {
    let p = Project::new("diagprovgone");
    p.write("lib.scrl", LIB_SRC);

    let result = check_project(&p, ENTRY_SRC);
    assert!(!result.success(), "expected the lib type error to surface");

    let out =
        diagnostic::render_diagnostics(&result.diagnostics, ENTRY_SRC, "main.scrl", &|_| None);
    assert!(
        !out.contains("pub fn probe") && !out.contains("import ./lib"),
        "snippet sliced from the entry file for a foreign diagnostic:\n{out}"
    );
    let lib_key = module_key(&p.dir, "lib.scrl");
    assert!(
        out.contains(lib_key.as_str()),
        "location line does not name the foreign module:\n{out}"
    );
}
