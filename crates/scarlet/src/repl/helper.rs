//! The line editor's view of Scarlet: what to colour, what completes, and when an entry is
//! finished.
//!
//! Completion and validation both read the language through the real scanner
//! and parser, so "is this entry finished" is the parser's answer rather than
//! a bracket count that would disagree with it about `'{'`.

use std::borrow::Cow;
use std::cell::RefCell;
use std::rc::Rc;

use rustyline::completion::{Completer, FilenameCompleter, Pair};
use rustyline::highlight::Highlighter;
use rustyline::hint::{Hinter, HistoryHinter};
use rustyline::validate::{ValidationContext, ValidationResult, Validator};
use rustyline::{Context, Helper};

use crate::highlight::highlight_at;
use crate::repl::command;
use crate::repl::entry::{self, Entry};
use crate::repl::names::{Accepted, Candidate};
use crate::term::Palette;
use crate::token::{is_name_continue, is_name_start};

pub struct ScarletHelper {
    palette: Palette,
    /// Shared with the session, which extends it as entries are accepted.
    names: Rc<RefCell<Accepted>>,
    hinter: HistoryHinter,
    filenames: FilenameCompleter,
}

impl ScarletHelper {
    pub fn new(palette: Palette, names: Rc<RefCell<Accepted>>) -> Self {
        ScarletHelper {
            palette,
            names,
            hinter: HistoryHinter::new(),
            filenames: FilenameCompleter::new(),
        }
    }
}

impl Helper for ScarletHelper {}

impl Highlighter for ScarletHelper {
    fn highlight<'l>(&self, line: &'l str, pos: usize) -> Cow<'l, str> {
        if line.starts_with(':') {
            return Cow::Owned(format!("{}{line}{}", self.palette.dim, self.palette.reset));
        }
        // Colour is display only, so a line the highlighter can't lay its
        // tokens over is shown as typed.
        match highlight_at(line, &self.palette, Some(pos)) {
            Ok(coloured) => Cow::Owned(coloured),
            Err(_) => Cow::Borrowed(line),
        }
    }

    fn highlight_prompt<'b, 's: 'b, 'p: 'b>(
        &'s self,
        prompt: &'p str,
        _default: bool,
    ) -> Cow<'b, str> {
        Cow::Owned(format!(
            "{}{prompt}{}",
            self.palette.scarlet, self.palette.reset
        ))
    }

    fn highlight_hint<'h>(&self, hint: &'h str) -> Cow<'h, str> {
        Cow::Owned(format!("{}{hint}{}", self.palette.dim, self.palette.reset))
    }

    /// Repaint on every keystroke and cursor move, because the colours depend on the whole line (a
    /// `(` becomes a call when a name comes before it) and on where the cursor is (the bracket it
    /// rests on).
    fn highlight_char(&self, _line: &str, _pos: usize, _forced: bool) -> bool {
        self.palette.enabled()
    }
}

impl Hinter for ScarletHelper {
    type Hint = String;

    /// The greyed-out text after the cursor. It's what Tab would insert, or else the rest of the
    /// last matching entry, and Right accepts it.
    ///
    /// We only show it at the end of the line, because a hint drawn mid-line would sit between
    /// the cursor and text the user can already see.
    fn hint(&self, line: &str, pos: usize, ctx: &Context<'_>) -> Option<String> {
        if pos < line.len() {
            return None;
        }
        self.completion_hint(line, pos)
            .or_else(|| self.hinter.hint(line, pos, ctx))
    }
}

impl ScarletHelper {
    /// The completion every candidate agrees on, minus what's already typed. We suggest nothing
    /// for an empty word, because the names in scope share no prefix and a hint that appears out
    /// of nowhere is noise.
    fn completion_hint(&self, line: &str, pos: usize) -> Option<String> {
        let (start, candidates) = match command::body(line) {
            // Leave path arguments to Tab, because hinting one would stat the filesystem on
            // every keystroke.
            Some(after_marker) if after_marker.contains(char::is_whitespace) => return None,
            Some(_) => complete_command(typed_command(line, pos)),
            None => complete_source(line, pos, &mut self.names.borrow_mut()),
        };
        let typed = line.get(start..pos).filter(|t| !t.is_empty())?;
        let shared = common_prefix(candidates.iter().map(|c| c.replacement.as_str()))?;
        Some(shared.strip_prefix(typed)?.to_string()).filter(|hint| !hint.is_empty())
    }
}

/// The longest prefix every candidate shares. It's what Tab would insert, so the ghost text and
/// the Tab key never disagree.
fn common_prefix<'a>(mut candidates: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    let first = candidates.next()?;
    let end = candidates.fold(first.len(), |end, c| {
        let shared = first
            .bytes()
            .zip(c.bytes())
            .take(end)
            .take_while(|(a, b)| a == b)
            .count();
        // Never split a character, because the user sees the prefix.
        (0..=shared)
            .rev()
            .find(|&n| first.is_char_boundary(n))
            .unwrap_or(0)
    });
    first.get(..end)
}

impl Validator for ScarletHelper {
    /// Enter submits a finished entry and opens a new line on an unfinished one. This never
    /// returns `Invalid`. The evaluator reports real syntax errors, and refusing to submit would
    /// trap the user in a line they can't get out of.
    fn validate(&self, ctx: &mut ValidationContext) -> rustyline::Result<ValidationResult> {
        let input = ctx.input();
        if input.trim().is_empty() || command::parse(input).is_some() {
            return Ok(ValidationResult::Valid(None));
        }
        Ok(match entry::parse(input) {
            Entry::Incomplete(_) => ValidationResult::Incomplete,
            Entry::Rejected(_) | Entry::Accepted(_) => ValidationResult::Valid(None),
        })
    }
}

impl Completer for ScarletHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        let Some(after_marker) = command::body(line) else {
            return Ok(complete_source(line, pos, &mut self.names.borrow_mut()));
        };
        match after_marker.split_once(char::is_whitespace) {
            // Still typing the command itself.
            None => Ok(complete_command(typed_command(line, pos))),
            // Its argument: a path for the commands that take one.
            Some((name, _)) if command::takes_path(name) => self.filenames.complete(line, pos, ctx),
            Some(_) => Ok((pos, Vec::new())),
        }
    }
}

/// The marker the line starts with and the command name typed so far. We keep the marker so a
/// candidate extends the line the user is actually looking at, instead of rewriting `/he` as
/// `:help`.
fn typed_command(line: &str, pos: usize) -> (&str, &str) {
    (
        line.get(..1).unwrap_or(":"),
        line.get(1..pos.max(1)).unwrap_or(""),
    )
}

fn complete_command((marker, typed): (&str, &str)) -> (usize, Vec<Pair>) {
    let items = command::names()
        .iter()
        .filter(|n| n.starts_with(typed))
        .map(|n| Pair {
            display: format!("{marker}{n}"),
            replacement: format!("{marker}{n} "),
        })
        .collect();
    (0, items)
}

/// Candidates for source text, and the byte offset they replace from.
fn complete_source(line: &str, pos: usize, names: &mut Accepted) -> (usize, Vec<Pair>) {
    // `import scarlet/…` completes module paths, which are not identifiers:
    // their `/` separators fall outside the word grammar.
    if let Some(path) = import_path_before(line, pos) {
        return (
            pos.saturating_sub(path.len()),
            pairs(Accepted::module_paths(path)),
        );
    }
    let Some((start, word)) = word_before(line, pos) else {
        return (pos, Vec::new());
    };
    match word.rsplit_once("::") {
        Some((qualifier, member)) => (
            start.saturating_add(qualifier.len()).saturating_add(2),
            pairs(names.qualified(qualifier, member)),
        ),
        None => (start, pairs(names.bare(word))),
    }
}

fn pairs(candidates: Vec<Candidate>) -> Vec<Pair> {
    candidates
        .into_iter()
        .map(|c| Pair {
            display: c.display(),
            replacement: c.name,
        })
        .collect()
}

/// The identifier ending at `pos`, with any `qualifier::` prefix, and where it starts. It's
/// `None` where no name can begin, such as inside a number literal. That's different from an
/// empty word at a boundary, which completes to everything in scope.
fn word_before(line: &str, pos: usize) -> Option<(usize, &str)> {
    let pos = pos.min(line.len());
    let bytes = line.as_bytes();
    let before = bytes.get(..pos)?;
    let name_start = |s: &[u8]| {
        s.len()
            .saturating_sub(s.iter().rev().take_while(|&&b| is_name_continue(b)).count())
    };
    let mut start = name_start(before);
    // One `qualifier::` in front of the member being typed.
    if let Some(head) = before.get(..start).and_then(|h| h.strip_suffix(b"::")) {
        let q = name_start(head);
        if q < head.len() {
            start = q;
        }
    }
    if start < pos && !bytes.get(start).copied().is_some_and(is_name_start) {
        return None;
    }
    Some((start, line.get(start..pos)?))
}

/// The half-written module path on an `import` line, if the cursor is in one.
fn import_path_before(line: &str, pos: usize) -> Option<&str> {
    let head = line.get(..pos)?;
    let (before, path) = head.rsplit_once(char::is_whitespace)?;
    if before.trim() != "import" {
        return None;
    }
    path.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '/')
        .then_some(path)
}

#[cfg(test)]
mod tests;
