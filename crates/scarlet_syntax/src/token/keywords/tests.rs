use super::Keyword;

#[test]
fn keyword_roundtrip() {
    for kw in Keyword::ALL {
        assert_eq!(Keyword::parse(kw.text()), Some(kw), "{kw:?}");
    }
}
