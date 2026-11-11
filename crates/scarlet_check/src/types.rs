//! A module's type definitions, each made whole before anything reads it:
//! first every custom and external type's name, then the aliases in the
//! order they use each other, then every custom type's variants and the
//! constructor each one gives.

use std::collections::HashMap;

use indexmap::IndexMap;
use petgraph::Directed;
use petgraph::algo::tarjan_scc;
use petgraph::stable_graph::{NodeIndex, StableGraph};
use scarlet_syntax::ast;
use scarlet_syntax::span::Span;
use scarlet_types::def::{FieldDef, TypeBody, TypeDef, VariantDef};
use scarlet_types::name::{CtorName, Name, TypeName};
use scarlet_types::ty::{Scheme, Type, TypeVar};

use crate::annot::{AnnotContext, AnnotVars, Annotations, NamedType, OwnTypes};
use crate::declare::{Declared, TypeDecl};
use crate::env::Env;
use crate::error::CheckError;
use crate::index::{Def, DefKind, EntityKind};
use crate::report::Report;

/// One of the module's own constructors.
pub(crate) struct Ctor {
    pub ctor: CtorName,
    pub labels: Vec<Name>,
    pub scheme: Scheme,
    pub span: Span,
    pub public: bool,
    pub doc: Option<String>,
}

pub(crate) struct TypeDefs {
    /// In declaration order. An alias in a cycle has none.
    pub types: IndexMap<Name, TypeDef>,
    pub ctors: IndexMap<Name, Ctor>,
}

impl OwnTypes for TypeDefs {
    fn own_type(&self, name: &str) -> Option<NamedType<'_>> {
        let def = self.types.get(name)?;
        Some(NamedType {
            name: def.name.clone(),
            arity: def.params.len(),
            span: def.span,
            alias: matches!(def.body, TypeBody::Alias(_)).then_some(def),
        })
    }
}

/// A type's name and parameters, known before its body.
struct Head {
    name: TypeName,
    params: Vec<TypeVar>,
    span: Span,
    /// The finished definition of an alias, once its turn has come.
    alias: Option<TypeDef>,
}

/// The heads known so far, which is what an annotation inside a type's body
/// can name.
struct Heads(IndexMap<Name, Head>);

impl OwnTypes for Heads {
    fn own_type(&self, name: &str) -> Option<NamedType<'_>> {
        let h = self.0.get(name)?;
        Some(NamedType {
            name: h.name.clone(),
            arity: h.params.len(),
            span: h.span,
            alias: h.alias.as_ref(),
        })
    }
}

pub(crate) fn define_types(
    env: &Env<'_>,
    declared: &Declared<'_>,
    report: &mut Report,
) -> TypeDefs {
    let type_name = |name: &str| TypeName::new(env.key.clone(), name.into());
    let mut heads = Heads(IndexMap::new());
    for t in &declared.types {
        let td = t.decl;
        if matches!(td.body, ast::TypeBody::Alias(_)) {
            continue;
        }
        let params = AnnotVars::params(&td.type_params, report).named().to_vec();
        heads.0.insert(
            td.identifier.name.as_str().into(),
            Head {
                name: type_name(&td.identifier.name),
                params,
                span: td.identifier.span,
                alias: None,
            },
        );
        define(env, report, td, t.public, DefKind::Type);
    }

    for (td, public) in alias_order(&declared.types, report) {
        let mut vars = AnnotVars::params(&td.type_params, report);
        let ast::TypeBody::Alias(rhs) = &td.body else {
            continue;
        };
        let target = annotations(env, &heads, td).read(&mut vars, rhs, report);
        let def = TypeDef {
            name: type_name(&td.identifier.name),
            params: vars.named().to_vec(),
            body: TypeBody::Alias(target),
            public,
            doc: td.doc.clone(),
            span: td.identifier.span,
        };
        heads.0.insert(
            td.identifier.name.as_str().into(),
            Head {
                name: def.name.clone(),
                params: def.params.clone(),
                span: def.span,
                alias: Some(def),
            },
        );
        define(env, report, td, public, DefKind::Type);
    }

    let mut types = IndexMap::new();
    let mut ctors = IndexMap::new();
    for t in &declared.types {
        let td = t.decl;
        let Some(head) = heads.0.get(td.identifier.name.as_str()) else {
            continue;
        };
        let def = match &td.body {
            ast::TypeBody::Alias(_) => match &head.alias {
                Some(def) => def.clone(),
                None => continue,
            },
            ast::TypeBody::External => TypeDef {
                name: head.name.clone(),
                params: head.params.clone(),
                body: TypeBody::External,
                public: t.public,
                doc: td.doc.clone(),
                span: td.identifier.span,
            },
            ast::TypeBody::Variants { ctors: vs, opaque } => {
                let constructors_public = t.public && !*opaque;
                let variants = variants(env, &heads, head, t, vs, constructors_public, report);
                let def = TypeDef {
                    name: head.name.clone(),
                    params: head.params.clone(),
                    body: TypeBody::Variants {
                        variants,
                        constructors_public,
                    },
                    public: t.public,
                    doc: td.doc.clone(),
                    span: td.identifier.span,
                };
                constructors(env, &def, constructors_public, &mut ctors, report);
                def
            }
        };
        types.insert(td.identifier.name.as_str().into(), def);
    }
    TypeDefs { types, ctors }
}

fn annotations<'e, 'a>(
    env: &'e Env<'a>,
    heads: &'e Heads,
    td: &ast::TypeDeclaration,
) -> Annotations<'e, 'a, Heads> {
    Annotations {
        env,
        own: heads,
        owner: Some(env.site(td.identifier.span, EntityKind::Type)),
    }
}

fn define(
    env: &Env<'_>,
    report: &mut Report,
    td: &ast::TypeDeclaration,
    public: bool,
    kind: DefKind,
) {
    report.defs.push(Def {
        site: env.site(td.identifier.span, kind.entity()),
        name: td.identifier.name.clone(),
        doc: td.doc.clone(),
        public,
        kind,
    });
}

/// The aliases that aren't in a cycle, each after the aliases it names. An
/// alias in a cycle is reported and left out.
fn alias_order<'a>(
    types: &[TypeDecl<'a>],
    report: &mut Report,
) -> Vec<(&'a ast::TypeDeclaration, bool)> {
    let aliases: Vec<(&ast::TypeDeclaration, &ast::TypeIdentifier, bool)> = types
        .iter()
        .filter_map(|t| match &t.decl.body {
            ast::TypeBody::Alias(rhs) => Some((t.decl, rhs, t.public)),
            ast::TypeBody::Variants { .. } | ast::TypeBody::External => None,
        })
        .collect();
    let mut graph: StableGraph<(), (), Directed> = StableGraph::new();
    let mut idx_of: HashMap<&str, NodeIndex> = HashMap::new();
    for (td, _, _) in &aliases {
        idx_of.insert(td.identifier.name.as_str(), graph.add_node(()));
    }
    for (td, rhs, _) in &aliases {
        let from = idx_of[td.identifier.name.as_str()];
        let mut deps = Vec::new();
        type_deps(rhs, &mut deps);
        for dep in deps {
            if let Some(&to) = idx_of.get(dep) {
                graph.add_edge(from, to, ());
            }
        }
    }
    let mut out = Vec::new();
    for component in tarjan_scc(&graph) {
        let cyclic = component.len() > 1 || graph.find_edge(component[0], component[0]).is_some();
        if cyclic {
            for n in &component {
                let (td, _, _) = aliases[n.index()];
                report.add(
                    td.identifier.span,
                    CheckError::RecursiveAlias(td.identifier.name.as_str().into()),
                );
            }
            continue;
        }
        let (td, _, public) = aliases[component[0].index()];
        out.push((td, public));
    }
    out
}

fn variants(
    env: &Env<'_>,
    heads: &Heads,
    head: &Head,
    t: &TypeDecl<'_>,
    ctors: &[ast::Constructor],
    constructors_public: bool,
    report: &mut Report,
) -> Vec<VariantDef> {
    let read = annotations(env, heads, t.decl);
    let mut out = Vec::new();
    for (ctor, _) in ctors.iter().zip(&t.dup_ctors).filter(|&(_, dup)| !dup) {
        let mut fields = Vec::with_capacity(ctor.fields.len());
        for f in &ctor.fields {
            let mut vars = AnnotVars::with_params(AnnotContext::TypeDefinition, &head.params);
            let ty = read.read(&mut vars, &f.typ, report);
            report.defs.push(Def {
                site: env.site(f.label.span, EntityKind::Field),
                name: f.label.name.clone(),
                doc: None,
                public: constructors_public,
                kind: DefKind::Field,
            });
            fields.push(FieldDef {
                label: f.label.name.as_str().into(),
                ty,
                span: f.label.span,
            });
        }
        out.push(VariantDef {
            name: ctor.identifier.name.as_str().into(),
            fields,
            doc: ctor.doc.clone(),
            span: ctor.identifier.span,
        });
    }
    out
}

/// A constructor value for each of `def`'s variants.
fn constructors(
    env: &Env<'_>,
    def: &TypeDef,
    public: bool,
    out: &mut IndexMap<Name, Ctor>,
    report: &mut Report,
) {
    let Some(variants) = def.variants() else {
        return;
    };
    let result = Type::named(
        def.name.clone(),
        def.params.iter().cloned().map(Type::Var).collect(),
    );
    let type_site = env.site(def.span, EntityKind::Type);
    for (i, v) in variants.iter().enumerate() {
        let ty = if v.fields.is_empty() {
            result.clone()
        } else {
            Type::function(
                v.fields.iter().map(|f| f.ty.clone()).collect(),
                result.clone(),
            )
        };
        let labels: Vec<Name> = v.fields.iter().map(|f| f.label.clone()).collect();
        report.defs.push(Def {
            site: env.site(v.span, EntityKind::Constructor),
            name: v.name.to_string(),
            doc: v.doc.clone(),
            public,
            kind: DefKind::Constructor {
                of: type_site.clone(),
                fields: labels.iter().map(|l| l.to_string()).collect(),
            },
        });
        out.insert(
            v.name.clone(),
            Ctor {
                ctor: CtorName {
                    type_name: def.name.clone(),
                    variant: i as u16,
                },
                labels,
                scheme: Scheme {
                    vars: def.params.clone(),
                    ty,
                },
                span: v.span,
                public,
                doc: v.doc.clone(),
            },
        );
    }
}

/// The bare type names an alias's target uses. A qualified name lives in
/// another module, so it is never an edge to a local alias.
fn type_deps<'a>(t: &'a ast::TypeIdentifier, out: &mut Vec<&'a str>) {
    match &t.kind {
        ast::TypeKind::NamedType(nt) => {
            if nt.qualifier.is_none() {
                out.push(nt.identifier.name.as_str());
            }
            for a in &nt.type_args {
                type_deps(a, out);
            }
        }
        ast::TypeKind::FunctionType(ft) => {
            for p in &ft.params {
                type_deps(p, out);
            }
            if let Some(r) = &ft.return_type {
                type_deps(r, out);
            }
        }
        ast::TypeKind::TupleType(tt) => {
            for e in &tt.elements {
                type_deps(e, out);
            }
        }
    }
}
