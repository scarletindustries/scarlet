//! Type annotations, read into finished types. Nothing here needs inference:
//! an annotation names types, and a lowercase name in it is a variable.

use scarlet_syntax::ast;
use scarlet_syntax::span::Span;
use scarlet_syntax::token::is_type_name;
use scarlet_types::def::{TypeBody, TypeDef};
use scarlet_types::name::{Name, TypeName};
use scarlet_types::ty::{Type, TypeVar};

use crate::env::Env;
use crate::error::CheckError;
use crate::index::{EntityKind, Site, Use, UseKind};
use crate::report::Report;

/// Where an annotation is written, which decides what a new lowercase name in
/// it means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AnnotContext {
    /// A function's signature: a new lowercase name is a generic the caller
    /// picks.
    Signature,
    /// A binding's type: no type variable is in scope.
    Binding,
    /// A type's body: its parameters are in scope, nothing else is, and every
    /// function type states its return type.
    TypeDefinition,
}

/// The variables one signature or one type's body names, in the order it
/// names them, numbered from 0.
pub(crate) struct AnnotVars {
    ctx: AnnotContext,
    named: Vec<TypeVar>,
    next: u32,
}

impl AnnotVars {
    pub fn new(ctx: AnnotContext) -> Self {
        AnnotVars {
            ctx,
            named: Vec::new(),
            next: 0,
        }
    }

    /// A type's parameters, in scope for its body.
    pub fn params(params: &[ast::Identifier], report: &mut Report) -> Self {
        let mut vars = AnnotVars::new(AnnotContext::TypeDefinition);
        for p in params {
            if vars
                .named
                .iter()
                .any(|v| v.name.as_deref() == Some(p.name.as_str()))
            {
                report.add(
                    p.span,
                    CheckError::DuplicateTypeParameter(p.name.as_str().into()),
                );
                continue;
            }
            vars.add(p.name.as_str().into());
        }
        vars
    }

    /// An annotation scope with `params` already in it.
    pub fn with_params(ctx: AnnotContext, params: &[TypeVar]) -> Self {
        AnnotVars {
            ctx,
            named: params.to_vec(),
            next: params.iter().map(|p| p.id + 1).max().unwrap_or(0),
        }
    }

    fn add(&mut self, name: Name) -> Type {
        let v = TypeVar {
            id: self.next,
            name: Some(name),
            constraint: None,
        };
        self.next += 1;
        self.named.push(v.clone());
        Type::Var(v)
    }

    /// A stand-in for an annotation that names nothing. Inference gives it a
    /// fresh variable, so checking carries on.
    fn placeholder(&mut self) -> Type {
        let v = TypeVar {
            id: self.next,
            name: None,
            constraint: None,
        };
        self.next += 1;
        Type::Var(v)
    }

    /// The variables the annotations named, which stand for whatever the
    /// caller picks.
    pub fn named(&self) -> &[TypeVar] {
        &self.named
    }
}

/// A type name an annotation can mean.
pub(crate) struct NamedType<'d> {
    pub name: TypeName,
    pub arity: usize,
    pub span: Span,
    /// Its definition, when it is an alias.
    pub alias: Option<&'d TypeDef>,
}

/// What the module's own type names mean, as far as they are known.
pub(crate) trait OwnTypes {
    fn own_type(&self, name: &str) -> Option<NamedType<'_>>;
}

/// What a written type means.
pub(crate) struct Annotations<'e, 'a, O: OwnTypes> {
    pub env: &'e Env<'a>,
    pub own: &'e O,
    /// The module-level definition the annotation is part of.
    pub owner: Option<Site>,
}

impl<O: OwnTypes> Annotations<'_, '_, O> {
    pub fn read(&self, vars: &mut AnnotVars, t: &ast::TypeIdentifier, report: &mut Report) -> Type {
        match self.type_of(vars, t, report) {
            Ok(ty) => ty,
            Err((span, e)) => {
                report.add(span, e);
                vars.placeholder()
            }
        }
    }

    pub fn read_opt(
        &self,
        vars: &mut AnnotVars,
        t: Option<&ast::TypeIdentifier>,
        report: &mut Report,
    ) -> Option<Type> {
        t.map(|t| self.read(vars, t, report))
    }

    fn type_of(
        &self,
        vars: &mut AnnotVars,
        t: &ast::TypeIdentifier,
        report: &mut Report,
    ) -> Result<Type, (Span, CheckError)> {
        match &t.kind {
            ast::TypeKind::NamedType(nt) => self.named(vars, nt, t.span, report),
            ast::TypeKind::TupleType(tt) => {
                let mut elems = Vec::with_capacity(tt.elements.len());
                for el in &tt.elements {
                    elems.push(self.type_of(vars, el, report)?);
                }
                Ok(Type::tuple(elems))
            }
            ast::TypeKind::FunctionType(ft) => {
                let mut params = Vec::with_capacity(ft.params.len());
                for p in &ft.params {
                    params.push(self.type_of(vars, p, report)?);
                }
                let ret = match &ft.return_type {
                    Some(r) => self.type_of(vars, r, report)?,
                    // A missing return type means "infer it", but nothing in
                    // a type's body is inferred.
                    None if vars.ctx == AnnotContext::TypeDefinition => {
                        return Err((t.span, CheckError::FunctionTypeNeedsReturn));
                    }
                    None => vars.placeholder(),
                };
                Ok(Type::function(params, ret))
            }
        }
    }

    fn named(
        &self,
        vars: &mut AnnotVars,
        nt: &ast::NamedType,
        span: Span,
        report: &mut Report,
    ) -> Result<Type, (Span, CheckError)> {
        let name = nt.identifier.name.as_str();
        let name_span = nt.identifier.span;

        if nt.qualifier.is_none() && !is_type_name(name) {
            if !nt.type_args.is_empty() {
                return Err((span, CheckError::TypeVariableWithArguments(name.into())));
            }
            if let Some(v) = vars.named.iter().find(|v| v.name.as_deref() == Some(name)) {
                return Ok(Type::Var(v.clone()));
            }
            if vars.ctx != AnnotContext::Signature {
                return Err((name_span, CheckError::UnknownTypeVariable(name.into())));
            }
            return Ok(vars.add(name.into()));
        }

        let mut args = Vec::with_capacity(nt.type_args.len());
        for ta in &nt.type_args {
            args.push(self.type_of(vars, ta, report)?);
        }
        let found = match &nt.qualifier {
            Some(q) => self.qualified(q, name),
            None => self.bare(name),
        };
        let Some(found) = found else {
            let e = match &nt.qualifier {
                Some(q) => CheckError::UnknownQualifiedType {
                    qualifier: q.name.as_str().into(),
                    name: name.into(),
                },
                None => CheckError::UnknownType {
                    name: name.into(),
                    exported_by: self.exporters(name),
                },
            };
            return Err((name_span, e));
        };
        if args.len() != found.arity {
            return Err((
                span,
                CheckError::TypeArity {
                    qualifier: nt.qualifier.as_ref().map(|q| q.name.as_str().into()),
                    name: name.into(),
                    expected: found.arity,
                    given: args.len(),
                },
            ));
        }
        report.uses.push(Use {
            span: name_span,
            kind: if nt.qualifier.is_some() {
                UseKind::Qualified
            } else {
                UseKind::Unqualified
            },
            target: Site {
                module: found.name.module().clone(),
                span: found.span,
                entity: EntityKind::Type,
            },
            owner: self.owner.clone(),
        });
        if let Some(q) = &nt.qualifier
            && let Some(imp) = self.env.imports.get(&q.name)
        {
            report.uses.push(Use {
                span: q.span,
                kind: UseKind::Qualifier,
                target: imp.alias.clone(),
                owner: self.owner.clone(),
            });
        }
        Ok(match found.alias.map(|d| (d, &d.body)) {
            Some((def, TypeBody::Alias(target))) => def.instantiate(target, &args),
            Some(_) | None => Type::named(found.name, args),
        })
    }

    /// A bare type name: one of the module's own, then the prelude's.
    fn bare(&self, name: &str) -> Option<NamedType<'_>> {
        if let Some(t) = self.own.own_type(name) {
            return Some(t);
        }
        Some(foreign(self.env.prelude?.public_type(name)?))
    }

    fn qualified(&self, q: &ast::Identifier, name: &str) -> Option<NamedType<'_>> {
        let imp = self.env.imports.get(&q.name)?;
        Some(foreign(self.env.interface_of(&imp.key)?.public_type(name)?))
    }

    /// The imports that export a public type `name`, by qualifier, in the
    /// order a suggestion lists them.
    fn exporters(&self, name: &str) -> Vec<Name> {
        let mut out: Vec<Name> = self
            .env
            .imports
            .iter()
            .filter(|(_, imp)| {
                self.env
                    .interface_of(&imp.key)
                    .is_some_and(|i| i.public_type(name).is_some())
            })
            .map(|(q, _)| q.clone())
            .collect();
        out.sort_by_key(|q| format!("{q}::{name}"));
        out
    }
}

fn foreign(def: &TypeDef) -> NamedType<'_> {
    NamedType {
        name: def.name.clone(),
        arity: def.params.len(),
        span: def.span,
        alias: matches!(def.body, TypeBody::Alias(_)).then_some(def),
    }
}
