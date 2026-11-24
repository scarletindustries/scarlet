use super::*;
use crate::repl::names::AcceptedEntry;

fn observed(src: &str) -> Accepted {
    let mut scanner = crate::scanner::new_scanner(src.to_string());
    let program = crate::parser::new_parser(&mut scanner).parse_program();
    let mut names = Accepted::default();
    names.accept(AcceptedEntry::new(src, &program.ast));
    names
}

fn candidates(line: &str, names: &mut Accepted) -> (usize, Vec<String>) {
    let (start, pairs) = complete_source(line, line.len(), names);
    (start, pairs.into_iter().map(|p| p.replacement).collect())
}

fn helper(names: Accepted) -> ScarletHelper {
    ScarletHelper::new(
        crate::term::Palette::for_stdout(),
        Rc::new(RefCell::new(names)),
    )
}

fn hint(line: &str, names: Accepted) -> Option<String> {
    helper(names).completion_hint(line, line.len())
}

#[test]
fn the_ghost_text_is_what_tab_would_insert() {
    assert_eq!(hint("printl", Accepted::default()).as_deref(), Some("n"));
    assert_eq!(
        hint("http::Del", observed("import scarlet/http")).as_deref(),
        Some("ete")
    );
}

/// Several candidates agree only as far as their shared prefix.
#[test]
fn the_ghost_text_stops_where_candidates_diverge() {
    let names = observed("fn parse_a() Int { 1 }\nfn parse_b() Int { 2 }\n");
    assert_eq!(hint("pars", names).as_deref(), Some("e_"));
}

#[test]
fn nothing_is_suggested_for_an_empty_word() {
    assert_eq!(hint("", Accepted::default()), None);
    assert_eq!(hint("1 + ", Accepted::default()), None);
}

#[test]
fn a_command_suggests_the_rest_of_its_name() {
    assert_eq!(hint(":he", Accepted::default()).as_deref(), Some("lp "));
    assert_eq!(hint("/he", Accepted::default()).as_deref(), Some("lp "));
}

/// This tests the whole editor path, not just the helper's inner function, because a candidate
/// that never reaches `Completer::complete` never reaches Tab.
#[test]
fn the_editor_completes_a_constructor_behind_its_alias() {
    let names = Rc::new(RefCell::new(observed("import scarlet/http")));
    let helper = ScarletHelper::new(crate::term::Palette::for_stdout(), names);
    let history = rustyline::history::MemHistory::new();
    let ctx = Context::new(&history);
    let line = "http::Del";
    let (start, items) = helper.complete(line, line.len(), &ctx).expect("complete");
    assert_eq!(start, "http::".len());
    assert_eq!(
        items.into_iter().map(|p| p.replacement).collect::<Vec<_>>(),
        vec!["Delete".to_string()]
    );
}

#[test]
fn a_bare_prefix_completes_prelude_names() {
    let (start, items) = candidates("printl", &mut Accepted::default());
    assert_eq!(start, 0);
    assert!(items.contains(&"println".to_string()), "{items:?}");
}

#[test]
fn completion_replaces_only_the_member_after_a_dot() {
    let mut names = observed("import scarlet/string");
    let line = "const x = string.";
    let (start, items) = candidates(line, &mut names);
    assert_eq!(start, line.len());
    assert!(!items.is_empty(), "no exports offered");
}

#[test]
fn a_meta_command_completes_its_own_name() {
    let (start, items) = complete_command((":", "he"));
    assert_eq!(start, 0);
    assert_eq!(
        items.into_iter().map(|p| p.replacement).collect::<Vec<_>>(),
        vec![":help ".to_string()]
    );
}

#[test]
fn an_import_line_completes_module_paths() {
    let (start, items) = candidates("import scarlet/str", &mut Accepted::default());
    assert_eq!(start, "import ".len());
    assert!(items.iter().any(|i| i == "scarlet/string"), "{items:?}");
}

#[test]
fn a_number_is_not_a_name_to_complete() {
    let (start, items) = candidates("1 + 2", &mut Accepted::default());
    assert_eq!(start, "1 + 2".len());
    assert!(items.is_empty(), "{items:?}");
}
