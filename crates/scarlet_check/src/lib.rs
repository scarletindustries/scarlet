//! The type checker. It checks one module, given the interfaces of the
//! modules it imports, and gives back that module's interface, its typed tree
//! and its diagnostics. It is a function: nothing outside the call is read or
//! changed, so a build can cache its result and re-run it whenever the source
//! or an import's interface changes.

#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
    )
)]
#![forbid(unsafe_code)]

mod annot;
mod callgraph;
mod check;
mod decls;
mod expr;
mod finalise;
pub mod index;
mod pattern;
mod scope;
pub mod typed;

use scarlet_syntax::ast;
use scarlet_syntax::diagnostic::Diagnostic;
use scarlet_types::interface::Interface;
use scarlet_types::name::ModuleKey;

pub use index::SourceIndex;
pub use typed::TypedModule;

/// Where the checker reads the interfaces of other modules. It covers every
/// module the checked one can reach, not just the ones it imports directly,
/// because a type from one module can turn up in another's interface.
pub trait Interfaces {
    fn interface(&self, key: &ModuleKey) -> Option<&Interface>;
}

/// What a module is being checked as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Imported by another module: it may only hold declarations.
    Library,
    /// The file a program is built from. Its code goes in `pub fn main()`.
    Entry,
    /// A script, like a REPL line: statements may run at module scope.
    Script,
}

/// Whether a binding nothing reads is an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnusedBindings {
    Report,
    Ignore,
}

pub struct CheckInput<'a> {
    pub key: &'a ModuleKey,
    pub module: &'a ast::BlockExpression,
    /// One entry per `import` in the module, in source order: the module it
    /// resolved to, or `None` when it didn't resolve. The build has already
    /// reported why.
    pub imports: &'a [Option<ModuleKey>],
    pub interfaces: &'a dyn Interfaces,
    pub role: Role,
    pub unused: UnusedBindings,
    /// The module's leading `/** */` comment, as the parser found it.
    pub doc: Option<String>,
    /// Every stdlib module's path, so `int::abs` with no import can say which
    /// import it needs.
    pub stdlib_modules: &'a [scarlet_syntax::module_path::ModulePath],
}

/// A module whose check found no errors. Lowering takes only this, so code is
/// never made from a module the checker rejected.
#[derive(Debug, Clone)]
pub struct CleanModule(TypedModule);

impl CleanModule {
    pub fn module(&self) -> &TypedModule {
        &self.0
    }
}

#[derive(Debug, Clone)]
pub struct CheckedModule {
    pub interface: Interface,
    /// `Some` only when the module has no errors.
    pub clean: Option<CleanModule>,
    pub diagnostics: Vec<Diagnostic>,
    pub index: SourceIndex,
}

pub fn check_module(input: &CheckInput<'_>) -> CheckedModule {
    check::Checker::new(input).run()
}
