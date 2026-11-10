//! The order a module's functions and consts are inferred in. A declaration
//! is inferred after every declaration it uses, and a group that uses each
//! other is inferred together, so mutual recursion checks against one shared
//! type inside the group and a generic one outside it.
//!
//! The edges come from a walk over the source that keeps track of which names
//! a local binding hides, as Gleam's call graph does. It only decides order:
//! the checker resolves every name itself afterwards.

use std::collections::{HashMap, HashSet};

use petgraph::Directed;
use petgraph::algo::tarjan_scc;
use petgraph::stable_graph::{NodeIndex, StableGraph};
use scarlet_syntax::ast;

/// One declaration's name and what to walk for its uses.
pub(crate) enum Body<'a> {
    Fn {
        params: &'a [ast::FunctionParameter],
        body: &'a ast::Expression,
    },
    Const(&'a ast::Expression),
}

/// The groups, as indexes into `decls`, each after the groups it uses.
pub(crate) fn groups(decls: &[(&str, Body<'_>)]) -> Vec<Vec<usize>> {
    let mut graph: StableGraph<(), (), Directed> = StableGraph::new();
    let mut node_of: HashMap<&str, NodeIndex> = HashMap::new();
    for (name, _) in decls {
        let n = graph.add_node(());
        node_of.insert(name, n);
    }
    for (name, body) in decls {
        let from = node_of[name];
        let mut w = Walker {
            targets: &node_of,
            graph: &mut graph,
            from,
            shadowed: HashSet::new(),
            undo: Vec::new(),
        };
        match body {
            Body::Fn { params, body } => {
                for p in *params {
                    w.define(&p.identifier.name);
                }
                w.expr(body);
            }
            Body::Const(init) => w.expr(init),
        }
    }
    tarjan_scc(&graph)
        .into_iter()
        .map(|c| c.into_iter().map(|n| n.index()).collect())
        .collect()
}

struct Walker<'a, 'g> {
    targets: &'a HashMap<&'a str, NodeIndex>,
    graph: &'g mut StableGraph<(), (), Directed>,
    from: NodeIndex,
    shadowed: HashSet<&'a str>,
    /// Names just added to `shadowed`, so leaving a scope can take them out
    /// again without copying the set.
    undo: Vec<&'a str>,
}

impl<'a> Walker<'a, '_> {
    fn define(&mut self, name: &'a str) {
        if self.shadowed.insert(name) {
            self.undo.push(name);
        }
    }

    fn referenced(&mut self, name: &str) {
        if self.shadowed.contains(name) {
            return;
        }
        if let Some(&to) = self.targets.get(name) {
            self.graph.add_edge(self.from, to, ());
        }
    }

    fn scoped(&mut self, f: impl FnOnce(&mut Self)) {
        let mark = self.undo.len();
        f(self);
        let len = self.undo.len();
        for name in self.undo.drain(mark.min(len)..) {
            self.shadowed.remove(name);
        }
    }

    fn expr(&mut self, e: &'a ast::Expression) {
        use ast::Expression as E;
        match e {
            E::Identifier(id) => self.referenced(&id.name),
            E::NumberLiteral(_)
            | E::StringLiteral(_)
            | E::ErrorNode(_)
            | E::ModuleAccessExpression(_) => {}
            E::BinaryLiteral(bl) => {
                for seg in &bl.segments {
                    self.expr(&seg.value);
                    if let Some(sz) = seg.spec.size_expr() {
                        self.expr(sz);
                    }
                }
            }
            E::InterpolatedString(is) => {
                for p in &is.parts {
                    if let ast::InterpPart::Expr(e) = p {
                        self.expr(e);
                    }
                }
            }
            E::ArrayExpression(a) => {
                for el in &a.elements {
                    match el {
                        ast::ArrayElement::Expression(e) => self.expr(e),
                        ast::ArrayElement::SpreadElement(s) => self.expr(&s.expression),
                    }
                }
            }
            E::TupleExpression(t) => t.elements.iter().for_each(|e| self.expr(e)),
            E::ArrayIndexExpression(ai) => {
                self.expr(&ai.expression);
                self.expr(&ai.index);
            }
            E::RangeExpression(r) => {
                self.expr(&r.start);
                self.expr(&r.end);
            }
            E::BinaryExpression(b) => {
                self.expr(&b.left);
                self.expr(&b.right);
            }
            E::UnaryExpression(u) => self.expr(&u.expression),
            E::PropertyAccessExpression(p) => self.expr(&p.left),
            E::FunctionCallExpression(fc) => {
                self.expr(&fc.callee);
                for arg in &fc.arguments {
                    match arg {
                        ast::CallArg::Positional(e) | ast::CallArg::Spread(e) => self.expr(e),
                        ast::CallArg::Labeled { value, .. } => self.expr(value),
                    }
                }
            }
            E::IfExpression(ie) => {
                self.expr(&ie.condition);
                self.expr(&ie.body);
                self.expr(&ie.else_body);
            }
            E::OrExpression(oe) => {
                self.expr(&oe.expression);
                self.scoped(|w| {
                    if let Some(recv) = &oe.receiver {
                        w.define(&recv.name);
                    }
                    w.expr(&oe.body);
                });
            }
            E::PipeExpression(p) => {
                self.expr(&p.left);
                self.expr(&p.right);
            }
            E::MatchExpression(me) => {
                self.expr(&me.subject);
                for arm in &me.arms {
                    self.scoped(|w| {
                        w.pattern(&arm.pattern);
                        if let Some(g) = &arm.guard {
                            w.expr(g);
                        }
                        w.expr(&arm.body);
                    });
                }
            }
            E::FunctionExpression(fe) => self.scoped(|w| {
                for p in &fe.params {
                    w.define(&p.identifier.name);
                }
                w.expr(&fe.body);
            }),
            E::BlockExpression(be) => self.scoped(|w| {
                for node in &be.body {
                    w.node(node);
                }
            }),
        }
    }

    fn node(&mut self, n: &'a ast::Node) {
        match n {
            ast::Node::Expression(e) => self.expr(e),
            ast::Node::Statement(s) => match s.as_ref() {
                ast::Statement::Binding(b) => {
                    self.expr(&b.init);
                    self.pattern(&b.pattern);
                }
                ast::Statement::Declaration { .. } | ast::Statement::ImportDeclaration(_) => {}
                ast::Statement::Backpass(bp) => {
                    self.expr(&bp.call);
                    for b in &bp.binders {
                        self.pattern(b);
                    }
                }
            },
        }
    }

    fn pattern(&mut self, p: &'a ast::Pattern) {
        p.for_each_binder(ast::OrAlternatives::All, &mut |b| match b {
            ast::PatternBinder::Name(id) => self.define(&id.name),
            ast::PatternBinder::SizeExpr(sz) => self.expr(sz),
        });
    }
}
