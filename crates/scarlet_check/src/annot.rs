//! Type annotations, turned into inference types.

use std::collections::HashMap;

use scarlet_syntax::ast;
use scarlet_syntax::span::Span;
use scarlet_syntax::token::is_type_name;
use scarlet_types::def::{TypeBody, TypeDef};
use scarlet_types::engine::Ty;
use scarlet_types::name::{Name, TypeName};

use crate::check::Checker;
use crate::index::{EntityKind, Site, UseKind};

/// Where an annotation is written, which decides what a new lowercase name
/// in it means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AnnotContext {
    /// A function's signature: a new lowercase name is a generic the caller
    /// picks.
    Signature,
    /// A binding's type: every type variable must already be in scope.
    Binding,
    /// A type's body: the parameters are in scope, nothing else is, and every
    /// function type states its return type.
    TypeDefinition,
}

/// The type variables one annotation, or one signature, has named so far.
pub(crate) struct Annot {
    pub ctx: AnnotContext,
    pub vars: HashMap<Name, Ty>,
}

impl Annot {
    pub fn new(ctx: AnnotContext) -> Self {
        Annot {
            ctx,
            vars: HashMap::new(),
        }
    }
}

/// A named type as an annotation sees it.
struct NamedRef {
    name: TypeName,
    arity: usize,
    /// The alias's definition, when it is one.
    alias: Option<TypeDef>,
    span: Span,
}

impl Checker<'_> {
    /// The type `t` names, or a fresh variable after reporting why it names
    /// none, so checking carries on.
    pub fn hydrate(&mut self, a: &mut Annot, t: &ast::TypeIdentifier) -> Ty {
        match self.type_from_ast(a, t) {
            Ok(ty) => ty,
            Err((span, msg)) => {
                self.error(span, msg);
                self.e.fresh()
            }
        }
    }

    pub fn hydrate_opt(&mut self, a: &mut Annot, t: Option<&ast::TypeIdentifier>) -> Ty {
        match t {
            Some(t) => self.hydrate(a, t),
            None => self.e.fresh(),
        }
    }

    fn type_from_ast(&mut self, a: &mut Annot, t: &ast::TypeIdentifier) -> Result<Ty, (Span, String)> {
        match &t.kind {
            ast::TypeKind::NamedType(nt) => self.named_type(a, nt, t.span),
            ast::TypeKind::TupleType(tt) => {
                let mut elems = Vec::with_capacity(tt.elements.len());
                for el in &tt.elements {
                    elems.push(self.type_from_ast(a, el)?);
                }
                Ok(self.e.tuple(elems))
            }
            ast::TypeKind::FunctionType(ft) => {
                let mut params = Vec::with_capacity(ft.params.len());
                for p in &ft.params {
                    params.push(self.type_from_ast(a, p)?);
                }
                let ret = match &ft.return_type {
                    Some(r) => self.type_from_ast(a, r)?,
                    // A missing return type means "infer it", but nothing in a
                    // type's body is inferred, so a fresh variable there would
                    // escape with nothing to solve it.
                    None if a.ctx == AnnotContext::TypeDefinition => {
                        return Err((
                            t.span,
                            "A function type in a type definition must declare its return type, like `fn(Int) Int`"
                                .to_string(),
                        ));
                    }
                    None => self.e.fresh(),
                };
                Ok(self.e.fun(params, ret))
            }
        }
    }

    fn named_type(&mut self, a: &mut Annot, nt: &ast::NamedType, span: Span) -> Result<Ty, (Span, String)> {
        let name = nt.identifier.name.as_str();
        let name_span = nt.identifier.span;

        if nt.qualifier.is_none() && !is_type_name(name) {
            if !nt.type_args.is_empty() {
                return Err((span, format!("Type variable '{name}' cannot take arguments")));
            }
            if let Some(&v) = a.vars.get(name) {
                return Ok(v);
            }
            if a.ctx != AnnotContext::Signature {
                return Err((name_span, format!("Unknown type variable '{name}'")));
            }
            let v = self.e.rigid(name.into());
            a.vars.insert(name.into(), v);
            return Ok(v);
        }

        let mut args = Vec::with_capacity(nt.type_args.len());
        for ta in &nt.type_args {
            args.push(self.type_from_ast(a, ta)?);
        }

        let display = match &nt.qualifier {
            Some(q) => format!("{}::{name}", q.name),
            None => name.to_string(),
        };
        let Some(found) = self.lookup_type(nt.qualifier.as_ref().map(|q| q.name.as_str()), name) else {
            let msg = match &nt.qualifier {
                Some(q) => format!(
                    "Unknown type '{display}'. Check that '{}' is imported and exports a type called '{name}'.",
                    q.name
                ),
                None => self.unknown_bare_type(name),
            };
            return Err((name_span, msg));
        };
        if args.len() != found.arity {
            let expected = found.arity;
            return Err((
                span,
                format!(
                    "Type '{display}' expects {expected} type argument{}, got {}",
                    if expected == 1 { "" } else { "s" },
                    args.len()
                ),
            ));
        }
        let kind = if nt.qualifier.is_some() {
            UseKind::Qualified
        } else {
            UseKind::Unqualified
        };
        let target = Site {
            module: found.name.module().clone(),
            span: found.span,
            entity: EntityKind::Type,
        };
        self.use_site(name_span, kind, target);
        if let Some(q) = &nt.qualifier {
            self.qualifier_use(q);
        }
        match found.alias {
            Some(def) => {
                let TypeBody::Alias(target) = &def.body else {
                    return Ok(self.e.con(found.name, args));
                };
                let mut subst: Vec<_> = def.params.iter().cloned().zip(args).collect();
                Ok(self.e.import(target, &mut subst))
            }
            None => Ok(self.e.con(found.name, args)),
        }
    }

    /// The type `name` means here: one of the module's own, then the
    /// prelude's. With a qualifier, the imported module's public one.
    fn lookup_type(&self, qualifier: Option<&str>, name: &str) -> Option<NamedRef> {
        match qualifier {
            Some(q) => {
                let key = self.qualifiers.get(q)?;
                let def = self.interface_of(key)?.public_type(name)?;
                Some(named_ref(def))
            }
            None => {
                if let Some(own) = self.types.get(name) {
                    return Some(NamedRef {
                        name: own.name.clone(),
                        arity: own.arity,
                        alias: own
                            .def
                            .as_ref()
                            .filter(|d| matches!(d.body, TypeBody::Alias(_)))
                            .cloned(),
                        span: own.span,
                    });
                }
                let def = self.prelude?.public_type(name)?;
                Some(named_ref(def))
            }
        }
    }

    fn unknown_bare_type(&self, name: &str) -> String {
        let mut quoted: Vec<String> = self
            .qualifiers
            .iter()
            .filter(|(_, key)| {
                self.interface_of(key)
                    .is_some_and(|i| i.public_type(name).is_some())
            })
            .map(|(q, _)| format!("'{q}::{name}'"))
            .collect();
        quoted.sort();
        match quoted.as_slice() {
            [] => format!("Unknown type '{name}'"),
            [one] => format!("Unknown type '{name}'. Did you mean {one}?"),
            many => format!("Unknown type '{name}'. Did you mean one of {}?", many.join(", ")),
        }
    }

    /// The `module` in `module::member`: a use of the import.
    pub fn qualifier_use(&mut self, q: &ast::Identifier) {
        let Some(alias) = self
            .defs
            .iter()
            .find(|d| d.site.entity == EntityKind::ModuleAlias && d.name == q.name)
            .map(|d| d.site.clone())
        else {
            return;
        };
        self.use_site(q.span, UseKind::Qualifier, alias);
    }
}

fn named_ref(def: &TypeDef) -> NamedRef {
    NamedRef {
        name: def.name.clone(),
        arity: def.params.len(),
        alias: matches!(def.body, TypeBody::Alias(_)).then(|| def.clone()),
        span: def.span,
    }
}
