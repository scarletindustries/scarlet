//! The interactive REPL.
//!
//! A session is a growing piece of *source text*: every entry that defined
//! something is replayed verbatim ahead of the current one (see
//! [`Session::eval`]). The line editor's coloring, completion and
//! multi-line behaviour all come from the same scanner/parser the evaluator
//! uses, via [`helper::ScarletHelper`].

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
use crate::bytecode;
use crate::bytecode::{CompileOptions, IncrementalSession, ModuleScope, UnusedBindings};
use crate::diagnostic;
use crate::term::Palette;
use command::Flow;
use entry::Entry;
use names::Names;

/// Where an entry's diagnostics say they come from. Not a real path: the
/// session's replayed source has no file.
const ORIGIN: &str = "<repl>";

pub fn run(version: &str) {
    let palette = Palette::for_stdout();
    let names = Rc::new(RefCell::new(Names::default()));
    let mut editor = match new_editor(palette, Rc::clone(&names)) {
        Ok(editor) => editor,
        Err(e) => {
            eprintln!("Failed to initialize readline: {e}");
            return;
        }
    };
    let history = history_path();
    if let Some(path) = &history {
        let _ = editor.load_history(path);
    }
    banner(version, &palette);

    let mut session = Session::new(palette, names);
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

        // `exit` predates the `:` commands and stays: it is what a REPL
        // reaches for first.
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
    names: Rc<RefCell<Names>>,
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
    editor.set_helper(Some(helper::ScarletHelper::new(palette, names)));
    // Enter submits when the entry parses and opens a line when it does not
    // (see the `Validator`). These open one unconditionally, for adding to an
    // entry that is already complete.
    //
    // Shift-Enter is deliberately absent: a terminal sends it as a plain
    // carriage return, indistinguishable from Enter, unless the user maps it.
    // `:help` says how. Ctrl-J is the spelling that works everywhere, since it
    // is a control character in its own right.
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

/// `$SCARLET_HISTORY`, else an XDG-ish state file. `None` when there is no
/// home directory to put it in, in which case history lives only as long as
/// the session.
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

/// The live session: the source it replays, and the names that source bound.
pub(crate) struct Session {
    /// Every import the session has seen, and every declaration it has
    /// accepted, replayed verbatim ahead of the current entry — imports
    /// first, as the language requires.
    ///
    /// Source, not retained AST: each turn is scanned on its own, so retained
    /// nodes would all restart their spans at line 1 and two same-shaped
    /// entries would share a `Span` — which the compiler keys expression
    /// types on. Replaying text keeps every span unique.
    imports: String,
    definitions: String,
    names: Rc<RefCell<Names>>,
    palette: Palette,
    /// Relative imports resolve against the directory the REPL was started
    /// in, which is the only directory a session can be said to be "in".
    base_dir: Option<PathBuf>,
}

impl Session {
    fn new(palette: Palette, names: Rc<RefCell<Names>>) -> Self {
        Session {
            imports: String::new(),
            definitions: String::new(),
            names,
            palette,
            base_dir: std::env::current_dir().ok(),
        }
    }

    fn palette(&self) -> &Palette {
        &self.palette
    }

    /// How a session compiles an entry: against the directory the REPL was
    /// started in, and without the unused-binding check, which no fragment of
    /// a session can pass (see [`UnusedBindings`]).
    fn compile_options(&self) -> CompileOptions<'_> {
        CompileOptions {
            unused_bindings: UnusedBindings::Ignore,
            module_scope: ModuleScope::Script,
            ..CompileOptions::new(self.base_dir.as_deref())
        }
    }

    /// The session's own source: everything it replays ahead of an entry.
    fn replay(&self) -> String {
        format!("{}{}", self.imports, self.definitions)
    }

    /// Compile the session's source plus `input` as one program and run it,
    /// printing the value unless it is `Nil` (a definition, or a call that ran
    /// only for its output). An entry joins the session only when it ran: its
    /// definitions are replayed ahead of every later entry, and one that
    /// stopped would stop each of them too.
    fn eval(&mut self, input: &str) {
        // A module's name alone is not a value, but at a prompt it is a fair
        // question: what is in it.
        let name = input.trim();
        if self.names.borrow().is_module(name) {
            return self.list_module(name);
        }
        let program = match entry::parse(input) {
            Entry::Accepted(program) => program,
            Entry::Incomplete(diagnostics) | Entry::Rejected(diagnostics) => {
                diagnostic::print_diagnostics(&diagnostics, input, ORIGIN, &|_| None);
                return;
            }
        };
        // The entry's own imports join the session's ahead of every
        // definition, so `import` works at any point in a session and not
        // only in its first entry.
        let parts = entry::split(input, &program);
        let combined_src = format!(
            "{}{}{}{}{}",
            self.imports, parts.imports, self.definitions, parts.definitions, parts.expressions
        );
        let mut scanner = crate::scanner::new_scanner(combined_src.clone());
        let combined = crate::parser::new_parser(&mut scanner).parse_program();
        if diagnostic::has_errors(&combined.diagnostics) {
            self.report(&combined.diagnostics, &combined_src);
            return;
        }
        let combined_ast = ast::Expression::BlockExpression(combined.ast);

        let result = bytecode::compile_with(&combined_ast, self.compile_options());
        if !result.diagnostics.is_empty() {
            self.report(&result.diagnostics, &combined_src);
            if !result.success() {
                return;
            }
        }
        let Some(runnable) = result.into_runnable() else {
            // A successful non-check compile always produces a program, so
            // reaching here means the stdlib seed failed and was already
            // reported.
            return;
        };
        let host = scarlet_vm::Host::of_this_process(Vec::new());
        let ran = {
            let mut out = std::io::stdout().lock();
            let ran = scarlet_vm::run_showing(&runnable, &host, &mut out);
            let _ = std::io::Write::flush(&mut out);
            ran
        };
        match ran {
            // Only an entry that ends in an expression has a value the user
            // wrote. What a definition leaves is the program's own, and
            // printing it says nothing about the entry.
            Ok(Some(shown)) if !parts.expressions.trim().is_empty() => println!("{shown}"),
            Ok(_) => {}
            Err(stop) => {
                if let Some(why) = crate::stop::message(&stop) {
                    eprintln!("{why}");
                }
                return;
            }
        }

        self.names.borrow_mut().observe(&program);
        self.imports.push_str(&parts.imports);
        self.definitions.push_str(&parts.definitions);
    }

    /// What the imported module `alias` offers: its first few public names,
    /// and how to see the rest.
    fn list_module(&self, alias: &str) {
        const SHOWN: usize = 15;
        let mut members = self.names.borrow_mut().qualified(alias, "");
        // Its functions and values first: what a module is for, ahead of the
        // types and constructors that go with them.
        members.sort_by_key(|m| m.name.starts_with(|c: char| c.is_ascii_uppercase()));
        let p = &self.palette;
        println!(
            "{alias} is a module. It has {} public names:",
            members.len()
        );
        for m in members.iter().take(SHOWN) {
            println!("  {alias}.{}", m.display());
        }
        if members.len() > SHOWN {
            println!(
                "{}  and {} more: type `{alias}.` and press Tab to see them all{}",
                p.dim,
                members.len() - SHOWN,
                p.reset
            );
        }
    }

    /// The inferred type of `expr`, from a throwaway checking session. Bound
    /// to a name because types are recorded at names: `_`-prefixed so the
    /// binding is not reported as unused.
    fn print_type(&mut self, expr: &str) {
        const BINDING: &str = "const _repl_type = ";

        let replay = self.replay();
        let probe_src = format!("{replay}{BINDING}{expr}\n");
        let mut scanner = crate::scanner::new_scanner(probe_src.clone());
        let probe = crate::parser::new_parser(&mut scanner).parse_program();
        if diagnostic::has_errors(&probe.diagnostics) {
            self.report(&probe.diagnostics, &probe_src);
            return;
        }

        let mut session = IncrementalSession::new();
        session.as_repl();
        let result = session.check(
            &ast::Expression::BlockExpression(probe.ast),
            self.base_dir.as_deref(),
        );
        if !result.success() {
            self.report(&result.diagnostics, &probe_src);
            return;
        }
        // The binding is the last line of the probe source, and its name
        // starts one column past `const `.
        let line = i32::try_from(replay.lines().count()).unwrap_or(0);
        let column = i32::try_from("const ".len()).unwrap_or(0);
        match session.hover(None, line, column) {
            Some((_, ty, _)) => println!("{expr}  {}{ty}{}", self.palette.bold, self.palette.reset),
            None => eprintln!("no type was inferred for that expression"),
        }
    }

    /// The Core IR of the session's functions whose name contains `needle`.
    /// Filtered, never whole: the program carries the entire stdlib.
    fn disassemble(&mut self, needle: &str) {
        let replay = self.replay();
        let mut scanner = crate::scanner::new_scanner(replay.clone());
        let parsed = crate::parser::new_parser(&mut scanner).parse_program();
        let result = bytecode::compile_with(
            &ast::Expression::BlockExpression(parsed.ast),
            self.compile_options(),
        );
        if !result.success() {
            self.report(&result.diagnostics, &replay);
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

    /// Evaluate a file as one entry, so its definitions join the session.
    fn load(&mut self, path: &Path) {
        match std::fs::read_to_string(path) {
            Ok(source) => self.eval(source.trim_end()),
            Err(e) => eprintln!("cannot read {}: {e}", path.display()),
        }
    }

    /// Write the session's replayed source — every entry that defined
    /// something — to a file that `scarlet run` can take.
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
        self.imports.clear();
        self.definitions.clear();
        self.names.borrow_mut().reset();
    }

    fn report(&self, diagnostics: &[diagnostic::Diagnostic], source: &str) {
        diagnostic::print_diagnostics(diagnostics, source, ORIGIN, &|_| None);
    }
}
