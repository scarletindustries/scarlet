use super::Kind;

fn fixed_spelling_kinds() -> Vec<Kind> {
    Kind::fixed_spelling_kinds()
}

/// The exhaustive match forces a new `Kind` variant to be classified as
/// fixed-spelling or not before this compiles again.
#[test]
fn fixed_spelling_list_is_classified() {
    for kind in fixed_spelling_kinds() {
        match kind {
            // These have no fixed spelling: they carry text or mark a position.
            Kind::Eof
            | Kind::LineEnd
            | Kind::Error(_)
            | Kind::Identifier(_)
            | Kind::LiteralNumber(_)
            | Kind::LiteralString(_)
            | Kind::InterpStringStart
            | Kind::InterpStringPart(_)
            | Kind::InterpStringEnd => {
                unreachable!("{kind:?} must not be in the fixed-spelling list")
            }
            // These have a fixed spelling, so they must be in `fixed_spelling_kinds`.
            Kind::LogicalAnd
            | Kind::LogicalOr
            | Kind::BitwiseOr
            | Kind::PuncPipe
            | Kind::Keyword(_)
            | Kind::PuncArrow
            | Kind::PuncBackArrow
            | Kind::PuncComma
            | Kind::PuncColon
            | Kind::PuncColonColon
            | Kind::PuncSemicolon
            | Kind::PuncDot
            | Kind::PuncDotdot
            | Kind::PuncOpenParen
            | Kind::PuncCloseParen
            | Kind::PuncOpenBrace
            | Kind::PuncCloseBrace
            | Kind::PuncOpenBracket
            | Kind::PuncCloseBracket
            | Kind::BinOpen
            | Kind::BinClose
            | Kind::PuncQuestionMark
            | Kind::PuncExclamationMark
            | Kind::PuncAt
            | Kind::PuncEquals
            | Kind::PuncEqualsComparator
            | Kind::PuncNotEqual
            | Kind::PuncGt
            | Kind::PuncLt
            | Kind::PuncGte
            | Kind::PuncLte
            | Kind::PuncPlus
            | Kind::PuncPlusplus
            | Kind::PuncMinus
            | Kind::PuncMinusminus
            | Kind::PuncMul
            | Kind::PuncDiv
            | Kind::PuncMod => {}
        }
    }
}

/// `Display`'s spellings are maintained separately from the scanner's
/// dispatch, so scan each one back and require the same kind.
#[test]
fn fixed_spelling_roundtrip() {
    for kind in fixed_spelling_kinds() {
        let src = kind.to_string();
        let mut s = crate::scanner::new_scanner(src.clone());
        let (tokens, diagnostics) = s.scan_all();
        assert!(
            diagnostics.is_empty(),
            "scanning {src:?} diagnosed: {diagnostics:?}"
        );
        let scanned: Vec<Kind> = tokens.into_iter().map(|t| t.kind).collect();
        assert_eq!(scanned, [kind, Kind::Eof], "scanning {src:?}");
    }
}
