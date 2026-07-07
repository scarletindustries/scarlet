mod editor;
mod printer;

pub use printer::print_diagnostics;

use crate::span::Span;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Hint,
}

/// Machine-readable discriminator for a diagnostic. Consumers that need to
/// react to a *class* of diagnostic (e.g. the REPL detecting incomplete input)
/// match on this instead of substring-matching `message`, so rewording a
/// message can never silently change downstream behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticCode {
    /// Parser or scanner hit end-of-input while more tokens were required.
    UnexpectedEof,
    ParseError,
    TypeError,
    ModuleError,
    UnusedBinding,
    Other,
}

#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub span: Span,
    pub severity: Severity,
    pub code: DiagnosticCode,
    pub message: String,
}

impl Diagnostic {
    pub fn error(span: Span, code: DiagnosticCode, message: String) -> Diagnostic {
        Diagnostic {
            span,
            severity: Severity::Error,
            code,
            message,
        }
    }

    pub fn hint(span: Span, code: DiagnosticCode, message: String) -> Diagnostic {
        Diagnostic {
            span,
            severity: Severity::Hint,
            code,
            message,
        }
    }
}

pub fn has_errors(diagnostics: &[Diagnostic]) -> bool {
    diagnostics.iter().any(|d| d.severity == Severity::Error)
}

pub fn count_errors(diagnostics: &[Diagnostic]) -> usize {
    diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .count()
}
