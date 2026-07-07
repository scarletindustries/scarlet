use super::Kind;

/// The single source of truth for al's keyword set. Adding a keyword means
/// adding one row here (plus the `Kind::Kw*` variant): `match_keyword`
/// (scanner), `keyword_text` (display), and every "is this a keyword" check
/// derive from this table, so the three cannot drift.
pub const KEYWORDS: &[(Kind, &str)] = &[
    (Kind::KwFunction, "fn"),
    (Kind::KwImport, "import"),
    (Kind::KwType, "type"),
    (Kind::KwIn, "in"),
    (Kind::KwMatch, "match"),
    (Kind::KwConst, "const"),
    (Kind::KwIf, "if"),
    (Kind::KwElse, "else"),
    (Kind::KwOr, "or"),
    (Kind::KwPub, "pub"),
    (Kind::KwOpaque, "opaque"),
    (Kind::KwAs, "as"),
];

#[inline]
pub fn match_keyword(s: &str) -> Option<Kind> {
    KEYWORDS.iter().find(|(_, w)| *w == s).map(|(k, _)| *k)
}

#[inline]
pub fn keyword_text(k: Kind) -> Option<&'static str> {
    KEYWORDS.iter().find(|(kk, _)| *kk == k).map(|(_, w)| *w)
}
