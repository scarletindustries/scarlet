mod editor;
mod printer;

pub use printer::{print_diagnostics, render_diagnostics};

use crate::span::Span;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Hint,
}

/// Machine-readable discriminator for a diagnostic. Consumers match on this
/// instead of substring-matching `message`, so rewording a message cannot
/// change downstream behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticCode {
    /// Parser or scanner hit end-of-input while more tokens were required.
    UnexpectedEof,
    ParseError,
    TypeError,
    ModuleError,
    UnusedBinding,
    /// Secondary note pointing at another location (e.g. "first defined here").
    RelatedLocation,
}

#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub span: Span,
    pub severity: Severity,
    pub code: DiagnosticCode,
    pub message: String,
    /// Which module's text `span` points into; `None` means the entry file.
    /// `compile_module_body` stamps this on every diagnostic it raises, so the
    /// printer never renders module A's span against file B's text.
    pub source: Option<crate::module_path::ModuleKey>,
}

impl Diagnostic {
    pub fn error(span: Span, code: DiagnosticCode, message: String) -> Diagnostic {
        Diagnostic {
            span,
            severity: Severity::Error,
            code,
            message,
            source: None,
        }
    }

    pub fn hint(span: Span, code: DiagnosticCode, message: String) -> Diagnostic {
        Diagnostic {
            span,
            severity: Severity::Hint,
            code,
            message,
            source: None,
        }
    }
}

pub fn has_errors(diagnostics: &[Diagnostic]) -> bool {
    diagnostics.iter().any(|d| d.severity == Severity::Error)
}

fn count_errors(diagnostics: &[Diagnostic]) -> usize {
    diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .count()
}
