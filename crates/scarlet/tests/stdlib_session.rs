//! A build that reads the stdlib from source must check a stdlib file, as its
//! own `scarlet/...` module, with the same diagnostics as a batch check: none,
//! for a clean stdlib module. Every module the check reaches must be clean too.
//!
//! This is a regression test. The old incremental session rewound one shared
//! inference engine between checks, and a prelude constructor's scheme once
//! aliased the fresh rigid variables of the first module checked after a
//! rewind. The open file then got phantom `Type mismatch` diagnostics at the
//! imported module's spans.

use scarlet_core::build::{self, Build, Entry};
use scarlet_core::check::{Role, UnusedBindings};
use scarlet_core::module::ModuleKey;
use std::path::{Path, PathBuf};

fn stdlib_root() -> PathBuf {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../scarlet_core/src/std");
    root.canonicalize().unwrap()
}

/// Checks `text` as stdlib module `module` in a build reading the stdlib from
/// `root`, and renders every diagnostic the check reaches.
fn check_as(root: &Path, text: &str, base_dir: &Path, module: Vec<String>) -> Vec<String> {
    let parsed = build::parse(text);
    assert!(
        parsed.diagnostics.is_empty(),
        "parse: {:#?}",
        parsed.diagnostics
    );

    let mut b = Build::new(Some(root.to_path_buf()));
    let reach = b.check_entry(Entry {
        source: scarlet::source::SourceFile::new(ModuleKey::for_stdlib(&module), None, text),
        ast: &parsed.ast,
        doc: parsed.doc,
        parse: parsed.diagnostics,
        base_dir: Some(base_dir),
        role: Role::Library,
        unused: UnusedBindings::Report,
    });
    b.diagnostics(&reach)
        .iter()
        .map(|d| {
            format!(
                "{}:{}:{}: {}",
                d.source.as_ref().map_or("?", |s| s.key.as_str()),
                d.span.start_line,
                d.span.start_column,
                d.message
            )
        })
        .collect()
}

fn check_stdlib_file(rel: &str, module: Vec<String>) -> Vec<String> {
    let root = stdlib_root();
    let path = root.join(rel);
    let text = std::fs::read_to_string(&path).unwrap();
    check_as(&root, &text, path.parent().unwrap(), module)
}

#[test]
fn resource_as_module_is_clean() {
    let diags = check_stdlib_file(
        "scarlet/resource.sca",
        vec!["scarlet".into(), "resource".into()],
    );
    assert!(diags.is_empty(), "phantom diagnostics: {diags:#?}");
}

#[test]
fn decimal_as_module_is_clean() {
    let diags = check_stdlib_file(
        "scarlet/decimal.sca",
        vec!["scarlet".into(), "decimal".into()],
    );
    assert!(diags.is_empty(), "phantom diagnostics: {diags:#?}");
}

#[test]
fn http_body_as_module_is_clean() {
    let diags = check_stdlib_file(
        "scarlet/http/body.sca",
        vec!["scarlet".into(), "http".into(), "body".into()],
    );
    assert!(diags.is_empty(), "phantom diagnostics: {diags:#?}");
}

fn check_buffer_as(text: &str, module: Vec<String>) -> Vec<String> {
    let root = stdlib_root();
    check_as(&root, text, &root.join("scarlet"), module)
}

#[test]
fn synthetic_entry_importing_result_first() {
    let diags = check_buffer_as(
        "import scarlet/result\n\npub fn f(r Result(Int, Nil)) Int {\n\tresult::unwrap(r, 0)\n}\n",
        vec!["scarlet".into(), "zzz".into()],
    );
    assert!(diags.is_empty(), "phantom diagnostics: {diags:#?}");
}

#[test]
fn synthetic_entry_importing_option_first() {
    let diags = check_buffer_as(
        "import scarlet/option\n\npub fn f(o Option(Int)) Int {\n\toption::unwrap(o, 0)\n}\n",
        vec!["scarlet".into(), "zzz".into()],
    );
    assert!(diags.is_empty(), "phantom diagnostics: {diags:#?}");
}

#[test]
fn io_as_module_is_clean() {
    let diags = check_stdlib_file("scarlet/io.sca", vec!["scarlet".into(), "io".into()]);
    assert!(diags.is_empty(), "phantom diagnostics: {diags:#?}");
}

#[test]
fn net_as_module_is_clean() {
    let diags = check_stdlib_file("scarlet/net.sca", vec!["scarlet".into(), "net".into()]);
    assert!(diags.is_empty(), "phantom diagnostics: {diags:#?}");
}
