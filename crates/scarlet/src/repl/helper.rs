//! The line editor's view of Scarlet: what to color, what completes, and when
//! an entry is finished.
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
use crate::repl::names::{Candidate, Names};
use crate::term::Palette;
use crate::token::{is_name_continue, is_name_start};

pub struct ScarletHelper {
    palette: Palette,
    /// Shared with the session, which extends it as entries are accepted.
    names: Rc<RefCell<Names>>,
    hinter: HistoryHinter,
    filenames: FilenameCompleter,
}

impl ScarletHelper {
    pub fn new(palette: Palette, names: Rc<RefCell<Names>>) -> Self {
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
        Cow::Owned(highlight_at(line, &self.palette, Some(pos)))
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

    /// Repaint on every keystroke and cursor move: the colors depend on the
    /// whole line (a `(` becomes a call when a name precedes it) and on where
    /// the cursor is (the bracket it rests on).
    fn highlight_char(&self, _line: &str, _pos: usize, _forced: bool) -> bool {
        self.palette.enabled()
    }
}

impl Hinter for ScarletHelper {
    type Hint = String;

    /// The greyed-out text past the cursor: what Tab would insert, falling
    /// back to the rest of the last matching entry. Right accepts it.
    ///
    /// Only at the end of the line — a hint drawn mid-line would sit between
    /// the cursor and text the user can see is already there.
    fn hint(&self, line: &str, pos: usize, ctx: &Context<'_>) -> Option<String> {
        if pos < line.len() {
            return None;
        }
        self.completion_hint(line, pos)
            .or_else(|| self.hinter.hint(line, pos, ctx))
    }
}

impl ScarletHelper {
    /// The completion every candidate agrees on, minus what is already typed.
    /// Nothing is suggested for an empty word: every name in scope shares no
    /// prefix, and a hint that appears out of nowhere is noise.
    fn completion_hint(&self, line: &str, pos: usize) -> Option<String> {
        let (start, candidates) = match command::body(line) {
            // A path argument is left to Tab: hinting it would stat the
            // filesystem on every keystroke.
            Some(after_marker) if after_marker.contains(char::is_whitespace) => return None,
            Some(_) => complete_command(typed_command(line, pos)),
            None => complete_source(line, pos, &mut self.names.borrow_mut()),
        };
        let typed = line.get(start..pos).filter(|t| !t.is_empty())?;
        let shared = common_prefix(candidates.iter().map(|c| c.replacement.as_str()))?;
        Some(shared.strip_prefix(typed)?.to_string()).filter(|hint| !hint.is_empty())
    }
}

/// The longest prefix shared by every candidate — what Tab would insert, so
/// the ghost text and the Tab key never disagree.
fn common_prefix<'a>(mut candidates: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    let first = candidates.next()?;
    let end = candidates.fold(first.len(), |end, c| {
        let shared = first
            .bytes()
            .zip(c.bytes())
            .take(end)
            .take_while(|(a, b)| a == b)
            .count();
        // Never split a character: the prefix is shown to the user.
        (0..=shared)
            .rev()
            .find(|&n| first.is_char_boundary(n))
            .unwrap_or(0)
    });
    first.get(..end)
}

impl Validator for ScarletHelper {
    /// Enter submits a finished entry and opens a new line on an unfinished
    /// one. Never `Invalid`: a genuine syntax error is the evaluator's to
    /// report, and refusing to submit would trap the user in a line they
    /// cannot get out of.
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

/// The marker the line opens with and the command name typed so far. The
/// marker is carried through so a candidate extends the line the user is
/// actually looking at, rather than rewriting `/he` as `:help`.
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
fn complete_source(line: &str, pos: usize, names: &mut Names) -> (usize, Vec<Pair>) {
    // `import scarlet/…` completes module paths, which are not identifiers:
    // their `/` separators fall outside the word grammar.
    if let Some(path) = import_path_before(line, pos) {
        return (pos - path.len(), pairs(Names::module_paths(path)));
    }
    let Some((start, word)) = word_before(line, pos) else {
        return (pos, Vec::new());
    };
    match word.rsplit_once('.') {
        Some((qualifier, member)) => (
            start + qualifier.len() + 1,
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

/// The identifier ending at `pos`, with any `qualifier.` prefix, and where it
/// starts. `None` where no name can begin — inside a number literal — as
/// against an empty word at a boundary, which completes to everything in
/// scope.
fn word_before(line: &str, pos: usize) -> Option<(usize, &str)> {
    let pos = pos.min(line.len());
    let bytes = line.as_bytes();
    let mut start = pos;
    while start > 0 && {
        let b = bytes[start - 1];
        is_name_continue(b) || b == b'.'
    } {
        start -= 1;
    }
    if start < pos && !bytes.get(start).copied().is_some_and(is_name_start) {
        return None;
    }
    Some((start, &line[start..pos]))
}

/// The partially written module path on an `import` line, if the cursor is in
/// one.
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
mod tests {
    use super::*;

    fn observed(src: &str) -> Names {
        let mut scanner = crate::scanner::new_scanner(src.to_string());
        let program = crate::parser::new_parser(&mut scanner).parse_program();
        let mut names = Names::default();
        names.observe(&program.ast);
        names
    }

    fn candidates(line: &str, names: &mut Names) -> (usize, Vec<String>) {
        let (start, pairs) = complete_source(line, line.len(), names);
        (start, pairs.into_iter().map(|p| p.replacement).collect())
    }

    fn helper(names: Names) -> ScarletHelper {
        ScarletHelper::new(
            crate::term::Palette::for_stdout(),
            Rc::new(RefCell::new(names)),
        )
    }

    fn hint(line: &str, names: Names) -> Option<String> {
        helper(names).completion_hint(line, line.len())
    }

    #[test]
    fn the_ghost_text_is_what_tab_would_insert() {
        assert_eq!(hint("printl", Names::default()).as_deref(), Some("n"));
        assert_eq!(
            hint("http.Del", observed("import scarlet/http")).as_deref(),
            Some("ete")
        );
    }

    /// Several candidates agree only as far as their shared prefix.
    #[test]
    fn the_ghost_text_stops_where_candidates_diverge() {
        let names = observed("fn parse_a() Int { 1 }\nfn parse_b() Int { 2 }\n");
        assert_eq!(hint("pars", names).as_deref(), Some("e_"));
    }

    #[test]
    fn nothing_is_suggested_for_an_empty_word() {
        assert_eq!(hint("", Names::default()), None);
        assert_eq!(hint("1 + ", Names::default()), None);
    }

    #[test]
    fn a_command_suggests_the_rest_of_its_name() {
        assert_eq!(hint(":he", Names::default()).as_deref(), Some("lp "));
        assert_eq!(hint("/he", Names::default()).as_deref(), Some("lp "));
    }

    /// The whole editor path, not just the helper's inner function: a
    /// candidate that never reaches `Completer::complete` never reaches Tab.
    #[test]
    fn the_editor_completes_a_constructor_behind_its_alias() {
        let names = Rc::new(RefCell::new(observed("import scarlet/http")));
        let helper = ScarletHelper::new(crate::term::Palette::for_stdout(), names);
        let history = rustyline::history::MemHistory::new();
        let ctx = Context::new(&history);
        let line = "http.Del";
        let (start, items) = helper.complete(line, line.len(), &ctx).expect("complete");
        assert_eq!(start, "http.".len());
        assert_eq!(
            items.into_iter().map(|p| p.replacement).collect::<Vec<_>>(),
            vec!["Delete".to_string()]
        );
    }

    #[test]
    fn a_bare_prefix_completes_prelude_names() {
        let (start, items) = candidates("printl", &mut Names::default());
        assert_eq!(start, 0);
        assert!(items.contains(&"println".to_string()), "{items:?}");
    }

    #[test]
    fn completion_replaces_only_the_member_after_a_dot() {
        let mut names = observed("import scarlet/string");
        let line = "const x = string.";
        let (start, items) = candidates(line, &mut names);
        assert_eq!(start, line.len());
        assert!(!items.is_empty(), "no exports offered");
    }

    #[test]
    fn a_meta_command_completes_its_own_name() {
        let (start, items) = complete_command((":", "he"));
        assert_eq!(start, 0);
        assert_eq!(
            items.into_iter().map(|p| p.replacement).collect::<Vec<_>>(),
            vec![":help ".to_string()]
        );
    }

    #[test]
    fn an_import_line_completes_module_paths() {
        let (start, items) = candidates("import scarlet/str", &mut Names::default());
        assert_eq!(start, "import ".len());
        assert!(items.iter().any(|i| i == "scarlet/string"), "{items:?}");
    }

    #[test]
    fn a_number_is_not_a_name_to_complete() {
        let (start, items) = candidates("1 + 2", &mut Names::default());
        assert_eq!(start, "1 + 2".len());
        assert!(items.is_empty(), "{items:?}");
    }
}
