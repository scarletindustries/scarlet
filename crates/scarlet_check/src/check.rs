//! The checker's state for one module, and the order its passes run in.

use std::collections::{HashMap, HashSet};

use indexmap::IndexMap;
use scarlet_syntax::ast;
use scarlet_syntax::diagnostic::{Diagnostic, DiagnosticCode};
use scarlet_syntax::module_path::{self, ModulePath};
use scarlet_syntax::span::Span;
use scarlet_types::def::TypeDef;
use scarlet_types::engine::{Engine, LocalScheme, Ty, UnifyError};
use scarlet_types::interface::{Interface, ValueDef, ValueKind};
use scarlet_types::known::{Known, prelude_key};
use scarlet_types::name::{ModuleKey, Name, TypeName};
use scarlet_types::ty::Scheme;

use crate::index::{Def, DefKind, EntityKind, Hover, Site, SourceIndex, Use, UseKind};
use crate::scope::Scope;
use crate::typed::{self, BinderId};
use crate::{CheckInput, CheckedModule, CleanModule, UnusedBindings};

/// A local binder while the module is being checked.
pub(crate) struct BinderState {
    pub name: Name,
    pub scheme: LocalScheme,
    pub span: Span,
    pub module_scope: bool,
    /// A `_name = e` binding with no written type. If `e` turns out to be
    /// `Nil`, it should have been written `Nil = e`.
    pub discards: bool,
    /// Whether any name resolved to it, in code that checked or not.
    pub used: bool,
}

/// The scheme a module-level value has right now.
#[derive(Clone)]
pub(crate) enum ValueScheme {
    /// A declaration in the group being inferred: one type, shared by every
    /// use in the group.
    Mono(Ty),
    /// A declaration of this module, generalised.
    Local(LocalScheme),
    /// A constructor, or anything else whose type was finished without
    /// inference.
    Finished(Scheme),
}

/// What one of this module's own module-level names is.
#[derive(Clone)]
pub(crate) struct OwnValue {
    pub kind: ValueKind,
    pub scheme: ValueScheme,
    pub span: Span,
    pub public: bool,
    pub doc: Option<String>,
}

/// One of this module's own types.
pub(crate) struct OwnType {
    pub name: TypeName,
    pub arity: usize,
    /// The definition, once its pass has made it. An alias in a cycle never
    /// gets one.
    pub def: Option<TypeDef>,
    pub span: Span,
}

pub(crate) struct Checker<'a> {
    pub input: &'a CheckInput<'a>,
    pub key: ModuleKey,
    pub path: ModulePath,
    pub e: Engine,
    pub diags: Vec<Diagnostic>,
    pub binders: Vec<BinderState>,
    pub scope: Scope,
    pub values: IndexMap<Name, OwnValue>,
    pub types: IndexMap<Name, OwnType>,
    /// Import qualifier to the module it names.
    pub qualifiers: IndexMap<Name, ModuleKey>,
    /// Each imported module's path as the import wrote it, for diagnostics.
    pub written: HashMap<ModuleKey, String>,
    /// The prelude's interface, unless this is the prelude.
    pub prelude: Option<&'a Interface>,
    /// Names user code can't declare again: the prelude's types and
    /// constructors.
    pub reserved: HashSet<Name>,
    /// Types whose parameters a binding never generalises over.
    pub restricted: Vec<TypeName>,
    pub defs: Vec<Def>,
    pub uses: Vec<Use>,
    pub hovers: Vec<(Span, Name, Ty)>,
    /// The module-level definition being checked, which owns the uses inside.
    pub owner: Option<Site>,
    pub functions: Vec<Option<typed::Function>>,
    pub consts: Vec<typed::Const>,
    /// The binder a function literal being checked calls itself by: `x` in
    /// `x = fn(..) { .. }`. The literal takes it as it opens its frame.
    pub pending_self: Option<(Name, BinderId)>,
    /// Whether the module holds something the parser couldn't read. Such a
    /// module is never clean, even though the parser, not the checker,
    /// reported it.
    pub has_error_nodes: bool,
}

impl<'a> Checker<'a> {
    pub fn new(input: &'a CheckInput<'a>) -> Self {
        let path: ModulePath = input.key.as_str().split('/').map(str::to_string).collect();
        let prelude = (*input.key != prelude_key())
            .then(|| input.interfaces.interface(&prelude_key()))
            .flatten();
        let mut reserved = HashSet::new();
        if let Some(p) = prelude {
            reserved.extend(p.types.keys().cloned());
            for (name, v) in &p.values {
                if matches!(v.kind, ValueKind::Constructor { .. }) {
                    reserved.insert(name.clone());
                }
            }
        }
        let process: ModulePath = vec!["scarlet".into(), "process".into()];
        Checker {
            input,
            key: input.key.clone(),
            path,
            e: Engine::new(Known::new()),
            diags: Vec::new(),
            binders: Vec::new(),
            scope: Scope::new(),
            values: IndexMap::new(),
            types: IndexMap::new(),
            qualifiers: IndexMap::new(),
            written: HashMap::new(),
            prelude,
            reserved,
            restricted: vec![TypeName::new(
                ModuleKey::for_stdlib(&process),
                "Subject".into(),
            )],
            defs: Vec::new(),
            uses: Vec::new(),
            hovers: Vec::new(),
            owner: None,
            functions: Vec::new(),
            consts: Vec::new(),
            pending_self: None,
            has_error_nodes: false,
        }
    }

    pub fn in_stdlib(&self) -> bool {
        module_path::is_stdlib(&self.path)
    }

    pub fn is_prelude(&self) -> bool {
        self.key == prelude_key()
    }

    pub fn run(mut self) -> CheckedModule {
        let block = self.input.module;
        self.bind_imports(block);
        let module = self.check_declarations(block);
        let statements = self.check_statements(&module.statements);
        let (functions, consts) = (
            std::mem::take(&mut self.functions),
            std::mem::take(&mut self.consts),
        );
        let functions: Vec<typed::Function> = functions.into_iter().flatten().collect();
        self.finalise(&functions, &consts, &statements);
        let interface = self.interface();
        let errors = scarlet_syntax::diagnostic::has_errors(&self.diags) || self.has_error_nodes;
        let index = self.source_index();
        let Checker {
            e, binders, diags, ..
        } = self;
        let types = e.freeze();
        let binders = binders
            .into_iter()
            .map(|b| typed::Binder {
                name: b.name,
                ty: b.scheme.ty,
                span: b.span,
                module_scope: b.module_scope,
            })
            .collect();
        let clean = (!errors).then(|| {
            CleanModule(typed::TypedModule {
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

    // --- Diagnostics ---

    pub fn error(&mut self, span: Span, message: impl Into<String>) {
        self.diags.push(self.diag(span, DiagnosticCode::TypeError, message.into()));
    }

    pub fn module_error(&mut self, span: Span, message: impl Into<String>) {
        self.diags
            .push(self.diag(span, DiagnosticCode::ModuleError, message.into()));
    }

    pub fn note(&mut self, span: Span, message: impl Into<String>) {
        let mut d = Diagnostic::hint(span, DiagnosticCode::RelatedLocation, message.into());
        d.source = Some(self.key.clone());
        self.diags.push(d);
    }

    fn diag(&self, span: Span, code: DiagnosticCode, message: String) -> Diagnostic {
        let mut d = Diagnostic::error(span, code, message);
        d.source = Some(self.key.clone());
        d
    }

    pub fn unused_binding(&mut self, name: &str, span: Span) {
        if self.input.unused == UnusedBindings::Ignore {
            return;
        }
        self.diags.push(self.diag(
            span,
            DiagnosticCode::UnusedBinding,
            format!("'{name}' is never used. Start its name with '_' if that's on purpose"),
        ));
    }

    /// Unify, reporting a failure at `span`. Returns whether it worked.
    pub fn unify(&mut self, expected: Ty, given: Ty, span: Span) -> bool {
        match self.e.unify(expected, given) {
            Ok(()) => true,
            Err(err) => {
                let msg = self.unify_message(err);
                self.error(span, msg);
                false
            }
        }
    }

    pub fn unify_message(&mut self, err: UnifyError) -> String {
        match err {
            UnifyError::Mismatch {
                expected,
                given,
                arity,
            } => {
                let exp = self.e.display(expected);
                let got = self.e.display(given);
                let mut msg = format!("Type mismatch: expected '{exp}', got '{got}'");
                if let Some(a) = arity {
                    msg.push_str(&format!(
                        "\nExpected a function that takes {} argument(s), but got one that takes {}.",
                        a.expected, a.given
                    ));
                }
                msg
            }
            UnifyError::Recursive => {
                "Infinite type detected: this would need a type that contains itself, as `x(x)` does"
                    .to_string()
            }
        }
    }

    // --- Binders ---

    /// Bind `name` in the innermost block. The binding is whole from here, so
    /// uses checked before it, its own initialiser among them, still mean
    /// whatever the name meant before.
    pub fn bind(&mut self, name: &str, scheme: LocalScheme, span: Span) -> BinderId {
        let id = self.new_binder(name, scheme, span);
        self.scope.insert(self.binders[id.0 as usize].name.clone(), id);
        id
    }

    pub fn bind_mono(&mut self, name: &str, ty: Ty, span: Span) -> BinderId {
        self.bind(name, LocalScheme::mono(ty), span)
    }

    /// A binder that isn't in scope yet.
    pub fn new_binder(&mut self, name: &str, scheme: LocalScheme, span: Span) -> BinderId {
        let id = BinderId(self.binders.len() as u32);
        let module_scope = self.scope.at_module_scope();
        let ty = scheme.ty;
        self.binders.push(BinderState {
            name: name.into(),
            scheme,
            span,
            module_scope,
            discards: false,
            used: false,
        });
        self.hovers.push((span, name.into(), ty));
        self.define(span, name, None, false, DefKind::Value);
        id
    }

    // --- The source index ---

    pub fn site(&self, span: Span, entity: EntityKind) -> Site {
        Site {
            module: self.key.clone(),
            span,
            entity,
        }
    }

    pub fn define(&mut self, span: Span, name: &str, doc: Option<String>, public: bool, kind: DefKind) {
        let site = self.site(span, kind.entity());
        self.defs.push(Def {
            site,
            name: name.to_string(),
            doc,
            public,
            kind,
        });
    }

    pub fn use_site(&mut self, span: Span, kind: UseKind, target: Site) {
        self.uses.push(Use {
            span,
            kind,
            target,
            owner: self.owner.clone(),
        });
    }

    fn source_index(&mut self) -> SourceIndex {
        let hovers = std::mem::take(&mut self.hovers)
            .into_iter()
            .map(|(span, name, ty)| Hover {
                span,
                name: name.to_string(),
                ty: self.e.zonk(ty),
            })
            .collect();
        SourceIndex {
            defs: std::mem::take(&mut self.defs),
            uses: std::mem::take(&mut self.uses),
            hovers,
            doc: self.input.doc.clone(),
        }
    }

    // --- Imports ---

    fn bind_imports(&mut self, block: &ast::BlockExpression) {
        let imports = self.input.imports;
        let decls = block.body.iter().filter_map(|n| match n {
            ast::Node::Statement(s) => match s.as_ref() {
                ast::Statement::ImportDeclaration(d) => Some(d),
                ast::Statement::Declaration { .. }
                | ast::Statement::Binding(_)
                | ast::Statement::Backpass(_) => None,
            },
            ast::Node::Expression(_) => None,
        });
        for (imp, key) in decls.zip(imports) {
            let Some(key) = key else { continue };
            let qualifier: Name = match &imp.alias {
                Some(a) => a.name.as_str().into(),
                None => imp
                    .path
                    .names
                    .last()
                    .map_or_else(|| key.as_str().into(), |n| n.as_str().into()),
            };
            let alias_span = imp.alias.as_ref().map_or(imp.span, |a| a.span);
            self.define(
                alias_span,
                &qualifier,
                None,
                false,
                DefKind::ModuleAlias {
                    decl: imp.span,
                    imports: key.clone(),
                },
            );
            // The module segment points into the imported module, so
            // go-to-definition on it opens that module.
            self.use_site(
                imp.path_span,
                UseKind::Import,
                Site {
                    module: key.clone(),
                    span: imp.path_span,
                    entity: EntityKind::ModuleAlias,
                },
            );
            if let Some(a) = &imp.alias {
                let target = self.site(alias_span, EntityKind::ModuleAlias);
                self.use_site(a.span, UseKind::Alias, target);
            }
            self.written.insert(key.clone(), imp.path.to_string());
            self.qualifiers.insert(qualifier, key.clone());
        }
    }

    /// The interface of the module `key`, this one excluded.
    pub fn interface_of(&self, key: &ModuleKey) -> Option<&'a Interface> {
        self.input.interfaces.interface(key)
    }

    /// How to name a module in a diagnostic: the path its import wrote, or
    /// its last segment. Never the key, which is an identity, not a name.
    pub fn module_name(&self, key: &ModuleKey) -> String {
        if let Some(w) = self.written.get(key) {
            return w.clone();
        }
        let k = key.as_str();
        k.rsplit('/').next().unwrap_or(k).to_string()
    }

    /// The definition of any type the module can reach.
    pub fn type_def(&self, name: &TypeName) -> Option<&TypeDef> {
        if *name.module() == self.key {
            return self.types.get(name.name()).and_then(|t| t.def.as_ref());
        }
        self.interface_of(name.module())?.types.get(name.name())
    }

    /// The span where a value of another module is declared, as a site.
    pub fn foreign_site(&self, key: &ModuleKey, v: &ValueDef) -> Site {
        Site {
            module: key.clone(),
            span: v.span,
            entity: entity_of(&v.kind),
        }
    }
}

pub(crate) fn entity_of(kind: &ValueKind) -> EntityKind {
    match kind {
        ValueKind::Fn { .. } | ValueKind::Builtin { .. } => EntityKind::Function,
        ValueKind::Const => EntityKind::Constant,
        ValueKind::Constructor { .. } => EntityKind::Constructor,
    }
}

impl Checker<'_> {
    /// What the module shows its importers. A value whose type still holds a
    /// variable nothing generalised would mean a different type in every
    /// module that used it, so it has to be written out.
    fn interface(&mut self) -> Interface {
        let mut iface = Interface::new(self.key.clone());
        iface.doc = self.input.doc.clone();
        for (name, t) in &self.types {
            match &t.def {
                Some(def) if def.public => {
                    iface.types.insert(name.clone(), def.clone());
                }
                Some(def) => {
                    iface.private_names.insert(name.clone());
                    iface.types.insert(name.clone(), def.clone());
                }
                None => {}
            }
        }
        let values: Vec<(Name, OwnValue)> = self
            .values
            .iter()
            .map(|(n, v)| (n.clone(), v.clone()))
            .collect();
        for (name, v) in values {
            if !v.public {
                iface.private_names.insert(name);
                continue;
            }
            let scheme = match &v.scheme {
                ValueScheme::Mono(t) => Scheme::mono(self.e.zonk(*t)),
                ValueScheme::Local(s) => self.e.zonk_scheme(s),
                ValueScheme::Finished(s) => s.clone(),
            };
            let open: Vec<_> = scheme
                .ty
                .vars()
                .into_iter()
                .filter(|var| !scheme.vars.contains(var))
                .collect();
            if !open.is_empty() {
                self.error(
                    v.span,
                    format!(
                        "The type of '{name}' isn't fully known: '{}'. Write its type out, so every module that uses it agrees on it",
                        scheme.ty
                    ),
                );
            }
            iface.values.insert(
                name,
                ValueDef {
                    scheme: scheme.canonical(),
                    kind: v.kind.clone(),
                    doc: v.doc.clone(),
                    span: v.span,
                },
            );
        }
        iface
    }
}
