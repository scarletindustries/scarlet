use super::Kind;

fn keyword_lookup(s: &str) -> Option<Kind> {
    match s {
        "fn" => Some(Kind::KwFunction),
        "import" => Some(Kind::KwImport),
        "type" => Some(Kind::KwType),
        "in" => Some(Kind::KwIn),
        "match" => Some(Kind::KwMatch),
        "const" => Some(Kind::KwConst),
        "if" => Some(Kind::KwIf),
        "else" => Some(Kind::KwElse),
        "or" => Some(Kind::KwOr),
        "pub" => Some(Kind::KwPub),
        "opaque" => Some(Kind::KwOpaque),
        "as" => Some(Kind::KwAs),
        _ => None,
    }
}

#[inline]
pub fn match_keyword(identifier: Option<&str>) -> Option<Kind> {
    identifier.and_then(keyword_lookup)
}
