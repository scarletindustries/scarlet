//! The interactive REPL.
//!
//! A session is a growing piece of *source text*. Every entry that defined something is replayed
//! word for word ahead of the current one (see [`Session::eval`]). The line editor's colouring,
//! completion and multi-line behaviour all come from the same scanner and parser the evaluator
//! uses, through [`helper::ScarletHelper`].

mod command;
mod entry;
mod helper;
mod names;

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use rustyline::error::ReadlineError;
use rustyline::history::FileHistory;
use rustyline::{Cmd, Config, Editor, EventHandler, KeyCode, KeyEvent, Modifiers};

use crate::ast;
use crate::build::{self, Build};
use crate::check::{Role, UnusedBindings, typed};
use crate::compile::{self, Compiled};
use crate::diagnostic;
use crate::sources::DiskSources;
use crate::term::Palette;
use command::Flow;
use entry::Entry;
use names::{Accepted, AcceptedEntry};

/// Where an entry's diagnostics say they come from. It isn't a real path, because the session's
/// replayed source has no file.
const ORIGIN: &str = "<repl>";

pub fn run(version: &str) {
    let palette = Palette::for_stdout();
    let accepted = Rc::new(RefCell::new(Accepted::default()));
    let mut editor = match new_editor(palette, Rc::clone(&accepted)) {
        Ok(editor) => editor,
        Err(e) => {
            eprintln!("Failed to initialise readline: {e}");
            return;
        }
    };
    let history = history_path();
    if let Some(path) = &history {
        let _ = editor.load_history(path);
    }
    banner(version, &palette);

    let mut session = Session::new(palette, accepted);
    loop {
        let line = match editor.readline(">>> ") {
            Ok(line) => line,
            // Ctrl-C abandons the entry being typed without ending the session.
            Err(ReadlineError::Interrupted) => continue,
            Err(ReadlineError::Eof) => {
                println!();
                break;
            }
            Err(e) => {
                eprintln!("readline: {e}");
                break;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let _ = editor.add_history_entry(line.as_str());

        // `exit` is older than the `:` commands, and it stays because it's what people try first
        // in a REPL.
        if matches!(line.trim(), "exit" | "quit") {
            break;
        }
        match command::parse(&line) {
            Some(cmd) => match command::execute(cmd, &mut session) {
                Flow::Continue => {}
                Flow::Clear => {
                    if let Err(e) = editor.clear_screen() {
                        eprintln!("cannot clear the screen: {e}");
                    }
                }
                Flow::Quit => break,
            },
            None => session.eval(&line),
        }
    }

    if let Some(path) = &history {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = editor.save_history(path);
    }
}

fn new_editor(
    palette: Palette,
    accepted: Rc<RefCell<Accepted>>,
) -> rustyline::Result<Editor<helper::ScarletHelper, FileHistory>> {
    let config = Config::builder()
        .max_history_size(10_000)?
        .history_ignore_dups(true)?
        .history_ignore_space(true)
        .completion_type(rustyline::CompletionType::List)
        .bell_style(rustyline::config::BellStyle::None)
        .indent_size(4)
        .build();
    let mut editor = Editor::with_config(config)?;
    editor.set_helper(Some(helper::ScarletHelper::new(palette, accepted)));
    // Enter submits when the entry parses and opens a new line when it doesn't (see the
    // `Validator`). These keys always open one, for adding to an entry that's already complete.
    //
    // We leave out Shift-Enter on purpose. A terminal sends it as a plain carriage return, the
    // same as Enter, unless the user maps it, and `:help` says how. Ctrl-J works everywhere,
    // because it's a control character in its own right.
    for key in [
        KeyEvent(KeyCode::Enter, Modifiers::ALT),
        KeyEvent(KeyCode::Char('J'), Modifiers::CTRL),
    ] {
        editor.bind_sequence(key, EventHandler::Simple(Cmd::Newline));
    }
    Ok(editor)
}

fn banner(version: &str, p: &Palette) {
    println!("{}scarlet{} {version} REPL", p.scarlet, p.reset);
    println!(
        "{}:help for commands and keys, Ctrl-D to quit{}",
        p.dim, p.reset
    );
    println!();
}

/// `$SCARLET_HISTORY`, or else an XDG-style state file. It's `None` when there's no home
/// directory to put it in, and then history lasts only as long as the session.
fn history_path() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("SCARLET_HISTORY") {
        return Some(PathBuf::from(explicit));
    }
    if let Some(state) = std::env::var_os("XDG_STATE_HOME") {
        return Some(PathBuf::from(state).join("scarlet").join("history"));
    }
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(
        PathBuf::from(home)
            .join(".local")
            .join("state")
            .join("scarlet")
            .join("history"),
    )
}

/// The live session: the entries it accepted, and the build that keeps every module it imports
/// checked, so the stdlib is checked once per session.
pub(crate) struct Session {
    /// The entries the session accepted. Each one's imports and definitions are replayed word
    /// for word ahead of the current entry, imports first, as the language requires.
    ///
    /// We keep source instead of the AST because each turn is scanned on its own. Kept nodes would
    /// all restart their spans at line 1, and two entries of the same shape would share a `Span`.
    /// Replaying text keeps every span unique. The line editor reads the same list to complete
    /// names, so it is shared.
    accepted: Rc<RefCell<Accepted>>,
    build: Build,
    sources: DiskSources,
    palette: Palette,
    /// Relative imports resolve against the directory the REPL was started in, which is the only
    /// directory a session can be said to be "in".
    base_dir: Option<PathBuf>,
}

impl Session {
    fn new(palette: Palette, accepted: Rc<RefCell<Accepted>>) -> Self {
        Session {
            accepted,
            build: Build::default(),
            sources: DiskSources::new(),
            palette,
            base_dir: std::env::current_dir().ok(),
        }
    }

    fn palette(&self) -> &Palette {
        &self.palette
    }

    /// Compile `source` as the session's script: against the directory the REPL was started in,
    /// and without the unused-binding check, which no fragment of a session can pass.
    fn compile(&mut self, source: &str, parsed: build::Parsed) -> Compiled {
        let opts = compile::Options {
            file: Some(Path::new(ORIGIN)),
            base_dir: self.base_dir.as_deref(),
            role: Role::Script,
            unused: UnusedBindings::Ignore,
            as_module: None,
            run: false,
        };
        compile::compile(&mut self.build, &mut self.sources, source, parsed, &opts)
    }

    /// The session's own source: everything it replays ahead of an entry.
    fn replay(&self) -> String {
        self.accepted.borrow().replay()
    }

    /// Compiles the session's source plus `input` as one program and runs it, printing the value
    /// unless it's `Nil` (a definition, or a call that ran only for its output). An entry joins
    /// the session only if it ran. Its definitions are replayed ahead of every later entry, so one
    /// that stopped would stop each of them too.
    fn eval(&mut self, input: &str) {
        // A module's name on its own isn't a value, but at a prompt it's a fair way to ask what's
        // in it.
        let name = input.trim();
        if self.accepted.borrow().is_module(name) {
            return self.list_module(name);
        }
        let program = match entry::parse(input) {
            Entry::Accepted(program) => program,
            Entry::Incomplete(diagnostics) | Entry::Rejected(diagnostics) => {
                diagnostic::print_diagnostics(&diagnostics, input, ORIGIN);
                return;
            }
        };
        // The entry's own imports join the session's ahead of every definition, so `import` works
        // at any point in a session and not only in its first entry.
        let parts = entry::split(input, &program);
        let combined_src = self.accepted.borrow().replay_with(&parts);
        let combined = build::parse(&combined_src);
        if diagnostic::has_errors(&combined.diagnostics) {
            self.report(&combined.diagnostics, &combined_src);
            return;
        }
        let result = self.compile(&combined_src, combined);
        if !result.diagnostics().is_empty() {
            self.report(result.diagnostics(), &combined_src);
            if !result.success() {
                return;
            }
        }
        let Some(runnable) = result.into_runnable() else {
            // A successful compile that isn't a check always produces a program, so getting here
            // means the stdlib seed failed and was already reported.
            return;
        };
        let host = scarlet_vm::Host::of_this_process(Vec::new());
        let ran = {
            let mut out = crate::output::ProgramOut::stdout();
            let ran = scarlet_vm::run_showing(&runnable, &host, &mut out);
            let _ = std::io::Write::flush(&mut out);
            ran
        };
        match ran {
            // Only an entry that ends in an expression has a value the user wrote. What a
            // definition leaves belongs to the program, and printing it says nothing about the
            // entry.
            Ok(Some(shown)) if !parts.expressions.trim().is_empty() => println!("{shown}"),
            Ok(_) => {}
            Err(stop) => {
                if let Some(why) = crate::stop::message(&stop) {
                    eprintln!("{why}");
                }
                return;
            }
        }

        self.accepted
            .borrow_mut()
            .accept(AcceptedEntry::new(input, &program));
    }

    /// Lists what the imported module `alias` offers: its first few public names, and how to see
    /// the rest.
    fn list_module(&self, alias: &str) {
        const SHOWN: usize = 15;
        let mut members = self.accepted.borrow_mut().qualified(alias, "");
        // Show functions and values first, because they're what a module is for, then the types
        // and constructors that go with them.
        members.sort_by_key(|m| m.name.starts_with(|c: char| c.is_ascii_uppercase()));
        let p = &self.palette;
        println!(
            "{alias} is a module. It has {} public names:",
            members.len()
        );
        for m in members.iter().take(SHOWN) {
            println!("  {alias}::{}", m.display());
        }
        if members.len() > SHOWN {
            println!(
                "{}  and {} more: type `{alias}::` and press Tab to see them all{}",
                p.dim,
                members.len() - SHOWN,
                p.reset
            );
        }
    }

    /// Prints the inferred type of `expr`. It is checked as the session's last statement, so it
    /// sees every name the session bound, and nothing runs. The type is the one the checker gave
    /// that statement.
    fn print_type(&mut self, expr: &str) {
        let is_one_expression = match entry::parse(expr) {
            Entry::Accepted(program) => {
                matches!(program.body.as_slice(), [ast::Node::Expression(_)])
            }
            Entry::Incomplete(diagnostics) | Entry::Rejected(diagnostics) => {
                diagnostic::print_diagnostics(&diagnostics, expr, ORIGIN);
                return;
            }
        };
        if !is_one_expression {
            eprintln!(":type takes one expression");
            return;
        }
        let probe_src = format!("{}{expr}\n", self.replay());
        let probe = build::parse(&probe_src);
        if diagnostic::has_errors(&probe.diagnostics) {
            self.report(&probe.diagnostics, &probe_src);
            return;
        }
        let result = self.compile(&probe_src, probe);
        if !result.success() {
            self.report(result.diagnostics(), &probe_src);
            return;
        }
        let ty = result
            .analysis()
            .and_then(|a| a.entry().checked.clean.as_ref())
            .and_then(|clean| {
                let module = clean.module();
                match module.statements.last()? {
                    typed::Stmt::Expr(e) => Some(module.types.get(e.ty)),
                    typed::Stmt::Let { .. } => None,
                }
            });
        match ty {
            Some(ty) => println!("{expr}  {}{ty}{}", self.palette.bold, self.palette.reset),
            None => eprintln!("no type was inferred for that expression"),
        }
    }

    /// Prints the Core IR of the session's functions whose name contains `needle`. It's always
    /// filtered, because the program carries the whole stdlib.
    fn disassemble(&mut self, needle: &str) {
        let replay = self.replay();
        let parsed = build::parse(&replay);
        let result = self.compile(&replay, parsed);
        if !result.success() {
            self.report(result.diagnostics(), &replay);
            return;
        }
        let Some(program) = result.into_runnable() else {
            return;
        };
        match crate::dis::listing(&program, crate::dis::Filter::Named(needle)) {
            Some(text) => print!("{text}"),
            None => eprintln!("no function matching '{needle}'"),
        }
    }

    /// Evaluates a file as one entry, so its definitions join the session.
    fn load(&mut self, path: &Path) {
        match std::fs::read_to_string(path) {
            Ok(source) => self.eval(source.trim_end()),
            Err(e) => eprintln!("cannot read {}: {e}", path.display()),
        }
    }

    /// Writes the session's replayed source, meaning every entry that defined something, to a file
    /// that `scarlet run` can take.
    fn save(&self, path: &Path) {
        let replay = self.replay();
        if replay.is_empty() {
            eprintln!("nothing defined yet");
            return;
        }
        match std::fs::write(path, &replay) {
            Ok(()) => println!("wrote {}", path.display()),
            Err(e) => eprintln!("cannot write {}: {e}", path.display()),
        }
    }

    fn reset(&mut self) {
        self.accepted.borrow_mut().reset();
    }

    fn report<P: diagnostic::Problem>(
        &self,
        diagnostics: &[diagnostic::Diagnostic<P>],
        source: &str,
    ) {
        diagnostic::print_diagnostics(diagnostics, source, ORIGIN);
    }
}
