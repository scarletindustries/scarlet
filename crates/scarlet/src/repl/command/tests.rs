use super::*;

#[test]
fn source_is_not_a_command() {
    assert!(parse("1 + 1").is_none());
    assert!(parse("const x = 1").is_none());
}

#[test]
fn an_alias_resolves_to_its_command() {
    assert!(matches!(parse(":t 1 + 1"), Some(Command::Type("1 + 1"))));
    assert!(matches!(parse(":q"), Some(Command::Quit)));
    assert!(matches!(parse(":exit"), Some(Command::Quit)));
    assert!(matches!(parse("/exit"), Some(Command::Quit)));
    assert!(matches!(parse(":?"), Some(Command::Help)));
}

/// Lots of people arrive with the `/` habit, and no Scarlet entry can start with one.
#[test]
fn a_slash_spells_a_command_too() {
    assert!(matches!(parse("/quit"), Some(Command::Quit)));
    assert!(matches!(parse("/t 1"), Some(Command::Type("1"))));
    assert!(parse("1 / 2").is_none());
}

#[test]
fn an_unknown_command_is_named_back() {
    assert!(matches!(parse(":hlep"), Some(Command::Unknown("hlep"))));
}

#[test]
fn only_path_arguments_complete_as_paths() {
    assert!(takes_path("load"));
    assert!(!takes_path("type"));
    assert!(!takes_path("nonexistent"));
}

#[test]
fn every_command_is_completable() {
    assert_eq!(names().len(), COMMANDS.len());
}
