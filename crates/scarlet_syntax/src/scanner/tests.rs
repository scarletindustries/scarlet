use super::*;
use crate::token::Keyword as Kw;
use Kind::*;

fn scan(src: &str) -> (Vec<Token>, Vec<Diagnostic<ScanError>>) {
    new_scanner(src).scan_all()
}

fn ident(s: &str) -> Kind {
    Identifier(s.into())
}

fn num(s: &str) -> Kind {
    LiteralNumber(s.into())
}

fn lit(s: &str) -> Kind {
    LiteralString(s.into())
}

fn part(s: &str) -> Kind {
    InterpStringPart(s.into())
}

#[track_caller]
fn assert_tok(toks: &[Token], i: usize, kind: Kind) {
    assert_eq!(toks[i].kind, kind, "token {i}");
}

fn kinds(input: &str) -> Vec<Kind> {
    scan(input).0.into_iter().map(|t| t.kind).collect()
}

/// Like `kinds`, but asserts the scan produced no diagnostics or Error tokens.
fn kinds_clean(input: &str) -> Vec<Kind> {
    let (toks, diags) = scan(input);
    assert!(diags.is_empty(), "scanner produced diagnostics: {diags:?}");
    let kinds: Vec<Kind> = toks.into_iter().map(|t| t.kind).collect();
    assert!(
        !kinds.iter().any(|k| matches!(k, Error(_))),
        "found error token"
    );
    kinds
}

// Inlined rather than read from examples/: these assert an exact token
// vector, which would otherwise freeze the example corpus.
const HELLO_SRC: &str = "// Hello world, plus string interpolation with ${}.\n\nprintln('hello, world')\n\nname = 'Scarlet'\nprintln('hello from ${name}')\nprintln('2 + 2 = ${2 + 2}')\n";

const FIZZBUZZ_SRC: &str = "fn fizzbuzz(n Int) String {\n\tmatch (n % 3, n % 5) {\n\t\t(0, 0) -> 'FizzBuzz'\n\t\t(0, _) -> 'Fizz'\n\t\t(_, 0) -> 'Buzz'\n\t\t_ -> '${n}'\n\t}\n}\n\nfn run(n Int, last Int) Nil {\n\tif n > last {\n\t\tNil\n\t} else {\n\t\tprintln(fizzbuzz(n))\n\t\trun(n + 1, last)\n\t}\n}\n\nrun(1, 20)\n";

#[test]
fn digit_separators_stay_in_the_token() {
    assert_eq!(
        kinds_clean("1_000_000 + 3.141_592"),
        vec![num("1_000_000"), PuncPlus, num("3.141_592"), Eof]
    );
    // A separator must sit between digits: anything else ends the
    // number, and what follows is scanned as whatever it is.
    assert_eq!(kinds("1_"), vec![num("1"), ident("_"), Eof]);
    assert_eq!(kinds("1__0"), vec![num("1"), ident("__0"), Eof]);
}

#[test]
fn scans_hello_world() {
    let kinds = kinds_clean(HELLO_SRC);

    #[rustfmt::skip]
        let expected = vec![
            // println('hello, world')
            ident("println"), PuncOpenParen, lit("hello, world"), PuncCloseParen,
            // name = 'Scarlet'
            ident("name"), PuncEquals, lit("Scarlet"),
            // println('hello from ${name}')
            ident("println"), PuncOpenParen,
            InterpStringStart, part("hello from "), PuncOpenBrace, ident("name"), PuncCloseBrace, InterpStringEnd,
            PuncCloseParen,
            // println('2 + 2 = ${2 + 2}')
            ident("println"), PuncOpenParen,
            InterpStringStart, part("2 + 2 = "), PuncOpenBrace, num("2"), PuncPlus, num("2"), PuncCloseBrace, InterpStringEnd,
            PuncCloseParen,
            Eof,
        ];

    assert_eq!(kinds, expected);
}

#[test]
fn scans_fizzbuzz() {
    let kinds = kinds_clean(FIZZBUZZ_SRC);

    #[rustfmt::skip]
        let expected = vec![
            // fn fizzbuzz(n Int) String {
            Keyword(Kw::Fn), ident("fizzbuzz"), PuncOpenParen, ident("n"), ident("Int"), PuncCloseParen, ident("String"), PuncOpenBrace,
            // match (n % 3, n % 5) {
            Keyword(Kw::Match), PuncOpenParen, ident("n"), PuncMod, num("3"), PuncComma, ident("n"), PuncMod, num("5"), PuncCloseParen, PuncOpenBrace,
            // (0, 0) -> 'FizzBuzz'
            PuncOpenParen, num("0"), PuncComma, num("0"), PuncCloseParen, PuncArrow, lit("FizzBuzz"),
            // (0, _) -> 'Fizz'
            PuncOpenParen, num("0"), PuncComma, ident("_"), PuncCloseParen, PuncArrow, lit("Fizz"),
            // (_, 0) -> 'Buzz'
            PuncOpenParen, ident("_"), PuncComma, num("0"), PuncCloseParen, PuncArrow, lit("Buzz"),
            // _ -> '${n}'
            ident("_"), PuncArrow, InterpStringStart, PuncOpenBrace, ident("n"), PuncCloseBrace, InterpStringEnd,
            // } }
            PuncCloseBrace, PuncCloseBrace,
            // fn run(n Int, last Int) Nil {
            Keyword(Kw::Fn), ident("run"), PuncOpenParen, ident("n"), ident("Int"), PuncComma, ident("last"), ident("Int"), PuncCloseParen, ident("Nil"), PuncOpenBrace,
            // if n > last {
            Keyword(Kw::If), ident("n"), PuncGt, ident("last"), PuncOpenBrace,
            // Nil
            ident("Nil"),
            // } else {
            PuncCloseBrace, Keyword(Kw::Else), PuncOpenBrace,
            // println(fizzbuzz(n))
            ident("println"), PuncOpenParen, ident("fizzbuzz"), PuncOpenParen, ident("n"), PuncCloseParen, PuncCloseParen,
            // run(n + 1, last)
            ident("run"), PuncOpenParen, ident("n"), PuncPlus, num("1"), PuncComma, ident("last"), PuncCloseParen,
            // } }
            PuncCloseBrace, PuncCloseBrace,
            // run(1, 20)
            ident("run"), PuncOpenParen, num("1"), PuncComma, num("20"), PuncCloseParen,
            Eof,
        ];

    assert_eq!(kinds, expected);
}

#[test]
fn test_interp_nesting() {
    // '${a}${b}' - two adjacent interpolations
    let ks = kinds("'${a}${b}'");
    assert_eq!(
        ks,
        vec![
            InterpStringStart,
            PuncOpenBrace,
            ident("a"),
            PuncCloseBrace,
            PuncOpenBrace,
            ident("b"),
            PuncCloseBrace,
            InterpStringEnd,
            Eof,
        ]
    );

    // nested braces inside ${...}
    let ks = kinds("'${ {a} }'");
    assert_eq!(
        ks,
        vec![
            InterpStringStart,
            PuncOpenBrace,
            PuncOpenBrace,
            ident("a"),
            PuncCloseBrace,
            PuncCloseBrace,
            InterpStringEnd,
            Eof,
        ]
    );

    // nested interpolated string inside ${...}
    let ks = kinds("'a${'b${c}d'}e'");
    assert_eq!(
        ks,
        vec![
            InterpStringStart,
            part("a"),
            PuncOpenBrace,
            InterpStringStart,
            part("b"),
            PuncOpenBrace,
            ident("c"),
            PuncCloseBrace,
            part("d"),
            InterpStringEnd,
            PuncCloseBrace,
            part("e"),
            InterpStringEnd,
            Eof,
        ]
    );
}

#[test]
fn test_punctuation() {
    let ks =
        kinds(", ( ) { } [ ] ; . .. -> + ++ - -- * / % ! != ? : :: = == > >= < <= << >> && || |");
    assert_eq!(
        ks,
        vec![
            PuncComma,
            PuncOpenParen,
            PuncCloseParen,
            PuncOpenBrace,
            PuncCloseBrace,
            PuncOpenBracket,
            PuncCloseBracket,
            PuncSemicolon,
            PuncDot,
            PuncDotdot,
            PuncArrow,
            PuncPlus,
            PuncPlusplus,
            PuncMinus,
            PuncMinusminus,
            PuncMul,
            PuncDiv,
            PuncMod,
            PuncExclamationMark,
            PuncNotEqual,
            PuncQuestionMark,
            PuncColon,
            PuncColonColon,
            PuncEquals,
            PuncEqualsComparator,
            PuncGt,
            PuncGte,
            PuncLt,
            PuncLte,
            BinOpen,
            BinClose,
            LogicalAnd,
            LogicalOr,
            BitwiseOr,
            Eof,
        ]
    );
}

#[test]
fn test_gt_lt_do_not_overconsume() {
    // Regression: `>` and `<` used to always consume the next char.
    let ks = kinds("a>b");
    assert_eq!(ks, vec![ident("a"), PuncGt, ident("b"), Eof]);
    let ks = kinds("a<b");
    assert_eq!(ks, vec![ident("a"), PuncLt, ident("b"), Eof]);
    let ks = kinds("a>=b");
    assert_eq!(ks, vec![ident("a"), PuncGte, ident("b"), Eof]);
}

#[test]
fn test_bin_open_close() {
    let ks = kinds("<<1, 2>>");
    assert_eq!(
        ks,
        vec![BinOpen, num("1"), PuncComma, num("2"), BinClose, Eof]
    );
    let ks = kinds("<<>>");
    assert_eq!(ks, vec![BinOpen, BinClose, Eof]);
    // Single < / > between << >> must remain comparison ops.
    let ks = kinds("<< a < b >>");
    assert_eq!(
        ks,
        vec![BinOpen, ident("a"), PuncLt, ident("b"), BinClose, Eof]
    );
    // Maximal munch: <<= is << then =, not < then <=.
    let ks = kinds("<<= >>=");
    assert_eq!(ks, vec![BinOpen, PuncEquals, BinClose, PuncEquals, Eof]);
}

#[test]
fn test_numbers() {
    let (toks, _) = scan("123 4.56 1..5");
    assert_tok(&toks, 0, num("123"));
    assert_tok(&toks, 1, num("4.56"));
    assert_tok(&toks, 2, num("1"));
    assert_tok(&toks, 3, PuncDotdot);
    assert_tok(&toks, 4, num("5"));
}

#[test]
fn hex_and_binary_literals() {
    assert_eq!(
        kinds_clean("0xFF 0xff 0X10"),
        vec![num("0xFF"), num("0xff"), num("0X10"), Eof]
    );
    assert_eq!(
        kinds_clean("0b1010 0B11"),
        vec![num("0b1010"), num("0B11"), Eof]
    );
    assert_eq!(
        kinds_clean("0xDE_AD_BE_EF"),
        vec![num("0xDE_AD_BE_EF"), Eof]
    );
    assert_eq!(kinds_clean("0b1111_0000"), vec![num("0b1111_0000"), Eof]);
    assert_eq!(
        kinds_clean("0x10..0x20"),
        vec![num("0x10"), PuncDotdot, num("0x20"), Eof]
    );
    // `08` is still decimal, because there's no octal prefix.
    assert_eq!(kinds_clean("08"), vec![num("08"), Eof]);
    // A `.` after hex/bin digits ends the number; hex floats are not a thing.
    assert_eq!(
        kinds_clean("0xFF.5"),
        vec![num("0xFF"), PuncDot, num("5"), Eof]
    );

    let (toks, diags) = scan("0x");
    assert!(matches!(toks[0].kind, Error(_)));
    assert_eq!(diags.len(), 1);
    assert!(diags[0].message().contains("hex literal has no digits"));

    let (toks, diags) = scan("0b2");
    assert!(matches!(toks[0].kind, Error(_)));
    assert_eq!(diags.len(), 1);
    assert!(diags[0].message().contains("binary literal has no digits"));

    let (toks, diags) = scan("0xFFG");
    assert!(matches!(toks[0].kind, Error(_)));
    assert_eq!(diags.len(), 1);
    assert!(diags[0].message().contains("invalid digit in hex literal"));
}

#[test]
fn test_string_literals() {
    let (toks, _) = scan("'hello' 'a\\nb'");
    assert_tok(&toks, 0, lit("hello"));
    assert_tok(&toks, 1, lit("a\nb"));
}

#[test]
fn test_keywords_vs_identifiers() {
    let ks = kinds("fn if else type foo _bar");
    assert_eq!(
        ks,
        vec![
            Keyword(Kw::Fn),
            Keyword(Kw::If),
            Keyword(Kw::Else),
            Keyword(Kw::Type),
            ident("foo"),
            ident("_bar"),
            Eof
        ]
    );
}

#[test]
fn test_trivia_attached_to_next_token() {
    let (toks, _) = scan("  // hi\nfoo");
    assert_eq!(toks[0].kind, ident("foo"));
    let trivia = &toks[0].leading_trivia;
    assert_eq!(trivia.len(), 2);
    assert_eq!(trivia[0], Trivia::LineComment("// hi".to_string()));
    assert_eq!(trivia[1], Trivia::Newline);
}

#[test]
fn unterminated_doc_comment_is_not_a_doc_comment() {
    let (toks, diags) = scan("/** Module prose\npub fn f() Int { 1 }\n");
    assert_eq!(toks[0].kind, Eof, "the whole file is swallowed as trivia");
    let trivia = &toks[0].leading_trivia;
    assert!(
        !trivia.iter().any(|t| matches!(t, Trivia::DocComment(_))),
        "an unterminated `/**` must not donate a doc comment: {trivia:?}"
    );
    assert!(matches!(trivia[0], Trivia::BlockComment(_)));
    assert_eq!(diags.len(), 1);
}

#[test]
fn unterminated_block_comment_reports_at_the_opening_delimiter() {
    let (_, diags) = scan("x = 1\n  /** oops\nmore text\n");
    assert_eq!(diags.len(), 1);
    assert_eq!(
        diags[0].message(),
        "This block comment is never closed. End it with `*/`"
    );
    // Line 1, column 2: the `/` of the `/**`, not EOF on line 3.
    assert_eq!(
        (diags[0].span.start_line, diags[0].span.start_column),
        (1, 2)
    );
}

#[test]
fn test_single_ampersand_is_error() {
    let (toks, diags) = scan("&");
    assert_eq!(toks[0].kind, Error("&".into()));
    assert_eq!(diags.len(), 1);
}

#[test]
fn test_double_quotes_accepted() {
    let (toks, diags) = scan("\"hello\"");
    assert_tok(&toks, 0, lit("hello"));
    assert!(diags.is_empty());
}

#[test]
fn test_double_quote_with_single_inside() {
    let (toks, _) = scan("\"it's fine\"");
    assert_tok(&toks, 0, lit("it's fine"));
}

#[test]
fn test_double_quote_interpolation() {
    let (toks, diags) = scan("\"hi ${x}\"");
    assert_eq!(toks[0].kind, InterpStringStart);
    assert!(diags.is_empty());
}

#[test]
fn test_mixed_quote_nesting() {
    let ks = kinds_clean("\"outer ${'inner'}\"");
    assert!(ks.contains(&InterpStringStart));
    assert!(ks.contains(&lit("inner")));
    assert!(ks.contains(&InterpStringEnd));
}

#[test]
fn test_lone_backtick_no_crash() {
    let (toks, diags) = scan("`");
    let ks: Vec<Kind> = toks.into_iter().map(|t| t.kind).collect();
    // Completing scan_all without a hang is the proof.
    assert_eq!(*ks.last().unwrap(), Eof);
    assert!(ks.iter().any(|k| matches!(k, Error(_))));
    assert!(!diags.is_empty());
}

#[test]
fn test_trailing_backslash_unterminated_string_no_crash() {
    let (toks, diags) = scan("x := \"\\");
    let ks: Vec<Kind> = toks.into_iter().map(|t| t.kind).collect();
    assert_eq!(*ks.last().unwrap(), Eof);
    assert!(ks.iter().any(|k| matches!(k, Error(_))));
    assert!(
        diags
            .iter()
            .any(|d| d.message().contains("This string is never closed"))
    );
}

#[test]
fn test_backtick_then_eof_terminates() {
    // A bare backtick at true EOF exercises the incr_pos-at-EOF path.
    assert_eq!(*kinds("a`").last().unwrap(), Eof);
}

#[test]
fn test_float_then_range() {
    // Regression: `1.5..10` used to lex as [1][.][5][..][10].
    let (toks, _) = scan("1.5..10");
    assert_tok(&toks, 0, num("1.5"));
    assert_tok(&toks, 1, PuncDotdot);
    assert_tok(&toks, 2, num("10"));
    assert_tok(&toks, 3, Eof);
}

#[test]
fn test_float_then_method() {
    let (toks, _) = scan("1.5.foo");
    assert_tok(&toks, 0, num("1.5"));
    assert_tok(&toks, 1, PuncDot);
    assert_tok(&toks, 2, ident("foo"));
    assert_tok(&toks, 3, Eof);
}

#[test]
fn test_bare_dollar_is_not_interpolation() {
    // A `$` not followed by `{` or a name start is a literal char.
    let (toks, _) = scan("'a$5'");
    assert_tok(&toks, 0, lit("a$5"));
    assert_tok(&toks, 1, Eof);

    // Bare `$` at end of string.
    assert_eq!(kinds("'a$'"), vec![lit("a$"), Eof]);

    // But `${` and `$ident` still trigger interpolation.
    assert_eq!(kinds("'a${x}'")[0], InterpStringStart);
    assert_eq!(kinds("'a$x'")[0], InterpStringStart);
}

#[test]
fn test_backslash_at_newline_is_unterminated_not_unknown_escape() {
    // The `\` must not consume the newline, or the next line is lost.
    let (toks, diags) = scan("x = 'abc\\\ny = 1");
    assert!(
        diags
            .iter()
            .any(|d| d.message().contains("This string is never closed")),
        "diagnostics: {diags:?}"
    );
    assert!(
        !diags.iter().any(|d| d.message().contains("Unknown escape")),
        "the newline must not be treated as an escape body: {diags:?}"
    );
    let ks: Vec<Kind> = toks.into_iter().map(|t| t.kind).collect();
    // `y = 1` after the broken string survives as ordinary tokens.
    assert!(ks.ends_with(&[ident("y"), PuncEquals, num("1"), Eof]));
}

#[test]
fn test_backslash_at_eof_single_unterminated_error() {
    let (toks, diags) = scan("x = '\\");
    assert_eq!(*toks.last().map(|t| t.kind.clone()).as_ref().unwrap(), Eof);
    assert_eq!(diags.len(), 1, "diagnostics: {diags:?}");
    assert!(diags[0].message().contains("This string is never closed"));
}

#[test]
fn test_crlf_sources_scan_clean() {
    let ks = kinds_clean("a = 1\r\nb = 2\r\n");
    assert_eq!(
        ks,
        vec![
            ident("a"),
            PuncEquals,
            num("1"),
            ident("b"),
            PuncEquals,
            num("2"),
            Eof
        ]
    );

    // \r\n collapses to a single Newline trivia on the next token.
    let (toks, _) = scan("a\r\nb");
    assert_eq!(toks[1].leading_trivia, vec![Trivia::Newline]);

    // A lone \r is plain whitespace, not a newline and not an error.
    let (toks, diags) = scan("a \r b");
    assert!(diags.is_empty(), "diagnostics: {diags:?}");
    assert!(toks[1].leading_trivia.is_empty());

    // Line comments end before the \r so their text stays clean.
    let (toks, _) = scan("// hi\r\nfoo");
    assert_eq!(
        toks[0].leading_trivia[0],
        Trivia::LineComment("// hi".to_string())
    );

    // CRLF line accounting matches LF: `b` starts on line 1.
    let (toks, _) = scan("a\r\nb");
    assert_eq!(toks[1].span.start_line, 1);
}

#[test]
fn test_string_cannot_swallow_half_a_crlf() {
    // An unterminated string ends at the `\r`, and the `\r\n` still scans
    // as one newline rather than leaving a stray token.
    let (toks, diags) = scan("x = 'abc\r\ny = 1");
    assert!(
        diags
            .iter()
            .any(|d| d.message().contains("This string is never closed")),
        "diagnostics: {diags:?}"
    );
    let ks: Vec<Kind> = toks.into_iter().map(|t| t.kind).collect();
    assert!(!ks.contains(&PuncDiv), "no stray tokens from the CRLF");
    assert!(ks.ends_with(&[ident("y"), PuncEquals, num("1"), Eof]));
}

#[test]
fn test_unterminated_plain_string_reports_at_opening_quote() {
    // Anchored at the opening quote (line 0, column 4), not at EOF.
    let (_, diags) = scan("x = 'abc");
    assert_eq!(diags.len(), 1, "diagnostics: {diags:?}");
    assert!(diags[0].message().contains("This string is never closed"));
    assert_eq!(
        (diags[0].span.start_line, diags[0].span.start_column),
        (0, 4)
    );
}

#[test]
fn test_unterminated_interp_string_content_reports_at_opening_quote() {
    // The `${b}` closes, then the trailing content runs into EOF. The
    // error still anchors at the opening quote, column 4.
    let (_, diags) = scan("x = 'a${b} cd");
    let unterminated: Vec<_> = diags
        .iter()
        .filter(|d| d.message().contains("This string is never closed"))
        .collect();
    assert_eq!(unterminated.len(), 1, "diagnostics: {diags:?}");
    assert_eq!(
        (
            unterminated[0].span.start_line,
            unterminated[0].span.start_column
        ),
        (0, 4)
    );
}

#[test]
fn test_unterminated_interpolation_reports_at_opening_quote() {
    // Regression: EOF inside `${...}` used to abandon the frame silently,
    // with no diagnostic at all.
    let (toks, diags) = scan("x = '${abc");
    assert_eq!(*toks.last().map(|t| t.kind.clone()).as_ref().unwrap(), Eof);
    let unterminated: Vec<_> = diags
        .iter()
        .filter(|d| d.message().contains("This string is never closed"))
        .collect();
    assert_eq!(unterminated.len(), 1, "diagnostics: {diags:?}");
    // Anchored at the opening quote (line 0, column 4), not at EOF.
    assert_eq!(
        (
            unterminated[0].span.start_line,
            unterminated[0].span.start_column
        ),
        (0, 4)
    );
}

#[test]
fn test_nested_unterminated_interpolation_one_error_per_string() {
    // Two abandoned frames -> two errors, each at its own opening quote.
    let (_, diags) = scan("'${\"${a");
    let cols: Vec<i32> = diags
        .iter()
        .filter(|d| d.message().contains("This string is never closed"))
        .map(|d| d.span.start_column)
        .collect();
    assert_eq!(cols.len(), 2, "diagnostics: {diags:?}");
    assert!(cols.contains(&0) && cols.contains(&3), "cols: {cols:?}");
}

#[test]
fn test_back_arrow_wins_longest_munch() {
    // `a<-b` is ident, `<-`, ident, not `a < -b`. A documented
    // behaviour change from before backpassing existed.
    assert_eq!(
        kinds_clean("a<-b"),
        vec![ident("a"), PuncBackArrow, ident("b"), Eof]
    );
    // `<=` and `<<` still win their own longest munches.
    assert_eq!(
        kinds_clean("a <= b"),
        vec![ident("a"), PuncLte, ident("b"), Eof]
    );
    assert_eq!(
        kinds_clean("a < b"),
        vec![ident("a"), PuncLt, ident("b"), Eof]
    );
}

#[test]
fn test_unknown_escape_sequence_diagnostic() {
    // An unknown escape is reported, then recovered from.
    let (toks, diags) = scan("x = \"a\\qb\"");
    let ks: Vec<Kind> = toks.into_iter().map(|t| t.kind).collect();
    assert_eq!(*ks.last().unwrap(), Eof);
    assert!(ks.contains(&lit("aqb")));
    assert!(
        diags
            .iter()
            .any(|d| d.message().contains("Unknown escape sequence '\\q'")),
        "diagnostics: {diags:?}"
    );
}

#[test]
fn test_unicode_escape_spells_a_code_point() {
    // A single scalar value, and one past the BMP (4-byte UTF-8). Both
    // scan cleanly, with no diagnostics.
    assert_eq!(kinds_clean("'\\u{E9}'"), vec![lit("\u{E9}"), Eof]);
    assert_eq!(kinds_clean("'\\u{1F469}'"), vec![lit("\u{1F469}"), Eof]);

    // Lowercase hex, and content on both sides survive untouched.
    let ks = kinds_clean("'a\\u{1f469}b'");
    assert!(ks.contains(&lit("a\u{1F469}b")), "tokens: {ks:?}");
}

#[test]
fn test_unicode_escape_in_interpolated_string() {
    // The interp-string scanner is a second call site with its own copy
    // of the escape loop; cover it separately.
    let ks = kinds_clean("'\\u{E9} $x'");
    assert!(ks.contains(&part("\u{E9} ")), "tokens: {ks:?}");
}

#[test]
fn test_unicode_escape_malformed_recovers_with_diagnostic() {
    // Missing '{': the 'u' is echoed back, matching the unknown-escape
    // recovery style, and nothing after it is consumed.
    let (toks, diags) = scan("'\\u41}'");
    let ks: Vec<Kind> = toks.into_iter().map(|t| t.kind).collect();
    assert!(ks.contains(&lit("u41}")), "tokens: {ks:?}");
    assert!(
        diags.iter().any(|d| d.message().contains("Expected '{'")),
        "diagnostics: {diags:?}"
    );

    // No closing '}'.
    let (_, diags) = scan("'\\u{41'");
    assert!(
        diags
            .iter()
            .any(|d| d.message().contains("is never closed")),
        "diagnostics: {diags:?}"
    );

    // No digits at all.
    let (toks, diags) = scan("'\\u{}'");
    let ks: Vec<Kind> = toks.into_iter().map(|t| t.kind).collect();
    assert!(ks.contains(&lit("\u{FFFD}")), "tokens: {ks:?}");
    assert!(
        diags
            .iter()
            .any(|d| d.message().contains("The '\\u{}' escape is empty")),
        "diagnostics: {diags:?}"
    );

    // A UTF-16 surrogate half: not a Unicode scalar value on its own.
    let (toks, diags) = scan("'\\u{D800}'");
    let ks: Vec<Kind> = toks.into_iter().map(|t| t.kind).collect();
    assert!(ks.contains(&lit("\u{FFFD}")), "tokens: {ks:?}");
    assert!(
        diags
            .iter()
            .any(|d| d.message().contains("surrogate range")),
        "diagnostics: {diags:?}"
    );

    // Past U+10FFFF.
    let (toks, diags) = scan("'\\u{110000}'");
    let ks: Vec<Kind> = toks.into_iter().map(|t| t.kind).collect();
    assert!(ks.contains(&lit("\u{FFFD}")), "tokens: {ks:?}");
    assert!(
        diags.iter().any(|d| d.message().contains("U+10FFFF")),
        "diagnostics: {diags:?}"
    );
}
