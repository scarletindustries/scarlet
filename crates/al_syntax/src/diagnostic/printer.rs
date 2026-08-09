use std::collections::HashMap;
use std::fmt::Write as _;

use super::editor::{Editor, build_editor_url, detect_editor};
use super::{Diagnostic, Severity, count_errors};
use crate::module_path::ModuleKey;
use crate::term::Palette;

/// A detected editor plus the canonical path to link to. Only constructed once
/// color is on, an editor was found and the path canonicalized, so holding one
/// is proof the link can be emitted.
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

fn get_source_line<'a>(lines: &'a [&'a str], line_number: i32) -> &'a str {
    if line_number < 1 || line_number as usize > lines.len() {
        return "";
    }
    lines[(line_number - 1) as usize]
}

/// The canonicalized absolute path of `file_path`. A `Some` implies the file
/// exists, which is the proof [`LinkTarget`] requires.
fn canonical_path(file_path: &str) -> Option<String> {
    std::fs::canonicalize(file_path)
        .ok()
        .and_then(|p| p.to_str().map(|s| s.to_string()))
}

/// Lazily-detected editor, shared by every link in one render. Detection forks
/// `ps`, so it runs at most once and only after a path has canonicalized: the
/// REPL's `<repl>` never does, so it never pays the fork.
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

    let color = match d.severity {
        Severity::Error => p.red,
        Severity::Hint => p.cyan,
    };
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

/// Header and location only, for a diagnostic whose source module resolved to
/// no text. A caret into some other file's text would be worse than none.
fn format_diagnostic_header(d: &Diagnostic, file_path: &str, p: &Palette) -> String {
    let color = match d.severity {
        Severity::Error => p.red,
        Severity::Hint => p.cyan,
    };
    let label = severity_label(d.severity);
    let display_line = d.span.start_line + 1;
    let display_col = d.span.start_column + 1;
    format!(
        "{}{color}{label}{}: {} {}at {file_path}:{display_line}:{display_col}{}",
        p.bold, p.reset, d.message, p.dim, p.reset
    )
}

/// A resolved non-entry source: owned text plus the path to display.
struct ResolvedSource {
    file_path: String,
    text: String,
    link: Option<LinkTarget>,
}

/// Print `diagnostics` to stderr. `source`/`file_path` are the ENTRY file, what
/// diagnostics with `source: None` render against. One stamped with a
/// `ModuleKey` renders against `resolve(&key)` instead, or header-only when the
/// resolver has no answer — never a snippet sliced from the wrong file.
pub fn print_diagnostics(
    diagnostics: &[Diagnostic],
    source: &str,
    file_path: &str,
    resolve: &dyn Fn(&ModuleKey) -> Option<(std::path::PathBuf, String)>,
) {
    let output = render_diagnostics(diagnostics, source, file_path, resolve);
    if !output.is_empty() {
        eprintln!("{output}");
    }
}

/// [`print_diagnostics`] as a pure function of its inputs (modulo the palette
/// and editor detection, which read the environment): the full rendered text,
/// empty when there is nothing to print.
pub fn render_diagnostics(
    diagnostics: &[Diagnostic],
    source: &str,
    file_path: &str,
    resolve: &dyn Fn(&ModuleKey) -> Option<(std::path::PathBuf, String)>,
) -> String {
    let palette = Palette::for_stderr();
    let mut editor = EditorProbe::new(palette.enabled());
    let entry = SourceView {
        file_path,
        lines: source.lines().collect(),
        link: editor.link_target(file_path),
    };

    // Resolve each distinct foreign source once; `None` is cached too so an
    // unresolvable module isn't re-probed per diagnostic.
    let mut resolved: HashMap<&ModuleKey, Option<ResolvedSource>> = HashMap::new();
    for key in diagnostics.iter().filter_map(|d| d.source.as_ref()) {
        resolved.entry(key).or_insert_with(|| {
            resolve(key).map(|(path, text)| {
                let file_path = path.display().to_string();
                let link = editor.link_target(&file_path);
                ResolvedSource {
                    file_path,
                    text,
                    link,
                }
            })
        });
    }

    let mut output: Vec<String> = Vec::new();
    for d in diagnostics {
        let rendered = match &d.source {
            None => format_diagnostic_with_lines(d, &entry, &palette),
            Some(key) => match resolved.get(key) {
                Some(Some(r)) => {
                    let view = SourceView {
                        file_path: &r.file_path,
                        lines: r.text.lines().collect(),
                        // Reuse the cached link rather than re-canonicalizing
                        // the same path once per diagnostic.
                        link: r.link.as_ref().map(|l| LinkTarget {
                            editor: l.editor,
                            abs_path: l.abs_path.clone(),
                        }),
                    };
                    format_diagnostic_with_lines(d, &view, &palette)
                }
                _ => format_diagnostic_header(d, key.as_str(), &palette),
            },
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
