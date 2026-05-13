mod editor;
mod printer;

pub use editor::{Editor, build_editor_url, detect_editor};
pub use printer::{format_diagnostic_with_lines, print_diagnostics};

use crate::span::{Span, point_span};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    Hint,
}

#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub span: Span,
    pub severity: Severity,
    pub message: String,
}

pub fn error_at(line: i32, column: i32, message: String) -> Diagnostic {
    Diagnostic {
        span: point_span(line, column),
        severity: Severity::Error,
        message,
    }
}

pub fn warning_at(line: i32, column: i32, message: String) -> Diagnostic {
    Diagnostic {
        span: point_span(line, column),
        severity: Severity::Warning,
        message,
    }
}

pub fn has_errors(diagnostics: &[Diagnostic]) -> bool {
    diagnostics.iter().any(|d| d.severity == Severity::Error)
}

pub fn count_errors(diagnostics: &[Diagnostic]) -> i32 {
    diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .count() as i32
}

pub fn count_warnings(diagnostics: &[Diagnostic]) -> i32 {
    diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Warning)
        .count() as i32
}
