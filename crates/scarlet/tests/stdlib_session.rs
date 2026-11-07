//! From-source stdlib sessions (`IncrementalSession::new_from_source` +
//! `check_as`) must produce the same diagnostics as a batch `check` of the
//! same file: none, for a clean stdlib module.
//!
//! Regression: `truncate_to` clears `vars` and restarts var numbering, but
//! schemes survive it holding `QuantVar::origin_id` var ids. Without the
//! epoch guard in `instantiate`, a prelude ctor scheme's origin id aliased
//! the first post-rewind module's freshly-minted rigid vars, so `Ok`/`Err`
//! instantiated *rigid* inside any generic fn and every ctor pattern
//! mis-unified — phantom `Type mismatch` diagnostics at the imported
//! module's spans, published for the open file.

use scarlet_core::bytecode::IncrementalSession;
use scarlet_core::parser;
use scarlet_core::scanner;
use std::path::PathBuf;

fn check_stdlib_file(rel: &str, module: Vec<String>) -> Vec<String> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../scarlet_core/src/std");
    let root = root.canonicalize().unwrap();
    let path = root.join(rel);
    let text = std::fs::read_to_string(&path).unwrap();

    let mut sc = scanner::new_scanner(text);
    let p = parser::new_parser(&mut sc);
    let r = p.parse_program();
    assert!(r.diagnostics.is_empty(), "parse: {:#?}", r.diagnostics);

    let mut s = IncrementalSession::new_from_source(root);
    let expr = scarlet_core::ast::Expression::BlockExpression(r.ast);
    let result = s.check_as(&expr, path.parent(), module);
    result
        .diagnostics
        .iter()
        .map(|d| {
            format!(
                "{}:{}: {}",
                d.span.start_line, d.span.start_column, d.message
            )
        })
        .collect()
}

#[test]
fn resource_as_module_is_clean() {
    let diags = check_stdlib_file(
        "scarlet/resource.scrl",
        vec!["scarlet".into(), "resource".into()],
    );
    assert!(diags.is_empty(), "phantom diagnostics: {diags:#?}");
}

#[test]
fn decimal_as_module_is_clean() {
    let diags = check_stdlib_file(
        "scarlet/decimal.scrl",
        vec!["scarlet".into(), "decimal".into()],
    );
    assert!(diags.is_empty(), "phantom diagnostics: {diags:#?}");
}

#[test]
fn http_body_as_module_is_clean() {
    let diags = check_stdlib_file(
        "scarlet/http/body.scrl",
        vec!["scarlet".into(), "http".into(), "body".into()],
    );
    assert!(diags.is_empty(), "phantom diagnostics: {diags:#?}");
}

fn check_buffer_as(text: &str, module: Vec<String>) -> Vec<String> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../scarlet_core/src/std");
    let root = root.canonicalize().unwrap();

    let mut sc = scanner::new_scanner(text.to_string());
    let p = parser::new_parser(&mut sc);
    let r = p.parse_program();
    assert!(r.diagnostics.is_empty(), "parse: {:#?}", r.diagnostics);

    let mut s = IncrementalSession::new_from_source(root.clone());
    let expr = scarlet_core::ast::Expression::BlockExpression(r.ast);
    let result = s.check_as(&expr, Some(&root.join("scarlet")), module);
    result
        .diagnostics
        .iter()
        .map(|d| {
            format!(
                "{}:{}: {}",
                d.span.start_line, d.span.start_column, d.message
            )
        })
        .collect()
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
    let diags = check_stdlib_file("scarlet/io.scrl", vec!["scarlet".into(), "io".into()]);
    assert!(diags.is_empty(), "phantom diagnostics: {diags:#?}");
}

#[test]
fn net_as_module_is_clean() {
    let diags = check_stdlib_file("scarlet/net.scrl", vec!["scarlet".into(), "net".into()]);
    assert!(diags.is_empty(), "phantom diagnostics: {diags:#?}");
}
