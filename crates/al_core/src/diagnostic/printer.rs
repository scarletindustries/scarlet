use std::fmt::Write as _;

use super::editor::{Editor, build_editor_url, detect_editor};
use super::{Diagnostic, Severity, count_errors};
use crate::term::Palette;

const LINK_START: &str = "\x1b]8;;";
const LINK_END: &str = "\x07";

/// Environment-derived rendering decisions, computed once per `print_diagnostics`
/// call so per-diagnostic formatting is a pure function of its inputs.
struct RenderCtx {
    editor: Option<Editor>,
    abs_path: String,
    palette: Palette,
    links: bool,
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

fn real_path(file_path: &str) -> String {
    std::fs::canonicalize(file_path)
        .ok()
        .and_then(|p| p.to_str().map(|s| s.to_string()))
        .unwrap_or_else(|| file_path.to_string())
}

fn format_diagnostic_with_lines(
    d: &Diagnostic,
    lines: &[&str],
    file_path: &str,
    ctx: &RenderCtx,
) -> String {
    let mut result = String::new();

    let p = &ctx.palette;
    let color = match d.severity {
        Severity::Error => p.red,
        Severity::Hint => p.cyan,
    };
    let label = severity_label(d.severity);

    let display_line = d.span.start_line + 1;
    let display_col = d.span.start_column + 1;
    let location = format!("{file_path}:{display_line}:{display_col}");
    let loc = match (ctx.links, ctx.editor) {
        (true, Some(ed)) => {
            let url = build_editor_url(ed, &ctx.abs_path, display_line, display_col);
            format!("{LINK_START}{url}{LINK_END}{location}{LINK_START}{LINK_END}")
        }
        _ => location,
    };

    let _ = writeln!(
        result,
        "{}{color}{label}{}: {} {}at {loc}{}",
        p.bold, p.reset, d.message, p.dim, p.reset
    );

    let line_num_width = display_line.to_string().len();
    let padding = " ".repeat(line_num_width);

    let source_line = get_source_line(lines, display_line);
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

    let caret_len =
        if d.span.end_line == d.span.start_line && d.span.end_column > d.span.start_column {
            (d.span.end_column - d.span.start_column) as usize
        } else {
            source_line.len().saturating_sub(col).max(1)
        };
    let carets = "^".repeat(caret_len);
    let _ = write!(
        result,
        "{padding}    {caret_padding}{color}{carets}{}",
        p.reset
    );

    result
}

pub fn print_diagnostics(diagnostics: &[Diagnostic], source: &str, file_path: &str) {
    let lines: Vec<&str> = source.lines().collect();

    let palette = Palette::for_stderr();
    let editor = detect_editor();
    let abs_path = real_path(file_path);
    let links = palette.enabled() && editor.is_some() && std::path::Path::new(&abs_path).exists();
    let ctx = RenderCtx {
        editor,
        abs_path,
        palette,
        links,
    };

    let mut output: Vec<String> = Vec::new();
    for d in diagnostics {
        output.push(format_diagnostic_with_lines(d, &lines, file_path, &ctx));
    }

    let error_count = count_errors(diagnostics);

    if error_count > 0 {
        let noun = if error_count == 1 { "error" } else { "errors" };
        let p = &ctx.palette;
        output.push(format!(
            "Found {}{}{error_count} {noun}{}",
            p.bold, p.red, p.reset
        ));
    }

    if !output.is_empty() {
        eprintln!("{}", output.join("\n"));
    }
}
