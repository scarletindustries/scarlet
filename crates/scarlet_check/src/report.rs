//! What a pass found: its diagnostics, and the definitions and uses it saw,
//! for the editor. Each pass makes one, fills it, and returns it.

use std::rc::Rc;

use scarlet_syntax::diagnostic::Diagnostic;
use scarlet_syntax::source::SourceFile;
use scarlet_syntax::span::Span;

use crate::error::CheckError;
use crate::index::{Def, Use};

pub(crate) struct Report {
    source: Rc<SourceFile>,
    pub diags: Vec<Diagnostic<CheckError>>,
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

    /// Report `problem` at `span`.
    pub fn add(&mut self, span: Span, problem: CheckError) {
        let mut d = Diagnostic::new(span, problem);
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
