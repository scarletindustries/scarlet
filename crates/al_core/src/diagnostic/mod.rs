mod editor;
mod printer;

pub use editor::{Editor, build_editor_url, detect_editor};
pub use printer::{format_diagnostic_with_lines, print_diagnostics};

use crate::span::{Span, point_span};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Hint,
}

#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub span: Span,
    pub severity: Severity,
    pub message: String,
}

impl Diagnostic {
    pub fn error(span: Span, message: String) -> Diagnostic {
        Diagnostic {
            span,
            severity: Severity::Error,
            message,
        }
    }

    pub fn hint(span: Span, message: String) -> Diagnostic {
        Diagnostic {
            span,
            severity: Severity::Hint,
            message,
        }
    }
}

pub fn error_at(line: i32, column: i32, message: String) -> Diagnostic {
    Diagnostic::error(point_span(line, column), message)
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
