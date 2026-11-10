//! Expressions and statements.

use scarlet_syntax::ast;
use scarlet_syntax::span::Span;
use scarlet_types::def::TypeDef;
use scarlet_types::engine::{LocalScheme, NotCallable, Shape, Ty};
use scarlet_types::interface::ValueKind;
use scarlet_types::name::{CtorName, Name, ValueName};
use scarlet_types::slots::{SlotError, slot_labeled};
use scarlet_types::ty::Constraint;

use crate::annot::{Annot, AnnotContext};
use crate::check::{Checker, entity_of};
use crate::index::{EntityKind, Site, UseKind};
use crate::pattern::{CtorRef, Number, PatBinds};
use crate::typed::{
    Arg, ArrayElem, Arm, BinOp, BinSeg, BinUnit, BinderId, Block, Expr, ExprKind, FieldAt, InterpPart,
    Lambda, OrShape, Pattern, PatternKind, Spread, Stmt, Var,
};

/// The span whose type decides a body's type: its last expression.
pub(crate) fn type_defining_span(expr: &ast::Expression) -> Span {
    match expr {
        ast::Expression::BlockExpression(b) => match b.body.last() {
            Some(ast::Node::Expression(e)) => type_defining_span(e),
            Some(n) => n.span(),
            None => b.span,
        },
        _ => expr.span(),
    }
}

/// One written constructor argument, for slotting: its label, and its index
/// among the arguments with its value.
type SlotItem<'a> = (Option<&'a str>, ((usize, &'a ast::Expression), Span));

/// A name in value position, resolved.
struct Resolved {
    var: Var,
    ty: Ty,
    /// What the name is, for a module-level one.
    kind: Option<ValueKind>,
}

/// Why a type has no readable field.
enum FieldMissing {
    /// It has no variants.
    NotCustom,
    /// This variant lacks the field.
    On(Name),
}

impl Checker<'_> {
    pub fn check_expr(&mut self, expr: &ast::Expression) -> Expr {
        let span = expr.span();
        let (ty, kind) = self.check_expr_kind(expr);
        Expr { ty, span, kind }
    }

    /// Check `expr` knowing the type it should have. Only a function literal
    /// uses the hint: its parameters take their types from it, so the body
    /// can read fields of them.
    pub fn check_expr_hinted(&mut self, expr: &ast::Expression, hint: Option<Ty>) -> Expr {
        if let Some(h) = hint
            && let ast::Expression::FunctionExpression(fe) = expr
            && let Shape::Fn(params, _) = self.e.shape(h)
            && params.len() == fe.params.len()
        {
            let (ty, lambda) = self.check_lambda(fe, Some(params));
            return Expr {
                ty,
                span: fe.span,
                kind: ExprKind::Lambda(lambda),
            };
        }
        self.check_expr(expr)
    }

    fn error_expr(&mut self) -> (Ty, ExprKind) {
        (self.e.fresh(), ExprKind::Error)
    }

    fn check_expr_kind(&mut self, expr: &ast::Expression) -> (Ty, ExprKind) {
        use ast::Expression as E;
        match expr {
            E::NumberLiteral(n) => match self.number(n) {
                Number::Int(i) => (self.e.int(), ExprKind::Int(i)),
                Number::Float(f) => (self.e.float(), ExprKind::Float(f)),
            },
            E::StringLiteral(s) => (self.e.string(), ExprKind::String(s.value.clone())),
            E::InterpolatedString(is) => {
                let parts = is
                    .parts
                    .iter()
                    .map(|p| match p {
                        ast::InterpPart::Literal(s) => InterpPart::Text(s.value.clone()),
                        ast::InterpPart::Expr(e) => InterpPart::Expr(self.check_expr(e)),
                    })
                    .collect();
                (self.e.string(), ExprKind::Interp(parts))
            }
            E::BinaryLiteral(bl) => self.check_binary_literal(bl),
            E::Identifier(id) => self.check_identifier(id),
            E::BlockExpression(b) => self.check_block(b),
            E::BinaryExpression(b) => self.check_binary(b),
            E::UnaryExpression(u) => {
                let operand = self.check_expr(&u.expression);
                let ty = match u.op {
                    ast::UnaryOp::Not => {
                        let b = self.e.bool();
                        self.unify(b, operand.ty, u.expression.span());
                        b
                    }
                    ast::UnaryOp::Neg => {
                        let n = self.e.fresh_constrained(Constraint::Numeric);
                        self.unify(n, operand.ty, u.expression.span());
                        n
                    }
                };
                (
                    ty,
                    ExprKind::Unary {
                        op: u.op,
                        operand: Box::new(operand),
                    },
                )
            }
            E::IfExpression(ie) => {
                let cond = self.check_expr(&ie.condition);
                let b = self.e.bool();
                self.unify(b, cond.ty, ie.condition.span());
                let then = self.check_expr(&ie.body);
                let els = self.check_expr(&ie.else_body);
                let ret = self.e.fresh();
                self.unify(ret, then.ty, type_defining_span(&ie.body));
                self.unify(ret, els.ty, type_defining_span(&ie.else_body));
                (
                    ret,
                    ExprKind::If {
                        cond: Box::new(cond),
                        then: Box::new(then),
                        els: Box::new(els),
                    },
                )
            }
            E::MatchExpression(m) => self.check_match(m),
            E::ArrayExpression(a) => self.check_array(a),
            E::TupleExpression(t) => {
                let elems: Vec<Expr> = t.elements.iter().map(|e| self.check_expr(e)).collect();
                let tys = elems.iter().map(|e| e.ty).collect();
                (self.e.tuple(tys), ExprKind::Tuple(elems))
            }
            E::ArrayIndexExpression(ai) => {
                let array = self.check_expr(&ai.expression);
                let elem = self.e.fresh();
                let arr = self.e.array(elem);
                self.unify(arr, array.ty, ai.expression.span());
                if let E::RangeExpression(r) = ai.index.as_ref() {
                    let start = self.check_int(&r.start);
                    let end = self.check_int(&r.end);
                    // A slice fails when its range isn't inside the array.
                    let slice = self.e.array(elem);
                    let nil = self.e.nil();
                    let result = self.known_result(slice, nil);
                    (
                        result,
                        ExprKind::Slice {
                            array: Box::new(array),
                            start: Box::new(start),
                            end: Box::new(end),
                        },
                    )
                } else {
                    let index = self.check_int(&ai.index);
                    let option = self.known_option(elem);
                    (
                        option,
                        ExprKind::Index {
                            array: Box::new(array),
                            index: Box::new(index),
                        },
                    )
                }
            }
            E::RangeExpression(r) => {
                let start = self.check_int(&r.start);
                let end = self.check_int(&r.end);
                let int = self.e.int();
                (
                    self.e.array(int),
                    ExprKind::Range {
                        start: Box::new(start),
                        end: Box::new(end),
                    },
                )
            }
            E::FunctionExpression(fe) => {
                let (ty, lambda) = self.check_lambda(fe, None);
                (ty, ExprKind::Lambda(lambda))
            }
            E::FunctionCallExpression(fc) => self.check_call(fc),
            E::PropertyAccessExpression(pa) => self.check_property(pa),
            E::ModuleAccessExpression(ma) => match self.module_member(ma) {
                Some(r) => (r.ty, ExprKind::Var(r.var)),
                None => self.error_expr(),
            },
            E::OrExpression(oe) => self.check_or(oe),
            // Pipes are removed before checking. One that survives is checked
            // for its operands' own errors.
            E::PipeExpression(p) => {
                self.check_expr(&p.left);
                self.check_expr(&p.right);
                self.has_error_nodes = true;
                self.error_expr()
            }
            // Nothing in it can be checked, and the module can't be clean. The
            // parser reported it, unless the tree was made some other way.
            E::ErrorNode(err) => {
                self.has_error_nodes = true;
                if self.input.parsed_cleanly {
                    let mut d = scarlet_syntax::diagnostic::Diagnostic::error(
                        err.span,
                        scarlet_syntax::diagnostic::DiagnosticCode::ParseError,
                        err.error.to_string(),
                    );
                    d.source = Some(self.key.clone());
                    self.diags.push(d);
                }
                self.error_expr()
            }
        }
    }

    fn check_int(&mut self, e: &ast::Expression) -> Expr {
        let x = self.check_expr(e);
        let int = self.e.int();
        self.unify(int, x.ty, e.span());
        x
    }

    pub fn known_option(&mut self, t: Ty) -> Ty {
        let n = self.e.known().option.clone();
        self.e.con(n, vec![t])
    }

    pub fn known_result(&mut self, ok: Ty, err: Ty) -> Ty {
        let n = self.e.known().result.clone();
        self.e.con(n, vec![ok, err])
    }

    // --- Blocks and statements ---

    fn check_block(&mut self, b: &ast::BlockExpression) -> (Ty, ExprKind) {
        self.scope.open_block();
        let mut stmts = Vec::with_capacity(b.body.len());
        let mut tail = None;
        for (i, node) in b.body.iter().enumerate() {
            let last = i + 1 == b.body.len();
            match node {
                ast::Node::Expression(e) => {
                    let x = self.check_expr(e);
                    if last {
                        tail = Some(Box::new(x));
                    } else {
                        stmts.push(Stmt::Expr(x));
                    }
                }
                ast::Node::Statement(s) => {
                    if let Some(stmt) = self.check_statement(s) {
                        stmts.push(stmt);
                    }
                }
            }
        }
        self.scope.close_block();
        let ty = match &tail {
            Some(t) => t.ty,
            None => self.e.nil(),
        };
        (ty, ExprKind::Block(Block { stmts, tail }))
    }

    /// The module's own statements, for an entry module or a script. They run
    /// in source order after the declarations.
    pub fn check_statements(&mut self, nodes: &[&ast::Node]) -> Vec<Stmt> {
        let mut out = Vec::with_capacity(nodes.len());
        for node in nodes {
            match node {
                ast::Node::Expression(e) => out.push(Stmt::Expr(self.check_expr(e))),
                ast::Node::Statement(s) => {
                    if let Some(stmt) = self.check_statement(s) {
                        out.push(stmt);
                    }
                }
            }
        }
        out
    }

    fn check_statement(&mut self, stmt: &ast::Statement) -> Option<Stmt> {
        match stmt {
            ast::Statement::Binding(b) => Some(match b.name() {
                Some(id) => self.check_name_binding(b, id),
                None => self.check_destructuring(b),
            }),
            ast::Statement::Declaration { decl, .. } => {
                let (kind, sp) = match decl.as_ref() {
                    ast::Declaration::Function(fd) => ("named function", fd.span),
                    ast::Declaration::Type(td) => ("type", td.span),
                    ast::Declaration::Const(cb) => ("const", cb.span),
                };
                self.error(sp, format!("{kind} declarations are only allowed at the top level"));
                None
            }
            ast::Statement::ImportDeclaration(_) => None,
            // Backpasses are removed before checking. One that survives was
            // refused by the parser, so check its call for its own errors.
            ast::Statement::Backpass(bp) => {
                self.has_error_nodes = true;
                Some(Stmt::Expr(self.check_expr(&bp.call)))
            }
        }
    }

    /// `x = e` or `x Type = e`. The name is bound after `e` is checked, so
    /// `e` still reads whatever `x` meant before (`x = x + 1`). A function
    /// literal can call itself by the name.
    fn check_name_binding(&mut self, b: &ast::Binding, id: &ast::Identifier) -> Stmt {
        let annotated = b.typ.as_ref().map(|t| {
            let mut a = Annot::new(AnnotContext::Binding);
            self.hydrate(&mut a, t)
        });
        self.e.enter_level();
        let self_ty = self.e.fresh();
        let binder = self.new_binder(&id.name, LocalScheme::mono(self_ty), id.span);
        let is_fn = matches!(b.init, ast::Expression::FunctionExpression(_));
        let saved = std::mem::replace(
            &mut self.pending_self,
            is_fn.then(|| (Name::from(id.name.as_str()), binder)),
        );
        let value = self.check_expr_hinted(&b.init, annotated);
        self.pending_self = saved;
        self.unify(self_ty, value.ty, b.init.span());
        self.e.leave_level();
        let ty = match annotated {
            Some(a) => {
                self.unify(a, value.ty, type_defining_span(&b.init));
                a
            }
            None => value.ty,
        };
        let r = self.restricted.clone();
        let scheme = self.e.generalize(ty, &r);
        let state = &mut self.binders[binder.0 as usize];
        state.scheme = scheme;
        // A `_name` throws its value away, and is held to the rule a `_` in a
        // pattern is: a value known to be `Nil` is matched with `Nil`. A
        // written type already says what is thrown away.
        state.discards = id.name.starts_with('_') && b.typ.is_none();
        self.scope.insert(id.name.as_str().into(), binder);
        let kind = if id.name.starts_with('_') {
            PatternKind::Wild
        } else {
            PatternKind::Bind(binder)
        };
        Stmt::Let {
            pattern: Pattern {
                ty,
                span: id.span,
                kind,
            },
            value,
        }
    }

    /// `(a, b) = e` or `Point(x:, ..) = e`: a `match` with one arm, which has
    /// to cover every value.
    fn check_destructuring(&mut self, b: &ast::Binding) -> Stmt {
        let value = self.check_expr(&b.init);
        let mut pb = PatBinds::default();
        let (mut pattern, ok) = self.check_pattern(&b.pattern, value.ty, &mut pb);
        if !ok {
            pattern.kind = PatternKind::Error;
        }
        Stmt::Let { pattern, value }
    }

    // --- Names ---

    fn check_identifier(&mut self, id: &ast::Identifier) -> (Ty, ExprKind) {
        let Some(r) = self.resolve_value(id) else {
            self.unknown_identifier(id);
            return self.error_expr();
        };
        if let Var::Local(_) = r.var
            && id.name.starts_with('_')
        {
            let msg = match id.name.trim_start_matches('_') {
                "" => format!(
                    "'{}' is a discard binding and can't be read. Give it a name to use its value",
                    id.name
                ),
                stripped => format!(
                    "'{}' starts with '_', which marks it unused, so it can't be read. Rename it to '{stripped}'",
                    id.name
                ),
            };
            self.error(id.span, msg);
            return (r.ty, ExprKind::Error);
        }
        (r.ty, ExprKind::Var(r.var))
    }

    fn unknown_identifier(&mut self, id: &ast::Identifier) {
        let name = &id.name;
        // A module's name isn't a value, so say what it is.
        if let Some(key) = self.qualifiers.get(name.as_str()).cloned() {
            let module = self.module_name(&key);
            let example = self.interface_of(&key).and_then(|m| {
                m.values
                    .keys()
                    .filter(|k| k.starts_with(|c: char| c.is_ascii_lowercase()))
                    .min()
                    .cloned()
            });
            let example = match example {
                Some(member) => format!(", like `{name}::{member}`"),
                None => String::new(),
            };
            self.error(
                id.span,
                format!("'{name}' is the module `{module}`, not a value. Use one of its members{example}"),
            );
            return;
        }
        let qualified = self.qualified_spellings(name);
        let msg = if !qualified.is_empty() {
            format!(
                "Unknown identifier '{name}'. A constructor of that name is {}",
                qualified.join(" or ")
            )
        } else if let Some(s) = self.suggest_name(name) {
            format!("Unknown identifier '{name}'. Did you mean '{s}'?")
        } else {
            format!("Unknown identifier '{name}'")
        };
        self.error(id.span, msg);
    }

    fn suggest_name(&self, name: &str) -> Option<String> {
        let mut candidates: Vec<&str> = Vec::new();
        if let Some(p) = self.prelude {
            candidates.extend(p.values.keys().map(|k| &**k));
        }
        candidates.extend(self.values.keys().map(|k| &**k));
        candidates.extend(self.scope.names().map(|n| &**n));
        closest_name(name, candidates.into_iter()).map(str::to_string)
    }

    /// Whether a bare name means any value here.
    pub fn is_value_name(&self, name: &str) -> bool {
        self.scope.lookup(name).is_some()
            || self.values.contains_key(name)
            || self.prelude.is_some_and(|p| p.values.contains_key(name))
    }

    /// What a bare name in value position means: a local, then one of the
    /// module's own values, then the prelude's. Records the use.
    fn resolve_value(&mut self, id: &ast::Identifier) -> Option<Resolved> {
        let name = id.name.as_str();
        if let Some(found) = self.scope.lookup(name) {
            let b = found.binder;
            self.binders[b.0 as usize].used = true;
            let state = &self.binders[b.0 as usize];
            let (module_scope, scheme, span) = (state.module_scope, state.scheme.clone(), state.span);
            if found.frame < self.scope.depth() && !module_scope {
                self.scope.capture(b, found.frame);
            }
            let ty = self.e.instantiate(&scheme);
            let target = self.site(span, EntityKind::Value);
            self.use_site(id.span, UseKind::Unqualified, target);
            self.hovers.push((id.span, name.into(), ty));
            return Some(Resolved {
                var: Var::Local(b),
                ty,
                kind: None,
            });
        }
        if let Some(v) = self.values.get(name).cloned() {
            let ty = self.instantiate_value(&v.scheme);
            let target = self.site(v.span, entity_of(&v.kind));
            self.use_site(id.span, UseKind::Unqualified, target);
            self.hovers.push((id.span, name.into(), ty));
            let value = ValueName {
                module: self.key.clone(),
                name: name.into(),
            };
            return Some(Resolved {
                var: var_of(&v.kind, value),
                ty,
                kind: Some(v.kind),
            });
        }
        let p = self.prelude?;
        let v = p.values.get(name)?;
        let ty = self.e.instantiate_scheme(&v.scheme);
        let target = self.foreign_site(&p.key, v);
        self.use_site(id.span, UseKind::Unqualified, target);
        self.hovers.push((id.span, name.into(), ty));
        let value = ValueName {
            module: p.key.clone(),
            name: name.into(),
        };
        Some(Resolved {
            var: var_of(&v.kind, value),
            ty,
            kind: Some(v.kind.clone()),
        })
    }

    /// `module::member`. `None` has been reported.
    fn module_member(&mut self, ma: &ast::ModuleAccessExpression) -> Option<Resolved> {
        let Some(key) = self.qualifiers.get(ma.module.name.as_str()).cloned() else {
            self.unknown_module(&ma.module);
            return None;
        };
        let module = self.module_name(&key);
        let iface = self.interface_of(&key)?;
        let member = ma.member.name.as_str();
        let Some(v) = iface.values.get(member) else {
            if iface.private_names.contains(member) {
                self.error(ma.member.span, format!("'{member}' is private in module '{module}'"));
            } else {
                self.error(ma.member.span, format!("Module '{module}' has no member '{member}'"));
            }
            return None;
        };
        let ty = self.e.instantiate_scheme(&v.scheme);
        let target = self.foreign_site(&key, v);
        self.use_site(ma.member.span, UseKind::Qualified, target);
        self.qualifier_use(&ma.module);
        self.hovers.push((ma.member.span, member.into(), ty));
        let value = ValueName {
            module: key,
            name: member.into(),
        };
        Some(Resolved {
            var: var_of(&v.kind, value),
            ty,
            kind: Some(v.kind.clone()),
        })
    }

    fn unknown_module(&mut self, name: &ast::Identifier) {
        let modules: Vec<String> = self
            .input
            .stdlib_modules
            .iter()
            .filter(|path| path.len() > 1 && path.last() == Some(&name.name))
            .map(|path| format!("`import {}`", path.join("/")))
            .collect();
        let msg = if modules.is_empty() {
            format!(
                "Unknown module '{}'. Import it before reaching into it with `::`",
                name.name
            )
        } else {
            format!(
                "Unknown module '{}': it is the name of a stdlib module that is not imported. Add {}",
                name.name,
                modules.join(" or ")
            )
        };
        self.module_error(name.span, msg);
    }

    // --- Operators ---

    fn check_binary(&mut self, b: &ast::BinaryExpression) -> (Ty, ExprKind) {
        use ast::BinaryOp as Op;
        let (constraint, compares, op) = match b.op {
            Op::And | Op::Or => {
                let left = self.check_expr(&b.left);
                let t = self.e.bool();
                self.unify(t, left.ty, b.left.span());
                let right = self.check_expr(&b.right);
                self.unify(t, right.ty, b.right.span());
                let (l, r) = (Box::new(left), Box::new(right));
                let kind = if b.op == Op::And {
                    ExprKind::And(l, r)
                } else {
                    ExprKind::Or(l, r)
                };
                return (t, kind);
            }
            Op::Eq | Op::Ne => {
                let left = self.check_expr(&b.left);
                let right = self.check_expr(&b.right);
                self.unify(left.ty, right.ty, b.span);
                let op = if b.op == Op::Eq { BinOp::Eq } else { BinOp::Ne };
                return (self.e.bool(), binop(op, left, right));
            }
            Op::Add => (Constraint::Addable, false, BinOp::Add),
            Op::Sub => (Constraint::Numeric, false, BinOp::Sub),
            Op::Mul => (Constraint::Numeric, false, BinOp::Mul),
            Op::Div => (Constraint::Numeric, false, BinOp::Div),
            Op::Mod => (Constraint::Numeric, false, BinOp::Mod),
            Op::Lt => (Constraint::Numeric, true, BinOp::Lt),
            Op::Gt => (Constraint::Numeric, true, BinOp::Gt),
            Op::Le => (Constraint::Numeric, true, BinOp::Le),
            Op::Ge => (Constraint::Numeric, true, BinOp::Ge),
        };
        let left = self.check_expr(&b.left);
        let right = self.check_expr(&b.right);
        let operand = self.e.fresh_constrained(constraint);
        self.unify(operand, left.ty, b.left.span());
        self.unify(operand, right.ty, b.right.span());
        let ty = if compares { self.e.bool() } else { operand };
        (ty, binop(op, left, right))
    }

    fn check_binary_literal(&mut self, bl: &ast::BinaryLiteral) -> (Ty, ExprKind) {
        let mut segs = Vec::with_capacity(bl.segments.len());
        for seg in &bl.segments {
            let value = self.check_expr(&seg.value);
            let expected = match seg.spec {
                ast::BinSpec::Int { .. } => self.e.int(),
                ast::BinSpec::Binary { .. } => self.e.binary(),
                ast::BinSpec::Utf8 => self.e.string(),
            };
            self.unify(expected, value.ty, seg.value.span());
            let unit = match &seg.spec {
                ast::BinSpec::Int { bits } => BinUnit::Bits(bits.as_ref().map(|b| Box::new(self.check_int(b)))),
                ast::BinSpec::Binary { bytes } => {
                    BinUnit::Bytes(bytes.as_ref().map(|b| Box::new(self.check_int(b))))
                }
                ast::BinSpec::Utf8 => BinUnit::Utf8,
            };
            segs.push(BinSeg { value, unit });
        }
        (self.e.binary(), ExprKind::Binary(segs))
    }

    fn check_array(&mut self, a: &ast::ArrayExpression) -> (Ty, ExprKind) {
        let elem = self.e.fresh();
        let mut elems = Vec::with_capacity(a.elements.len());
        for el in &a.elements {
            match el {
                ast::ArrayElement::Expression(e) => {
                    let x = self.check_expr(e);
                    self.unify(elem, x.ty, type_defining_span(e));
                    elems.push(ArrayElem::Elem(x));
                }
                ast::ArrayElement::SpreadElement(s) => {
                    let x = self.check_expr(&s.expression);
                    let arr = self.e.array(elem);
                    self.unify(arr, x.ty, s.expression.span());
                    elems.push(ArrayElem::Spread(x));
                }
            }
        }
        (self.e.array(elem), ExprKind::Array(elems))
    }

    // --- Matches ---

    fn check_match(&mut self, m: &ast::MatchExpression) -> (Ty, ExprKind) {
        let subject = self.check_expr(&m.subject);
        let result = self.e.fresh();
        let mut arms = Vec::with_capacity(m.arms.len());
        for arm in &m.arms {
            self.scope.open_block();
            let mut pb = PatBinds::default();
            let (mut pattern, ok) = self.check_pattern(&arm.pattern, subject.ty, &mut pb);
            if !ok {
                pattern.kind = PatternKind::Error;
            }
            let guard = arm.guard.as_ref().map(|g| {
                let x = self.check_expr(g);
                let b = self.e.bool();
                self.unify(b, x.ty, type_defining_span(g));
                x
            });
            let body = self.check_expr(&arm.body);
            self.unify(result, body.ty, type_defining_span(&arm.body));
            self.scope.close_block();
            arms.push(Arm { pattern, guard, body });
        }
        (
            result,
            ExprKind::Match {
                subject: Box::new(subject),
                arms,
            },
        )
    }

    // --- Functions ---

    fn check_lambda(&mut self, fe: &ast::FunctionExpression, hints: Option<Vec<Ty>>) -> (Ty, Lambda) {
        self.e.enter_level();
        let self_name = self.pending_self.take();
        self.scope.open_frame(self_name.clone());
        let mut annot = Annot::new(AnnotContext::Signature);
        let mut param_tys = Vec::with_capacity(fe.params.len());
        let mut params = Vec::with_capacity(fe.params.len());
        for (i, p) in fe.params.iter().enumerate() {
            let ty = match &p.typ {
                Some(t) => self.hydrate(&mut annot, t),
                None => match &hints {
                    Some(h) if i < h.len() => h[i],
                    _ => self.e.fresh(),
                },
            };
            param_tys.push(ty);
            params.push(self.bind_mono(&p.identifier.name, ty, p.identifier.span));
        }
        let body = self.check_expr(&fe.body);
        let ret = match &fe.return_type {
            Some(rt) => {
                let a = self.hydrate(&mut annot, rt);
                self.unify(a, body.ty, type_defining_span(&fe.body));
                a
            }
            None => body.ty,
        };
        let captures = self.scope.close_frame();
        self.e.leave_level();
        let ty = self.e.fun(param_tys, ret);
        (
            ty,
            Lambda {
                params,
                body: Box::new(body),
                captures,
                self_name: self_name.map(|(_, b)| b),
            },
        )
    }

    // --- Calls ---

    fn check_call(&mut self, fc: &ast::FunctionCallExpression) -> (Ty, ExprKind) {
        let simple = match fc.callee.as_ref() {
            ast::Expression::Identifier(id) => match self.resolve_value(id) {
                Some(r) => Some((r, id.span)),
                None => {
                    self.unknown_identifier(id);
                    for a in &fc.arguments {
                        self.check_arg_value(a);
                    }
                    return self.error_expr();
                }
            },
            ast::Expression::ModuleAccessExpression(ma) => match self.module_member(ma) {
                Some(r) => Some((r, ma.member.span)),
                None => {
                    for a in &fc.arguments {
                        self.check_arg_value(a);
                    }
                    return self.error_expr();
                }
            },
            _ => None,
        };
        if let Some((r, callee_span)) = simple {
            if let Some(ValueKind::Constructor { ctor, name, labels }) = &r.kind {
                let c = CtorRef {
                    ctor: ctor.clone(),
                    name: name.clone(),
                    labels: labels.clone(),
                    ty: r.ty,
                };
                return self.check_ctor_call(c, &fc.arguments, fc.span);
            }
            let labels = match &r.kind {
                Some(ValueKind::Fn { params }) if !params.is_empty() => Some(params.clone()),
                Some(_) | None => None,
            };
            let callee = Expr {
                ty: r.ty,
                span: callee_span,
                kind: ExprKind::Var(r.var),
            };
            return self.check_args(callee, labels.as_deref(), &fc.arguments, fc.span);
        }
        let callee = self.check_expr(&fc.callee);
        self.check_args(callee, None, &fc.arguments, fc.span)
    }

    fn check_arg_value(&mut self, a: &ast::CallArg) -> Expr {
        match a {
            ast::CallArg::Positional(e) | ast::CallArg::Spread(e) => self.check_expr(e),
            ast::CallArg::Labeled { value, .. } => self.check_expr(value),
        }
    }

    /// A call's arguments, checked in source order against the callee's
    /// parameters. `labels` is the callee's parameter names when a call may
    /// label its arguments, which is only a module function named directly.
    fn check_args(&mut self, callee: Expr, labels: Option<&[Name]>, args: &[ast::CallArg], span: Span) -> (Ty, ExprKind) {
        let (params, ret) = match self.e.callable(callee.ty, args.len()) {
            Ok(pr) => pr,
            Err(NotCallable::Arity { params, ret }) => {
                self.error(span, format!("Expected {} argument(s), got {}", params.len(), args.len()));
                (params, ret)
            }
            Err(NotCallable::NotFn) => {
                let s = self.e.display(callee.ty);
                self.error(
                    span,
                    format!("This value of type '{s}' is not a function and cannot be called"),
                );
                for a in args {
                    self.check_arg_value(a);
                }
                return self.error_expr();
            }
        };
        let labelled = args.iter().any(|a| matches!(a, ast::CallArg::Labeled { .. }));
        let slot_of: Vec<Option<usize>> = match labels {
            Some(names) if labelled => self.slot_call_args(names, args),
            _ => (0..args.len()).map(Some).collect(),
        };
        let mut out = Vec::with_capacity(args.len());
        for (i, arg) in args.iter().enumerate() {
            let slot = slot_of[i];
            let hint = slot.and_then(|p| params.get(p).copied());
            let (value, arg_span) = match arg {
                ast::CallArg::Positional(e) => (self.check_expr_hinted(e, hint), e.span()),
                ast::CallArg::Labeled { label, value, .. } => {
                    if labels.is_none() {
                        self.error(
                            label.span,
                            "Labelled arguments need a callee whose parameter names are known: a constructor, or a function named directly",
                        );
                    }
                    (self.check_expr_hinted(value, hint), value.span())
                }
                ast::CallArg::Spread(e) => {
                    self.error(
                        e.span(),
                        "Spread arguments are only allowed in constructor record-update calls",
                    );
                    (self.check_expr(e), e.span())
                }
            };
            if let Some(p) = hint {
                self.unify(p, value.ty, arg_span);
            }
            out.push(Arg {
                slot: slot.unwrap_or(i) as u32,
                value,
            });
        }
        (
            ret,
            ExprKind::Call {
                callee: Box::new(callee),
                args: out,
            },
        )
    }

    /// Which parameter each argument fills, reporting labels that name none
    /// and parameters given twice.
    fn slot_call_args(&mut self, names: &[Name], args: &[ast::CallArg]) -> Vec<Option<usize>> {
        let items: Vec<(Option<&str>, (usize, Span))> = args
            .iter()
            .enumerate()
            .filter_map(|(i, a)| match a {
                ast::CallArg::Positional(e) => Some((None, (i, e.span()))),
                ast::CallArg::Labeled { label, .. } => Some((Some(label.name.as_str()), (i, label.span))),
                ast::CallArg::Spread(_) => None,
            })
            .collect();
        let fields: Vec<Option<&str>> = names.iter().map(|n| Some(&**n)).collect();
        let (by_pos, errors) = slot_labeled(&fields, items);
        for e in errors {
            match e {
                // A wrong count, which `callable` has reported.
                SlotError::ExtraPositional(_) => {}
                SlotError::UnknownLabel(label, (_, span)) => self.error(
                    span,
                    format!(
                        "This function has no parameter '{label}'. Available: {}",
                        names.join(", ")
                    ),
                ),
                SlotError::Duplicate((_, span), p) => {
                    let dup = names.get(p).map_or("_", |n| &**n);
                    self.error(span, format!("Parameter '{dup}' is specified more than once"));
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
    /// one `..base` spread for a record update.
    fn check_ctor_call(&mut self, c: CtorRef, args: &[ast::CallArg], span: Span) -> (Ty, ExprKind) {
        let (params, result) = match self.e.shape(c.ty) {
            Shape::Fn(params, ret) => (params, ret),
            Shape::Var(_) | Shape::Con(..) | Shape::Tuple(_) => (Vec::new(), c.ty),
        };
        let mut spread: Option<(u32, &ast::Expression)> = None;
        let mut written = 0u32;
        for a in args {
            match a {
                ast::CallArg::Spread(e) => {
                    if spread.is_some() {
                        self.error(e.span(), "A constructor call can only have one spread");
                    }
                    spread = Some((written, e));
                }
                ast::CallArg::Positional(_) | ast::CallArg::Labeled { .. } => written += 1,
            }
        }
        let items: Vec<SlotItem<'_>> = args
            .iter()
            .enumerate()
            .filter_map(|(i, a)| match a {
                ast::CallArg::Positional(e) => Some((None, ((i, e), e.span()))),
                ast::CallArg::Labeled { label, value, .. } => {
                    Some((Some(label.name.as_str()), ((i, value), label.span)))
                }
                ast::CallArg::Spread(_) => None,
            })
            .collect();
        let (by_pos, _) = self.slot_ctor(&c, items, spread.is_none().then_some((span, "")));

        // The spread first, so its type settles the constructor's parameters
        // before a function literal argument takes a hint from them.
        let spread = spread.map(|(at, e)| {
            let base = self.check_expr(e);
            self.unify(result, base.ty, e.span());
            let unfilled: Vec<usize> = (0..c.labels.len())
                .filter(|&i| by_pos.get(i).is_none_or(Option::is_none))
                .collect();
            let fills = self.spread_fills(&c, result, &unfilled, e.span());
            Box::new(Spread { base, at, fills })
        });
        let mut checked: Vec<(usize, Arg)> = Vec::with_capacity(args.len());
        for (slot, sub) in by_pos.into_iter().enumerate() {
            let Some((i, value)) = sub else { continue };
            let expected = params.get(slot).copied();
            let v = self.check_expr_hinted(value, expected);
            if let Some(p) = expected {
                self.unify(p, v.ty, value.span());
            }
            checked.push((
                i,
                Arg {
                    slot: slot as u32,
                    value: v,
                },
            ));
        }
        // Back into source order, which is the order they run in.
        checked.sort_by_key(|(i, _)| *i);
        let args = checked.into_iter().map(|(_, a)| a).collect();
        (
            result,
            ExprKind::Construct {
                ctor: c.ctor,
                args,
                spread,
            },
        )
    }

    /// Where `base.field` reads each field a spread fills. A field the type's
    /// variants don't all have can't be filled, because which variant `base`
    /// is isn't known until the program runs.
    fn spread_fills(&mut self, c: &CtorRef, result: Ty, unfilled: &[usize], span: Span) -> Vec<(u32, FieldAt)> {
        let Shape::Con(type_name, type_args) = self.e.shape(result) else {
            return Vec::new();
        };
        let Some(def) = self.type_def(&type_name).cloned() else {
            return Vec::new();
        };
        let mut fills = Vec::new();
        for &i in unfilled {
            let Some(label) = c.labels.get(i) else { continue };
            match self.field_in_variants(&def, &type_args, label, span) {
                Ok((_, at)) => fills.push((i as u32, at)),
                Err(FieldMissing::On(variant)) => {
                    let why = missing_on(&variant, type_name.name());
                    self.error(
                        span,
                        format!("The spread cannot fill field '{label}', which {why}; write '{label}:' in the call"),
                    );
                }
                Err(FieldMissing::NotCustom) => {}
            }
        }
        fills
    }

    // --- Fields ---

    fn check_property(&mut self, pa: &ast::PropertyAccessExpression) -> (Ty, ExprKind) {
        if let (ast::Expression::Identifier(id), ast::PropertyKey::Field(field)) = (pa.left.as_ref(), &pa.right)
            && !self.is_value_name(&id.name)
            && self.qualifiers.contains_key(id.name.as_str())
        {
            self.error(
                id.span,
                format!(
                    "'{0}' is a module, not a value. Reach its members with `::`, like `{0}::{1}`",
                    id.name, field.name
                ),
            );
            return self.error_expr();
        }
        let record = self.check_expr(&pa.left);
        match &pa.right {
            ast::PropertyKey::TupleIndex(num) => {
                let Ok(index) = num.value.parse::<u32>() else {
                    self.error(num.span, format!("Tuple index must be an integer, got '{}'", num.value));
                    return self.error_expr();
                };
                match self.e.shape(record.ty) {
                    Shape::Tuple(elems) => match elems.get(index as usize) {
                        Some(&ty) => (
                            ty,
                            ExprKind::TupleIndex {
                                tuple: Box::new(record),
                                index,
                            },
                        ),
                        None => {
                            self.error(
                                num.span,
                                format!(
                                    "Tuple index {index} is out of range, because the tuple has {} elements",
                                    elems.len()
                                ),
                            );
                            self.error_expr()
                        }
                    },
                    Shape::Var(_) | Shape::Con(..) | Shape::Fn(..) => {
                        self.error(num.span, format!("Can't use .{index} here, because the value isn't a tuple"));
                        self.error_expr()
                    }
                }
            }
            ast::PropertyKey::Field(field) => match self.field_access(record.ty, field) {
                Some((ty, at)) => (
                    ty,
                    ExprKind::Field {
                        record: Box::new(record),
                        label: field.name.as_str().into(),
                        at,
                    },
                ),
                None => self.error_expr(),
            },
        }
    }

    /// `.field` on a value of type `receiver`. Which variant the value is
    /// isn't known until the program runs, so every variant must hold the
    /// field, at one type, wherever it sits in each.
    fn field_access(&mut self, receiver: Ty, field: &ast::Identifier) -> Option<(Ty, FieldAt)> {
        let (type_name, type_args) = match self.e.shape(receiver) {
            Shape::Con(n, a) => (n, a),
            Shape::Var(_) => {
                self.error(
                    field.span,
                    format!(
                        "Can't access field '{}' because the value's type isn't known here. Add a type annotation",
                        field.name
                    ),
                );
                return None;
            }
            Shape::Fn(..) | Shape::Tuple(_) => {
                let s = self.e.display(receiver);
                self.error(field.span, format!("Type '{s}' has no field '{}'", field.name));
                return None;
            }
        };
        let Some(def) = self.type_def(&type_name).cloned() else {
            self.error(field.span, format!("Type '{}' has no field '{}'", type_name.name(), field.name));
            return None;
        };
        match self.field_in_variants(&def, &type_args, &field.name, field.span) {
            Ok((ty, at)) => {
                let label = format!("{}.{}", type_name.name(), field.name);
                self.hovers.push((field.span, label.into(), ty));
                if let Some(site) = self.field_site(&def, &field.name) {
                    self.use_site(field.span, UseKind::Unqualified, site);
                }
                Some((ty, at))
            }
            Err(FieldMissing::NotCustom) => {
                self.error(field.span, format!("Type '{}' has no accessible fields", type_name.name()));
                None
            }
            Err(FieldMissing::On(variant)) => {
                let why = missing_on(&variant, type_name.name());
                self.error(field.span, format!("Field '{}' {why}", field.name));
                None
            }
        }
    }

    /// The field's definition: where it is first declared among the type's
    /// variants.
    fn field_site(&self, def: &TypeDef, label: &str) -> Option<Site> {
        let f = def
            .variants()?
            .iter()
            .flat_map(|v| &v.fields)
            .find(|f| &*f.label == label)?;
        Some(Site {
            module: def.name.module().clone(),
            span: f.span,
            entity: EntityKind::Field,
        })
    }

    /// Where field `label` sits in each variant of `def`, and its type. The
    /// field types of each variant are unified, reporting a mismatch at
    /// `span`.
    fn field_in_variants(
        &mut self,
        def: &TypeDef,
        args: &[Ty],
        label: &str,
        span: Span,
    ) -> Result<(Ty, FieldAt), FieldMissing> {
        let variants = def.variants().ok_or(FieldMissing::NotCustom)?;
        if variants.is_empty() {
            return Err(FieldMissing::NotCustom);
        }
        let mut subst: Vec<_> = def.params.iter().cloned().zip(args.iter().copied()).collect();
        let mut ty: Option<Ty> = None;
        let mut slots = Vec::with_capacity(variants.len());
        for v in variants {
            let Some((i, f)) = v.fields.iter().enumerate().find(|(_, f)| &*f.label == label) else {
                return Err(FieldMissing::On(v.name.clone()));
            };
            slots.push(i as u32);
            let fty = self.e.import(&f.ty, &mut subst);
            match ty {
                Some(t) => {
                    self.unify(t, fty, span);
                }
                None => ty = Some(fty),
            }
        }
        let ty = ty.ok_or(FieldMissing::NotCustom)?;
        let at = match slots.first() {
            Some(&first) if slots.iter().all(|&s| s == first) => FieldAt::Same(first),
            Some(_) | None => FieldAt::PerVariant(
                slots
                    .iter()
                    .enumerate()
                    .map(|(v, &s)| (v as u16, s))
                    .collect(),
            ),
        };
        Ok((ty, at))
    }

    // --- `or` ---

    fn check_or(&mut self, oe: &ast::OrExpression) -> (Ty, ExprKind) {
        let value = self.check_expr(&oe.expression);
        let success = self.e.fresh();
        let shape = match self.e.shape(value.ty) {
            Shape::Con(n, _) if n == self.e.known().option => Some(OrShape::Option),
            Shape::Con(n, _) if n == self.e.known().result => Some(OrShape::Result),
            Shape::Con(..) | Shape::Var(_) | Shape::Fn(..) | Shape::Tuple(_) => None,
        };
        let Some(shape) = shape else {
            let s = self.e.display(value.ty);
            self.error(
                oe.expression.span(),
                format!("'or' requires the left side to be Option(_) or Result(_, _), got '{s}'"),
            );
            self.scope.open_block();
            self.check_expr(&oe.body);
            self.scope.close_block();
            return self.error_expr();
        };
        let err = match shape {
            OrShape::Option => {
                if let Some(recv) = &oe.receiver {
                    self.error(
                        recv.span,
                        "'or' on an Option can't bind a value, because None carries nothing",
                    );
                }
                let expected = self.known_option(success);
                self.unify(expected, value.ty, oe.expression.span());
                None
            }
            OrShape::Result => {
                let e = self.e.fresh();
                let expected = self.known_result(success, e);
                self.unify(expected, value.ty, oe.expression.span());
                Some(e)
            }
        };
        self.scope.open_block();
        let error: Option<BinderId> = match (err, &oe.receiver) {
            (Some(e), Some(recv)) => Some(self.bind_mono(&recv.name, e, recv.span)),
            (Some(_), None) | (None, _) => None,
        };
        let fallback = self.check_expr(&oe.body);
        self.scope.close_block();
        self.unify(success, fallback.ty, type_defining_span(&oe.body));
        (
            success,
            ExprKind::OrElse {
                shape,
                value: Box::new(value),
                error,
                fallback: Box::new(fallback),
            },
        )
    }
}

fn binop(op: BinOp, left: Expr, right: Expr) -> ExprKind {
    ExprKind::BinOp {
        op,
        left: Box::new(left),
        right: Box::new(right),
    }
}

fn var_of(kind: &ValueKind, value: ValueName) -> Var {
    match kind {
        ValueKind::Fn { .. } => Var::Fn(value),
        ValueKind::Const => Var::Const(value),
        ValueKind::Builtin { intrinsic, .. } => Var::Builtin(*intrinsic),
        ValueKind::Constructor { ctor, .. } => Var::Ctor(CtorName {
            type_name: ctor.type_name.clone(),
            variant: ctor.variant,
        }),
    }
}

/// The end of a sentence that names a field some variant lacks.
fn missing_on(variant: &str, type_name: &str) -> String {
    format!("is not present on every variant of '{type_name}' (missing on '{variant}')")
}

/// The candidate closest to `name` by edit distance, if any is close enough
/// to be what was meant. Ties go to the earliest. This is rustc's rule: an
/// edit distance up to `max(len, 3) / 3`.
fn closest_name<'a>(name: &str, candidates: impl Iterator<Item = &'a str>) -> Option<&'a str> {
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
