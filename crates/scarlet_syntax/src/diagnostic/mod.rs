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

/// A problem a diagnostic reports: a typed value, one variant per kind of
/// failure, carrying what it is about. Its text is its `Display`, made only
/// when someone reads it.
pub trait Problem: std::fmt::Display {
    fn code(&self) -> DiagnosticCode;

    /// Whether the problem stops a build, or only points something out.
    fn severity(&self) -> Severity {
        Severity::Error
    }
}

/// A problem at a place in the source.
#[derive(Debug, Clone)]
pub struct Diagnostic<P> {
    pub span: Span,
    pub problem: P,
    /// The text `span` points into. `None` only for a diagnostic made before
    /// there was a module to belong to, like the parser's, which its caller
    /// shows against the text it parsed.
    pub source: Option<std::rc::Rc<crate::source::SourceFile>>,
}

impl<P> Diagnostic<P> {
    pub fn new(span: Span, problem: P) -> Self {
        Diagnostic {
            span,
            problem,
            source: None,
        }
    }

    /// The same diagnostic, its problem wrapped in a wider kind.
    pub fn map<Q>(self, f: impl FnOnce(P) -> Q) -> Diagnostic<Q> {
        Diagnostic {
            span: self.span,
            problem: f(self.problem),
            source: self.source,
        }
    }
}

impl<P: Problem> Diagnostic<P> {
    pub fn code(&self) -> DiagnosticCode {
        self.problem.code()
    }

    pub fn severity(&self) -> Severity {
        self.problem.severity()
    }

    /// The problem as a person reads it.
    pub fn message(&self) -> String {
        self.problem.to_string()
    }
}

pub fn has_errors<P: Problem>(diagnostics: &[Diagnostic<P>]) -> bool {
    diagnostics.iter().any(|d| d.severity() == Severity::Error)
}

fn count_errors<P: Problem>(diagnostics: &[Diagnostic<P>]) -> usize {
    diagnostics
        .iter()
        .filter(|d| d.severity() == Severity::Error)
        .count()
}
