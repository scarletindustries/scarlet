use super::*;
use crate::{parser, scanner};

fn observed(src: &str) -> Accepted {
    let mut scanner = scanner::new_scanner(src.to_string());
    let program = parser::new_parser(&mut scanner).parse_program();
    let mut names = Accepted::default();
    names.accept(AcceptedEntry::new(src, &program.ast));
    names
}

fn labels(candidates: &[Candidate]) -> Vec<String> {
    candidates.iter().map(|c| c.name.clone()).collect()
}

#[test]
fn the_prelude_is_completable_with_no_session_at_all() {
    let mut names = Accepted::default();
    assert!(labels(&names.bare("printl")).contains(&"println".to_string()));
    assert!(
        labels(&names.bare("f")).contains(&"fn".to_string()),
        "keyword"
    );
}

#[test]
fn a_definition_becomes_completable() {
    let mut names = observed("fn triple(n Int) Int { n * 3 }\nconst k = 1\n");
    assert_eq!(labels(&names.bare("tri")), vec!["triple"]);
    assert_eq!(labels(&names.bare("k")), vec!["k"]);
}

#[test]
fn a_type_declaration_offers_its_constructors() {
    let mut names = observed("type Shape {\n\tCircle(r Int)\n\tSquare(s Int)\n}\n");
    assert_eq!(labels(&names.bare("Circ")), vec!["Circle"]);
    assert_eq!(labels(&names.bare("Shape")), vec!["Shape"]);
}

#[test]
fn an_import_completes_the_modules_exports_behind_its_alias() {
    let mut names = observed("import scarlet/string\n");
    let items = labels(&names.qualified("string", ""));
    assert!(!items.is_empty(), "no exports for scarlet/string");
    assert!(names.qualified("nope", "").is_empty(), "unknown qualifier");
}

/// `http::Post` is a constructor of an exported type, not a value export, so the name list has
/// to reach into the type to find it.
#[test]
fn a_constructor_completes_behind_its_modules_alias() {
    let mut names = observed("import scarlet/http\n");
    assert_eq!(labels(&names.qualified("http", "Po")), vec!["Post"]);
    assert_eq!(labels(&names.qualified("http", "Del")), vec!["Delete"]);
    assert!(labels(&names.qualified("http", "Met")).contains(&"Method".to_string()));
}

#[test]
fn an_aliased_import_completes_under_the_alias_only() {
    let mut names = observed("import scarlet/string as str\n");
    assert!(!names.qualified("str", "").is_empty());
    assert!(names.qualified("string", "").is_empty());
    assert!(labels(&names.bare("st")).contains(&"str".to_string()));
}

#[test]
fn a_function_candidate_shows_its_parameters() {
    let println = names_candidate("println");
    assert_eq!(println.display(), "println(x)");
}

fn names_candidate(name: &str) -> Candidate {
    let mut names = Accepted::default();
    names
        .bare(name)
        .into_iter()
        .find(|c| c.name == name)
        .unwrap_or_else(|| panic!("{name} is not a candidate"))
}
