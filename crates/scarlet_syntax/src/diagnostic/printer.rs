use std::fmt::Write as _;

use super::editor::{Editor, build_editor_url, detect_editor};
use super::{Diagnostic, Severity, count_errors};
use crate::term::Palette;

/// A detected editor and the canonical path to link to. It's only built once
/// colour is on, an editor was found and the path canonicalised, so holding one
/// proves the link can be written.
struct LinkTarget {
    editor: Editor,
    abs_path: String,
}

/// The text a diagnostic's span points into, plus the path to print for it.
/// A snippet is only ever sliced from a view whose identity matches the
/// diagnostic's `source`, so a span from module A cannot index file B's text.
struct SourceView<'a> {
    file_path: &'a str,
    lines: Vec<&'a str>,
    link: Option<LinkTarget>,
}

fn severity_label(severity: Severity) -> &'static str {
    match severity {
        Severity::Error => "error",
        Severity::Hint => "hint",
    }
}

fn severity_color(severity: Severity, p: &Palette) -> &'static str {
    match severity {
        Severity::Error => p.red,
        Severity::Hint => p.cyan,
    }
}

fn get_source_line<'a>(lines: &'a [&'a str], line_number: i32) -> &'a str {
    if line_number < 1 || line_number as usize > lines.len() {
        return "";
    }
    lines[(line_number - 1) as usize]
}

/// The canonical absolute path of `file_path`. A `Some` means the file exists,
/// which is the proof [`LinkTarget`] needs.
fn canonical_path(file_path: &str) -> Option<String> {
    std::fs::canonicalize(file_path)
        .ok()
        .and_then(|p| p.to_str().map(|s| s.to_string()))
}

/// The editor, detected lazily and shared by every link in one render.
/// Detection forks `ps`, so it runs at most once, and only after a path has
/// canonicalised. The REPL's `<repl>` never does, so the REPL never pays for
/// the fork.
struct EditorProbe {
    enabled: bool,
    detected: Option<Option<Editor>>,
}

impl EditorProbe {
    fn new(enabled: bool) -> Self {
        EditorProbe {
            enabled,
            detected: None,
        }
    }

    fn link_target(&mut self, file_path: &str) -> Option<LinkTarget> {
        if !self.enabled {
            return None;
        }
        let abs_path = canonical_path(file_path)?;
        let editor = (*self.detected.get_or_insert_with(detect_editor))?;
        Some(LinkTarget { editor, abs_path })
    }
}

fn format_diagnostic_with_lines(d: &Diagnostic, view: &SourceView<'_>, p: &Palette) -> String {
    let mut result = String::new();

    let color = severity_color(d.severity, p);
    let label = severity_label(d.severity);

    let file_path = view.file_path;
    let lines = &view.lines;
    let display_line = d.span.start_line + 1;
    let source_line = get_source_line(lines, display_line);
    // Span columns count bytes, but printed locations and editor line:col URLs
    // are read as characters, so re-measure the prefix in chars.
    let display_col = source_line
        .get(..d.span.start_column as usize)
        .map(|s| s.chars().count())
        .unwrap_or(d.span.start_column as usize) as i32
        + 1;
    let location = format!("{file_path}:{display_line}:{display_col}");
    let loc = match &view.link {
        Some(link) => {
            let url = build_editor_url(link.editor, &link.abs_path, display_line, display_col);
            p.hyperlink(&url, &location)
        }
        None => location,
    };

    let _ = writeln!(
        result,
        "{}{color}{label}{}: {} {}at {loc}{}",
        p.bold, p.reset, d.message, p.dim, p.reset
    );

    let line_num_width = display_line.to_string().len();
    let padding = " ".repeat(line_num_width);

    let _ = writeln!(
        result,
        "{}{display_line}  |{} {source_line}",
        p.blue, p.reset
    );

    let col = d.span.start_column as usize;
    let mut caret_padding = String::new();
    for ch in source_line.get(..col).unwrap_or(source_line).chars() {
        caret_padding.push(if ch == '\t' { '\t' } else { ' ' });
    }

    // In chars, not bytes: `caret_padding` above is one char per source char,
    // so byte-counting overshoots on multibyte lines.
    let caret_len =
        if d.span.end_line == d.span.start_line && d.span.end_column > d.span.start_column {
            source_line
                .get(col..d.span.end_column as usize)
                .map(|s| s.chars().count())
                .unwrap_or(1)
        } else {
            source_line
                .get(col..)
                .map(|s| s.chars().count())
                .unwrap_or(1)
                .max(1)
        };
    let carets = "^".repeat(caret_len);
    let _ = write!(
        result,
        "{padding}    {caret_padding}{color}{carets}{}",
        p.reset
    );

    result
}

/// Prints `diagnostics` to stderr, each against the text it points into.
/// `source` and `file_path` are the text a diagnostic with no source of its
/// own was made from.
pub fn print_diagnostics(diagnostics: &[Diagnostic], source: &str, file_path: &str) {
    let output = render_diagnostics(diagnostics, source, file_path);
    if !output.is_empty() {
        eprintln!("{output}");
    }
}

/// [`print_diagnostics`], returning the text instead. Only the palette and
/// editor detection read the environment; everything else comes from the
/// inputs.
pub fn render_diagnostics(diagnostics: &[Diagnostic], source: &str, file_path: &str) -> String {
    let palette = Palette::for_stderr();
    let mut editor = EditorProbe::new(palette.enabled());
    let mut output: Vec<String> = Vec::new();
    for d in diagnostics {
        let rendered = match &d.source {
            None => {
                let view = SourceView {
                    file_path,
                    lines: source.lines().collect(),
                    link: editor.link_target(file_path),
                };
                format_diagnostic_with_lines(d, &view, &palette)
            }
            Some(src) => {
                let name = src.display_name();
                let view = SourceView {
                    file_path: &name,
                    lines: src.text.lines().collect(),
                    link: editor.link_target(&name),
                };
                format_diagnostic_with_lines(d, &view, &palette)
            }
        };
        output.push(rendered);
    }

    let error_count = count_errors(diagnostics);

    if error_count > 0 {
        let noun = if error_count == 1 { "error" } else { "errors" };
        let p = &palette;
        output.push(format!(
            "Found {}{}{error_count} {noun}{}",
            p.bold, p.red, p.reset
        ));
    }

    output.join("\n")
}
