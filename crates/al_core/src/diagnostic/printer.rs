use std::fmt::Write as _;

use super::editor::{build_editor_url, detect_editor};
use super::{Diagnostic, Severity, count_errors};

pub const COLOR_RESET: &str = "\x1b[0m";
pub const COLOR_BOLD: &str = "\x1b[1m";
pub const COLOR_DIM: &str = "\x1b[2m";
pub const COLOR_RED: &str = "\x1b[31m";
pub const COLOR_CYAN: &str = "\x1b[36m";
pub const COLOR_BLUE: &str = "\x1b[34m";
pub const LINK_START: &str = "\x1b]8;;";
pub const LINK_END: &str = "\x07";

fn severity_color(severity: Severity) -> &'static str {
    match severity {
        Severity::Error => COLOR_RED,
        Severity::Hint => COLOR_CYAN,
    }
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

pub fn format_diagnostic_with_lines(d: &Diagnostic, lines: &[&str], file_path: &str) -> String {
    let mut result = String::new();

    let color = severity_color(d.severity);
    let label = severity_label(d.severity);

    let abs_path = real_path(file_path);
    let display_line = d.span.start_line + 1;
    let display_col = d.span.start_column + 1;
    let location = format!("{file_path}:{display_line}:{display_col}");
    let editor = detect_editor();
    let link_url = build_editor_url(editor, &abs_path, display_line, display_col);

    let _ = writeln!(
        result,
        "{COLOR_BOLD}{color}{label}{COLOR_RESET}: {} {COLOR_DIM}at {LINK_START}{link_url}{LINK_END}{location}{LINK_START}{LINK_END}{COLOR_RESET}",
        d.message
    );

    let line_num_width = display_line.to_string().len();
    let padding = " ".repeat(line_num_width);

    let source_line = get_source_line(lines, display_line);
    let _ = writeln!(
        result,
        "{COLOR_BLUE}{display_line}  |{COLOR_RESET} {source_line}"
    );

    let mut caret_padding = String::new();
    let source_bytes = source_line.as_bytes();
    for i in 0..d.span.start_column {
        if (i as usize) < source_bytes.len() && source_bytes[i as usize] == b'\t' {
            caret_padding.push('\t');
        } else {
            caret_padding.push(' ');
        }
    }

    let caret_len = if d.span.end_column > d.span.start_column {
        (d.span.end_column - d.span.start_column) as usize
    } else {
        1
    };
    let carets = "^".repeat(caret_len);
    let _ = write!(
        result,
        "{padding}    {caret_padding}{color}{carets}{COLOR_RESET}"
    );

    result
}

pub fn print_diagnostics(diagnostics: &[Diagnostic], source: &str, file_path: &str) {
    let lines: Vec<&str> = source.lines().collect();

    let mut output: Vec<String> = Vec::new();
    for d in diagnostics {
        output.push(format_diagnostic_with_lines(d, &lines, file_path));
    }

    let error_count = count_errors(diagnostics);

    if error_count > 0 {
        let noun = if error_count == 1 { "error" } else { "errors" };
        output.push(format!(
            "Found {COLOR_BOLD}{COLOR_RED}{error_count} {noun}{COLOR_RESET}"
        ));
    }

    if !output.is_empty() {
        println!("{}", output.join("\n"));
    }
}
