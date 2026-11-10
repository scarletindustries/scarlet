//! A module's declarations, in the order each fact is needed:
//!
//! 1. sort the nodes, and report duplicate and reserved names
//! 2. name every custom and external type
//! 3. aliases, in the order they use each other
//! 4. the bodies of custom types, and their constructors
//! 5. function signatures and `@vm` functions
//! 6. functions and consts, inferred a group at a time in dependency order

use std::collections::HashMap;

use petgraph::Directed;
use petgraph::algo::tarjan_scc;
use petgraph::stable_graph::{NodeIndex, StableGraph};
use scarlet_ir::intrinsic::Intrinsic;
use scarlet_syntax::ast;
use scarlet_syntax::span::Span;
use scarlet_types::def::{FieldDef, TypeBody, TypeDef, VariantDef};
use scarlet_types::engine::Ty;
use scarlet_types::interface::ValueKind;
use scarlet_types::name::{CtorName, Name, TypeName};
use scarlet_types::ty::{Scheme, Type, TypeVar};

use crate::Role;
use crate::annot::{Annot, AnnotContext};
use crate::callgraph::{self, Body};
use crate::check::{Checker, OwnType, OwnValue, ValueScheme};
use crate::index::{DefKind, EntityKind};
use crate::typed;

/// The nodes of a module that aren't declarations or imports, for the
/// statement walk.
pub(crate) struct ModuleParts<'a> {
    pub statements: Vec<&'a ast::Node>,
}

enum Decl<'a> {
    Fn {
        fd: &'a ast::FunctionDeclaration,
        public: bool,
        body: &'a ast::Expression,
    },
    Const {
        cb: &'a ast::ConstBinding,
        public: bool,
    },
}

impl<'a> Decl<'a> {
    fn name(&self) -> &'a str {
        match self {
            Decl::Fn { fd, .. } => &fd.identifier.name,
            Decl::Const { cb, .. } => &cb.identifier.name,
        }
    }
}

/// A function's signature, read before any body is inferred.
struct Signature {
    params: Vec<Ty>,
    ret: Ty,
    ty: Ty,
}

impl<'a> Checker<'a> {
    pub fn check_declarations(&mut self, block: &'a ast::BlockExpression) -> ModuleParts<'a> {
        let in_prelude = self.is_prelude();
        let in_stdlib = self.in_stdlib();
        let mut seen_types: HashMap<&str, Span> = HashMap::new();
        let mut seen_values: HashMap<&str, Span> = HashMap::new();
        let mut type_decls: Vec<(&ast::TypeDeclaration, bool, Vec<bool>)> = Vec::new();
        let mut decls: Vec<Decl<'a>> = Vec::new();
        let mut vm_fns: Vec<(&ast::FunctionDeclaration, bool, Intrinsic)> = Vec::new();
        let mut statements = Vec::new();

        for node in &block.body {
            let ast::Node::Statement(stmt) = node else {
                self.non_declaration(node);
                statements.push(node);
                continue;
            };
            match stmt.as_ref() {
                ast::Statement::Declaration { decl, public } => match decl.as_ref() {
                    ast::Declaration::Type(td) => {
                        self.validate_attributes(&td.attributes, in_stdlib, false);
                        let dup = self.check_duplicate(&mut seen_types, &td.identifier);
                        self.check_reserved(&td.identifier, in_prelude);
                        let mut dups = Vec::new();
                        if let ast::TypeBody::Variants { ctors, .. } = &td.body {
                            for ctor in ctors {
                                self.check_reserved(&ctor.identifier, in_prelude);
                                dups.push(self.check_duplicate(&mut seen_values, &ctor.identifier));
                            }
                        }
                        if !dup {
                            type_decls.push((td, *public, dups));
                        }
                    }
                    ast::Declaration::Function(fd) => {
                        self.validate_attributes(&fd.attributes, in_stdlib, true);
                        if fd.identifier.name == "main" && self.input.role == Role::Entry {
                            self.main_rules(fd, *public);
                        }
                        if self.check_duplicate(&mut seen_values, &fd.identifier) {
                            continue;
                        }
                        match &fd.body {
                            ast::FnBody::Vm(key) => match Intrinsic::from_key(&key.name) {
                                Some(i) => vm_fns.push((fd, *public, i)),
                                None => self.error(key.span, format!("unknown @vm builtin '{}'", key.name)),
                            },
                            ast::FnBody::Block(body) => decls.push(Decl::Fn {
                                fd,
                                public: *public,
                                body,
                            }),
                        }
                    }
                    ast::Declaration::Const(cb) => {
                        if !self.check_duplicate(&mut seen_values, &cb.identifier) {
                            decls.push(Decl::Const { cb, public: *public });
                        }
                    }
                },
                ast::Statement::ImportDeclaration(_) => {}
                ast::Statement::Binding(_) | ast::Statement::Backpass(_) => {
                    self.non_declaration(node);
                    statements.push(node);
                }
            }
        }

        for (td, public, _) in &type_decls {
            if !matches!(td.body, ast::TypeBody::Alias(_)) {
                self.name_type(td, *public);
            }
        }
        self.register_aliases(&type_decls);
        for (td, public, dups) in &type_decls {
            if let ast::TypeBody::Variants { ctors, opaque } = &td.body {
                self.define_variants(td, ctors, *public && !*opaque, *public, dups);
            }
        }

        // Signatures are read one level in, so their variables are generalised
        // when each declaration's group is.
        self.e.enter_level();
        for &(fd, public, intrinsic) in &vm_fns {
            let sig = self.signature(fd);
            self.e.leave_level();
            let scheme = self.e.generalize(sig.ty, &[]);
            self.e.enter_level();
            let params = param_names(fd);
            self.define(
                fd.identifier.span,
                &fd.identifier.name,
                fd.doc.clone(),
                public,
                DefKind::Function {
                    params: params.iter().map(|p| p.to_string()).collect(),
                },
            );
            self.hovers.push((fd.identifier.span, fd.identifier.name.as_str().into(), sig.ty));
            self.values.insert(
                fd.identifier.name.as_str().into(),
                OwnValue {
                    kind: ValueKind::Builtin { intrinsic, params },
                    scheme: ValueScheme::Local(scheme),
                    span: fd.identifier.span,
                    public,
                    doc: fd.doc.clone(),
                },
            );
            self.functions.push(Some(typed::Function {
                name: fd.identifier.name.as_str().into(),
                span: fd.identifier.span,
                params: Vec::new(),
                ty: sig.ty,
                body: typed::FnBody::Builtin(intrinsic),
            }));
        }
        let mut sigs: Vec<Option<Signature>> = Vec::with_capacity(decls.len());
        for d in &decls {
            match d {
                Decl::Fn { fd, public, .. } => {
                    let sig = self.signature(fd);
                    self.values.insert(
                        fd.identifier.name.as_str().into(),
                        OwnValue {
                            kind: ValueKind::Fn {
                                params: param_names(fd),
                            },
                            scheme: ValueScheme::Mono(sig.ty),
                            span: fd.identifier.span,
                            public: *public,
                            doc: fd.doc.clone(),
                        },
                    );
                    sigs.push(Some(sig));
                }
                Decl::Const { cb, public } => {
                    let ty = self.e.fresh();
                    self.values.insert(
                        cb.identifier.name.as_str().into(),
                        OwnValue {
                            kind: ValueKind::Const,
                            scheme: ValueScheme::Mono(ty),
                            span: cb.identifier.span,
                            public: *public,
                            doc: cb.doc.clone(),
                        },
                    );
                    sigs.push(None);
                }
            }
        }
        self.e.leave_level();

        let walk: Vec<(&str, Body<'_>)> = decls
            .iter()
            .map(|d| {
                let body = match d {
                    Decl::Fn { fd, body, .. } => Body::Fn {
                        params: &fd.params,
                        body,
                    },
                    Decl::Const { cb, .. } => Body::Const(&cb.init),
                };
                (d.name(), body)
            })
            .collect();
        let first_fn = self.functions.len();
        self.functions
            .extend(decls.iter().map(|_| None::<typed::Function>));
        for group in callgraph::groups(&walk) {
            self.infer_group(&decls, &mut sigs, &group, first_fn);
        }
        ModuleParts { statements }
    }

    /// A program starts at `main`, so it must be public and take nothing.
    fn main_rules(&mut self, fd: &ast::FunctionDeclaration, public: bool) {
        if !public {
            self.error(
                fd.identifier.span,
                "`main` must be public, because a program starts at `pub fn main()`",
            );
        }
        if !fd.params.is_empty() {
            self.error(
                fd.identifier.span,
                "`main` takes no parameters. Read the program's arguments with `os.argv()` instead",
            );
        }
    }

    /// Report a statement at module scope where the role doesn't allow one.
    /// It is still checked, so the rest of the file is reported too.
    fn non_declaration(&mut self, node: &ast::Node) {
        match self.input.role {
            Role::Library => self.error(
                node.span(),
                "Modules may only contain declarations at the top level",
            ),
            Role::Entry => self.error(
                node.span(),
                "Statements aren't allowed at module scope. Put a program's code inside `pub fn main()`",
            ),
            Role::Script => {}
        }
    }

    fn validate_attributes(&mut self, attrs: &[ast::Attribute], in_stdlib: bool, on_fn: bool) {
        for a in attrs {
            match a.name.name.as_str() {
                "vm" => {
                    if !in_stdlib {
                        self.error(a.span, "'@vm' is only allowed in the standard library");
                    }
                    if !on_fn {
                        self.error(a.span, "'@vm' may only be used on functions");
                    }
                }
                other => self.error(a.span, format!("Unknown attribute '@{other}'")),
            }
        }
    }

    /// Report a name declared twice. The second declaration is dropped, so
    /// returns whether it was a duplicate.
    fn check_duplicate<'n>(&mut self, seen: &mut HashMap<&'n str, Span>, id: &'n ast::Identifier) -> bool {
        match seen.insert(id.name.as_str(), id.span) {
            Some(prev) => {
                self.error(id.span, format!("'{}' is already defined", id.name));
                self.note(prev, "first definition was here");
                true
            }
            None => false,
        }
    }

    fn check_reserved(&mut self, id: &ast::Identifier, in_prelude: bool) {
        if !in_prelude && self.reserved.contains(id.name.as_str()) {
            self.error(
                id.span,
                format!("'{}' is defined in the prelude and cannot be redefined", id.name),
            );
        }
    }

    fn type_name(&self, name: &str) -> TypeName {
        TypeName::new(self.key.clone(), name.into())
    }

    /// A type's parameters, as rigid variables in an annotation scope.
    fn type_params(&mut self, params: &[ast::Identifier]) -> (Annot, Vec<Ty>) {
        let mut a = Annot::new(AnnotContext::TypeDefinition);
        let mut tys = Vec::with_capacity(params.len());
        for p in params {
            if a.vars.contains_key(p.name.as_str()) {
                self.error(p.span, format!("Duplicate type parameter '{}'", p.name));
                continue;
            }
            let v = self.e.rigid(p.name.as_str().into());
            a.vars.insert(p.name.as_str().into(), v);
            tys.push(v);
        }
        (a, tys)
    }

    /// The finished form of a type written in terms of a declaration's
    /// parameters, with the parameters numbered from 0.
    fn finish_in_params(&mut self, ty: Ty, params: &[Ty]) -> (Type, Vec<TypeVar>) {
        let raw: Vec<TypeVar> = params
            .iter()
            .filter_map(|&p| match self.e.zonk(p) {
                Type::Var(v) => Some(v),
                Type::Named { .. } | Type::Fn { .. } | Type::Tuple(_) => None,
            })
            .collect();
        let canonical: Vec<TypeVar> = raw
            .iter()
            .enumerate()
            .map(|(i, v)| TypeVar {
                id: i as u32,
                name: v.name.clone(),
                constraint: None,
            })
            .collect();
        let t = self.e.zonk(ty).substitute(&|v| {
            raw.iter()
                .position(|r| r == v)
                .map(|i| Type::Var(canonical[i].clone()))
        });
        (t, canonical)
    }

    fn name_type(&mut self, td: &ast::TypeDeclaration, public: bool) {
        let name = self.type_name(&td.identifier.name);
        let arity = td.type_params.len();
        let def = match td.body {
            ast::TypeBody::External => Some(TypeDef {
                name: name.clone(),
                params: (0..arity)
                    .map(|i| TypeVar {
                        id: i as u32,
                        name: td.type_params[i].name.as_str().into(),
                        constraint: None,
                    })
                    .collect(),
                body: TypeBody::External,
                public,
                doc: td.doc.clone(),
                span: td.identifier.span,
            }),
            ast::TypeBody::Variants { .. } | ast::TypeBody::Alias(_) => None,
        };
        self.types.insert(
            td.identifier.name.as_str().into(),
            OwnType {
                name,
                arity,
                def,
                span: td.identifier.span,
            },
        );
        self.define(td.identifier.span, &td.identifier.name, td.doc.clone(), public, DefKind::Type);
    }

    fn register_aliases(&mut self, type_decls: &[(&ast::TypeDeclaration, bool, Vec<bool>)]) {
        let aliases: Vec<(&ast::TypeDeclaration, &ast::TypeIdentifier, bool)> = type_decls
            .iter()
            .filter_map(|(td, public, _)| match &td.body {
                ast::TypeBody::Alias(rhs) => Some((*td, rhs, *public)),
                ast::TypeBody::Variants { .. } | ast::TypeBody::External => None,
            })
            .collect();
        if aliases.is_empty() {
            return;
        }
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
        for component in tarjan_scc(&graph) {
            let cyclic = component.len() > 1 || graph.find_edge(component[0], component[0]).is_some();
            if cyclic {
                for n in &component {
                    let (td, _, _) = aliases[n.index()];
                    self.error(
                        td.identifier.span,
                        format!(
                            "Recursive type alias '{}'. An alias can't refer back to itself",
                            td.identifier.name
                        ),
                    );
                }
                continue;
            }
            let (td, rhs, public) = aliases[component[0].index()];
            let (mut annot, params) = self.type_params(&td.type_params);
            let owner = self.site(td.identifier.span, EntityKind::Type);
            let prev = self.owner.replace(owner);
            let target = self.hydrate(&mut annot, rhs);
            self.owner = prev;
            let (target, params) = self.finish_in_params(target, &params);
            let name = self.type_name(&td.identifier.name);
            self.types.insert(
                td.identifier.name.as_str().into(),
                OwnType {
                    name: name.clone(),
                    arity: params.len(),
                    def: Some(TypeDef {
                        name,
                        params,
                        body: TypeBody::Alias(target),
                        public,
                        doc: td.doc.clone(),
                        span: td.identifier.span,
                    }),
                    span: td.identifier.span,
                },
            );
            self.define(td.identifier.span, &td.identifier.name, td.doc.clone(), public, DefKind::Type);
        }
    }

    /// A custom type's variants, and a constructor value for each. `dups`
    /// marks the constructors whose names were taken already, which are
    /// dropped.
    fn define_variants(
        &mut self,
        td: &ast::TypeDeclaration,
        ctors: &[ast::Constructor],
        ctors_public: bool,
        public: bool,
        dups: &[bool],
    ) {
        let name = self.type_name(&td.identifier.name);
        let (mut annot, params) = self.type_params(&td.type_params);
        let type_site = self.site(td.identifier.span, EntityKind::Type);
        let prev = self.owner.replace(type_site.clone());

        let mut variants = Vec::new();
        let mut field_tys: Vec<Vec<Ty>> = Vec::new();
        for (ctor, _) in ctors.iter().zip(dups).filter(|&(_, dup)| !dup) {
            let mut fields = Vec::with_capacity(ctor.fields.len());
            let mut tys = Vec::with_capacity(ctor.fields.len());
            for f in &ctor.fields {
                let ty = self.hydrate(&mut annot, &f.typ);
                tys.push(ty);
                fields.push((f, ty));
                self.define(f.label.span, &f.label.name, None, ctors_public, DefKind::Field);
            }
            variants.push((ctor, fields));
            field_tys.push(tys);
        }
        self.owner = prev;

        let mut defs = Vec::with_capacity(variants.len());
        for (ctor, fields) in &variants {
            let fields = fields
                .iter()
                .map(|(f, ty)| {
                    let (ty, _) = self.finish_in_params(*ty, &params);
                    FieldDef {
                        label: f.label.name.as_str().into(),
                        ty,
                        span: f.label.span,
                    }
                })
                .collect();
            defs.push(VariantDef {
                name: ctor.identifier.name.as_str().into(),
                fields,
                doc: ctor.doc.clone(),
                span: ctor.identifier.span,
            });
        }
        let own_ty = self_type(&mut self.e, &name, &params);
        let (_, type_params) = self.finish_in_params(own_ty, &params);
        let def = TypeDef {
            name: name.clone(),
            params: type_params.clone(),
            body: TypeBody::Variants {
                variants: defs.clone(),
                constructors_public: ctors_public,
            },
            public,
            doc: td.doc.clone(),
            span: td.identifier.span,
        };
        if let Some(own) = self.types.get_mut(td.identifier.name.as_str()) {
            own.def = Some(def);
        }

        let result = Type::named(name.clone(), type_params.iter().cloned().map(Type::Var).collect());
        for (i, v) in defs.iter().enumerate() {
            let ty = if v.fields.is_empty() {
                result.clone()
            } else {
                Type::function(v.fields.iter().map(|f| f.ty.clone()).collect(), result.clone())
            };
            let labels: Vec<Name> = v.fields.iter().map(|f| f.label.clone()).collect();
            self.values.insert(
                v.name.clone(),
                OwnValue {
                    kind: ValueKind::Constructor {
                        ctor: CtorName {
                            type_name: name.clone(),
                            variant: i as u16,
                        },
                        name: v.name.clone(),
                        labels: labels.clone(),
                    },
                    scheme: ValueScheme::Finished(Scheme {
                        vars: type_params.clone(),
                        ty,
                    }),
                    span: v.span,
                    public: ctors_public,
                    doc: v.doc.clone(),
                },
            );
            self.define(
                v.span,
                &v.name,
                v.doc.clone(),
                ctors_public,
                DefKind::Constructor {
                    of: type_site.clone(),
                    fields: labels.iter().map(|l| l.to_string()).collect(),
                },
            );
        }
    }

    fn signature(&mut self, fd: &ast::FunctionDeclaration) -> Signature {
        let mut annot = Annot::new(AnnotContext::Signature);
        let params: Vec<Ty> = fd
            .params
            .iter()
            .map(|p| self.hydrate_opt(&mut annot, p.typ.as_ref()))
            .collect();
        let ret = self.hydrate_opt(&mut annot, fd.return_type.as_ref());
        let ty = self.e.fun(params.clone(), ret);
        Signature {
            params,
            ret,
            ty,
        }
    }

    fn infer_group(&mut self, decls: &[Decl<'a>], sigs: &mut [Option<Signature>], group: &[usize], first_fn: usize) {
        self.e.enter_level();
        let mut inferred: Vec<(usize, Ty)> = Vec::with_capacity(group.len());
        for &i in group {
            match &decls[i] {
                Decl::Fn { fd, public, body } => {
                    let Some(sig) = sigs[i].take() else { continue };
                    let site = self.site(fd.identifier.span, EntityKind::Function);
                    self.define(
                        fd.identifier.span,
                        &fd.identifier.name,
                        fd.doc.clone(),
                        *public,
                        DefKind::Function {
                            params: fd.params.iter().map(|p| p.identifier.name.clone()).collect(),
                        },
                    );
                    let prev = self.owner.replace(site);
                    let function = self.check_function(fd, body, sig);
                    self.owner = prev;
                    self.hovers
                        .push((fd.identifier.span, fd.identifier.name.as_str().into(), function.ty));
                    inferred.push((i, function.ty));
                    self.functions[first_fn + i] = Some(function);
                }
                Decl::Const { cb, public } => {
                    let site = self.site(cb.identifier.span, EntityKind::Constant);
                    self.define(
                        cb.identifier.span,
                        &cb.identifier.name,
                        cb.doc.clone(),
                        *public,
                        DefKind::Constant,
                    );
                    let prev = self.owner.replace(site);
                    let mut annot = Annot::new(AnnotContext::Signature);
                    let annotated = cb.typ.as_ref().map(|t| self.hydrate(&mut annot, t));
                    let value = self.check_expr_hinted(&cb.init, annotated);
                    let ty = match annotated {
                        Some(a) => {
                            self.unify(a, value.ty, cb.identifier.span);
                            a
                        }
                        None => value.ty,
                    };
                    if let Some(OwnValue {
                        scheme: ValueScheme::Mono(placeholder),
                        ..
                    }) = self.values.get(cb.identifier.name.as_str()).cloned()
                    {
                        self.unify(placeholder, ty, cb.init.span());
                    }
                    self.owner = prev;
                    self.hovers
                        .push((cb.identifier.span, cb.identifier.name.as_str().into(), ty));
                    inferred.push((i, ty));
                    self.consts.push(typed::Const {
                        name: cb.identifier.name.as_str().into(),
                        span: cb.identifier.span,
                        value,
                    });
                }
            }
        }
        self.e.leave_level();
        for (i, ty) in inferred {
            let (name, restricted) = match &decls[i] {
                Decl::Fn { fd, .. } => (fd.identifier.name.as_str(), false),
                Decl::Const { cb, .. } => (cb.identifier.name.as_str(), true),
            };
            // A const has already been evaluated, so it falls under the value
            // restriction. A function's variables all sit under its arrow.
            let scheme = if restricted {
                let r = self.restricted.clone();
                self.e.generalize(ty, &r)
            } else {
                self.e.generalize(ty, &[])
            };
            if let Some(v) = self.values.get_mut(name) {
                v.scheme = ValueScheme::Local(scheme);
            }
        }
    }

    fn check_function(&mut self, fd: &ast::FunctionDeclaration, body: &ast::Expression, sig: Signature) -> typed::Function {
        self.scope.open_frame(None);
        let params: Vec<_> = fd
            .params
            .iter()
            .zip(&sig.params)
            .map(|(p, &ty)| self.bind_mono(&p.identifier.name, ty, p.identifier.span))
            .collect();
        let body = self.check_expr(body);
        self.unify(sig.ret, body.ty, defining_span_of(fd));
        self.scope.close_frame();
        typed::Function {
            name: fd.identifier.name.as_str().into(),
            span: fd.identifier.span,
            params,
            ty: sig.ty,
            body: typed::FnBody::Expr(body),
        }
    }

    /// A finished scheme brought into this check.
    pub fn instantiate_value(&mut self, scheme: &ValueScheme) -> Ty {
        match scheme {
            ValueScheme::Mono(t) => *t,
            ValueScheme::Local(s) => self.e.instantiate(s),
            ValueScheme::Finished(s) => self.e.instantiate_scheme(s),
        }
    }
}

fn defining_span_of(fd: &ast::FunctionDeclaration) -> Span {
    match &fd.body {
        ast::FnBody::Block(b) => crate::expr::type_defining_span(b),
        ast::FnBody::Vm(k) => k.span,
    }
}

fn self_type(e: &mut scarlet_types::engine::Engine, name: &TypeName, params: &[Ty]) -> Ty {
    e.con(name.clone(), params.to_vec())
}

fn param_names(fd: &ast::FunctionDeclaration) -> Vec<Name> {
    fd.params
        .iter()
        .map(|p| p.identifier.name.as_str().into())
        .collect()
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
