use std::fmt::Write as _;
use std::process::ExitCode;

use clap::{Arg, ArgAction, Command};

use al_core::term::Palette;

const LOGO: [&str; 2] = ["▄▀█ █░░", "█▀█ █▄▄"];
const LEARN_MORE: &str = "https://al.alistair.sh";

/// Hand-maintained: clap can describe flags but not show idiomatic usage.
fn examples_for(sub: &str) -> &'static [&'static str] {
    match sub {
        "run" => &["al run hello.al", "al run server.al"],
        "check" => &["al check src/main.al"],
        "fmt" => &["al fmt .", "al fmt --check src/", "al fmt --stdin < a.al"],
        _ => &[],
    }
}

fn root_examples() -> &'static [&'static str] {
    &["al run hello.al", "al check src/", "al fmt --check ."]
}

fn is_hidden(c: &Command) -> bool {
    c.is_hide_set()
}

fn takes_value(a: &Arg) -> bool {
    matches!(a.get_action(), ArgAction::Set | ArgAction::Append)
}

fn is_managed_flag(a: &Arg) -> bool {
    matches!(
        a.get_action(),
        ArgAction::Help | ArgAction::HelpShort | ArgAction::HelpLong | ArgAction::Version
    )
}

fn value_name(a: &Arg) -> String {
    a.get_value_names()
        .and_then(|v| v.first())
        .map(|s| s.to_string())
        .unwrap_or_else(|| a.get_id().as_str().to_uppercase())
}

fn positional_token(a: &Arg) -> String {
    let base = a.get_id().as_str();
    if a.is_required_set() {
        format!("<{base}>")
    } else {
        format!("[{base}]")
    }
}

fn about_of(c: &Command) -> String {
    c.get_about().map(|s| s.to_string()).unwrap_or_default()
}

/// `name <arg> [arg]` — the left column of the command list.
fn sub_synopsis(sub: &Command) -> String {
    let mut s = sub.get_name().to_string();
    for p in sub.get_positionals() {
        s.push(' ');
        s.push_str(&positional_token(p));
    }
    s
}

/// `-s, --long <VAL>`, padded so `--long` lines up with or without a short.
fn opt_label(a: &Arg) -> String {
    let mut s = String::new();
    match a.get_short() {
        Some(c) => {
            let _ = write!(s, "-{c}, ");
        }
        None => s.push_str("    "),
    }
    if let Some(l) = a.get_long() {
        let _ = write!(s, "--{l}");
    }
    if takes_value(a) {
        let _ = write!(s, " <{}>", value_name(a));
    }
    s
}

fn display_width(s: &str) -> usize {
    s.chars().count()
}

/// One `label  description` row. `width` is measured on the uncolored label so
/// ANSI codes do not skew alignment.
fn row(out: &mut String, p: &Palette, label: &str, desc: &str, width: usize) {
    let pad = " ".repeat(width.saturating_sub(display_width(label)) + 2);
    let _ = write!(out, "  {label}{pad}");
    if desc.is_empty() {
        out.push('\n');
    } else {
        let _ = writeln!(out, "{}{desc}{}", p.dim, p.reset);
    }
}

fn heading(out: &mut String, p: &Palette, text: &str) {
    let _ = writeln!(out, "\n{}{text}{}", p.bold, p.reset);
}

fn examples_block(o: &mut String, p: &Palette, exs: &[&str]) {
    if !exs.is_empty() {
        heading(o, p, "EXAMPLES");
        for ex in exs {
            let _ = writeln!(o, "  {}{ex}{}", p.dim, p.reset);
        }
    }
}

fn footer(o: &mut String, p: &Palette, lead: &str, more: &str) {
    let _ = writeln!(
        o,
        "\n  {}{lead}{}  {more}{}\n",
        p.dim,
        p.reset,
        p.hyperlink(LEARN_MORE, LEARN_MORE)
    );
}

fn logo_block(out: &mut String, p: &Palette, version: &str, tagline: &str) {
    let _ = writeln!(
        out,
        "  {}{}{}    {}al{} {}{version}{}",
        p.cyan, LOGO[0], p.reset, p.bold, p.reset, p.dim, p.reset
    );
    let _ = writeln!(
        out,
        "  {}{}{}    {}{tagline}{}",
        p.cyan, LOGO[1], p.reset, p.dim, p.reset
    );
}

fn version_str(cmd: &Command) -> &str {
    cmd.get_version().unwrap_or("")
}

fn global_options() -> [(String, &'static str); 2] {
    [
        ("-h, --help".to_string(), "Show help"),
        ("-V, --version".to_string(), "Show version"),
    ]
}

/// `al` with no args — compact landing screen.
pub fn home(cmd: &Command) {
    let p = Palette::for_stdout();
    let mut o = String::new();
    o.push('\n');
    logo_block(&mut o, &p, version_str(cmd), &about_of(cmd));

    heading(&mut o, &p, "QUICK START");
    let rows = [
        ("al run <file>", "Run a program"),
        ("al repl", "Start an interactive REPL"),
        ("al --help", "Show all commands"),
    ];
    let w = rows.iter().map(|(l, _)| l.len()).max().unwrap_or(0);
    for (l, d) in rows {
        row(&mut o, &p, l, d, w);
    }

    footer(&mut o, &p, "Learn more", "");
    print!("{o}");
}

/// `al --help` / `al help` — the full reference.
fn full_help(cmd: &Command) -> String {
    let p = Palette::for_stdout();
    let mut o = String::new();
    o.push('\n');
    logo_block(&mut o, &p, version_str(cmd), &about_of(cmd));

    heading(&mut o, &p, "USAGE");
    let _ = writeln!(o, "  {}al{} <command> [options]", p.bold, p.reset);

    let subs: Vec<&Command> = cmd.get_subcommands().filter(|c| !is_hidden(c)).collect();

    let opts = global_options();
    let label_w = subs
        .iter()
        .map(|s| display_width(&sub_synopsis(s)))
        .chain(opts.iter().map(|(l, _)| l.len()))
        .max()
        .unwrap_or(0);

    heading(&mut o, &p, "COMMANDS");
    for s in &subs {
        row(&mut o, &p, &sub_synopsis(s), &about_of(s), label_w);
    }

    heading(&mut o, &p, "OPTIONS");
    for (l, d) in &opts {
        row(&mut o, &p, l, d, label_w);
    }

    examples_block(&mut o, &p, root_examples());
    footer(&mut o, &p, "Learn more", "");
    o
}

/// `al <cmd> --help` / `al help <cmd>` — one command in detail.
fn command_help(sub: &Command) -> String {
    let p = Palette::for_stdout();
    let name = sub.get_name();
    let mut o = String::new();

    let _ = writeln!(o, "\n  {}al {name}{}{}", p.bold, p.reset, {
        let a = about_of(sub);
        if a.is_empty() {
            String::new()
        } else {
            format!("  {}— {a}{}", p.dim, p.reset)
        }
    });

    let positionals: Vec<&Arg> = sub.get_positionals().collect();
    let options: Vec<&Arg> = sub
        .get_arguments()
        .filter(|a| !a.is_positional() && !is_managed_flag(a))
        .collect();

    heading(&mut o, &p, "USAGE");
    let mut usage = format!("{}al {name}{}", p.bold, p.reset);
    for pa in &positionals {
        let _ = write!(usage, " {}", positional_token(pa));
    }
    if !options.is_empty() {
        usage.push_str(" [options]");
    }
    let _ = writeln!(o, "  {usage}");

    let mut opt_rows: Vec<(String, String)> = options
        .iter()
        .map(|a| {
            (
                opt_label(a),
                a.get_help().map(|h| h.to_string()).unwrap_or_default(),
            )
        })
        .collect();
    opt_rows.push(("-h, --help".to_string(), "Show help".to_string()));

    let label_w = positionals
        .iter()
        .map(|a| display_width(&positional_token(a)))
        .chain(opt_rows.iter().map(|(l, _)| display_width(l)))
        .max()
        .unwrap_or(0);

    if !positionals.is_empty() {
        heading(&mut o, &p, "ARGUMENTS");
        for a in &positionals {
            let req = if a.is_required_set() {
                "required"
            } else {
                "optional"
            };
            let help = a.get_help().map(|h| h.to_string()).unwrap_or_default();
            let desc = if help.is_empty() {
                format!("({req})")
            } else {
                format!("{help} ({req})")
            };
            row(&mut o, &p, &positional_token(a), &desc, label_w);
        }
    }

    heading(&mut o, &p, "OPTIONS");
    for (l, d) in &opt_rows {
        row(&mut o, &p, l, d, label_w);
    }

    examples_block(&mut o, &p, examples_for(name));
    footer(&mut o, &p, "See also", "al --help · ");
    o
}

/// Dispatch help: `None` prints the full reference, `Some(name)` one command.
/// An unknown name prints the full reference and exits 2.
pub fn help(cmd: &Command, sub: Option<&str>) -> ExitCode {
    match sub {
        None => {
            print!("{}", full_help(cmd));
            ExitCode::SUCCESS
        }
        Some(name) => match cmd.get_subcommands().find(|c| c.get_name() == name) {
            Some(s) => {
                print!("{}", command_help(s));
                ExitCode::SUCCESS
            }
            None => {
                let p = Palette::for_stderr();
                eprintln!(
                    "{}error{}: unknown command {}{name}{}",
                    p.error, p.reset, p.bold, p.reset
                );
                print!("{}", full_help(cmd));
                ExitCode::from(2)
            }
        },
    }
}

pub fn version(cmd: &Command) {
    let p = Palette::for_stdout();
    println!(
        "{}al{} {}{}{}",
        p.bold,
        p.reset,
        p.dim,
        version_str(cmd),
        p.reset
    );
}

/// Render a clap parse failure in our own voice, never clap's formatter.
/// clap's `Display` puts its own `Usage:`/`For more information` footer after
/// a blank line, so only the message block above that blank line is kept.
pub fn error(err: &clap::Error) {
    let p = Palette::for_stderr();
    let raw = err.to_string();

    let mut parts: Vec<String> = Vec::new();
    for line in raw.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with("Usage:") || t.starts_with("For more information") {
            break;
        }
        parts.push(t.trim_start_matches("error:").trim().to_string());
    }
    let msg = if parts.is_empty() {
        "invalid arguments".to_string()
    } else {
        parts.join(" ")
    };

    eprintln!("\n  {}error{}  {msg}", p.error, p.reset);
    eprintln!(
        "  {}run {}al --help{}{} for usage{}\n",
        p.dim, p.bold, p.reset, p.dim, p.reset
    );
}
