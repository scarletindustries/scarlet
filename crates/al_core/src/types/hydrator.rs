use std::collections::{HashMap, HashSet};

use smallvec::SmallVec;

use super::{ArenaSlice, InferEngine, StrId, Ty, TypeBody, TypeEnv, pool};
use crate::ast;
use crate::diagnostic::{Diagnostic, DiagnosticCode};
use crate::span::Span;
use crate::token::is_type_name;

/// A successfully-resolved type-name occurrence: the use-site span plus the
/// canonical identity (owning module + interned name) of the `TypeInfo` it
/// resolved to. The hydrator has no `Compiler` handle, so it accumulates these
/// and the `Compiler::hydrate` wrappers drain them
/// ([`Hydrator::take_type_refs`]) to record `Reference`s into the reference
/// graph. Recording-only: it never affects the produced `Ty`.
#[derive(Debug, Clone, Copy)]
pub struct TypeRefHit {
    /// Span of the written type name (the reference occurrence site).
    pub span: Span,
    /// Canonical type name as declared (not the local import alias).
    pub name: StrId,
    /// Owning module path, interned in `InferEngine.str_slices`.
    pub module: ArenaSlice<pool::StrSlices>,
}

/// The Hydrator converts a syntactic type annotation (`ast::TypeIdentifier`)
/// into a `Ty` for the inference engine.
///
/// It owns the policy for type-variable names appearing in annotations:
///   - Lowercase identifiers are type variables; uppercase identifiers are
///     concrete (nominal) types and must resolve in the environment.
///   - The same lowercase name within one annotation maps to a single shared
///     var, so `fn(x a, y a) a` ties all three positions together.
///   - Variables it mints are recorded as *rigid* so that `instantiate` will
///     not replace them with fresh unbound vars while checking the annotated
///     body — the body must be polymorphic in exactly those names.
///   - When `permit_new` is disabled (constructor fields, alias RHS, binding
///     annotations) an unseen lowercase name is an error rather than an
///     implicit fresh var.
///   - When `require_fn_return` is set (type-definition bodies only) a
///     function type must spell out its return type; elsewhere an omitted
///     return hydrates to a fresh var that inference constrains.
#[derive(Debug)]
pub struct Hydrator {
    created: HashMap<String, (Ty, i32)>,
    rigid_ids: HashSet<i32>,
    permit_new: bool,
    require_fn_return: bool,
    type_refs: Vec<TypeRefHit>,
}

/// Result of [`Hydrator::add_type_variable`]: the (possibly pre-existing)
/// type-variable `Ty`, its var id, and whether the name was already seeded.
/// Returning the id here means callers never re-derive it by pattern-matching
/// on `TypeNode::Var` — the invariant lives entirely inside this module.
#[derive(Debug, Clone, Copy)]
pub struct AddedTypeVar {
    pub ty: Ty,
    pub id: i32,
    pub duplicate: bool,
}

impl Default for Hydrator {
    fn default() -> Self {
        Self::new()
    }
}

impl Hydrator {
    pub fn new() -> Self {
        Self {
            created: HashMap::new(),
            rigid_ids: HashSet::new(),
            permit_new: true,
            require_fn_return: false,
            type_refs: Vec::new(),
        }
    }

    pub fn disallow_new_type_variables(&mut self) {
        self.permit_new = false;
    }

    /// Require every function type in the annotation to declare its return
    /// type. Set only for type-definition bodies (constructor fields, alias
    /// RHS): there a fresh return var has no inference context to constrain
    /// it, so it would be generalized into the stored scheme and escape
    /// unconstrained. Binding annotations keep the fresh var — it unifies
    /// with the initializer's type, so it is sound there.
    pub fn require_declared_fn_return(&mut self) {
        self.require_fn_return = true;
    }

    /// Drain the type-name occurrences resolved since the last call. The
    /// `Compiler::hydrate` wrappers call this after every `type_from_ast` to
    /// record the references; a `Hydrator` is reused across the params and
    /// return of one signature, so draining per call keeps occurrences from
    /// being recorded twice.
    pub fn take_type_refs(&mut self) -> Vec<TypeRefHit> {
        std::mem::take(&mut self.type_refs)
    }

    pub fn rigid_ids(&self) -> &HashSet<i32> {
        &self.rigid_ids
    }

    /// Pre-seed a declared type parameter (the `a` in `type Foo(a, b) { .. }`).
    /// On a repeated name the *existing* var is returned with `duplicate: true`
    /// and no fresh var is minted, so both textual occurrences of the parameter
    /// resolve to the same id. Unlike implicit annotation vars these are *not*
    /// added to `rigid_ids`.
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
                    // An omitted return type means "infer it" — but type
                    // definitions (`require_fn_return` set: constructor fields
                    // and alias RHS) have no inference context, so a fresh var
                    // there would escape unconstrained and defeat soundness.
                    None => {
                        if !self.require_fn_return {
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

        // Lowercase identifiers are type variables; uppercase identifiers are
        // concrete types and must be defined in the environment.
        if !is_type_name(name) {
            if !arg_tys.is_empty() {
                return Err(err(
                    span,
                    format!("Type variable '{}' cannot take arguments", name),
                ));
            }
            if !self.permit_new {
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
                // Record the resolved occurrence (canonical name + owning
                // module) so the compiler can target the type's definition in
                // the reference graph. Recording-only: the produced `Ty` below
                // is byte-for-byte unchanged.
                self.type_refs.push(TypeRefHit {
                    span: name_span,
                    name: ti.name,
                    module: ti.module,
                });
                match ti.body {
                    TypeBody::Alias { target } => {
                        Ok(engine.substitute_type_vars(target, ti.type_params, &arg_tys))
                    }
                    // Custom, External, and Unresolved heads all hydrate to a
                    // nominal application carrying the type's registered id —
                    // the identity unification and every semantic lookup use.
                    // The display name is the canonical `ti.name` (under
                    // `import mod.{T as X}` the local string is the alias `X`).
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
