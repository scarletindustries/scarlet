//! The REPL's `:` commands.
//!
//! One table drives parsing, `:help`, and Tab completion, so a command cannot
//! exist without being documented or completable.

use crate::repl::Session;
use crate::term::Palette;

/// What a command does with the rest of its line.
#[derive(PartialEq, Eq, Clone, Copy)]
enum Arg {
    None,
    /// A path, which Tab completes against the filesystem.
    Path(&'static str),
    Text(&'static str),
}

struct Spec {
    name: &'static str,
    aliases: &'static [&'static str],
    arg: Arg,
    help: &'static str,
}

const COMMANDS: &[Spec] = &[
    Spec {
        name: "help",
        aliases: &["h", "?"],
        arg: Arg::None,
        help: "show this help",
    },
    Spec {
        name: "type",
        aliases: &["t"],
        arg: Arg::Text("expr"),
        help: "the inferred type of an expression",
    },
    Spec {
        name: "dis",
        aliases: &[],
        arg: Arg::Text("name"),
        help: "print the Core IR of the functions whose name contains <name>",
    },
    Spec {
        name: "load",
        aliases: &[],
        arg: Arg::Path("file"),
        help: "evaluate a file into the session",
    },
    Spec {
        name: "save",
        aliases: &[],
        arg: Arg::Path("file"),
        help: "write the session's definitions to a file",
    },
    Spec {
        name: "reset",
        aliases: &[],
        arg: Arg::None,
        help: "forget every definition",
    },
    Spec {
        name: "clear",
        aliases: &[],
        arg: Arg::None,
        help: "clear the screen",
    },
    Spec {
        name: "quit",
        aliases: &["q", "exit"],
        arg: Arg::None,
        help: "leave the REPL",
    },
];

/// A parsed command line. Unknown names are represented rather than rejected
/// here, so the REPL reports them itself instead of trying to run `:hlep` as
/// Scarlet source.
pub enum Command<'a> {
    Help,
    Quit,
    Clear,
    Reset,
    Type(&'a str),
    Dis(&'a str),
    Load(&'a str),
    Save(&'a str),
    Unknown(&'a str),
}

/// What the caller does next. `Clear` is its own outcome because clearing the
/// screen belongs to the line editor, which owns the terminal: printing the
/// escape from here would race the editor's own repaint.
#[derive(PartialEq, Eq)]
pub enum Flow {
    Continue,
    Clear,
    Quit,
}

/// The command `line` names, or `None` if the line is Scarlet source.
///
/// `:` is the spelling, `/` is accepted too: no Scarlet entry can begin with
/// either, and everyone arrives from some other tool's habit.
pub fn parse(line: &str) -> Option<Command<'_>> {
    let rest = body(line.trim())?;
    let (name, arg) = match rest.split_once(char::is_whitespace) {
        Some((name, arg)) => (name, arg.trim()),
        None => (rest, ""),
    };
    Some(match spec(name).map(|s| s.name) {
        Some("help") => Command::Help,
        Some("quit") => Command::Quit,
        Some("clear") => Command::Clear,
        Some("reset") => Command::Reset,
        Some("type") => Command::Type(arg),
        Some("dis") => Command::Dis(arg),
        Some("load") => Command::Load(arg),
        Some("save") => Command::Save(arg),
        _ => Command::Unknown(name),
    })
}

pub fn execute(cmd: Command<'_>, session: &mut Session) -> Flow {
    match cmd {
        Command::Help => print_help(session.palette()),
        Command::Quit => return Flow::Quit,
        Command::Clear => return Flow::Clear,
        // Silence would be indistinguishable from the command not running.
        Command::Reset => {
            session.reset();
            println!("session reset");
        }
        Command::Type(expr) => {
            if let Some(expr) = require_arg("type", expr) {
                session.print_type(expr);
            }
        }
        Command::Dis(name) => {
            if let Some(name) = require_arg("dis", name) {
                session.disassemble(name);
            }
        }
        Command::Load(path) => {
            if let Some(path) = require_arg("load", path) {
                session.load(path.as_ref());
            }
        }
        Command::Save(path) => {
            if let Some(path) = require_arg("save", path) {
                session.save(path.as_ref());
            }
        }
        Command::Unknown(name) => {
            eprintln!("unknown command ':{name}' — :help lists them");
        }
    }
    Flow::Continue
}

/// The argument, or a usage line naming what was missing.
fn require_arg<'a>(command: &str, arg: &'a str) -> Option<&'a str> {
    if !arg.is_empty() {
        return Some(arg);
    }
    let placeholder = match spec(command).map(|s| s.arg) {
        Some(Arg::Path(p) | Arg::Text(p)) => p,
        _ => return Some(arg),
    };
    eprintln!("usage: :{command} <{placeholder}>");
    None
}

/// `line` past its command marker, or `None` if it is not a command line.
/// The one place the markers are spelled, so parsing and completion cannot
/// disagree about what counts as a command.
pub fn body(line: &str) -> Option<&str> {
    line.strip_prefix(':').or_else(|| line.strip_prefix('/'))
}

fn spec(name: &str) -> Option<&'static Spec> {
    COMMANDS
        .iter()
        .find(|c| c.name == name || c.aliases.contains(&name))
}

/// Every command name, for Tab completion. Aliases are omitted: completing to
/// the full name teaches the name that `:help` documents.
pub fn names() -> Vec<&'static str> {
    COMMANDS.iter().map(|c| c.name).collect()
}

/// Whether `name`'s argument is a path, and so completes against the
/// filesystem.
pub fn takes_path(name: &str) -> bool {
    spec(name).is_some_and(|s| matches!(s.arg, Arg::Path(_)))
}

fn print_help(p: &Palette) {
    println!("{}commands{}", p.bold, p.reset);
    for c in COMMANDS {
        let arg = match c.arg {
            Arg::None => String::new(),
            Arg::Path(a) | Arg::Text(a) => format!(" <{a}>"),
        };
        let aliases = if c.aliases.is_empty() {
            String::new()
        } else {
            let spellings: Vec<String> = c.aliases.iter().map(|a| format!(":{a}")).collect();
            format!("  ({})", spellings.join(", "))
        };
        let invocation = format!("{}{arg}", c.name);
        println!(
            "  :{invocation:<20}{help}{dim}{aliases}{reset}",
            help = c.help,
            dim = p.dim,
            reset = p.reset
        );
    }
    println!();
    println!("{}keys{}", p.bold, p.reset);
    for (keys, what) in [
        ("Tab", "complete the name being typed"),
        (
            "Enter",
            "evaluate, or open a new line if the entry is unfinished",
        ),
        ("Alt-Enter", "open a new line without evaluating"),
        ("Ctrl-J", "the same, for terminals that eat Alt-Enter"),
        ("Up / Down", "move between the lines of a multi-line entry"),
        ("Right", "accept the greyed-out suggestion"),
        ("Ctrl-R", "search earlier entries"),
        ("Ctrl-L", "clear the screen"),
        ("Ctrl-C", "abandon the entry being typed"),
        ("Ctrl-D", "leave the REPL"),
    ] {
        println!("  {keys:<12}{what}");
    }
    println!();
    println!(
        "{}Shift-Enter reaches a terminal as a plain Enter. To use it for a new{}",
        p.dim, p.reset
    );
    println!(
        "{}line, map it to Alt-Enter — in Ghostty: keybind = shift+enter=text:\\x1b\\r{}",
        p.dim, p.reset
    );
}

#[cfg(test)]
mod tests {
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

    /// `/` is the habit half the world arrives with, and no Scarlet entry can
    /// start with one.
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
}
