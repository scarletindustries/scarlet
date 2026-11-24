use super::*;

/// What a wrong reference count is reported as, which `scarlet run`
/// prints after "internal error:".
#[test]
fn a_wrong_count_says_which_way_it_went_wrong() {
    let Stop::BadProgram(low) = Stop::from(DeadCell) else {
        unreachable!("a dead cell is a bad program")
    };
    assert_eq!(
        low.to_string(),
        "a reference to a cell that was already freed"
    );
    let Stop::BadProgram(high) = Stop::broken(Broken::CellsLeft { cells: 3 }) else {
        unreachable!("cells left are a bad program")
    };
    assert_eq!(
        high.to_string(),
        "3 cells were still held after the run gave up everything it had"
    );
}

/// What is said of a program that uses its constructors wrongly, and of a cell
/// that is none of them.
#[test]
fn a_constructor_that_is_not_as_listed_is_named() {
    let ctor = VariantRef {
        type_id: scarlet_ir::TypeId(263),
        variant_idx: 4,
    };
    let (got, want) = (1, 2);
    for (why, says) in [
        (
            Broken::NoCtor(ctor),
            "constructor 263.4, which the program does not list",
        ),
        (
            Broken::CtorFields { ctor, got, want },
            "constructor 263.4 with 1 fields, not 2",
        ),
        (Broken::CtorCell, "a constructor cell of no constructor"),
        (Broken::ClosureCell, "a closure cell with no function"),
    ] {
        assert_eq!(BadProgram(why).to_string(), says);
    }
}

/// A fault an audit finds is a bug. No memory to look for one is not, and is
/// not the program's heap running out either.
#[test]
fn an_audit_that_could_not_finish_is_not_a_fault() {
    assert_eq!(Stop::from(AuditFailure::NoRoom), Stop::NoRoomToAudit);
    let found = AuditFailure::Found(AuditFault::Boxes);
    assert_eq!(
        Stop::from(found),
        Stop::broken(Broken::Audit(AuditFault::Boxes))
    );
}
