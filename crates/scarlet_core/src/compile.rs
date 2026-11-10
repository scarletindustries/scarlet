//! Compiling a program: check its entry and everything that reaches, then,
//! when every module is clean, lower them all into one [`Program`].

use std::path::Path;

use scarlet_check::{Role, UnusedBindings};
use scarlet_ir::core_ir::Program;
use scarlet_lower::program::{LinkError, TypeDefs, build_program};
use scarlet_syntax::ast;
use scarlet_syntax::diagnostic::{Diagnostic, DiagnosticCode, has_errors};
use scarlet_syntax::module_path::{ModuleKey, ModulePath};
use scarlet_syntax::source::SourceFile;
use scarlet_syntax::span::Span;
use scarlet_types::def::TypeDef;
use scarlet_types::name::{TypeName, ValueName};

use crate::build::{Build, Entry};

pub struct Options<'a> {
    /// The file the text came from, when it came from one.
    pub file: Option<&'a Path>,
    /// The directory `./` imports are relative to.
    pub base_dir: Option<&'a Path>,
    pub role: Role,
    pub unused: UnusedBindings,
    /// Check the file as this stdlib module instead of as a program.
    pub as_module: Option<ModulePath>,
    /// Whether the program is about to run, so it needs a `main`.
    pub run: bool,
}

impl<'a> Options<'a> {
    pub fn program(base_dir: Option<&'a Path>) -> Self {
        Options {
            file: None,
            base_dir,
            role: Role::Entry,
            unused: UnusedBindings::Report,
            as_module: None,
            run: true,
        }
    }
}

pub struct Compiled {
    pub key: ModuleKey,
    pub diagnostics: Vec<Diagnostic>,
    program: Option<Program>,
}

impl Compiled {
    pub fn success(&self) -> bool {
        !has_errors(&self.diagnostics)
    }

    /// The program, when everything checked and the entry is one that runs.
    pub fn into_runnable(self) -> Option<Program> {
        self.program
    }
}

/// Compile `text`, which parsed to `parsed`, with `build` holding the
/// modules it imports.
pub fn compile(
    build: &mut Build,
    text: &str,
    parsed: crate::build::Parsed,
    opts: &Options<'_>,
) -> Compiled {
    let block = &parsed.ast;
    let (key, role) = match &opts.as_module {
        Some(path) => (ModuleKey::for_stdlib(path), Role::Library),
        None => (ModuleKey::main(), opts.role),
    };
    let source = SourceFile::new(key.clone(), opts.file.map(Path::to_path_buf), text);
    let reach = build.check_entry(Entry {
        source,
        ast: block,
        doc: parsed.doc.clone(),
        parse: parsed.diagnostics.clone(),
        base_dir: opts.base_dir,
        role,
        unused: opts.unused,
    });
    let mut diagnostics = build.diagnostics(&reach);
    let main = (role == Role::Entry)
        .then(|| entry_rules(block, opts.run, &mut diagnostics))
        .flatten();
    if has_errors(&diagnostics) || role == Role::Library {
        return Compiled {
            key,
            diagnostics,
            program: None,
        };
    }
    let clean: Option<Vec<_>> = reach
        .iter()
        .map(|k| build.module(k).and_then(|m| m.checked.clean.as_ref()))
        .collect();
    let Some(clean) = clean else {
        return Compiled {
            key,
            diagnostics,
            program: None,
        };
    };
    let main = main.map(|name| ValueName {
        module: key.clone(),
        name: name.into(),
    });
    let program = match build_program(&clean, &Defs(build), main.as_ref()) {
        Ok(p) => Some(p),
        Err(LinkError::Prelude(msg)) => {
            diagnostics.push(Diagnostic::error(
                Span::DUMMY,
                DiagnosticCode::TypeError,
                msg,
            ));
            None
        }
    };
    Compiled {
        key,
        diagnostics,
        program,
    }
}

/// The entry's `main`, when it has a usable one. A program about to run must
/// have one. The checker reports a `main` that isn't public or takes
/// parameters.
fn entry_rules(
    block: &ast::BlockExpression,
    run: bool,
    out: &mut Vec<Diagnostic>,
) -> Option<&'static str> {
    let main = block.body.iter().find_map(|n| match n {
        ast::Node::Statement(s) => match s.as_ref() {
            ast::Statement::Declaration { decl, public } => match decl.as_ref() {
                ast::Declaration::Function(fd) if fd.identifier.name == "main" => {
                    Some((fd, *public))
                }
                ast::Declaration::Function(_)
                | ast::Declaration::Type(_)
                | ast::Declaration::Const(_) => None,
            },
            ast::Statement::ImportDeclaration(_)
            | ast::Statement::Binding(_)
            | ast::Statement::Backpass(_) => None,
        },
        ast::Node::Expression(_) => None,
    });
    let Some((fd, public)) = main else {
        if run {
            out.push(Diagnostic::error(
                Span::point(block.span.start_line, block.span.start_column),
                DiagnosticCode::TypeError,
                "There's no `main` function. A program starts at `pub fn main()`, so add one"
                    .to_string(),
            ));
        }
        return None;
    };
    (public && fd.params.is_empty()).then_some("main")
}

/// Type definitions, read from the interfaces the build holds.
pub struct Defs<'b>(&'b Build);

impl TypeDefs for Defs<'_> {
    fn type_def(&self, name: &TypeName) -> Option<&TypeDef> {
        self.0
            .module(name.module())?
            .checked
            .interface
            .types
            .get(name.name())
    }
}

/// Parse and compile `text` as a program, in a build of its own. The parse
/// errors come first.
pub fn compile_text(text: &str, base_dir: Option<&Path>) -> Compiled {
    let parsed = crate::build::parse(text);
    let mut build = Build::new(None);
    compile(&mut build, text, parsed, &Options::program(base_dir))
}

/// One public name of a module, for completion.
pub struct Export {
    pub name: String,
    /// A function's parameter names, or a constructor's field labels.
    pub params: Vec<String>,
}

/// The public names of the module `path`, checking it first if `build`
/// hasn't. Types come first, and each name is listed once: a record type
/// names its only constructor after itself.
pub fn exports(build: &mut Build, path: &ModulePath) -> Vec<Export> {
    let key = ModuleKey::of(path);
    if build.module(&key).is_none() {
        let source = format!("import {key}\n");
        let parsed = crate::build::parse(&source);
        if has_errors(&parsed.diagnostics) {
            return Vec::new();
        }
        build.check_entry(Entry {
            source: SourceFile::new(ModuleKey::main(), None, source.as_str()),
            ast: &parsed.ast,
            doc: None,
            parse: Vec::new(),
            base_dir: None,
            role: Role::Script,
            unused: UnusedBindings::Ignore,
        });
    }
    let Some(m) = build.module(&key) else {
        return Vec::new();
    };
    let iface = &m.checked.interface;
    let types = iface.types.values().filter(|t| t.public).map(|t| Export {
        name: t.name.name().to_string(),
        params: Vec::new(),
    });
    let values = iface.values.iter().map(|(name, v)| Export {
        name: name.to_string(),
        params: v
            .kind
            .labels()
            .unwrap_or_default()
            .iter()
            .map(|l| l.to_string())
            .collect(),
    });
    let mut seen = std::collections::HashSet::new();
    types
        .chain(values)
        .filter(|e| seen.insert(e.name.clone()))
        .collect()
}
