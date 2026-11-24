use std::path::Path;

use super::{Census, DEFAULT_MIN_BOOLS, inspect, states};

fn census_of(source: &str) -> Census {
    let mut census = Census::default();
    inspect(
        Path::new("test.sca"),
        source,
        DEFAULT_MIN_BOOLS,
        &mut census,
    );
    census
}

#[test]
fn states_is_two_to_the_n() {
    assert_eq!(states(2), "4");
    assert_eq!(states(3), "8");
    assert_eq!(states(64), "2^64");
}

/// This is the shape the census exists to find. The fixture keeps three
/// Bools even though T-310 collapsed the `h1.sca` type that prompted it,
/// because this tests the finder, not how `h1.sca` is written today.
#[test]
fn finds_a_bool_cluster() {
    let census = census_of(
        "pub opaque type HeadFlags {\n\
             \tconn_close Bool\n\
             \tconn_keep_alive Bool\n\
             \texpect_100_continue Bool\n\
             }\n",
    );
    assert_eq!(census.uninspected.len(), 0);
    assert_eq!(census.findings.len(), 1);
    let finding = &census.findings[0];
    assert_eq!(finding.type_name, "HeadFlags");
    assert!(finding.opaque);
    assert_eq!(
        finding.bools,
        ["conn_close", "conn_keep_alive", "expect_100_continue"]
    );
}

/// One `Bool` is under the floor, and a second field of another type does
/// not make a cluster, because the census counts `Bool` fields, not fields.
#[test]
fn one_bool_is_not_a_cluster() {
    let census = census_of("pub type Flags {\n\tclose Bool\n\tcount Int\n}\n");
    assert_eq!(census.findings.len(), 0);
    assert_eq!(census.type_decls, 1);
}

/// Bools in two different constructors of one sum type never exist at the
/// same time, so they are not a cluster. In the same way, `bool_cluster`
/// reads one Rust struct, not all the variants of an enum together.
#[test]
fn bools_in_separate_constructors_do_not_combine() {
    let census = census_of("pub type Step {\n\tOpen(a Bool)\n\tShut(b Bool)\n}\n");
    assert_eq!(census.findings.len(), 0);
}

/// A text search gets this wrong: `type` and `Bool` inside a comment or a
/// string are not a declaration. The scanner never emits them as tokens, so
/// neither the finding count nor the coverage check changes.
#[test]
fn comments_and_strings_are_not_declarations() {
    let census = census_of(
        "// pub type Ghost {\n\
             //\ta Bool\n\
             //\tb Bool\n\
             // }\n\
             pub const doc = \"pub type Ghost { a Bool b Bool }\"\n",
    );
    assert_eq!(census.findings.len(), 0);
    assert_eq!(census.type_decls, 0);
    assert_eq!(census.uninspected.len(), 0);
}

/// The walk reaches a declaration nested in a block body. The coverage check
/// must agree, and a nested bool cluster is a finding. The old walk only
/// looked at the top level and reported this file as not inspected.
#[test]
fn a_nested_declaration_is_inspected() {
    let census = census_of(
        "pub fn make() {\n\
             \ttype Inner {\n\
             \t\tOne(a Bool, b Bool)\n\
             \t}\n\
             \t0\n\
             }\n",
    );
    assert_eq!(census.uninspected.len(), 0);
    assert_eq!(census.type_decls, 1);
    assert_eq!(census.findings.len(), 1);
    assert_eq!(census.findings[0].type_name, "Inner");
}

/// A file that does not parse yields no declarations, which is the same
/// empty list a clean file yields. It must be counted as uninspected.
#[test]
fn a_file_that_does_not_parse_is_not_clean() {
    let census = census_of("pub type Broken {\n\ta Bool\n\tb Bool\n");
    assert_eq!(census.findings.len(), 0);
    assert_eq!(census.uninspected.len(), 1);
}
