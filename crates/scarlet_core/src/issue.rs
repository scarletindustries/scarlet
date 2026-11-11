//! Every problem a build can report, from any layer: the scanner and parser,
//! module loading, the checker, the program's entry, and lowering. Each is a
//! typed value; its text is made by `Display`, only when someone reads it.

use std::fmt;
use std::rc::Rc;

use scarlet_check::CheckError;
use scarlet_lower::program::LinkError;
use scarlet_syntax::diagnostic::{DiagnosticCode, Problem, Severity};
use scarlet_syntax::module_path::{ModuleKey, ResolveError};
use scarlet_syntax::parser::SyntaxError;

use crate::reference::UnusedDef;

/// Why an import's module couldn't be loaded.
#[derive(Debug, Clone)]
pub enum ModuleError {
    Resolve(ResolveError),
    /// The module imports, directly or not, the module that imports it.
    Cycle(ModuleKey),
    /// The module's file couldn't be read.
    Unreadable {
        key: ModuleKey,
        error: Rc<std::io::Error>,
    },
}

impl fmt::Display for ModuleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ModuleError::Resolve(ResolveError::BareName(p)) => write!(
                f,
                "Unknown module '{p}'. Package imports aren't supported yet, so use a relative path like `./{p}`"
            ),
            ModuleError::Resolve(ResolveError::NoBaseDir) => write!(
                f,
                "Relative imports don't work without a file, such as in the REPL"
            ),
            ModuleError::Resolve(e) => e.fmt(f),
            ModuleError::Cycle(key) => write!(
                f,
                "Import cycle at module '{key}'. Modules can't import each other in a loop"
            ),
            ModuleError::Unreadable { key, error } => {
                write!(f, "Couldn't read module '{key}': {error}")
            }
        }
    }
}

/// Anything a build reports.
#[derive(Debug, Clone)]
pub enum Issue {
    Syntax(SyntaxError),
    Module(ModuleError),
    Check(CheckError),
    /// A program about to run has no `main`.
    NoMain,
    Link(LinkError),
    /// A definition nothing reachable uses, which the editor points out.
    Unused(UnusedDef),
}

impl Problem for Issue {
    fn code(&self) -> DiagnosticCode {
        match self {
            Issue::Syntax(e) => e.code(),
            Issue::Module(_) => DiagnosticCode::ModuleError,
            Issue::Check(e) => e.code(),
            Issue::NoMain | Issue::Link(_) => DiagnosticCode::TypeError,
            Issue::Unused(e) => e.code(),
        }
    }

    fn severity(&self) -> Severity {
        match self {
            Issue::Syntax(e) => e.severity(),
            Issue::Check(e) => e.severity(),
            Issue::Unused(e) => e.severity(),
            Issue::Module(_) | Issue::NoMain | Issue::Link(_) => Severity::Error,
        }
    }
}

impl fmt::Display for Issue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Issue::Syntax(e) => e.fmt(f),
            Issue::Module(e) => e.fmt(f),
            Issue::Check(e) => e.fmt(f),
            Issue::NoMain => write!(
                f,
                "There's no `main` function. A program starts at `pub fn main()`, so add one"
            ),
            Issue::Link(e) => e.fmt(f),
            Issue::Unused(e) => e.fmt(f),
        }
    }
}
