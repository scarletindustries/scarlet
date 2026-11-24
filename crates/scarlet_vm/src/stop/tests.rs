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
