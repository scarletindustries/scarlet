//! What a pass found: its diagnostics, and the definitions and uses it saw,
//! for the editor. Each pass makes one, fills it, and returns it.

use std::rc::Rc;

use scarlet_syntax::diagnostic::{Diagnostic, DiagnosticCode};
use scarlet_syntax::source::SourceFile;
use scarlet_syntax::span::Span;

use crate::index::{Def, Use};

pub(crate) struct Report {
    source: Rc<SourceFile>,
    pub diags: Vec<Diagnostic>,
    pub defs: Vec<Def>,
    pub uses: Vec<Use>,
}

impl Report {
    pub fn new(source: &Rc<SourceFile>) -> Self {
        Report {
            source: source.clone(),
            diags: Vec::new(),
            defs: Vec::new(),
            uses: Vec::new(),
        }
    }

    fn push(&mut self, span: Span, code: DiagnosticCode, message: String) {
        let mut d = Diagnostic::error(span, code, message);
        d.source = Some(self.source.clone());
        self.diags.push(d);
    }

    pub fn error(&mut self, span: Span, message: impl Into<String>) {
        self.push(span, DiagnosticCode::TypeError, message.into());
    }

    pub fn module_error(&mut self, span: Span, message: impl Into<String>) {
        self.push(span, DiagnosticCode::ModuleError, message.into());
    }

    pub fn parse_error(&mut self, span: Span, message: impl Into<String>) {
        self.push(span, DiagnosticCode::ParseError, message.into());
    }

    pub fn unused(&mut self, span: Span, name: &str) {
        self.push(
            span,
            DiagnosticCode::UnusedBinding,
            format!("'{name}' is never used. Start its name with '_' if that's on purpose"),
        );
    }

    pub fn note(&mut self, span: Span, message: impl Into<String>) {
        let mut d = Diagnostic::hint(span, DiagnosticCode::RelatedLocation, message.into());
        d.source = Some(self.source.clone());
        self.diags.push(d);
    }

    /// Add what another pass found.
    pub fn absorb(&mut self, other: Report) {
        self.diags.extend(other.diags);
        self.defs.extend(other.defs);
        self.uses.extend(other.uses);
    }
}
