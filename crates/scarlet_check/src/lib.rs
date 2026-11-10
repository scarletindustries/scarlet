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
mod declare;
mod env;
mod finalise;
pub mod index;
mod infer;
mod interface;
mod report;
mod resolve;
mod resolve_pat;
mod resolved;
mod scope;
pub mod typed;
mod types;

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    /// Imported by another module: it may only hold declarations.
    Library,
    /// The file a program is built from. Its code goes in `pub fn main()`.
    Entry,
    /// A script, like a REPL line: statements may run at module scope.
    Script,
}

/// Whether a binding nothing reads is an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UnusedBindings {
    Report,
    Ignore,
}

pub struct CheckInput<'a> {
    /// The module's text, which every diagnostic points into. Its key is
    /// the module's identity.
    pub source: &'a std::rc::Rc<scarlet_syntax::source::SourceFile>,
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
    /// Whether the parser read the module with no errors. A module that holds
    /// something the parser couldn't read then has nothing reporting it, so
    /// the checker does.
    pub parsed_cleanly: bool,
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

/// Check one module. Each pass takes what the ones before it made and gives
/// back what it makes, with what it found to report:
///
/// 1. the environment: the imports and the prelude;
/// 2. the module's declarations, and then its types;
/// 3. resolution, which settles every name and orders the declarations into
///    groups that use each other;
/// 4. inference, a group at a time, after which the types are frozen;
/// 5. the checks that need final types, and the interface.
pub fn check_module<'a>(input: &'a CheckInput<'a>) -> CheckedModule {
    let mut report = report::Report::new(input.source);
    let env = env::Env::new(input, &mut report);
    let declared = declare::declare(&env, input.module, &mut report);
    let defs = types::define_types(&env, &declared, &mut report);
    let (resolved, found) = resolve::resolve(&env, &declared, &defs);
    report.absorb(found);
    let (inferred, found) = infer::infer(&env, &declared, &defs, &resolved);
    report.absorb(found);
    let infer::Inferred {
        engine,
        functions,
        consts,
        statements,
        binder_types,
        schemes,
        hovers,
        refused,
    } = inferred;
    let types = engine.freeze();
    let finished = finalise::Finished {
        env: &env,
        defs: &defs,
        types: &types,
        binders: &resolved.binders,
        binder_types: &binder_types,
        unused: &resolved.unused,
        refused: &refused,
    };
    report.absorb(finalise::finalise(
        &finished,
        &functions,
        &consts,
        &statements,
    ));
    let (interface, found) = interface::interface(&env, &declared, &defs, &schemes, &types);
    report.absorb(found);

    let report::Report {
        diags, defs, uses, ..
    } = report;
    let binders: Vec<typed::Binder> = resolved
        .binders
        .iter()
        .zip(&binder_types)
        .map(|(b, &ty)| typed::Binder {
            name: b.name.clone(),
            ty,
            module_scope: b.module_scope,
        })
        .collect();
    let mut spots: Vec<(scarlet_syntax::span::Span, scarlet_types::engine::Ty)> = binders
        .iter()
        .zip(&resolved.binders)
        .map(|(b, info)| (info.span, b.ty))
        .collect();
    spots.extend(hovers);
    let index = SourceIndex {
        defs,
        uses,
        hovers: spots
            .into_iter()
            .map(|(span, ty)| index::Hover {
                span,
                ty: types.get(ty),
            })
            .collect(),
        doc: input.doc.clone(),
    };
    let clean =
        (input.parsed_cleanly && !scarlet_syntax::diagnostic::has_errors(&diags)).then(|| {
            CleanModule(TypedModule {
                key: interface.key.clone(),
                types,
                binders,
                functions,
                consts,
                statements,
            })
        });
    CheckedModule {
        interface,
        clean,
        diagnostics: diags,
        index,
    }
}
