use super::{clean_doc_comment, qualifier_before};

#[test]
fn qualifier_found_right_after_the_colons() {
    assert_eq!(qualifier_before("x = http::", 10), Some("http".to_string()));
}

#[test]
fn qualifier_found_mid_partial_word() {
    assert_eq!(
        qualifier_before("x = http::ge", 12),
        Some("http".to_string())
    );
}

#[test]
fn no_qualifier_on_a_bare_identifier() {
    assert_eq!(qualifier_before("x = gre", 7), None);
}

#[test]
fn a_field_access_is_not_a_qualifier() {
    assert_eq!(qualifier_before("x = rec.", 8), None);
}

#[test]
fn a_type_annotation_colon_is_not_a_qualifier() {
    assert_eq!(qualifier_before("fn f(a: ", 8), None);
}

#[test]
fn lone_leading_colons_have_no_qualifier() {
    assert_eq!(qualifier_before("::", 2), None);
}

#[test]
fn column_past_end_of_line_clamps() {
    assert_eq!(qualifier_before("a::", 99), Some("a".to_string()));
}

#[test]
fn soft_wrapped_lines_stay_one_paragraph() {
    let got = clean_doc_comment("/**\n * One line\n * and its continuation.\n */");
    assert_eq!(got, "One line\nand its continuation.");
}

#[test]
fn a_blank_line_separates_paragraphs() {
    let got = clean_doc_comment("/**\n * Title.\n *\n * Body.\n */");
    assert_eq!(got, "Title.\n\nBody.");
}

#[test]
fn indented_code_block_survives() {
    let got = clean_doc_comment("/**\n * Example:\n *\n *     new(15, 1)\n */");
    assert_eq!(got, "Example:\n\n    new(15, 1)");
}

#[test]
fn single_line_doc_is_unchanged() {
    assert_eq!(
        clean_doc_comment("/** Sum of `a` and `b`. */"),
        "Sum of `a` and `b`."
    );
}

#[test]
fn double_star_closer_leaves_no_stray_gutter() {
    assert_eq!(clean_doc_comment("/** Doc. **/"), "Doc.");
    assert_eq!(clean_doc_comment("/**\n * Doc.\n **/"), "Doc.");
}

#[test]
fn trailing_markdown_rule_survives() {
    assert_eq!(clean_doc_comment("/**\n * a\n * ***\n*/"), "a\n***");
}
