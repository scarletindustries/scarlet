//! Every name a module declares, before anything about them is checked: its
//! values, its types, and the statements that are neither. Duplicates,
//! reserved names, attributes and `main`'s rules are reported here.

use std::collections::HashMap;

use scarlet_ir::intrinsic::Intrinsic;
use scarlet_syntax::ast;
use scarlet_syntax::span::Span;
use scarlet_types::interface::ValueKind;
use scarlet_types::name::Name;

use crate::Role;
use crate::env::Env;
use crate::index::{Def, DefKind, EntityKind};
use crate::report::Report;

/// A module-level value's position in [`Declared::values`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct DeclId(pub u32);

pub(crate) enum DeclBody<'a> {
    Fn(&'a ast::FunctionDeclaration, &'a ast::Expression),
    Builtin(&'a ast::FunctionDeclaration, Intrinsic),
    Const(&'a ast::ConstBinding),
}

pub(crate) struct Decl<'a> {
    pub name: Name,
    pub span: Span,
    pub public: bool,
    pub doc: Option<String>,
    pub body: DeclBody<'a>,
}

impl Decl<'_> {
    /// What the interface says this value is.
    pub fn kind(&self) -> ValueKind {
        match &self.body {
            DeclBody::Fn(fd, _) => ValueKind::Fn { params: params(fd) },
            DeclBody::Builtin(fd, intrinsic) => ValueKind::Builtin {
                intrinsic: *intrinsic,
                params: params(fd),
            },
            DeclBody::Const(_) => ValueKind::Const,
        }
    }

    pub fn entity(&self) -> EntityKind {
        match self.body {
            DeclBody::Fn(..) | DeclBody::Builtin(..) => EntityKind::Function,
            DeclBody::Const(_) => EntityKind::Constant,
        }
    }
}

fn params(fd: &ast::FunctionDeclaration) -> Vec<Name> {
    fd.params
        .iter()
        .map(|p| p.identifier.name.as_str().into())
        .collect()
}

/// A `type` declaration that survived the duplicate check.
pub(crate) struct TypeDecl<'a> {
    pub decl: &'a ast::TypeDeclaration,
    pub public: bool,
    /// One per constructor: whether its name was taken already, which drops
    /// it.
    pub dup_ctors: Vec<bool>,
}

pub(crate) struct Declared<'a> {
    pub values: Vec<Decl<'a>>,
    by_name: HashMap<Name, DeclId>,
    pub types: Vec<TypeDecl<'a>>,
    /// The nodes that aren't declarations or imports, in source order.
    pub statements: Vec<&'a ast::Node>,
}

impl<'a> Declared<'a> {
    pub fn get(&self, id: DeclId) -> &Decl<'a> {
        &self.values[id.0 as usize]
    }

    pub fn find(&self, name: &str) -> Option<DeclId> {
        self.by_name.get(name).copied()
    }

    pub fn ids(&self) -> impl Iterator<Item = DeclId> + use<'_, 'a> {
        (0..self.values.len() as u32).map(DeclId)
    }
}

pub(crate) fn declare<'a>(
    env: &Env<'_>,
    block: &'a ast::BlockExpression,
    report: &mut Report,
) -> Declared<'a> {
    let in_prelude = env.is_prelude();
    let in_stdlib = env.in_stdlib();
    let mut seen_types: HashMap<&str, Span> = HashMap::new();
    let mut seen_values: HashMap<&str, Span> = HashMap::new();
    let mut out = Declared {
        values: Vec::new(),
        by_name: HashMap::new(),
        types: Vec::new(),
        statements: Vec::new(),
    };
    for node in &block.body {
        let ast::Node::Statement(stmt) = node else {
            non_declaration(env, node, report);
            out.statements.push(node);
            continue;
        };
        match stmt.as_ref() {
            ast::Statement::Declaration { decl, public } => match decl.as_ref() {
                ast::Declaration::Type(td) => {
                    validate_attributes(&td.attributes, in_stdlib, false, report);
                    let dup = duplicate(&mut seen_types, &td.identifier, report);
                    reserved(env, &td.identifier, in_prelude, report);
                    let mut dup_ctors = Vec::new();
                    if let ast::TypeBody::Variants { ctors, .. } = &td.body {
                        for ctor in ctors {
                            reserved(env, &ctor.identifier, in_prelude, report);
                            dup_ctors.push(duplicate(&mut seen_values, &ctor.identifier, report));
                        }
                    }
                    if !dup {
                        out.types.push(TypeDecl {
                            decl: td,
                            public: *public,
                            dup_ctors,
                        });
                    }
                }
                ast::Declaration::Function(fd) => {
                    validate_attributes(&fd.attributes, in_stdlib, true, report);
                    if fd.identifier.name == "main" && env.input.role == Role::Entry {
                        main_rules(fd, *public, report);
                    }
                    if duplicate(&mut seen_values, &fd.identifier, report) {
                        continue;
                    }
                    let body = match &fd.body {
                        ast::FnBody::Vm(key) => match Intrinsic::from_key(&key.name) {
                            Some(i) => DeclBody::Builtin(fd, i),
                            None => {
                                report
                                    .error(key.span, format!("unknown @vm builtin '{}'", key.name));
                                continue;
                            }
                        },
                        ast::FnBody::Block(body) => DeclBody::Fn(fd, body),
                    };
                    out.push(Decl {
                        name: fd.identifier.name.as_str().into(),
                        span: fd.identifier.span,
                        public: *public,
                        doc: fd.doc.clone(),
                        body,
                    });
                }
                ast::Declaration::Const(cb) => {
                    if !duplicate(&mut seen_values, &cb.identifier, report) {
                        out.push(Decl {
                            name: cb.identifier.name.as_str().into(),
                            span: cb.identifier.span,
                            public: *public,
                            doc: cb.doc.clone(),
                            body: DeclBody::Const(cb),
                        });
                    }
                }
            },
            ast::Statement::ImportDeclaration(_) => {}
            ast::Statement::Binding(_) | ast::Statement::Backpass(_) => {
                non_declaration(env, node, report);
                out.statements.push(node);
            }
        }
    }
    for d in &out.values {
        let kind = match &d.body {
            DeclBody::Fn(fd, _) | DeclBody::Builtin(fd, _) => DefKind::Function {
                params: fd
                    .params
                    .iter()
                    .map(|p| p.identifier.name.clone())
                    .collect(),
            },
            DeclBody::Const(_) => DefKind::Constant,
        };
        report.defs.push(Def {
            site: env.site(d.span, kind.entity()),
            name: d.name.to_string(),
            doc: d.doc.clone(),
            public: d.public,
            kind,
        });
    }
    out
}

impl<'a> Declared<'a> {
    fn push(&mut self, d: Decl<'a>) {
        self.by_name
            .insert(d.name.clone(), DeclId(self.values.len() as u32));
        self.values.push(d);
    }
}

/// A statement at module scope where the role doesn't allow one. It is still
/// checked, so the rest of the file is reported too.
fn non_declaration(env: &Env<'_>, node: &ast::Node, report: &mut Report) {
    match env.input.role {
        Role::Library => report.error(node.span(), "Modules may only contain declarations at the top level"),
        Role::Entry => report.error(
            node.span(),
            "Statements aren't allowed at module scope. Put a program's code inside `pub fn main()`",
        ),
        Role::Script => {}
    }
}

/// A program starts at `main`, so it must be public and take nothing.
fn main_rules(fd: &ast::FunctionDeclaration, public: bool, report: &mut Report) {
    if !public {
        report.error(
            fd.identifier.span,
            "`main` must be public, because a program starts at `pub fn main()`",
        );
    }
    if !fd.params.is_empty() {
        report.error(
            fd.identifier.span,
            "`main` takes no parameters. Read the program's arguments with `os.argv()` instead",
        );
    }
}

fn validate_attributes(
    attrs: &[ast::Attribute],
    in_stdlib: bool,
    on_fn: bool,
    report: &mut Report,
) {
    for a in attrs {
        match a.name.name.as_str() {
            "vm" => {
                if !in_stdlib {
                    report.error(a.span, "'@vm' is only allowed in the standard library");
                }
                if !on_fn {
                    report.error(a.span, "'@vm' may only be used on functions");
                }
            }
            other => report.error(a.span, format!("Unknown attribute '@{other}'")),
        }
    }
}

/// Report a name declared twice. The second declaration is dropped, so this
/// says whether it was one.
fn duplicate<'n>(
    seen: &mut HashMap<&'n str, Span>,
    id: &'n ast::Identifier,
    report: &mut Report,
) -> bool {
    match seen.insert(id.name.as_str(), id.span) {
        Some(prev) => {
            report.error(id.span, format!("'{}' is already defined", id.name));
            report.note(prev, "first definition was here");
            true
        }
        None => false,
    }
}

fn reserved(env: &Env<'_>, id: &ast::Identifier, in_prelude: bool, report: &mut Report) {
    if !in_prelude && env.reserved.contains(id.name.as_str()) {
        report.error(
            id.span,
            format!(
                "'{}' is defined in the prelude and cannot be redefined",
                id.name
            ),
        );
    }
}
