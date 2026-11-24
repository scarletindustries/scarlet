use super::line_ends;
use crate::scanner;

// The source with a `;` where each line end goes.
fn marked(src: &str) -> String {
    let (tokens, _) = scanner::new_scanner(src.to_string()).scan_all();
    let ends = line_ends(&tokens);
    let mut out = String::new();
    for (tok, end) in tokens.iter().zip(ends) {
        if tok.kind == crate::token::Kind::Eof {
            break;
        }
        if end {
            out.push(';');
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&tok.kind.to_string());
    }
    out
}

#[test]
fn a_break_between_statements_ends_the_line() {
    assert_eq!(marked("{\na\nb\n}"), "{ a; b }");
}

#[test]
fn breaks_in_brackets_are_spaces() {
    assert_eq!(
        marked("{\nf(\na,\nb\n)\n[\nx\n]\n}"),
        "{ f ( a , b ); [ x ] }"
    );
}

#[test]
fn an_operator_at_either_end_continues_the_line() {
    assert_eq!(
        marked("{\na +\nb\nc\n|> d\ne\n.f\n}"),
        "{ a + b; c |> d; e . f }"
    );
}

#[test]
fn a_block_inside_brackets_has_lines_again() {
    assert_eq!(marked("f(fn() {\na\nb\n})"), "f ( fn ( ) { a; b } )");
}

#[test]
fn minus_and_paren_start_a_new_line() {
    assert_eq!(marked("{\na\n-b\nc\n(d)\n}"), "{ a; - b; c; ( d ) }");
}

#[test]
fn else_continues_an_if() {
    assert_eq!(
        marked("{\nif a { b }\nelse { c }\n}"),
        "{ if a { b } else { c } }"
    );
}
