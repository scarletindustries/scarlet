use super::*;

const P: Palette = crate::term::Palette::ansi_for_test();

#[track_caller]
fn hl(src: &str, p: &Palette, cursor: Option<usize>) -> String {
    highlight_at(src, p, cursor).expect("the scanner's tokens map onto the source")
}

#[test]
fn overlapping_tokens_are_an_error() {
    let token = |column| Token {
        kind: Kind::Identifier("ab".into()),
        span: Span {
            start_line: 0,
            start_column: column,
            end_line: 0,
            end_column: column + 2,
        },
        leading_trivia: Vec::new(),
    };
    assert_eq!(
        paint("abc", &[token(0), token(1)], &P, None),
        Err(HighlightError::TokenOverlap {
            previous_end: 2,
            start: 1
        })
    );
}

/// Whatever the colouring, the visible text must survive byte for byte: a
/// line editor lays the line out by the width of what it printed.
fn strip(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find('\x1b') {
        out.push_str(&rest[..i]);
        let after = rest[i..].find('m').map_or(rest.len(), |m| i + m + 1);
        rest = &rest[after..];
    }
    out.push_str(rest);
    out
}

#[track_caller]
fn assert_transparent(src: &str) {
    assert_eq!(strip(&hl(src, &P, None)), src, "source text changed");
}

#[test]
fn coloring_preserves_the_source_text() {
    for src in [
        "fn add(a Int, b Int) Int { a + b }",
        "// just a comment",
        "const x = 1 // trailing\nconst y = 2",
        "/* block */ const z = 'hi'",
        "println('hello ${name}, you are ${age + 1}')",
        "const s = 'unterminated",
        "match x { Ok(v) -> v, Err(_) -> 0 }",
        "const emoji = 'héllo 🌶'",
        "",
        "   ",
        "§",
    ] {
        assert_transparent(src);
    }
}

#[test]
fn keywords_strings_and_numbers_get_distinct_colors() {
    let out = hl("const x = 'hi'", &P, None);
    assert!(
        out.contains("\x1b[35mconst"),
        "keyword not magenta: {out:?}"
    );
    assert!(out.contains("\x1b[32m'hi'"), "string not green: {out:?}");
    let out = hl("const x = 42", &P, None);
    assert!(out.contains("\x1b[33m42"), "number not yellow: {out:?}");
}

#[test]
fn a_comment_between_tokens_is_dimmed() {
    let out = hl("const x = 1 // note\n", &P, None);
    assert!(out.contains("\x1b[2m// note"), "comment not dim: {out:?}");
}

#[test]
fn a_type_name_and_a_callee_differ_from_a_binding() {
    let out = hl("println(Int)", &P, None);
    assert!(out.contains("\x1b[34mprintln"), "callee not blue: {out:?}");
    assert!(out.contains("\x1b[36mInt"), "type not cyan: {out:?}");
    assert!(!hl("x", &P, None).contains('\x1b'), "plain name coloured");
}

#[test]
fn the_bracket_pair_under_the_cursor_is_bolded() {
    let src = "f(a, b)";
    let out = hl(src, &P, Some(1));
    assert_eq!(out.matches("\x1b[1m").count(), 2, "want both ends: {out:?}");
    // Away from any bracket, nothing is bolded.
    let out = hl(src, &P, Some(4));
    assert_eq!(out.matches("\x1b[1m").count(), 0, "{out:?}");
}

#[test]
fn an_unmatched_bracket_has_no_partner_to_bold() {
    let out = hl("f(a", &P, Some(1));
    assert!(!out.contains("\x1b[1m"), "{out:?}");
}

#[test]
fn an_interpolation_hole_is_not_string_colored() {
    let out = hl("'a ${b} c'", &P, None);
    // The name inside the hole keeps its own (absent) colour, so the green
    // run must be broken by the reset that ends the `${`.
    assert!(strip(&out) == "'a ${b} c'");
    assert!(out.contains("\x1b[32m"), "string parts not green: {out:?}");
    assert!(
        !out.contains("\x1b[32mb"),
        "hole coloured as string: {out:?}"
    );
}

#[test]
fn a_qualified_member_being_typed_is_colored() {
    let out = hl("http.Del", &P, None);
    assert!(
        out.contains("\x1b[36mDel"),
        "type-cased member not cyan: {out:?}"
    );
}

#[test]
fn a_disabled_palette_adds_nothing() {
    let plain = crate::term::Palette::plain_for_test();
    assert_eq!(hl("const x = 'hi'", &plain, None), "const x = 'hi'");
}
