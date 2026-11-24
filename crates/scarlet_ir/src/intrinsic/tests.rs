use super::{Intrinsic, PAIRS};

#[test]
fn every_key_names_its_own_variant() {
    for &(intrinsic, key) in PAIRS {
        assert_eq!(Intrinsic::from_key(key), Some(intrinsic), "{key}");
    }
}

#[test]
fn an_unknown_key_names_nothing() {
    assert_eq!(Intrinsic::from_key("no_such_intrinsic"), None);
}
