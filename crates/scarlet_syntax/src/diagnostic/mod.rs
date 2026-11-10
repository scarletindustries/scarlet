mod editor;
mod printer;

pub use printer::{print_diagnostics, render_diagnostics};

use crate::span::Span;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Hint,
}

/// What kind of diagnostic this is, for code to match on. Consumers match on
/// this instead of searching `message`, so rewording a message can't change
/// what they do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticCode {
    /// The parser or scanner reached the end of the input while it still
    /// needed more.
    UnexpectedEof,
    ParseError,
    TypeError,
    ModuleError,
    UnusedBinding,
    /// A secondary note pointing at another location, like "first defined
    /// here".
    RelatedLocation,
}

#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub span: Span,
    pub severity: Severity,
    pub code: DiagnosticCode,
    pub message: String,
    /// The text `span` points into. `None` only for a diagnostic made before
    /// there was a module to belong to, like the parser's, which its caller
    /// shows against the text it parsed.
    pub source: Option<std::rc::Rc<crate::source::SourceFile>>,
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
