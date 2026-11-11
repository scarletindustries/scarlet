//! Name resolution: what every name in the module means. This is the only
//! place that knows about scopes. It gives the resolved tree, the binder
//! table, the order to infer declarations in, and every name-level error:
//! unknown and private names, labels, a constructor's fields, a pattern's
//! names, and bindings nothing reads.

use std::collections::{BTreeSet, HashSet};

use petgraph::Directed;
use petgraph::algo::tarjan_scc;
use petgraph::stable_graph::StableGraph;
use scarlet_syntax::ast;
use scarlet_syntax::span::Span;
use scarlet_types::interface::ValueKind;
use scarlet_types::name::{CtorName, Name, ValueName};
use scarlet_types::slots::{SlotError, slot_labeled};
use scarlet_types::ty::Scheme;

use crate::annot::{AnnotContext, AnnotVars, Annotations};
use crate::declare::{DeclBody, DeclId, Declared};
use crate::env::Env;
use crate::error::{CheckError, FieldSite, NestedDecl, Unreadable};
use crate::index::{Def, DefKind, EntityKind, Site, Use, UseKind};
use crate::report::Report;
use crate::resolved::*;
use crate::scope::Scope;
use crate::typed::{BinderId, Var};
use crate::types::TypeDefs;

/// Where in the module a walk is: the declaration it is inside, which owns
/// the uses it finds.
#[derive(Clone)]
pub(crate) struct Walk {
    pub owner: Option<Site>,
    decl: Option<DeclId>,
}

/// A constructor a call or pattern names.
pub(crate) struct CtorInfo {
    pub ctor: CtorName,
    pub name: Name,
    pub labels: Vec<Name>,
    pub scheme: Scheme,
}

/// A name in value position, resolved, with what a call needs to know.
struct Found {
    r: Ref,
    /// The parameter names a call may label its arguments with.
    labels: Option<Vec<Name>>,
    ctor: Option<CtorInfo>,
}

pub(crate) struct Resolver<'r, 'a> {
    pub env: &'r Env<'a>,
    pub declared: &'r Declared<'a>,
    pub defs: &'r TypeDefs,
    pub scope: Scope,
    pub binders: Vec<BinderInfo>,
    pub used: HashSet<BinderId>,
    pub report: Report,
    /// For each declaration, the declarations its body names.
    edges: Vec<BTreeSet<DeclId>>,
}

pub(crate) fn resolve(
    env: &Env<'_>,
    declared: &Declared<'_>,
    defs: &TypeDefs,
) -> (Resolved, Report) {
    let mut r = Resolver {
        env,
        declared,
        defs,
        scope: Scope::new(),
        binders: Vec::new(),
        used: HashSet::new(),
        report: Report::new(env.input.source),
        edges: vec![BTreeSet::new(); declared.values.len()],
    };
    let decls: Vec<RDecl> = declared.ids().map(|id| r.decl(id)).collect();
    let top = Walk {
        owner: None,
        decl: None,
    };
    let statements = declared
        .statements
        .iter()
        .filter_map(|n| r.node(n, &top))
        .collect();
    let unused = r.unused();
    let groups = groups(&r.edges);
    let resolved = Resolved {
        decls,
        statements,
        binders: r.binders,
        groups,
        unused,
    };
    (resolved, r.report)
}

/// The declarations in groups that use each other, each after the groups it
/// uses.
fn groups(edges: &[BTreeSet<DeclId>]) -> Vec<Vec<DeclId>> {
    let mut graph: StableGraph<(), (), Directed> = StableGraph::new();
    let nodes: Vec<_> = edges.iter().map(|_| graph.add_node(())).collect();
    for (from, tos) in edges.iter().enumerate() {
        for to in tos {
            graph.add_edge(nodes[from], nodes[to.0 as usize], ());
        }
    }
    tarjan_scc(&graph)
        .into_iter()
        .map(|c| c.into_iter().map(|n| DeclId(n.index() as u32)).collect())
        .collect()
}

impl<'r, 'a> Resolver<'r, 'a> {
    fn annotations(&self, w: &Walk) -> Annotations<'r, 'a, TypeDefs> {
        Annotations {
            env: self.env,
            own: self.defs,
            owner: w.owner.clone(),
        }
    }

    fn decl(&mut self, id: DeclId) -> RDecl {
        let d = self.declared.get(id);
        let w = Walk {
            owner: Some(self.env.site(d.span, d.entity())),
            decl: Some(id),
        };
        match &d.body {
            DeclBody::Fn(fd, body) => {
                let mut vars = AnnotVars::new(AnnotContext::Signature);
                let read = self.annotations(&w);
                let param_tys: Vec<_> = fd
                    .params
                    .iter()
                    .map(|p| read.read_opt(&mut vars, p.typ.as_ref(), &mut self.report))
                    .collect();
                let ret = read.read_opt(&mut vars, fd.return_type.as_ref(), &mut self.report);
                let body_span = type_defining_span(body);
                let (params, body) = self
                    .in_frame(None, |r| {
                        let params: Vec<_> = fd
                            .params
                            .iter()
                            .zip(param_tys)
                            .map(|(p, ty)| (r.bind_param(p), ty))
                            .collect();
                        (params, r.expr(body, &w))
                    })
                    .0;
                RDecl::Fn {
                    params,
                    ret,
                    vars: vars.named().to_vec(),
                    body,
                    body_span,
                }
            }
            DeclBody::Builtin(fd, _) => {
                let mut vars = AnnotVars::new(AnnotContext::Signature);
                let read = self.annotations(&w);
                let params = fd
                    .params
                    .iter()
                    .map(|p| read.read_opt(&mut vars, p.typ.as_ref(), &mut self.report))
                    .collect();
                let ret = read.read_opt(&mut vars, fd.return_type.as_ref(), &mut self.report);
                RDecl::Builtin {
                    params,
                    ret,
                    vars: vars.named().to_vec(),
                }
            }
            DeclBody::Const(cb) => {
                let mut vars = AnnotVars::new(AnnotContext::Signature);
                let annot =
                    self.annotations(&w)
                        .read_opt(&mut vars, cb.typ.as_ref(), &mut self.report);
                RDecl::Const {
                    annot,
                    value: self.expr(&cb.init, &w),
                    value_span: cb.init.span(),
                }
            }
        }
    }

    // --- Scopes ---

    /// Run `f` in a block of its own. The names it binds go out of scope at
    /// the end.
    pub fn in_block<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        self.scope.open_block();
        let r = f(self);
        self.scope.close_block();
        r
    }

    /// Run `f` as a function body. Returns what it gave and the enclosing
    /// locals the body reads.
    fn in_frame<R>(
        &mut self,
        self_name: Option<(Name, BinderId)>,
        f: impl FnOnce(&mut Self) -> R,
    ) -> (R, Vec<BinderId>) {
        self.scope.open_frame(self_name);
        let r = f(self);
        let captures = self.scope.close_frame();
        (r, captures)
    }

    /// A new binder, not yet in scope.
    fn new_binder(
        &mut self,
        id: &ast::Identifier,
        discards: bool,
        doc: Option<String>,
    ) -> BinderId {
        let b = BinderId(self.binders.len() as u32);
        self.binders.push(BinderInfo {
            name: id.name.as_str().into(),
            span: id.span,
            module_scope: self.scope.at_module_scope(),
            discards,
        });
        self.report.defs.push(Def {
            site: self.env.site(id.span, EntityKind::Value),
            name: id.name.clone(),
            doc,
            public: false,
            kind: DefKind::Value,
        });
        b
    }

    /// A new binder, in scope from here on.
    pub fn bind(&mut self, id: &ast::Identifier, discards: bool) -> BinderId {
        let b = self.new_binder(id, discards, None);
        self.scope.insert(id.name.as_str().into(), b);
        b
    }

    fn bind_param(&mut self, p: &ast::FunctionParameter) -> BinderId {
        self.bind(&p.identifier, false)
    }

    /// Report the bindings nothing read, in source order. A name starting
    /// with `_` is unused on purpose.
    /// The binders nothing reads, in source order. A `_` name says that
    /// is on purpose.
    fn unused(&self) -> Vec<BinderId> {
        let mut unused: Vec<BinderId> = (0..self.binders.len() as u32)
            .map(BinderId)
            .filter(|b| !self.binders[b.0 as usize].name.starts_with('_') && !self.used.contains(b))
            .collect();
        unused.sort_by_key(|b| {
            let sp = self.binders[b.0 as usize].span;
            (sp.start_line, sp.start_column)
        });
        unused
    }

    pub fn use_of(&mut self, span: Span, kind: UseKind, target: Site, w: &Walk) {
        self.report.uses.push(Use {
            span,
            kind,
            target,
            owner: w.owner.clone(),
        });
    }

    pub fn qualifier_use(&mut self, q: &ast::Identifier, w: &Walk) {
        if let Some(imp) = self.env.imports.get(&q.name) {
            let target = imp.alias.clone();
            self.use_of(q.span, UseKind::Qualifier, target, w);
        }
    }

    // --- Statements ---

    fn node(&mut self, n: &ast::Node, w: &Walk) -> Option<RStmt> {
        match n {
            ast::Node::Expression(e) => Some(RStmt::Expr(self.expr(e, w))),
            ast::Node::Statement(s) => self.statement(s, w),
        }
    }

    fn statement(&mut self, s: &ast::Statement, w: &Walk) -> Option<RStmt> {
        match s {
            ast::Statement::Binding(b) => Some(match b.name() {
                Some(id) => self.name_binding(b, id, w),
                None => {
                    let value = self.expr(&b.init, w);
                    let annot = b.typ.as_ref().map(|t| {
                        let mut vars = AnnotVars::new(AnnotContext::Binding);
                        self.annotations(w).read(&mut vars, t, &mut self.report)
                    });
                    let pattern = self.pattern(&b.pattern, w);
                    RStmt::Let {
                        pattern,
                        annot,
                        value,
                        value_span: b.init.span(),
                        name: None,
                    }
                }
            }),
            ast::Statement::Declaration { decl, .. } => {
                let (kind, sp) = match decl.as_ref() {
                    ast::Declaration::Function(fd) => (NestedDecl::Function, fd.span),
                    ast::Declaration::Type(td) => (NestedDecl::Type, td.span),
                    ast::Declaration::Const(cb) => (NestedDecl::Const, cb.span),
                };
                self.report.add(sp, CheckError::NestedDeclaration(kind));
                None
            }
            ast::Statement::ImportDeclaration(_) => None,
            // The parser removes backpasses, so one that survives is one it
            // refused.
            ast::Statement::Backpass(bp) => {
                self.unreadable(bp.call.span(), Unreadable::Backpass);
                Some(RStmt::Expr(self.expr(&bp.call, w)))
            }
        }
    }

    /// Something the parser should have refused. It did, unless the tree was
    /// made some other way, and then this says so.
    fn unreadable(&mut self, span: Span, what: Unreadable) {
        if self.env.input.parsed_cleanly {
            self.report.add(span, CheckError::Unreadable(what));
        }
    }

    /// `x = e` or `x Type = e`. `x` is bound after `e`, so `e` still reads
    /// whatever `x` meant before. A function literal can call itself by it.
    fn name_binding(&mut self, b: &ast::Binding, id: &ast::Identifier, w: &Walk) -> RStmt {
        let annot = b.typ.as_ref().map(|t| {
            let mut vars = AnnotVars::new(AnnotContext::Binding);
            self.annotations(w).read(&mut vars, t, &mut self.report)
        });
        let discards = id.name.starts_with('_') && b.typ.is_none();
        let binder = self.new_binder(id, discards, b.doc.clone());
        let value = match &b.init {
            ast::Expression::FunctionExpression(fe) => {
                let kind = self.lambda(fe, Some((id.name.as_str().into(), binder)), w);
                RExpr {
                    span: fe.span,
                    kind,
                }
            }
            init => self.expr(init, w),
        };
        self.scope.insert(id.name.as_str().into(), binder);
        let pattern = RPattern {
            span: id.span,
            kind: if id.name.starts_with('_') {
                RPatKind::Wild
            } else {
                RPatKind::Bind(binder)
            },
        };
        RStmt::Let {
            pattern,
            annot,
            value,
            value_span: type_defining_span(&b.init),
            name: Some(binder),
        }
    }

    // --- Expressions ---

    pub fn expr(&mut self, e: &ast::Expression, w: &Walk) -> RExpr {
        let span = e.span();
        RExpr {
            span,
            kind: self.expr_kind(e, w),
        }
    }

    fn boxed(&mut self, e: &ast::Expression, w: &Walk) -> Box<RExpr> {
        Box::new(self.expr(e, w))
    }

    fn expr_kind(&mut self, e: &ast::Expression, w: &Walk) -> RKind {
        use ast::Expression as E;
        match e {
            E::NumberLiteral(n) => match number(n, &mut self.report) {
                Number::Int(i) => RKind::Int(i),
                Number::Float(f) => RKind::Float(f),
            },
            E::StringLiteral(s) => RKind::String(s.value.clone()),
            E::InterpolatedString(is) => RKind::Interp(
                is.parts
                    .iter()
                    .map(|p| match p {
                        ast::InterpPart::Literal(s) => RPart::Text(s.value.clone()),
                        ast::InterpPart::Expr(x) => RPart::Expr(self.expr(x, w)),
                    })
                    .collect(),
            ),
            E::BinaryLiteral(bl) => RKind::Binary(
                bl.segments
                    .iter()
                    .map(|seg| RBinSeg {
                        value: self.expr(&seg.value, w),
                        unit: match &seg.spec {
                            ast::BinSpec::Int { bits } => {
                                RUnit::Bits(bits.as_ref().map(|b| self.boxed(b, w)))
                            }
                            ast::BinSpec::Binary { bytes } => {
                                RUnit::Bytes(bytes.as_ref().map(|b| self.boxed(b, w)))
                            }
                            ast::BinSpec::Utf8 => RUnit::Utf8,
                        },
                    })
                    .collect(),
            ),
            E::Identifier(id) => match self.value(id, w) {
                Some(found) => RKind::Ref {
                    r: found.r,
                    at: id.span,
                },
                None => {
                    self.unknown_identifier(id);
                    RKind::Error
                }
            },
            E::ModuleAccessExpression(ma) => match self.member(ma, w) {
                Some(found) => RKind::Ref {
                    r: found.r,
                    at: ma.member.span,
                },
                None => RKind::Error,
            },
            E::BlockExpression(b) => self.block(b, w),
            E::BinaryExpression(b) => RKind::BinOp {
                op: b.op,
                left: self.boxed(&b.left, w),
                right: self.boxed(&b.right, w),
            },
            E::UnaryExpression(u) => RKind::Unary {
                op: u.op,
                operand: self.boxed(&u.expression, w),
            },
            E::IfExpression(ie) => RKind::If {
                cond: self.boxed(&ie.condition, w),
                then: self.boxed(&ie.body, w),
                els: self.boxed(&ie.else_body, w),
                then_span: type_defining_span(&ie.body),
                els_span: type_defining_span(&ie.else_body),
            },
            E::MatchExpression(m) => RKind::Match {
                subject: self.boxed(&m.subject, w),
                arms: m
                    .arms
                    .iter()
                    .map(|arm| {
                        self.in_block(|r| RArm {
                            pattern: r.pattern(&arm.pattern, w),
                            guard: arm
                                .guard
                                .as_ref()
                                .map(|g| (r.expr(g, w), type_defining_span(g))),
                            body: r.expr(&arm.body, w),
                            body_span: type_defining_span(&arm.body),
                        })
                    })
                    .collect(),
            },
            E::ArrayExpression(a) => RKind::Array(
                a.elements
                    .iter()
                    .map(|el| match el {
                        ast::ArrayElement::Expression(x) => RElem::Elem(self.expr(x, w)),
                        ast::ArrayElement::SpreadElement(s) => {
                            RElem::Spread(self.expr(&s.expression, w))
                        }
                    })
                    .collect(),
            ),
            E::TupleExpression(t) => {
                RKind::Tuple(t.elements.iter().map(|x| self.expr(x, w)).collect())
            }
            E::ArrayIndexExpression(ai) => match ai.index.as_ref() {
                E::RangeExpression(r) => RKind::Slice {
                    array: self.boxed(&ai.expression, w),
                    start: self.boxed(&r.start, w),
                    end: self.boxed(&r.end, w),
                },
                index => RKind::Index {
                    array: self.boxed(&ai.expression, w),
                    index: self.boxed(index, w),
                },
            },
            E::RangeExpression(r) => RKind::Range {
                start: self.boxed(&r.start, w),
                end: self.boxed(&r.end, w),
            },
            E::FunctionExpression(fe) => self.lambda(fe, None, w),
            E::FunctionCallExpression(fc) => self.call(fc, w),
            E::PropertyAccessExpression(pa) => self.property(pa, w),
            E::OrExpression(oe) => {
                let value = self.boxed(&oe.expression, w);
                let (receiver, fallback) = self.in_block(|r| {
                    let receiver = oe.receiver.as_ref().map(|id| (r.bind(id, false), id.span));
                    (receiver, r.boxed(&oe.body, w))
                });
                RKind::OrElse {
                    value,
                    receiver,
                    fallback,
                    fallback_span: type_defining_span(&oe.body),
                }
            }
            // The parser removes pipes, so one that survives is one it
            // refused.
            E::PipeExpression(p) => {
                self.expr(&p.left, w);
                self.expr(&p.right, w);
                self.unreadable(e.span(), Unreadable::Pipe);
                RKind::Error
            }
            E::ErrorNode(err) => {
                self.unreadable(err.span, Unreadable::Node(err.error.clone()));
                RKind::Error
            }
        }
    }

    fn block(&mut self, b: &ast::BlockExpression, w: &Walk) -> RKind {
        self.in_block(|r| {
            let mut stmts = Vec::with_capacity(b.body.len());
            let mut tail = None;
            for (i, node) in b.body.iter().enumerate() {
                match node {
                    ast::Node::Expression(e) if i + 1 == b.body.len() => tail = Some(r.boxed(e, w)),
                    node => stmts.extend(r.node(node, w)),
                }
            }
            RKind::Block { stmts, tail }
        })
    }

    fn lambda(
        &mut self,
        fe: &ast::FunctionExpression,
        self_name: Option<(Name, BinderId)>,
        w: &Walk,
    ) -> RKind {
        let self_binder = self_name.as_ref().map(|(_, b)| *b);
        let mut vars = AnnotVars::new(AnnotContext::Signature);
        let ((params, ret, body), captures) = self.in_frame(self_name, |r| {
            let read = r.annotations(w);
            let mut tys = Vec::with_capacity(fe.params.len());
            for p in &fe.params {
                tys.push(read.read_opt(&mut vars, p.typ.as_ref(), &mut r.report));
            }
            let ret = read.read_opt(&mut vars, fe.return_type.as_ref(), &mut r.report);
            let params: Vec<_> = fe
                .params
                .iter()
                .zip(tys)
                .map(|(p, ty)| (r.bind_param(p), ty))
                .collect();
            (params, ret, r.expr(&fe.body, w))
        });
        RKind::Lambda(Box::new(RLambda {
            params,
            ret,
            vars: vars.named().to_vec(),
            body,
            body_span: type_defining_span(&fe.body),
            captures,
            self_name: self_binder,
        }))
    }

    // --- Names ---

    /// What a bare name means: a local, then one of the module's own values
    /// or constructors, then the prelude's. Records the use.
    fn value(&mut self, id: &ast::Identifier, w: &Walk) -> Option<Found> {
        let name = id.name.as_str();
        if let Some(found) = self.scope.lookup(name) {
            let b = found.binder;
            let info = &self.binders[b.0 as usize];
            let (module_scope, span) = (info.module_scope, info.span);
            if found.frame < self.scope.depth() && !module_scope {
                self.scope.capture(b, found.frame);
            }
            self.used.insert(b);
            let target = self.env.site(span, EntityKind::Value);
            self.use_of(id.span, UseKind::Unqualified, target, w);
            if name.starts_with('_') {
                self.report
                    .add(id.span, CheckError::ReadsDiscarded(name.into()));
            }
            return Some(Found {
                r: Ref::Local(b),
                labels: None,
                ctor: None,
            });
        }
        if let Some(d) = self.declared.find(name) {
            let decl = self.declared.get(d);
            let target = self.env.site(decl.span, decl.entity());
            self.use_of(id.span, UseKind::Unqualified, target, w);
            if let Some(from) = w.decl {
                self.edges[from.0 as usize].insert(d);
            }
            let value = ValueName {
                module: self.env.key.clone(),
                name: name.into(),
            };
            let kind = decl.kind();
            return Some(Found {
                r: Ref::Own(d, var_of(&kind, value)),
                labels: call_labels(&kind),
                ctor: None,
            });
        }
        if let Some(c) = self.defs.ctors.get(name) {
            let target = self.env.site(c.span, EntityKind::Constructor);
            self.use_of(id.span, UseKind::Unqualified, target, w);
            let info = CtorInfo {
                ctor: c.ctor.clone(),
                name: name.into(),
                labels: c.labels.clone(),
                scheme: c.scheme.clone(),
            };
            return Some(Found {
                r: Ref::Ctor(info.ctor.clone(), info.scheme.clone()),
                labels: None,
                ctor: Some(info),
            });
        }
        let (key, v) = self.env.prelude_value(name)?;
        let target = Env::foreign_site(key, v);
        self.use_of(id.span, UseKind::Unqualified, target, w);
        Some(foreign(key, name, v))
    }

    /// `module::member`. `None` has been reported.
    fn member(&mut self, ma: &ast::ModuleAccessExpression, w: &Walk) -> Option<Found> {
        let Some(imp) = self.env.imports.get(&ma.module.name) else {
            self.unknown_module(&ma.module);
            return None;
        };
        let key = imp.key.clone();
        let module = self.env.imports.module_name(&key);
        let iface = self.env.interface_of(&key)?;
        let member = ma.member.name.as_str();
        let Some(v) = iface.values.get(member) else {
            if iface.private_names.contains(member) {
                self.report.add(
                    ma.member.span,
                    CheckError::PrivateMember {
                        module,
                        member: member.into(),
                    },
                );
            } else {
                self.report.add(
                    ma.member.span,
                    CheckError::NoMember {
                        module,
                        member: member.into(),
                    },
                );
            }
            return None;
        };
        let target = Env::foreign_site(&key, v);
        self.use_of(ma.member.span, UseKind::Qualified, target, w);
        self.qualifier_use(&ma.module, w);
        Some(foreign(&key, member, v))
    }

    fn unknown_module(&mut self, name: &ast::Identifier) {
        let stdlib = self
            .env
            .input
            .stdlib_modules
            .iter()
            .filter(|path| path.len() > 1 && path.last() == Some(&name.name))
            .cloned()
            .collect();
        self.report.add(
            name.span,
            CheckError::UnknownModule {
                name: name.name.as_str().into(),
                stdlib,
            },
        );
    }

    fn unknown_identifier(&mut self, id: &ast::Identifier) {
        let name = &id.name;
        if let Some(imp) = self.env.imports.get(name) {
            let module = self.env.imports.module_name(&imp.key);
            let example = self.env.interface_of(&imp.key).and_then(|m| {
                m.values
                    .keys()
                    .filter(|k| k.starts_with(|c: char| c.is_ascii_lowercase()))
                    .min()
                    .cloned()
            });
            self.report.add(
                id.span,
                CheckError::ModuleNotValue {
                    name: name.as_str().into(),
                    module,
                    example,
                },
            );
            return;
        }
        let ctor_in = self.env.qualifiers_with_ctor(name);
        let suggestion = if ctor_in.is_empty() {
            self.suggest(name).map(|s| s.as_str().into())
        } else {
            None
        };
        self.report.add(
            id.span,
            CheckError::UnknownIdentifier {
                name: name.as_str().into(),
                ctor_in,
                suggestion,
            },
        );
    }

    fn suggest(&self, name: &str) -> Option<String> {
        let mut candidates: Vec<&str> = Vec::new();
        if let Some(p) = self.env.prelude {
            candidates.extend(p.values.keys().map(|k| &**k));
        }
        candidates.extend(self.declared.values.iter().map(|d| &*d.name));
        candidates.extend(self.defs.ctors.keys().map(|k| &**k));
        candidates.extend(self.scope.names().map(|n| &**n));
        closest_name(name, candidates.into_iter()).map(str::to_string)
    }

    /// Whether a bare name means any value here.
    pub fn is_value_name(&self, name: &str) -> bool {
        self.scope.lookup(name).is_some()
            || self.declared.find(name).is_some()
            || self.defs.ctors.contains_key(name)
            || self.env.prelude_value(name).is_some()
    }

    // --- Calls ---

    fn call(&mut self, fc: &ast::FunctionCallExpression, w: &Walk) -> RKind {
        let found = match fc.callee.as_ref() {
            ast::Expression::Identifier(id) => match self.value(id, w) {
                Some(f) => Some((f, id.span)),
                None => {
                    self.unknown_identifier(id);
                    return self.args_of_nothing(fc, w);
                }
            },
            ast::Expression::ModuleAccessExpression(ma) => match self.member(ma, w) {
                Some(f) => Some((f, ma.member.span)),
                None => return self.args_of_nothing(fc, w),
            },
            _ => None,
        };
        let (callee, labels) = match found {
            Some((f, span)) => {
                if let Some(c) = f.ctor {
                    return self.construct(c, &fc.arguments, fc.span, span, w);
                }
                (
                    RExpr {
                        span,
                        kind: RKind::Ref { r: f.r, at: span },
                    },
                    f.labels,
                )
            }
            None => (self.expr(&fc.callee, w), None),
        };
        let args = self.args(&fc.arguments, labels.as_deref(), w);
        RKind::Call {
            callee: Box::new(callee),
            args,
        }
    }

    /// The arguments of a call whose callee didn't resolve, resolved so
    /// their own names count.
    fn args_of_nothing(&mut self, fc: &ast::FunctionCallExpression, w: &Walk) -> RKind {
        let args = self.args(&fc.arguments, None, w);
        RKind::Call {
            callee: Box::new(RExpr {
                span: fc.callee.span(),
                kind: RKind::Error,
            }),
            args,
        }
    }

    /// A call's arguments, each with the parameter it fills. `labels` is the
    /// callee's parameter names, when a call may label its arguments.
    fn args(&mut self, args: &[ast::CallArg], labels: Option<&[Name]>, w: &Walk) -> Vec<RArg> {
        let labelled = args
            .iter()
            .any(|a| matches!(a, ast::CallArg::Labeled { .. }));
        let slots: Vec<Option<usize>> = match labels {
            Some(names) if labelled => self.slot_args(names, args),
            _ => (0..args.len()).map(Some).collect(),
        };
        args.iter()
            .zip(slots)
            .enumerate()
            .map(|(i, (a, slot))| {
                let value = match a {
                    ast::CallArg::Positional(e) => self.expr(e, w),
                    ast::CallArg::Labeled { label, value, .. } => {
                        if labels.is_none() {
                            self.report
                                .add(label.span, CheckError::LabelNeedsKnownCallee);
                        }
                        self.expr(value, w)
                    }
                    ast::CallArg::Spread(e) => {
                        self.report
                            .add(e.span(), CheckError::SpreadOutsideConstructor);
                        self.expr(e, w)
                    }
                };
                RArg {
                    slot: slot.unwrap_or(i) as u32,
                    value,
                    fills: slot.is_some(),
                }
            })
            .collect()
    }

    /// Which parameter each argument fills, reporting labels that name none
    /// and parameters given twice.
    fn slot_args(&mut self, names: &[Name], args: &[ast::CallArg]) -> Vec<Option<usize>> {
        let items: Vec<(Option<&str>, (usize, Span))> = args
            .iter()
            .enumerate()
            .filter_map(|(i, a)| match a {
                ast::CallArg::Positional(e) => Some((None, (i, e.span()))),
                ast::CallArg::Labeled { label, .. } => {
                    Some((Some(label.name.as_str()), (i, label.span)))
                }
                ast::CallArg::Spread(_) => None,
            })
            .collect();
        let fields: Vec<Option<&str>> = names.iter().map(|n| Some(&**n)).collect();
        let (by_pos, errors) = slot_labeled(&fields, items);
        for e in errors {
            match e {
                // A wrong count, which inference reports.
                SlotError::ExtraPositional(_) => {}
                SlotError::UnknownLabel(label, (_, span)) => self.report.add(
                    span,
                    CheckError::NoSuchParameter {
                        label: label.into(),
                        available: names.to_vec(),
                    },
                ),
                SlotError::Duplicate((_, span), p) => {
                    let dup = names.get(p).map_or("_", |n| &**n);
                    self.report
                        .add(span, CheckError::ParameterTwice(dup.into()));
                }
            }
        }
        let mut out = vec![None; args.len()];
        for (p, slot) in by_pos.into_iter().enumerate() {
            if let Some((i, _)) = slot {
                out[i] = Some(p);
            }
        }
        out
    }

    /// A constructor applied to arguments: positional, labelled, and at most
    /// one `..base` for a record update.
    fn construct(
        &mut self,
        c: CtorInfo,
        args: &[ast::CallArg],
        span: Span,
        at: Span,
        w: &Walk,
    ) -> RKind {
        let mut spread: Option<(u32, &ast::Expression)> = None;
        let mut written = 0u32;
        for a in args {
            match a {
                ast::CallArg::Spread(e) => {
                    if spread.is_some() {
                        self.report.add(e.span(), CheckError::TwoSpreadsInCall);
                    }
                    spread = Some((written, e));
                }
                ast::CallArg::Positional(_) | ast::CallArg::Labeled { .. } => written += 1,
            }
        }
        let items: Vec<(Option<&str>, (usize, Span))> = args
            .iter()
            .enumerate()
            .filter_map(|(i, a)| match a {
                ast::CallArg::Positional(e) => Some((None, (i, e.span()))),
                ast::CallArg::Labeled { label, .. } => {
                    Some((Some(label.name.as_str()), (i, label.span)))
                }
                ast::CallArg::Spread(_) => None,
            })
            .collect();
        let by_pos = self.slot_ctor(
            &c,
            items,
            spread.is_none().then_some((span, FieldSite::Call)),
        );
        let mut slot_of = vec![None; args.len()];
        for (slot, i) in by_pos.into_iter().enumerate() {
            if let Some(i) = i {
                slot_of[i] = Some(slot as u32);
            }
        }
        let spread = spread.map(|(at, e)| (at, self.boxed(e, w)));
        let args = args
            .iter()
            .zip(slot_of)
            .enumerate()
            .filter_map(|(i, (a, slot))| {
                let value = match a {
                    ast::CallArg::Positional(e) => self.expr(e, w),
                    ast::CallArg::Labeled { value, .. } => self.expr(value, w),
                    ast::CallArg::Spread(_) => return None,
                };
                Some(RArg {
                    slot: slot.unwrap_or(i as u32),
                    value,
                    fills: slot.is_some(),
                })
            })
            .collect();
        RKind::Construct(Box::new(RConstruct {
            ctor: c.ctor,
            scheme: c.scheme,
            labels: c.labels,
            args,
            spread,
            at,
        }))
    }

    /// Put a constructor's arguments, positional and labelled, into the order
    /// its fields are declared in. With `missing`, a field nothing gives is an
    /// error too. Calls and patterns both come through here.
    pub fn slot_ctor<T>(
        &mut self,
        c: &CtorInfo,
        items: Vec<(Option<&str>, (T, Span))>,
        missing: Option<(Span, FieldSite)>,
    ) -> Vec<Option<T>> {
        let fields: Vec<Option<&str>> = c.labels.iter().map(|l| Some(&**l)).collect();
        let arity = fields.len();
        let (by_pos, errors) = slot_labeled(&fields, items);
        for e in errors {
            match e {
                SlotError::ExtraPositional((_, span)) => self.report.add(
                    span,
                    CheckError::TooManyFields {
                        ctor: c.name.clone(),
                        arity,
                    },
                ),
                SlotError::UnknownLabel(label, (_, span)) => self.report.add(
                    span,
                    CheckError::NoSuchField {
                        ctor: c.name.clone(),
                        label: label.into(),
                        available: c.labels.clone(),
                    },
                ),
                SlotError::Duplicate((_, span), field) => {
                    let dup = c.labels.get(field).map_or("_", |l| &**l);
                    self.report.add(span, CheckError::FieldTwice(dup.into()));
                }
            }
        }
        if let Some((span, site)) = missing
            && by_pos.iter().any(Option::is_none)
        {
            let absent: Vec<Name> = (0..arity)
                .filter(|&i| by_pos[i].is_none())
                .map(|i| c.labels.get(i).cloned().unwrap_or_else(|| "_".into()))
                .collect();
            self.report.add(
                span,
                CheckError::MissingFields {
                    ctor: c.name.clone(),
                    fields: absent,
                    site,
                },
            );
        }
        by_pos.into_iter().map(|s| s.map(|(v, _)| v)).collect()
    }

    // --- Fields ---

    fn property(&mut self, pa: &ast::PropertyAccessExpression, w: &Walk) -> RKind {
        if let (ast::Expression::Identifier(id), ast::PropertyKey::Field(field)) =
            (pa.left.as_ref(), &pa.right)
            && !self.is_value_name(&id.name)
            && self.env.imports.get(&id.name).is_some()
        {
            self.report.add(
                id.span,
                CheckError::ModuleField {
                    module: id.name.as_str().into(),
                    field: field.name.as_str().into(),
                },
            );
            return RKind::Error;
        }
        let record = self.boxed(&pa.left, w);
        match &pa.right {
            ast::PropertyKey::TupleIndex(num) => match num.value.parse::<u32>() {
                Ok(index) => RKind::TupleIndex {
                    tuple: record,
                    index,
                    index_span: num.span,
                },
                Err(_) => {
                    self.report
                        .add(num.span, CheckError::TupleIndexNotInt(num.value.clone()));
                    RKind::Error
                }
            },
            ast::PropertyKey::Field(field) => RKind::Field {
                record,
                label: field.name.as_str().into(),
                label_span: field.span,
            },
        }
    }
}

/// A value another module declares.
fn foreign(
    key: &scarlet_types::name::ModuleKey,
    name: &str,
    v: &scarlet_types::interface::ValueDef,
) -> Found {
    match &v.kind {
        ValueKind::Constructor { ctor, name, labels } => {
            let info = CtorInfo {
                ctor: ctor.clone(),
                name: name.clone(),
                labels: labels.clone(),
                scheme: v.scheme.clone(),
            };
            Found {
                r: Ref::Ctor(info.ctor.clone(), info.scheme.clone()),
                labels: None,
                ctor: Some(info),
            }
        }
        kind => Found {
            r: Ref::Foreign(
                var_of(
                    kind,
                    ValueName {
                        module: key.clone(),
                        name: name.into(),
                    },
                ),
                v.scheme.clone(),
            ),
            labels: call_labels(kind),
            ctor: None,
        },
    }
}

/// The parameter names a call to a value of `kind` may label its arguments
/// with. Only a function named directly has them.
fn call_labels(kind: &ValueKind) -> Option<Vec<Name>> {
    match kind {
        ValueKind::Fn { params } if !params.is_empty() => Some(params.clone()),
        ValueKind::Fn { .. }
        | ValueKind::Builtin { .. }
        | ValueKind::Const
        | ValueKind::Constructor { .. } => None,
    }
}

fn var_of(kind: &ValueKind, value: ValueName) -> Var {
    match kind {
        ValueKind::Fn { .. } => Var::Fn(value),
        ValueKind::Const => Var::Const(value),
        ValueKind::Builtin { intrinsic, .. } => Var::Builtin(*intrinsic),
        ValueKind::Constructor { ctor, .. } => Var::Ctor(ctor.clone()),
    }
}

/// The span whose type decides a body's type: its last expression.
fn type_defining_span(expr: &ast::Expression) -> Span {
    match expr {
        ast::Expression::BlockExpression(b) => match b.body.last() {
            Some(ast::Node::Expression(e)) => type_defining_span(e),
            Some(n) => n.span(),
            None => b.span,
        },
        _ => expr.span(),
    }
}

/// A number literal's value.
pub(crate) enum Number {
    Int(i64),
    Float(f64),
}

/// The value of a number literal. Out of range is reported, and gives a zero
/// of the same kind so checking carries on.
pub(crate) fn number(n: &ast::NumberLiteral, report: &mut Report) -> Number {
    let digits = n.digits();
    if digits.contains('.') {
        match digits.parse() {
            Ok(f) => Number::Float(f),
            Err(_) => {
                report.add(n.span, CheckError::InvalidNumber(n.value.clone()));
                Number::Float(0.0)
            }
        }
    } else {
        match n.as_int() {
            Some(i) => Number::Int(i),
            None => {
                report.add(n.span, CheckError::IntOutOfRange(n.value.clone()));
                Number::Int(0)
            }
        }
    }
}

/// The candidate closest to `name` by edit distance, if any is close enough
/// to be what was meant. Ties go to the earliest. This is rustc's rule: an
/// edit distance up to `max(len, 3) / 3`.
fn closest_name<'n>(name: &str, candidates: impl Iterator<Item = &'n str>) -> Option<&'n str> {
    let mut best = None;
    let mut best_dist = std::cmp::max(name.chars().count(), 3) / 3 + 1;
    for c in candidates {
        let d = levenshtein(name, c);
        if d < best_dist {
            best_dist = d;
            best = Some(c);
        }
    }
    best
}

fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut curr = vec![0; b.len() + 1];
    for i in 1..=a.len() {
        curr[0] = i;
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b.len()]
}
