use super::*;

#[test]
fn going_in_twice_comes_back_out() {
    let contra = Variance::Contravariant;
    assert_eq!(contra.then(contra), Variance::Covariant);
}

#[test]
fn out_and_in_together_lock_a_parameter() {
    assert_eq!(
        Variance::Covariant.join(Variance::Contravariant),
        Variance::Invariant
    );
    assert_eq!(
        Variance::Unused.join(Variance::Covariant),
        Variance::Covariant
    );
}

#[test]
fn nothing_flows_through_an_unused_position() {
    assert_eq!(Variance::Invariant.then(Variance::Unused), Variance::Unused);
    assert_eq!(Variance::Unused.then(Variance::Invariant), Variance::Unused);
}
