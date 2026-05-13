use super::Kind;

pub const KEYWORDS: &[(&str, Kind)] = &[
    ("fn", Kind::KwFunction),
    ("import", Kind::KwImport),
    ("from", Kind::KwFrom),
    ("true", Kind::KwTrue),
    ("false", Kind::KwFalse),
    ("enum", Kind::KwEnum),
    ("export", Kind::KwExport),
    ("struct", Kind::KwStruct),
    ("in", Kind::KwIn),
    ("match", Kind::KwMatch),
    ("none", Kind::KwNone),
    ("const", Kind::KwConst),
    ("if", Kind::KwIf),
    ("else", Kind::KwElse),
    ("error", Kind::KwError),
    ("or", Kind::KwOr),
];

fn keyword_lookup(s: &str) -> Option<Kind> {
    match s {
        "fn" => Some(Kind::KwFunction),
        "import" => Some(Kind::KwImport),
        "from" => Some(Kind::KwFrom),
        "true" => Some(Kind::KwTrue),
        "false" => Some(Kind::KwFalse),
        "enum" => Some(Kind::KwEnum),
        "export" => Some(Kind::KwExport),
        "struct" => Some(Kind::KwStruct),
        "in" => Some(Kind::KwIn),
        "match" => Some(Kind::KwMatch),
        "none" => Some(Kind::KwNone),
        "const" => Some(Kind::KwConst),
        "if" => Some(Kind::KwIf),
        "else" => Some(Kind::KwElse),
        "error" => Some(Kind::KwError),
        "or" => Some(Kind::KwOr),
        _ => None,
    }
}

#[inline]
pub fn is_keyword(identifier: &str) -> bool {
    keyword_lookup(identifier).is_some()
}

#[inline]
pub fn match_keyword(identifier: Option<&str>) -> Option<Kind> {
    identifier.and_then(keyword_lookup)
}
