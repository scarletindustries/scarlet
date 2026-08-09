use std::collections::{HashMap, HashSet};

use smallvec::SmallVec;

use super::{ArenaSlice, InferEngine, StrId, Ty, TypeBody, TypeEnv, pool};
use al_syntax::ast;
use al_syntax::diagnostic::{Diagnostic, DiagnosticCode};
use al_syntax::span::Span;
use al_syntax::token::is_type_name;

/// A resolved type-name occurrence: the use-site span plus the canonical
/// identity of the `TypeInfo` it resolved to. The hydrator has no `Compiler`
/// handle, so it accumulates these for the `Compiler::hydrate` wrappers to
/// drain into the reference graph. It never affects the produced `Ty`.
#[derive(Debug, Clone, Copy)]
pub struct TypeRefHit {
    /// Span of the written type name (the reference occurrence site).
    pub span: Span,
    /// Canonical type name as declared (not the local import alias).
    pub name: StrId,
    /// Owning module path, interned in `InferEngine.str_slices`.
    pub module: ArenaSlice<pool::StrSlices>,
}

/// Where the annotation being hydrated appears. Each position answers two
/// questions: may an unseen lowercase name mint a fresh type variable, and may
/// a function type omit its return type?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnnotationContext {
    /// An annotation that opens its own type-variable scope: fn signatures,
    /// lambda parameters, toplevel const annotations. New lowercase names mint
    /// fresh rigid vars, and a bare `fn(..)` return gets a fresh var that
    /// inference constrains against the body.
    Signature,
    /// A `let x: T = ...` annotation. An unseen lowercase name is an error. A
    /// bare `fn(..)` return is still allowed: the fresh var unifies with the
    /// initializer's type.
    Binding,
    /// A type-definition body: constructor fields or an alias RHS. Type
    /// parameters are pre-seeded via [`Hydrator::add_type_variable`], so an
    /// unseen lowercase name is an error. Every function type must declare its
    /// return type: there is no inference context, so a fresh return var would
    /// be generalized into the stored scheme and escape unconstrained.
    TypeDefinition,
}

impl AnnotationContext {
    fn permits_new_type_variables(self) -> bool {
        matches!(self, Self::Signature)
    }

    fn requires_fn_return(self) -> bool {
        matches!(self, Self::TypeDefinition)
    }
}

/// Converts a syntactic type annotation (`ast::TypeIdentifier`) into a `Ty`.
///
/// It owns the policy for type-variable names in annotations. Lowercase
/// identifiers are type variables and uppercase ones are nominal types that
/// must resolve in the environment. The same lowercase name within one
/// annotation maps to one shared var, so `fn(x a, y a) a` ties all three
/// positions together. Minted vars are recorded as *rigid* so `instantiate`
/// will not replace them while checking the annotated body.
#[derive(Debug)]
pub struct Hydrator {
    created: HashMap<String, (Ty, i32)>,
    rigid_ids: HashSet<i32>,
    context: AnnotationContext,
    type_refs: Vec<TypeRefHit>,
}

/// Result of [`Hydrator::add_type_variable`]. Returning the id here keeps
/// callers from re-deriving it by matching on `TypeNode::Var`.
#[derive(Debug, Clone, Copy)]
pub struct AddedTypeVar {
    pub ty: Ty,
    pub id: i32,
    pub duplicate: bool,
}

impl Hydrator {
    pub fn new(context: AnnotationContext) -> Self {
        Self {
            created: HashMap::new(),
            rigid_ids: HashSet::new(),
            context,
            type_refs: Vec::new(),
        }
    }

    /// Drain the type-name occurrences resolved since the last call. A
    /// `Hydrator` is reused across one signature's params and return, so
    /// draining per call keeps occurrences from being recorded twice.
    pub fn take_type_refs(&mut self) -> Vec<TypeRefHit> {
        std::mem::take(&mut self.type_refs)
    }

    pub fn rigid_ids(&self) -> &HashSet<i32> {
        &self.rigid_ids
    }

    /// Pre-seed a declared type parameter (the `a` in `type Foo(a, b) { .. }`).
    /// A repeated name returns the existing var with `duplicate: true`. Unlike
    /// implicit annotation vars these are *not* added to `rigid_ids`.
    pub fn add_type_variable(&mut self, name: &str, engine: &mut InferEngine) -> AddedTypeVar {
        if let Some(&(ty, id)) = self.created.get(name) {
            return AddedTypeVar {
                ty,
                id,
                duplicate: true,
            };
        }
        let (ty, id) = engine.fresh_generic_var();
        let name_id = engine.intern(name);
        engine.name_var(ty, name_id);
        self.created.insert(name.to_string(), (ty, id));
        AddedTypeVar {
            ty,
            id,
            duplicate: false,
        }
    }

    pub fn type_from_ast(
        &mut self,
        t: &ast::TypeIdentifier,
        env: &TypeEnv,
        engine: &mut InferEngine,
    ) -> Result<Ty, Diagnostic> {
        match &t.kind {
            ast::TypeKind::NamedType(nt) => self.named_type(nt, t.span, env, engine),

            ast::TypeKind::TupleType(tt) => {
                let mut elements: SmallVec<[Ty; 4]> = SmallVec::with_capacity(tt.elements.len());
                for el in &tt.elements {
                    elements.push(self.type_from_ast(el, env, engine)?);
                }
                Ok(engine.mk_tuple(&elements))
            }

            ast::TypeKind::FunctionType(ft) => {
                let mut params: SmallVec<[Ty; 4]> = SmallVec::with_capacity(ft.params.len());
                for p in &ft.params {
                    params.push(self.type_from_ast(p, env, engine)?);
                }
                let ret = match &ft.return_type {
                    Some(r) => self.type_from_ast(r, env, engine)?,
                    // An omitted return type means "infer it", but a type
                    // definition has no inference context, so a fresh var
                    // there would escape unconstrained.
                    None => {
                        if !self.context.requires_fn_return() {
                            engine.fresh_var()
                        } else {
                            return Err(err(
                                t.span,
                                "Function type in a type definition must declare a return type"
                                    .to_string(),
                            ));
                        }
                    }
                };
                Ok(engine.mk_fun(&params, ret))
            }
        }
    }

    fn named_type(
        &mut self,
        nt: &ast::NamedType,
        span: Span,
        env: &TypeEnv,
        engine: &mut InferEngine,
    ) -> Result<Ty, Diagnostic> {
        let name = &nt.identifier.name;
        let name_span = nt.identifier.span;

        if let Some(&(v, _)) = self.created.get(name) {
            if !nt.type_args.is_empty() {
                return Err(err(
                    span,
                    format!("Type variable '{}' cannot take arguments", name),
                ));
            }
            return Ok(v);
        }

        let mut arg_tys: SmallVec<[Ty; 4]> = SmallVec::with_capacity(nt.type_args.len());
        for ta in &nt.type_args {
            arg_tys.push(self.type_from_ast(ta, env, engine)?);
        }

        // Lowercase identifiers are type variables.
        if !is_type_name(name) {
            if !arg_tys.is_empty() {
                return Err(err(
                    span,
                    format!("Type variable '{}' cannot take arguments", name),
                ));
            }
            if !self.context.permits_new_type_variables() {
                return Err(err(name_span, format!("Unknown type variable '{}'", name)));
            }
            let (t, id) = engine.fresh_generic_var();
            self.rigid_ids.insert(id);
            let name_id = engine.intern(name.as_str());
            engine.name_var(t, name_id);
            self.created.insert(name.clone(), (t, id));
            return Ok(t);
        }

        match env.lookup_type_info(name) {
            Some(ti) => {
                let arity = ti.arity();
                if arg_tys.len() != arity {
                    return Err(arity_error(span, name, arity, arg_tys.len()));
                }
                // Recording-only: the produced `Ty` below is unchanged.
                self.type_refs.push(TypeRefHit {
                    span: name_span,
                    name: ti.name,
                    module: ti.module,
                });
                match ti.body {
                    TypeBody::Alias { target } => {
                        Ok(engine.substitute_type_vars(target, ti.type_params, &arg_tys))
                    }
                    // Custom, External and Unresolved heads all hydrate to a
                    // nominal application carrying the registered id, which is
                    // the identity unification uses. The display name is the
                    // canonical `ti.name`, not the local import alias.
                    _ => Ok(engine.mk_con_id(ti.id, ti.name, &arg_tys)),
                }
            }
            None => Err(err(name_span, format!("Unknown type '{}'", name))),
        }
    }
}

fn err(span: Span, message: String) -> Diagnostic {
    Diagnostic::error(span, DiagnosticCode::TypeError, message)
}

fn arity_error(span: Span, name: &str, expected: usize, given: usize) -> Diagnostic {
    err(
        span,
        format!(
            "Type '{}' expects {} type argument{}, got {}",
            name,
            expected,
            if expected == 1 { "" } else { "s" },
            given
        ),
    )
}
