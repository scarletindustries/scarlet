use scarlet_syntax::scanner::new_scanner;
use scarlet_syntax::token::Kind;

use super::outputs;

/// Number literals are the one part of the lexical layer that's a hand-copied regex
/// instead of coming from a table. A change to `Scanner::scan_number` regenerates
/// byte-identical files, so `generated_files_are_up_to_date` can't see the regexes drift.
/// This test pins the shape they encode instead. When it fails, the scanner has moved, and
/// `lexical_js`'s `number` and `tm_language`'s `numbers` patterns must move with it.
///
/// We added it after `_` digit separators reached the scanner but neither regex, and the
/// tree-sitter corpus job went red on two examples.
#[test]
fn number_regexes_still_describe_the_scanner() {
    // Each case is a source and the LiteralNumber texts it must yield, in order.
    let cases: &[(&str, &[&str])] = &[
        ("1", &["1"]),
        ("1_000_000", &["1_000_000"]),
        ("1.5", &["1.5"]),
        ("1_0.0_1", &["1_0.0_1"]),
        // A `_` or a `.` with no digit after it ends the token, which is
        // why neither regex folds `_` into the digit class.
        ("1_", &["1"]),
        ("1.", &["1"]),
        // The scanner mustn't fuse a range like `0..10` into one token.
        ("0..10", &["0", "10"]),
        ("0xFF", &["0xFF"]),
        ("0Xff", &["0Xff"]),
        ("0b1010", &["0b1010"]),
        ("0B11", &["0B11"]),
        ("0xDE_AD", &["0xDE_AD"]),
        ("0b1111_0000", &["0b1111_0000"]),
        ("0x10..0x20", &["0x10", "0x20"]),
        // A prefix with no digits is an Error token, not a LiteralNumber.
        ("0x", &[]),
        ("0b", &[]),
    ];
    for (source, want) in cases {
        let (tokens, _) = new_scanner(*source).scan_all();
        let got: Vec<String> = tokens
            .iter()
            .filter_map(|t| match &t.kind {
                Kind::LiteralNumber(text) => Some(text.to_string()),
                _ => None,
            })
            .collect();
        assert_eq!(
            got, *want,
            "Scanner::scan_number changed on {source:?}. Update the `number` \
                 regex in lexical_js and the `numbers` patterns in tm_language, \
                 then regenerate with `cargo xtask gen-editor-syntax`"
        );
    }
}

/// Regenerating must be a no-op against the checked-in files, so CI fails
/// when the scanner's token tables change without a regeneration.
#[test]
fn generated_files_are_up_to_date() {
    for (path, contents) in outputs() {
        let on_disk = std::fs::read_to_string(&path).unwrap_or_default();
        assert_eq!(
            on_disk,
            contents,
            "{} is stale. Run `cargo xtask gen-editor-syntax`",
            path.display()
        );
    }
}
