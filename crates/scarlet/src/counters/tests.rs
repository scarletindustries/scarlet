use super::*;

#[test]
fn the_total_is_the_sum_of_every_kind() {
    let counts = Counts {
        ops: vec![("Const", 2), ("Ret", 3), ("Move", 0)],
        cells_made: 7,
        cells_reused: 1,
        cells_live: 4,
    };
    let parsed: Value = serde_json::from_str(&json(&counts)).expect("JSON");
    assert_eq!(parsed["instructions"], 5);
    assert_eq!(parsed["ops"]["Ret"], 3);
    assert_eq!(parsed["ops"]["Move"], 0);
    assert_eq!(parsed["cells_made"], 7);
    assert_eq!(parsed["cells_reused"], 1);
    assert_eq!(parsed["cells_live"], 4);
}
