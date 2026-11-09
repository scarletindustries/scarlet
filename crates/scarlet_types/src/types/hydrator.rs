use std::collections::{HashMap, HashSet};

use smallvec::SmallVec;

use super::{ArenaSlice, InferEngine, StrId, Ty, TypeBody, TypeInfo, pool};
use scarlet_syntax::ast;
use scarlet_syntax::diagnostic::{Diagnostic, DiagnosticCode};
use scarlet_syntax::span::Span;
use scarlet_syntax::token::is_type_name;

/// One place a type name was written and resolved: the span where it was used,
/// plus the canonical identity of the `TypeInfo` it resolved to. The hydrator
/// has no `Compiler` handle, so it collects these for the `Compiler::hydrate`
/// wrappers to drain into the reference graph. They never change the `Ty` it
/// produces.
#[derive(Debug, Clone, Copy)]
pub struct TypeRefHit {
    /// The span of the written type name, where the reference occurs.
    pub span: Span,
    /// The type's canonical name as declared, not its local import alias.
    pub name: StrId,
    /// The owning module's path, interned in `InferEngine.str_slices`.
    pub module: ArenaSlice<pool::StrSlices>,
    /// The `(name, span)` of the module qualifier, when the type was written
    /// as `module::Type`. The drain records a `Qualifier` occurrence from it,
    /// so the import counts as used and the qualifier resolves in the editor.
    pub qualifier: Option<(StrId, Span)>,
}

/// Where the annotation being hydrated appears. Each position answers two
/// questions. Can a lowercase name we haven't seen make a new type variable?
/// And can a function type leave out its return type?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnnotationContext {
    /// An annotation that opens its own scope for type variables: function
    /// signatures, lambda parameters and top-level const annotations. A new
    /// lowercase name makes a fresh rigid variable, which the body can't solve
    /// to a concrete type. A `fn(..)` with no return type gets a fresh
    /// variable that inference works out from the body.
    Signature,
    /// A `let x: T = ...` annotation. A lowercase name we haven't seen is an
    /// error. A `fn(..)` with no return type is still allowed, because the
    /// fresh variable unifies with (is solved to match) the initialiser's type.
    Binding,
    /// The body of a type definition: constructor fields, or the right-hand
    /// side of an alias. The type parameters are added up front with
    /// [`Hydrator::add_type_variable`], so a lowercase name we haven't seen is
    /// an error. Every function type must declare its return type. There's no
    /// inference here, so a fresh return variable would be generalised into
    /// the stored scheme and escape with nothing constraining it.
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

/// Turns a written type annotation (`ast::TypeIdentifier`) into a `Ty`.
///
/// It decides what type-variable names in annotations mean. Lowercase names are
/// type variables, and uppercase ones are nominal types that must resolve in
/// the environment. The same lowercase name within one annotation is one
/// shared variable, so `fn(x a, y a) a` ties all three positions together. New
/// variables are recorded as *rigid*, so `instantiate` won't replace them while
/// the annotated body is checked.
#[derive(Debug)]
pub struct Hydrator {
    created: HashMap<String, (Ty, i32)>,
    rigid_ids: HashSet<i32>,
    context: AnnotationContext,
    type_refs: Vec<TypeRefHit>,
}

/// What [`Hydrator::add_type_variable`] returns. Handing back the id here saves
/// callers from working it out again by matching on `TypeNode::Var`.
#[derive(Debug, Clone, Copy)]
pub struct AddedTypeVar {
    pub ty: Ty,
    pub id: i32,
    pub duplicate: bool,
}

/// What a type name means where an annotation is read.
pub trait TypeNames {
    /// The type `name` names, reached through the module alias `qualifier`
    /// when the source wrote one (`map::Map`).
    fn type_named(&self, qualifier: Option<&str>, name: &str) -> Option<TypeInfo>;
    /// Every `alias::Name` spelling that would name a type called `name`,
    /// sorted.
    fn qualified_spellings(&self, name: &str) -> Vec<String>;
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

    /// Drain the type names resolved since the last call. One `Hydrator` is
    /// reused across a signature's parameters and return type, so draining
    /// after each one stops a name being recorded twice.
    pub fn take_type_refs(&mut self) -> Vec<TypeRefHit> {
        std::mem::take(&mut self.type_refs)
    }

    pub fn rigid_ids(&self) -> &HashSet<i32> {
        &self.rigid_ids
    }

    /// Add a declared type parameter up front, like the `a` in
    /// `type Foo(a, b) { .. }`. A repeated name returns the existing variable
    /// with `duplicate: true`. Unlike the variables an annotation makes
    /// implicitly, these are *not* added to `rigid_ids`.
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
        types: &dyn TypeNames,
        engine: &mut InferEngine,
    ) -> Result<Ty, Diagnostic> {
        match &t.kind {
            ast::TypeKind::NamedType(nt) => self.named_type(nt, t.span, types, engine),

            ast::TypeKind::TupleType(tt) => {
                let mut elements: SmallVec<[Ty; 4]> = SmallVec::with_capacity(tt.elements.len());
                for el in &tt.elements {
                    elements.push(self.type_from_ast(el, types, engine)?);
                }
                Ok(engine.mk_tuple(&elements))
            }

            ast::TypeKind::FunctionType(ft) => {
                let mut params: SmallVec<[Ty; 4]> = SmallVec::with_capacity(ft.params.len());
                for p in &ft.params {
                    params.push(self.type_from_ast(p, types, engine)?);
                }
                let ret = match &ft.return_type {
                    Some(r) => self.type_from_ast(r, types, engine)?,
                    // A missing return type means "infer it". But nothing is
                    // inferred in a type definition, so a fresh variable there
                    // would escape with nothing constraining it.
                    None => {
                        if !self.context.requires_fn_return() {
                            engine.fresh_var()
                        } else {
                            return Err(err(
                                t.span,
                                "A function type in a type definition must declare its return type, like `fn(Int) Int`"
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
        types: &dyn TypeNames,
        engine: &mut InferEngine,
    ) -> Result<Ty, Diagnostic> {
        let name = &nt.identifier.name;
        let name_span = nt.identifier.span;

        // A qualified name can only be a nominal type in the named module. The
        // type-variable paths below are for bare lowercase names.
        if nt.qualifier.is_none()
            && let Some(&(v, _)) = self.created.get(name)
        {
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
            arg_tys.push(self.type_from_ast(ta, types, engine)?);
        }

        // Lowercase identifiers are type variables.
        if nt.qualifier.is_none() && !is_type_name(name) {
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

        let qualifier = nt.qualifier.as_ref().map(|q| q.name.as_str());
        let display = match qualifier {
            Some(q) => format!("{q}::{name}"),
            None => name.clone(),
        };

        match types.type_named(qualifier, name) {
            Some(ti) => {
                let arity = ti.arity();
                if arg_tys.len() != arity {
                    return Err(arity_error(span, &display, arity, arg_tys.len()));
                }
                // This only records the reference. It doesn't change the `Ty`
                // below.
                self.type_refs.push(TypeRefHit {
                    span: name_span,
                    name: ti.name,
                    module: ti.module,
                    qualifier: nt
                        .qualifier
                        .as_ref()
                        .map(|q| (engine.intern(&q.name), q.span)),
                });
                match ti.body {
                    TypeBody::Alias { target } => {
                        Ok(engine.substitute_type_vars(target, ti.type_params, &arg_tys))
                    }
                    // Custom, External and Unresolved heads all hydrate to a
                    // nominal type carrying the registered id, which is the
                    // identity unification compares. The display name is the
                    // canonical `ti.name`, not the local import alias.
                    TypeBody::Unresolved | TypeBody::Custom { .. } | TypeBody::External => {
                        Ok(engine.mk_con_id(ti.id, ti.name, &arg_tys))
                    }
                }
            }
            None => Err(err(
                name_span,
                match &nt.qualifier {
                    Some(q) => format!(
                        "Unknown type '{display}'. Check that '{}' is imported and exports a type called '{}'.",
                        q.name, name
                    ),
                    None => unknown_bare_type(types, name),
                },
            )),
        }
    }
}

/// The "Unknown type" message for a bare name. When an import does export a
/// type by that name, it points at the qualified spelling: `Map` after
/// `import scarlet/map` is `map::Map`.
fn unknown_bare_type(types: &dyn TypeNames, name: &str) -> String {
    let quoted: Vec<String> = types
        .qualified_spellings(name)
        .iter()
        .map(|s| format!("'{s}'"))
        .collect();
    match quoted.as_slice() {
        [] => format!("Unknown type '{name}'"),
        [one] => format!("Unknown type '{name}'. Did you mean {one}?"),
        many => format!(
            "Unknown type '{name}'. Did you mean one of {}?",
            many.join(", ")
        ),
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
