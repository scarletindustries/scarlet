use std::collections::HashMap;

use crate::ast;
use crate::d;
use crate::diagnostic::{Diagnostic, Severity};
use crate::parser;
use crate::scanner;
use crate::span::Span;
use crate::token::{Kind, Trivia, TriviaKind};

pub mod doc;
use doc::{
    Doc, block, delimited, delimited_hug, delimited_no_trailing, delimited_ws, group, hard_braces,
    hardline, hardlines, join, line, nil, text,
};

const MAX_WIDTH: isize = 100;

pub struct FormatResult {
    pub output: String,
    pub has_errors: bool,
    pub diagnostics: Vec<Diagnostic>,
}

pub fn format_with_debug(input: &str, debug: bool) -> FormatResult {
    let mut s = scanner::new_scanner(input.to_string());
    let scanned_tokens = s.scan_all();
    let scanner_diagnostics = s.get_diagnostics();

    if debug {
        for tok in &scanned_tokens {
            eprintln!(
                "Token: {:?} \"{}\" trivia: {}",
                tok.kind,
                tok.literal.as_deref().unwrap_or(""),
                tok.leading_trivia.len()
            );
            for t in &tok.leading_trivia {
                eprintln!("  Trivia: {:?} \"{}\"", t.kind, t.text.replace('\n', "\\n"));
            }
        }
    }

    let mut trivia_map: HashMap<(i32, i32), Vec<Trivia>> = HashMap::new();
    let mut eof_trivia: Vec<Trivia> = Vec::new();
    for tok in &scanned_tokens {
        if tok.kind == Kind::Eof {
            eof_trivia = tok.leading_trivia.clone();
        } else if !tok.leading_trivia.is_empty() {
            trivia_map.insert((tok.line, tok.column), tok.leading_trivia.clone());
        }
    }

    let mut p = parser::new_parser_from_tokens(scanned_tokens, scanner_diagnostics);
    let result = p.parse_program();

    let errors: Vec<Diagnostic> = result
        .diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .cloned()
        .collect();

    if !errors.is_empty() {
        return FormatResult {
            output: input.to_string(),
            has_errors: true,
            diagnostics: errors,
        };
    }

    let f = Formatter { trivia_map };
    let body = f.program(&result.ast, &eof_trivia);
    let mut output = doc::layout(&body, MAX_WIDTH);
    while output.ends_with('\n') {
        output.pop();
    }
    output.push('\n');

    FormatResult {
        output,
        has_errors: false,
        diagnostics: result.diagnostics,
    }
}

struct Formatter {
    trivia_map: HashMap<(i32, i32), Vec<Trivia>>,
}

impl Formatter {
    // ------------------------------------------------------------------ trivia

    fn trivia_at(&self, s: Span) -> Option<&[Trivia]> {
        self.trivia_map
            .get(&(s.start_line, s.start_column))
            .map(|v| v.as_slice())
    }

    fn trivia_at_end(&self, s: Span) -> Option<&[Trivia]> {
        self.trivia_map
            .get(&(s.end_line, s.end_column - 1))
            .map(|v| v.as_slice())
    }

    /// Render the leading trivia for a node into a doc that ends ready for the
    /// node text (i.e. trailing hardline if there were comments). The number of
    /// blank lines preserved is capped at `max_blanks`.
    fn leading_trivia(&self, s: Span, first: bool, max_blanks: usize) -> Doc {
        let Some(trivia) = self.trivia_at(s) else {
            return if first { nil() } else { hardline() };
        };
        self.trivia_doc(trivia, first, max_blanks)
    }

    fn trivia_doc(&self, trivia: &[Trivia], first: bool, max_blanks: usize) -> Doc {
        let mut parts: Vec<Doc> = Vec::new();
        let mut newlines = 0usize;
        let mut emitted_any = !first;
        for t in trivia {
            match t.kind {
                TriviaKind::Newline => newlines += 1,
                TriviaKind::Whitespace => {}
                TriviaKind::LineComment | TriviaKind::BlockComment | TriviaKind::DocComment => {
                    let blanks = if emitted_any {
                        newlines.saturating_sub(1).min(max_blanks)
                    } else {
                        0
                    };
                    if emitted_any {
                        parts.push(hardlines(1 + blanks));
                    }
                    parts.push(text(t.text.trim_end().to_string()));
                    emitted_any = true;
                    newlines = 0;
                }
            }
        }
        let blanks = if emitted_any {
            newlines.saturating_sub(1).min(max_blanks)
        } else {
            0
        };
        if emitted_any || !first {
            parts.push(hardlines(1 + blanks));
        }
        doc::concat(parts)
    }

    fn trailing_comments(&self, s: Span) -> Doc {
        let Some(trivia) = self.trivia_at_end(s) else {
            return nil();
        };
        let mut parts = Vec::new();
        for t in trivia {
            if matches!(
                t.kind,
                TriviaKind::LineComment | TriviaKind::BlockComment | TriviaKind::DocComment
            ) {
                parts.push(hardline());
                parts.push(text(t.text.trim_end().to_string()));
            }
        }
        doc::concat(parts)
    }

    fn has_comment_at(&self, s: Span) -> bool {
        self.trivia_at(s).is_some_and(|tr| {
            tr.iter().any(|t| {
                matches!(
                    t.kind,
                    TriviaKind::LineComment | TriviaKind::BlockComment | TriviaKind::DocComment
                )
            })
        })
    }

    // ------------------------------------------------------------------ program

    fn program(&self, block: &ast::BlockExpression, eof_trivia: &[Trivia]) -> Doc {
        let mut parts = Vec::new();
        for (i, node) in block.body.iter().enumerate() {
            parts.push(self.leading_trivia(node.span(), i == 0, 2));
            parts.push(self.node(node));
        }
        parts.push(self.trivia_doc(eof_trivia, block.body.is_empty(), 2));
        doc::concat(parts)
    }

    fn node(&self, n: &ast::Node) -> Doc {
        match n {
            ast::Node::Statement(s) => self.statement(s),
            ast::Node::Expression(e) => self.expr(e),
        }
    }

    // -------------------------------------------------------------- statements

    fn statement(&self, stmt: &ast::Statement) -> Doc {
        match stmt {
            ast::Statement::VariableBinding(s) => {
                let ty = match &s.typ {
                    Some(t) => d![text(" "), self.type_(t)],
                    None => nil(),
                };
                d![
                    text(&*s.identifier.name),
                    ty,
                    text(" = "),
                    self.expr(&s.init)
                ]
            }
            ast::Statement::TupleDestructuringBinding(s) => {
                let pats: Vec<Doc> = s.patterns.iter().map(|p| self.pattern(p)).collect();
                d![delimited("(", pats, ")"), text(" = "), self.expr(&s.init)]
            }
            ast::Statement::TypedDiscard(s) => {
                d![text(&*s.ty_name.name), text(" = "), self.expr(&s.init)]
            }
            ast::Statement::CtorDestructuringBinding(s) => {
                d![self.pattern(&s.pattern), text(" = "), self.expr(&s.init)]
            }
            ast::Statement::Declaration(dcl) => self.declaration(dcl, false),
            ast::Statement::PublicDeclaration(dcl) => self.declaration(dcl, true),
            ast::Statement::ImportDeclaration(s) => {
                let mut out = vec![text("import "), text(s.path.join("/"))];
                if let Some(a) = &s.alias {
                    out.push(text(" as "));
                    out.push(text(&*a.name));
                }
                if !s.items.is_empty() {
                    let items: Vec<Doc> = s
                        .items
                        .iter()
                        .map(|it| match &it.alias {
                            Some(a) => d![text(&*it.name.name), text(" as "), text(&*a.name)],
                            None => text(&*it.name.name),
                        })
                        .collect();
                    out.push(text("."));
                    out.push(delimited("{", items, "}"));
                }
                doc::concat(out)
            }
        }
    }

    fn attributes(&self, attrs: &[ast::Attribute]) -> Doc {
        if attrs.is_empty() {
            return nil();
        }
        let mut parts = Vec::new();
        for a in attrs {
            parts.push(text("@"));
            parts.push(text(&*a.name.name));
            if !a.args.is_empty() {
                let args: Vec<Doc> = a.args.iter().map(|id| text(&*id.name)).collect();
                parts.push(delimited("(", args, ")"));
            }
            parts.push(hardline());
        }
        doc::concat(parts)
    }

    fn declaration(&self, dcl: &ast::Declaration, is_public: bool) -> Doc {
        let attrs = match dcl {
            ast::Declaration::Function(f) => self.attributes(&f.attributes),
            ast::Declaration::Type(t) => self.attributes(&t.attributes),
            ast::Declaration::Const(_) => nil(),
        };
        let prefix = if is_public { text("pub ") } else { nil() };
        let body = match dcl {
            ast::Declaration::Const(s) => {
                let ty = match &s.typ {
                    Some(t) => d![text(" "), self.type_(t)],
                    None => nil(),
                };
                d![
                    text("const "),
                    text(&*s.identifier.name),
                    ty,
                    text(" = "),
                    self.expr(&s.init)
                ]
            }
            ast::Declaration::Function(func) => self.fn_decl(func),
            ast::Declaration::Type(s) => self.type_decl(s),
        };
        d![attrs, prefix, body]
    }

    fn fn_header(
        &self,
        name: Option<&str>,
        params: &[ast::FunctionParameter],
        ret: &Option<ast::TypeIdentifier>,
    ) -> Doc {
        let head = match name {
            Some(n) => text(format!("fn {n}")),
            None => text("fn"),
        };
        let ps: Vec<Doc> = params
            .iter()
            .map(|p| match &p.typ {
                Some(t) => d![text(&*p.identifier.name), text(" "), self.type_(t)],
                None => text(&*p.identifier.name),
            })
            .collect();
        let r = match ret {
            Some(t) => d![text(" "), self.type_(t)],
            None => nil(),
        };
        d![head, delimited("(", ps, ")"), r]
    }

    fn fn_decl(&self, f: &ast::FunctionDeclaration) -> Doc {
        let header = self.fn_header(Some(&f.identifier.name), &f.params, &f.return_type);
        match &f.body {
            ast::FnBody::Block(b) => d![header, text(" "), self.fn_body(b)],
            ast::FnBody::Vm(_) => header,
        }
    }

    fn fn_expr(&self, f: &ast::FunctionExpression) -> Doc {
        d![
            self.fn_header(None, &f.params, &None),
            text(" "),
            self.lambda_body(&f.body),
        ]
    }

    /// A function declaration body is always a hard multi-line block — a named
    /// `fn` never collapses onto its signature line. (Lambdas keep the braceless
    /// single-expression form via `lambda_body`.)
    fn fn_body(&self, body: &ast::Expression) -> Doc {
        if let ast::Expression::BlockExpression(b) = body {
            let trail = self.trailing_comments(b.span);
            if b.body.is_empty() && matches!(trail, Doc::Nil) {
                return text("{}");
            }
        }
        self.body_as_hard_block(body)
    }

    fn lambda_body(&self, body: &ast::Expression) -> Doc {
        if let ast::Expression::BlockExpression(b) = body
            && b.body.len() == 1
            && let ast::Node::Expression(inner) = &b.body[0]
            && !self.has_comment_at(inner.span())
            && matches!(self.trailing_comments(b.span), Doc::Nil)
        {
            return group(self.expr(inner));
        }
        if !matches!(body, ast::Expression::BlockExpression(_)) && !self.has_comment_at(body.span())
        {
            return group(self.expr(body));
        }
        self.body_as_block(body)
    }

    fn type_decl(&self, s: &ast::TypeDeclaration) -> Doc {
        let params = if s.type_params.is_empty() {
            nil()
        } else {
            let ps: Vec<Doc> = s.type_params.iter().map(|p| text(&*p.name)).collect();
            delimited("(", ps, ")")
        };
        let prefix = match &s.body {
            ast::TypeBody::Variants { opaque: true, .. } => text("opaque "),
            _ => nil(),
        };
        let head = d![prefix, text("type "), text(&*s.identifier.name), params];
        match &s.body {
            ast::TypeBody::External => head,
            ast::TypeBody::Alias(t) => d![head, text(" = "), self.type_(t)],
            ast::TypeBody::Variants { ctors, .. } => {
                // Shorthand: a single constructor named after the type with at
                // least one field emits the field list directly inside `{}`.
                if ctors.len() == 1
                    && ctors[0].identifier.name == s.identifier.name
                    && !ctors[0].fields.is_empty()
                {
                    let c = &ctors[0];
                    let mut parts = Vec::new();
                    for (i, f) in c.fields.iter().enumerate() {
                        if i > 0 {
                            parts.push(hardline());
                        }
                        parts.push(d![text(&*f.label.name), text(" "), self.type_(&f.typ)]);
                    }
                    return d![head, text(" "), hard_braces(doc::concat(parts))];
                }
                let mut parts = Vec::new();
                for (i, c) in ctors.iter().enumerate() {
                    parts.push(self.leading_trivia(c.span, i == 0, 1));
                    parts.push(self.constructor(c));
                }
                d![head, text(" "), hard_braces(doc::concat(parts))]
            }
        }
    }

    fn constructor(&self, c: &ast::Constructor) -> Doc {
        if c.fields.is_empty() {
            return text(&*c.identifier.name);
        }
        let fields: Vec<Doc> = c
            .fields
            .iter()
            .map(|f| d![text(&*f.label.name), text(" "), self.type_(&f.typ)])
            .collect();
        d![text(&*c.identifier.name), delimited_ws("(", fields, ")")]
    }

    // ----------------------------------------------------------------- expressions

    fn expr(&self, e: &ast::Expression) -> Doc {
        use ast::Expression as E;
        match e {
            E::StringLiteral(s) => text(quoted(&s.value)),
            E::InterpolatedString(s) => {
                let literal: String = s
                    .parts
                    .iter()
                    .filter_map(|p| match p {
                        E::StringLiteral(sl) => Some(sl.value.as_str()),
                        _ => None,
                    })
                    .collect();
                let q = pick_quote(&literal);
                let mut out = String::from(q);
                for part in &s.parts {
                    match part {
                        E::StringLiteral(sl) => out.push_str(&escape_string(&sl.value, q)),
                        other => {
                            out.push_str("${");
                            // Layout sub-expression flat at infinite width.
                            out.push_str(&doc::layout(&self.expr(other), isize::MAX));
                            out.push('}');
                        }
                    }
                }
                out.push(q);
                text(out)
            }
            E::NumberLiteral(n) => text(&*n.value),
            E::Identifier(id) => text(&*id.name),
            E::BinaryExpression(b) => group(d![
                self.expr(&b.left),
                text(format!(" {}", b.op.kind)),
                line(),
                self.expr(&b.right),
            ]),
            E::UnaryExpression(u) => {
                let op = match u.op.kind {
                    Kind::PuncExclamationMark => "!",
                    Kind::PuncMinus => "-",
                    // The parser only ever produces `!` or `-` as unary operators.
                    #[allow(clippy::unreachable)]
                    _ => unreachable!(),
                };
                // `- -x` must not collapse into `--x`: the scanner greedily relexes
                // the `--` as the rejected decrement token, so a valid program would
                // no longer parse after formatting. Keep the operators apart with a
                // space whenever the operand's own leading glyph is `-`.
                let gap = if u.op.kind == Kind::PuncMinus && starts_with_minus(&u.expression) {
                    text(" ")
                } else {
                    nil()
                };
                d![text(op), gap, self.expr(&u.expression)]
            }
            E::BlockExpression(b) => self.block_expr(b),
            E::IfExpression(_) => self.if_chain(e),
            E::MatchExpression(m) => self.match_expr(m),
            E::OrExpression(o) => {
                let recv = match &o.receiver {
                    Some(r) => d![text(&*r.name), text(" -> ")],
                    None => nil(),
                };
                d![
                    self.expr(&o.expression),
                    text(" or "),
                    recv,
                    self.expr(&o.body)
                ]
            }
            E::FunctionExpression(f) => self.fn_expr(f),
            E::FunctionCallExpression(c) => {
                let args: Vec<Doc> = c.arguments.iter().map(|a| self.call_arg(a)).collect();
                // A block-shaped final argument hugs the call's parentheses
                // (`f(a, fn() { … })`) instead of forcing one argument per
                // line.
                let parens = if c.arguments.last().is_some_and(arg_can_hug) {
                    delimited_hug("(", args, ")")
                } else {
                    delimited("(", args, ")")
                };
                d![self.expr(&c.callee), parens]
            }
            E::PropertyAccessExpression(p) => {
                d![self.expr(&p.left), text("."), self.expr(&p.right)]
            }
            E::ArrayExpression(a) => {
                let items: Vec<Doc> = a.elements.iter().map(|e| self.array_elem(e)).collect();
                delimited("[", items, "]")
            }
            E::BinaryLiteral(b) => {
                let segs: Vec<Doc> = b
                    .segments
                    .iter()
                    .map(|s| {
                        let is_string = matches!(
                            s.value,
                            ast::Expression::StringLiteral(_)
                                | ast::Expression::InterpolatedString(_)
                        );
                        d![
                            self.expr(&s.value),
                            self.bin_size_spec(s.kind, &s.size, is_string)
                        ]
                    })
                    .collect();
                delimited("<<", segs, ">>")
            }
            E::TupleExpression(t) => {
                let items: Vec<Doc> = t.elements.iter().map(|e| self.expr(e)).collect();
                delimited("(", items, ")")
            }
            E::ArrayIndexExpression(a) => {
                d![
                    self.expr(&a.expression),
                    text("["),
                    self.expr(&a.index),
                    text("]")
                ]
            }
            E::RangeExpression(r) => d![self.expr(&r.start), text(".."), self.expr(&r.end)],
            E::ErrorNode(er) => text(format!("/* error: {} */", er.message)),
        }
    }

    fn call_arg(&self, a: &ast::CallArg) -> Doc {
        match a {
            ast::CallArg::Positional(e) => self.expr(e),
            ast::CallArg::Labeled { label, value } => {
                d![text(&*label.name), text(": "), self.expr(value)]
            }
            ast::CallArg::Spread(e) => d![text(".."), self.expr(e)],
        }
    }

    /// Render the `:spec` suffix of a `<<>>` segment, inverting
    /// `parse_bin_size_spec`. Int-kind with a literal width emits the bare
    /// `:N` shorthand; a dynamic Int width uses `:size(..)`. A string-literal
    /// segment's Utf8 kind is the parser default, so its `:utf8` is omitted.
    fn bin_size_spec(
        &self,
        kind: ast::BinKind,
        size: &Option<ast::Expression>,
        value_is_string: bool,
    ) -> Doc {
        match (kind, size) {
            (ast::BinKind::Int, None) => nil(),
            (ast::BinKind::Int, Some(ast::Expression::NumberLiteral(n))) => {
                d![text(":"), text(&*n.value)]
            }
            (ast::BinKind::Int, Some(e)) => d![text(":size("), self.expr(e), text(")")],
            (ast::BinKind::Binary, None) => text(":binary"),
            (ast::BinKind::Binary, Some(e)) => d![text(":bytes("), self.expr(e), text(")")],
            (ast::BinKind::Utf8, _) if value_is_string => nil(),
            (ast::BinKind::Utf8, _) => text(":utf8"),
        }
    }

    fn array_elem(&self, e: &ast::ArrayElement) -> Doc {
        match e {
            ast::ArrayElement::Expression(e) => self.expr(e),
            ast::ArrayElement::SpreadElement(se) => match &se.expression {
                Some(inner) => d![text(".."), self.expr(inner)],
                None => text(".."),
            },
        }
    }

    fn body_as_block(&self, body: &ast::Expression) -> Doc {
        match body {
            ast::Expression::BlockExpression(b) => self.block_expr(b),
            other => block(self.expr(other)),
        }
    }

    fn block_expr(&self, b: &ast::BlockExpression) -> Doc {
        let trail = self.trailing_comments(b.span);
        if b.body.is_empty() && matches!(trail, Doc::Nil) {
            return text("{}");
        }
        let mut parts = Vec::new();
        let mut hard = !matches!(trail, Doc::Nil);
        for (i, n) in b.body.iter().enumerate() {
            if self.has_comment_at(n.span()) {
                hard = true;
            }
            parts.push(self.leading_trivia(n.span(), i == 0, 1));
            parts.push(self.node(n));
        }
        let body = d![doc::concat(parts), trail];
        if b.body.len() <= 1 && !hard {
            block(body)
        } else {
            hard_braces(body)
        }
    }

    fn if_chain(&self, e: &ast::Expression) -> Doc {
        // Unroll an if/else-if/.../else chain into a flat list of clauses so
        // they share one group and break together. If any branch is a
        // multi-statement block, force every branch body to hard-break so the
        // chain reads symmetrically.
        let mut bodies: Vec<&ast::Expression> = Vec::new();
        let mut conds: Vec<&ast::Expression> = Vec::new();
        let mut cur = e;
        loop {
            match cur {
                ast::Expression::IfExpression(i) => {
                    conds.push(&i.condition);
                    bodies.push(&i.body);
                    cur = &i.else_body;
                }
                other => {
                    bodies.push(other);
                    break;
                }
            }
        }
        // An if/else stays on one line only when it is a single, comment-free
        // if/else (no `else if` chain) whose every branch is a bare atom — i.e.
        // a ternary. Anything heavier (a chain, a non-trivial branch, a
        // comment) breaks every clause onto its own lines so the whole thing
        // reads symmetrically, the way `match` always does.
        let is_chain = conds.len() > 1;
        let cond_comment = conds.iter().any(|c| self.has_comment_at(c.span()));
        let trivial =
            !is_chain && !cond_comment && bodies.iter().all(|b| self.is_trivial_branch(b));
        let body = |b: &ast::Expression| {
            if trivial {
                self.body_as_block(b)
            } else {
                self.body_as_hard_block(b)
            }
        };
        let mut clauses: Vec<Doc> = Vec::new();
        for (i, c) in conds.iter().enumerate() {
            let kw = if i == 0 { "if " } else { "else if " };
            clauses.push(d![text(kw), self.expr(c), text(" "), body(bodies[i])]);
        }
        clauses.push(d![text("else "), body(bodies[conds.len()])]);
        if trivial {
            group(join(clauses, line()))
        } else {
            join(clauses, text(" "))
        }
    }

    /// A branch is trivial when it is a single, comment-free expression
    /// statement whose expression is a bare atom (identifier or literal). These
    /// are the only branches allowed to keep an `if` on one line.
    fn is_trivial_branch(&self, b: &ast::Expression) -> bool {
        let ast::Expression::BlockExpression(blk) = b else {
            return false;
        };
        if blk.body.len() != 1 || !matches!(self.trailing_comments(blk.span), Doc::Nil) {
            return false;
        }
        let ast::Node::Expression(inner) = &blk.body[0] else {
            return false;
        };
        if self.has_comment_at(inner.span()) {
            return false;
        }
        matches!(
            inner,
            ast::Expression::Identifier(_)
                | ast::Expression::NumberLiteral(_)
                | ast::Expression::StringLiteral(_)
        )
    }

    fn body_as_hard_block(&self, body: &ast::Expression) -> Doc {
        let inner = match body {
            ast::Expression::BlockExpression(b) => {
                let mut parts = Vec::new();
                for (i, n) in b.body.iter().enumerate() {
                    parts.push(self.leading_trivia(n.span(), i == 0, 1));
                    parts.push(self.node(n));
                }
                d![doc::concat(parts), self.trailing_comments(b.span)]
            }
            other => self.expr(other),
        };
        hard_braces(inner)
    }

    fn match_expr(&self, m: &ast::MatchExpression) -> Doc {
        let mut arms: Vec<Doc> = Vec::new();
        for (i, arm) in m.arms.iter().enumerate() {
            arms.push(self.leading_trivia(arm.pattern.span(), i == 0, 1));
            let guard = match &arm.guard {
                Some(g) => d![text(" if "), self.expr(g)],
                None => nil(),
            };
            let head = if matches!(arm.pattern, ast::Pattern::Wildcard { .. }) {
                text("else")
            } else {
                self.pattern(&arm.pattern)
            };
            arms.push(d![head, guard, text(" -> "), self.expr(&arm.body)]);
        }
        let trail = self.trailing_comments(m.span);
        d![
            text("match "),
            self.expr(&m.subject),
            text(" "),
            hard_braces(d![doc::concat(arms), trail]),
        ]
    }

    // ------------------------------------------------------------------ patterns

    fn pattern(&self, p: &ast::Pattern) -> Doc {
        use ast::Pattern as P;
        match p {
            P::Wildcard { .. } => text("_"),
            P::Var { name } => text(&*name.name),
            P::Literal(ast::PatternLiteral::Number(n)) => text(&*n.value),
            P::Literal(ast::PatternLiteral::String(s)) => text(quoted(&s.value)),
            P::Tuple { elements, .. } => {
                let es: Vec<Doc> = elements.iter().map(|e| self.pattern(e)).collect();
                delimited("(", es, ")")
            }
            P::Array { elements, .. } => {
                let es: Vec<Doc> = elements
                    .iter()
                    .map(|e| match e {
                        ast::ArrayPatternElement::Pattern(p) => self.pattern(p),
                        ast::ArrayPatternElement::Spread { binding, .. } => match binding {
                            Some(b) => d![text(".."), text(&*b.name)],
                            None => text(".."),
                        },
                    })
                    .collect();
                delimited("[", es, "]")
            }
            P::Binary { segments, rest, .. } => {
                let mut segs: Vec<Doc> = segments
                    .iter()
                    .map(|s| {
                        let is_string = matches!(
                            s.value,
                            ast::Pattern::Literal(ast::PatternLiteral::String(_))
                        );
                        d![
                            self.pattern(&s.value),
                            self.bin_size_spec(s.kind, &s.size, is_string)
                        ]
                    })
                    .collect();
                match rest {
                    None => delimited("<<", segs, ">>"),
                    Some(r) => {
                        segs.push(match &r.binding {
                            Some(b) => d![text(".."), text(&*b.name)],
                            None => text(".."),
                        });
                        delimited_no_trailing("<<", segs, ">>")
                    }
                }
            }
            P::Constructor {
                name, args, rest, ..
            } => {
                if args.is_empty() && !*rest {
                    return text(&*name.name);
                }
                let mut items: Vec<Doc> = args
                    .iter()
                    .map(|a| match a {
                        ast::PatternArg::Positional(p) => self.pattern(p),
                        ast::PatternArg::Labeled { label, pattern } => {
                            d![text(&*label.name), text(": "), self.pattern(pattern)]
                        }
                    })
                    .collect();
                if *rest {
                    items.push(text(".."));
                    // No trailing comma: the parser rejects `..,` (rest must be
                    // the last arg), so a wrapped pattern must end `..\n)`.
                    return d![text(&*name.name), delimited_no_trailing("(", items, ")")];
                }
                d![text(&*name.name), delimited("(", items, ")")]
            }
            P::Or { patterns, .. } => {
                let ps: Vec<Doc> = patterns.iter().map(|p| self.pattern(p)).collect();
                join(ps, text(" | "))
            }
            P::Range { start, end, .. } => {
                d![self.pattern(start), text(".."), self.pattern(end)]
            }
        }
    }

    // ------------------------------------------------------------------ types

    fn type_(&self, t: &ast::TypeIdentifier) -> Doc {
        match &t.kind {
            ast::TypeKind::TupleType(tt) => {
                let es: Vec<Doc> = tt.elements.iter().map(|e| self.type_(e)).collect();
                delimited("(", es, ")")
            }
            ast::TypeKind::FunctionType(ft) => {
                let ps: Vec<Doc> = ft.params.iter().map(|p| self.type_(p)).collect();
                let r = match &ft.return_type {
                    Some(t) => d![text(" "), self.type_(t)],
                    None => nil(),
                };
                d![text("fn"), delimited("(", ps, ")"), r]
            }
            ast::TypeKind::NamedType(n) => {
                if n.type_args.is_empty() {
                    text(&*n.identifier.name)
                } else {
                    let args: Vec<Doc> = n.type_args.iter().map(|a| self.type_(a)).collect();
                    d![text(&*n.identifier.name), delimited("(", args, ")")]
                }
            }
        }
    }
}

/// Prefer single quotes; switch to double only when the content has a single
/// quote and no double quote (so the switch saves an escape).
fn pick_quote(s: &str) -> char {
    if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    }
}

fn quoted(s: &str) -> String {
    let q = pick_quote(s);
    format!("{q}{}{q}", escape_string(s, q))
}

fn escape_string(s: &str, quote: char) -> String {
    let mut result = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            '\\' => result.push_str("\\\\"),
            '\0' => result.push_str("\\0"),
            '$' => result.push_str("\\$"),
            c if c == quote => {
                result.push('\\');
                result.push(c);
            }
            other => result.push(other),
        }
    }
    result
}

/// Whether a call argument in final position may hug the call's parentheses.
/// Only block-shaped expressions hug — ones that read `head { … }` so the
/// call's closing paren can follow their closing brace. Anything else (calls,
/// arrays, binary expressions, …) keeps the standard one-argument-per-line
/// fallback even when it spans multiple lines.
fn arg_can_hug(a: &ast::CallArg) -> bool {
    let e = match a {
        ast::CallArg::Positional(e) => e,
        ast::CallArg::Labeled { value, .. } => value,
        ast::CallArg::Spread(e) => e,
    };
    matches!(
        e,
        ast::Expression::FunctionExpression(_)
            | ast::Expression::MatchExpression(_)
            | ast::Expression::BlockExpression(_)
            | ast::Expression::IfExpression(_)
    )
}

/// Whether the formatted form of `e` begins with a `-` glyph. The check walks
/// the leftmost spine of the expression — a prefix `-` is rendered before its
/// operand, a binary/call/index/range/property/or expression before its left
/// child — so it mirrors which glyph the formatter actually emits first.
fn starts_with_minus(e: &ast::Expression) -> bool {
    use ast::Expression as E;
    match e {
        E::UnaryExpression(u) => u.op.kind == Kind::PuncMinus,
        E::BinaryExpression(b) => starts_with_minus(&b.left),
        E::PropertyAccessExpression(p) => starts_with_minus(&p.left),
        E::ArrayIndexExpression(a) => starts_with_minus(&a.expression),
        E::FunctionCallExpression(c) => starts_with_minus(&c.callee),
        E::OrExpression(o) => starts_with_minus(&o.expression),
        E::RangeExpression(r) => starts_with_minus(&r.start),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(src: &str) -> String {
        format_with_debug(src, false).output
    }

    #[test]
    fn idempotent_on_examples() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples");
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|s| s.to_str()) != Some("al") {
                continue;
            }
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            if name == "demo_syntax_errors.al" {
                continue;
            }
            let src = std::fs::read_to_string(&path).unwrap();
            let r1 = format_with_debug(&src, false);
            if r1.has_errors {
                // Source itself does not parse — outside the formatter's remit.
                continue;
            }
            let once = r1.output;
            let r2 = format_with_debug(&once, false);
            assert!(
                !r2.has_errors,
                "formatted output of {name} does not re-parse:\n{once}"
            );
            assert_eq!(once, r2.output, "formatter not idempotent for {name}");
        }
    }

    #[test]
    fn fn_body_always_breaks() {
        let out = fmt("fn f(x Int) Int { if x > 0 { 1 } else { 2 } }\n");
        assert_eq!(out, "fn f(x Int) Int {\n\tif x > 0 { 1 } else { 2 }\n}\n");
    }

    #[test]
    fn trivial_if_stays_inline_in_broken_body() {
        let out = fmt("fn max(a Int, b Int) Int { if a > b { a } else { b } }\n");
        assert_eq!(
            out,
            "fn max(a Int, b Int) Int {\n\tif a > b { a } else { b }\n}\n"
        );
    }

    #[test]
    fn non_trivial_if_branch_breaks() {
        let out = fmt("fn fu(id Int) Option { if id == 0 { None } else { Some(id) } }\n");
        assert_eq!(
            out,
            "fn fu(id Int) Option {\n\tif id == 0 {\n\t\tNone\n\t} else {\n\t\tSome(id)\n\t}\n}\n"
        );
    }

    #[test]
    fn if_chain_breaks_even_when_branches_trivial() {
        let out = fmt(
            "fn c(n Int) String { if n < 0 { 'neg' } else if n == 0 { 'zero' } else { 'pos' } }\n",
        );
        assert_eq!(
            out,
            "fn c(n Int) String {\n\tif n < 0 {\n\t\t'neg'\n\t} else if n == 0 {\n\t\t'zero'\n\t} else {\n\t\t'pos'\n\t}\n}\n"
        );
    }

    #[test]
    fn fn_body_braced() {
        let out = fmt("fn perimeter(a Int, b Int, c Int) Int { a + b + c }\n");
        assert_eq!(
            out,
            "fn perimeter(a Int, b Int, c Int) Int {\n\ta + b + c\n}\n"
        );
    }

    #[test]
    fn long_if_breaks() {
        let src = "fn f(a Int, b Int, c Int) String { if !g(a, b, c) { 'Invalid' } else if a == b && b == c { 'Equilateral' } else if a == b || b == c || a == c { 'Isosceles' } else { 'Scalene' } }\n";
        let out = fmt(src);
        assert!(
            out.contains("\t} else if a == b && b == c {\n\t\t'Equilateral'\n"),
            "got:\n{out}"
        );
        assert!(
            out.contains("\t} else {\n\t\t'Scalene'\n\t}\n"),
            "got:\n{out}"
        );
    }

    #[test]
    fn type_shorthand_always_breaks() {
        let out = fmt("type Point { Point(x Int y Int) }\n");
        assert_eq!(out, "type Point {\n\tx Int\n\ty Int\n}\n");
    }

    #[test]
    fn type_single_nullary_ctor_explicit() {
        let out = fmt("type Nil { Nil }\n");
        assert_eq!(out, "type Nil {\n\tNil\n}\n");
    }

    #[test]
    fn lambda_braceless() {
        let out = fmt("f = fn(x Int) { x * 2 }\n");
        assert_eq!(out, "f = fn(x Int) x * 2\n");
    }

    #[test]
    fn pub_preserves_blank_line() {
        let out = fmt("pub fn a() Int { 1 }\n\npub fn b() Int { 2 }\n");
        assert_eq!(
            out,
            "pub fn a() Int {\n\t1\n}\n\npub fn b() Int {\n\t2\n}\n"
        );
    }

    #[test]
    fn type_multi_variant_explicit() {
        let out = fmt("type Maybe(a) { Just(value A) Nothing }\n");
        assert_eq!(out, "type Maybe(a) {\n\tJust(value A)\n\tNothing\n}\n");
    }

    #[test]
    fn match_top_level_wildcard_emits_else() {
        let out = fmt("fn f(x Int) String { match x { 1 -> 'one'\n _ -> 'other' } }\n");
        assert!(out.contains("else -> 'other'\n"), "got: {out}");
        let out2 = fmt("fn g(o Option(Int)) String { match o { Some(_) -> 'y'\n else -> 'n' } }\n");
        assert!(out2.contains("Some(_) -> 'y'"), "nested _: {out2}");
        assert!(out2.contains("else -> 'n'\n"), "top else: {out2}");
    }

    #[test]
    fn double_quotes_normalise_to_single() {
        assert_eq!(fmt("x = \"hello\"\n"), "x = 'hello'\n");
    }

    #[test]
    fn single_quote_in_content_uses_double() {
        assert_eq!(fmt("x = \"it's fine\"\n"), "x = \"it's fine\"\n");
        assert_eq!(fmt("x = 'it\\'s fine'\n"), "x = \"it's fine\"\n");
    }

    #[test]
    fn both_quotes_prefers_single_with_escape() {
        assert_eq!(fmt("x = \"it's \\\"ok\\\"\"\n"), "x = 'it\\'s \"ok\"'\n");
    }

    #[test]
    fn vm_attribute_on_own_line() {
        let out = fmt("@vm(tcp_listen) pub fn listen(p Int) Result(Server, String)\n");
        assert_eq!(
            out,
            "@vm(tcp_listen)\npub fn listen(p Int) Result(Server, String)\n"
        );
    }

    #[test]
    fn external_type_body_less() {
        let out = fmt("pub type Socket\n");
        assert_eq!(out, "pub type Socket\n");
    }

    #[test]
    fn opaque_type_round_trips() {
        let out = fmt("pub opaque type Id {\n\tn Int\n}\n");
        assert_eq!(out, "pub opaque type Id {\n\tn Int\n}\n");
        let out = fmt("pub opaque type Maybe { Yes No }\n");
        assert_eq!(out, "pub opaque type Maybe {\n\tYes\n\tNo\n}\n");
    }

    #[test]
    fn match_arms_newline_separated_no_commas() {
        let out = fmt("match x { Ok(a) -> a\n Err(e) -> e }\n");
        assert_eq!(out, "match x {\n\tOk(a) -> a\n\tErr(e) -> e\n}\n");
    }

    #[test]
    fn or_handler_overflow_breaks_subject_args() {
        // `subject(...) or e -> handler(...)` wider than the line: the subject
        // call's arguments break and the handler call stays intact — not the
        // other way around (breaking the handler orphans its arguments at the
        // far right of the line).
        let out = fmt(
            "http.serve('0.0.0.0', 8080, fn(_req) http.text('Hello from al/http!')) or e -> println('serve failed: ${e}')\n",
        );
        assert_eq!(
            out,
            "http.serve(\n\t'0.0.0.0',\n\t8080,\n\tfn(_req) http.text('Hello from al/http!'),\n) or e -> println('serve failed: ${e}')\n"
        );
    }

    #[test]
    fn or_block_handler_keeps_subject_flat() {
        // A `{ … }` handler gives the line a natural break point: the subject
        // stays flat and the block body breaks.
        let src = "http.serve('0.0.0.0', 8080, fn(_req) http.text('Hello from al/http!')) or e -> {\n\tprintln('serve failed: ${e}')\n}\n";
        assert_eq!(fmt(src), src);
    }

    #[test]
    fn long_call_breaks_per_arg() {
        let out = fmt(
            "x = MakeThing(first_label: some_function_call(a, b, c), second_label: another_one(d, e, f), third_label: yet_more(g, h, i))\n",
        );
        assert!(out.contains("MakeThing(\n\tfirst_label: "), "got:\n{out}");
        assert!(out.contains(",\n\tthird_label: "), "got:\n{out}");
        assert!(out.contains(",\n)\n"), "got:\n{out}");
    }

    #[test]
    fn block_lambda_arg_hugs_call_parens() {
        // A multi-statement lambda as the only argument hugs the call's
        // parentheses instead of forcing the argument list to break.
        let src = "scheduler.spawn(fn() {\n\tprintln('Hello!')\n\tprintln('Hello!')\n})\n";
        assert_eq!(fmt(src), src);
    }

    #[test]
    fn match_arg_hugs_call_parens() {
        let src = "println(match 10 {\n\t0 -> 'zero'\n\telse -> 'else'\n})\n";
        assert_eq!(fmt(src), src);
    }

    #[test]
    fn block_lambda_after_leading_args_hugs() {
        // Leading arguments stay on the call head line; the lambda hugs.
        let src = "scheduler.run_after(1000, fn() {\n\tprintln('a')\n\tprintln('b')\n})\n";
        assert_eq!(fmt(src), src);
    }

    #[test]
    fn hug_falls_back_when_head_does_not_fit() {
        // The head line (callee, leading arguments, and the lambda's `{`) is
        // wider than the limit: every argument goes onto its own line.
        let out = fmt(&format!(
            "serve('{}', '{}', fn(request) {{\n\thandle(request)\n\thandle(request)\n}})\n",
            "a".repeat(40),
            "b".repeat(40),
        ));
        assert_eq!(
            out,
            format!(
                "serve(\n\t'{}',\n\t'{}',\n\tfn(request) {{\n\t\thandle(request)\n\t\thandle(request)\n\t}},\n)\n",
                "a".repeat(40),
                "b".repeat(40),
            )
        );
    }

    #[test]
    fn multiline_lambda_in_non_final_position_breaks_args() {
        // Only a *final* block-shaped argument hugs. A multi-line lambda
        // followed by another argument forces the standard per-argument
        // layout.
        let src = "f(fn() {\n\ta()\n\tb()\n}, other)\n";
        assert_eq!(
            fmt(src),
            "f(\n\tfn() {\n\t\ta()\n\t\tb()\n\t},\n\tother,\n)\n"
        );
    }

    #[test]
    fn hugging_call_as_argument_breaks_outer_call() {
        // A call whose lambda hugs still renders across multiple lines, so an
        // outer call wrapping it breaks per-argument (the hug never leaks
        // upward through another call).
        let src = "outer(inner(fn() {\n\ta()\n\tb()\n}), other)\n";
        assert_eq!(
            fmt(src),
            "outer(\n\tinner(fn() {\n\t\ta()\n\t\tb()\n\t}),\n\tother,\n)\n"
        );
    }

    #[test]
    fn hugged_lambda_body_still_wraps_wide_statements() {
        // Statements inside a hugged lambda still re-probe their own width:
        // a too-wide call inside the body breaks per-argument.
        let a = "a".repeat(45);
        let b = "b".repeat(45);
        let src = format!("go(fn() {{\n\tfirst()\n\tprocess('{a}', '{b}')\n}})\n");
        let expected =
            format!("go(fn() {{\n\tfirst()\n\tprocess(\n\t\t'{a}',\n\t\t'{b}',\n\t)\n}})\n");
        assert_eq!(fmt(&src), expected);
    }

    #[test]
    fn hugged_call_chains_format_each_link() {
        // Each call in a member chain makes its own hug decision.
        let src = "list.map(fn(x) {\n\tlog(x)\n\tx * 2\n}).filter(fn(x) {\n\tlog(x)\n\tx > 0\n})\n";
        assert_eq!(fmt(src), src);
    }

    #[test]
    fn blank_line_preserved() {
        let out = fmt("fn a() Int { 1 }\n\nfn b() Int { 2 }\n");
        assert_eq!(out, "fn a() Int {\n\t1\n}\n\nfn b() Int {\n\t2\n}\n");
    }

    #[test]
    fn excess_blank_lines_collapse() {
        let out = fmt("fn a() Int { 1 }\n\n\n\n\nfn b() Int { 2 }\n");
        assert_eq!(out, "fn a() Int {\n\t1\n}\n\n\nfn b() Int {\n\t2\n}\n");
    }

    #[test]
    fn binary_literal_round_trip() {
        let out = fmt("x = <<1, 2:4, n:size(w), s:utf8, body:bytes(len), tail:binary>>\n");
        assert_eq!(
            out,
            "x = <<1, 2:4, n:size(w), s:utf8, body:bytes(len), tail:binary>>\n"
        );
        assert_eq!(fmt("x = <<>>\n"), "x = <<>>\n");
    }

    #[test]
    fn binary_pattern_round_trip() {
        let out =
            fmt("fn f(b Binary) Int { match b { <<a, b:4, _:4, ..rest>> -> a\n else -> 0 } }\n");
        assert!(out.contains("<<a, b:4, _:4, ..rest>> -> a"), "got:\n{out}");
        let out2 = fmt("fn g(b Binary) Int { match b { <<x, ..>> -> x\n <<>> -> 0 } }\n");
        assert!(out2.contains("<<x, ..>> -> x"), "got:\n{out2}");
        assert!(out2.contains("<<>> -> 0"), "got:\n{out2}");
    }

    #[test]
    fn constructor_pattern_rest_wraps_without_trailing_comma() {
        // A constructor pattern with a `..` rest marker that wraps across lines
        // must not emit `..,`: the parser rejects a comma after `..` (rest must
        // be the last arg), so the formatted source would no longer parse.
        let src = "result = match value {\n\tVeryLongConstructorNameHere(first_argument_name, \
                   second_argument_name, third_argument_name, fourth_argument_name, \
                   fifth_argument_name, ..) -> 1\n\telse -> 0\n}\n";
        let out = fmt(src);
        assert!(
            out.contains("\t\t.."),
            "expected rest marker to wrap:\n{out}"
        );
        assert!(!out.contains("..,"), "trailing comma after rest:\n{out}");
        // The formatted output must re-parse and be idempotent (round-trip).
        let r = format_with_debug(&out, false);
        assert!(!r.has_errors, "formatted output does not re-parse:\n{out}");
        assert_eq!(out, r.output, "formatter not idempotent:\n{out}");
    }

    #[test]
    fn comment_preserved_above_fn() {
        let out = fmt("// hello\nfn a() Int { 1 }\n");
        assert_eq!(out, "// hello\nfn a() Int {\n\t1\n}\n");
    }

    #[test]
    fn triangle_case() {
        let src = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/triangle.al"),
        )
        .unwrap();
        let out = fmt(&src);
        assert!(
            out.contains("\t} else if a == b && b == c {\n\t\t'Equilateral'\n"),
            "classify_triangle should break per-clause:\n{out}"
        );
        assert!(
            out.contains("TriangleInfo(\n\t\tsides: "),
            "TriangleInfo call should break per-arg:\n{out}"
        );
    }

    /// Number of top-level items the source parses into. Used to prove the
    /// formatter does not silently split or merge program structure.
    fn top_level_items(src: &str) -> usize {
        let mut s = scanner::new_scanner(src.to_string());
        let toks = s.scan_all();
        let diags = s.get_diagnostics();
        let mut p = parser::new_parser_from_tokens(toks, diags);
        p.parse_program().ast.body.len()
    }

    #[test]
    fn wrapped_subtraction_keeps_operator_trailing() {
        // A single subtraction wide enough to exceed MAX_WIDTH must wrap with the
        // operator at the END of the left operand's line. The parser treats a `-`
        // at the start of a line as a fresh unary expression (see parse_level's
        // PuncMinus newline guard), so if the operator wraps to the continuation
        // line the lone subtraction silently re-parses as `left` followed by a
        // standalone `(-right)` — changing program semantics in place.
        let src = format!("result = {} - {}\n", "a".repeat(50), "b".repeat(50));
        assert_eq!(top_level_items(&src), 1, "source should be one binding");

        let out = fmt(&src);
        assert!(
            out.contains('\n'),
            "expected the subtraction to wrap:\n{out}"
        );
        for line in out.lines() {
            assert!(
                !line.trim_start().starts_with('-'),
                "operator wrapped to the start of a continuation line; this \
                 re-parses as unary negation:\n{out}"
            );
        }

        // The formatted text must re-parse to the same single binding, not a
        // binding plus a stray unary expression.
        assert_eq!(
            top_level_items(&out),
            1,
            "formatting split one subtraction into two top-level items:\n{out}"
        );
    }

    #[test]
    fn nested_unary_minus_keeps_space() {
        // `- -x` must not be printed as `--x`: the scanner greedily relexes `--`
        // as the rejected decrement token, so the formatted source would fail to
        // parse on the round trip — corrupting a valid file in place.
        let out = fmt("x = - -y\n");
        assert_eq!(out, "x = - -y\n");
        assert!(
            !out.contains("--"),
            "nested unary minus collapsed into the `--` decrement token:\n{out}"
        );

        let r = format_with_debug(&out, false);
        assert!(!r.has_errors, "formatted output does not re-parse:\n{out}");
        assert_eq!(out, r.output, "formatter not idempotent:\n{out}");
    }

    #[test]
    fn deeply_nested_unary_minus_keeps_spaces() {
        let out = fmt("x = - - -y\n");
        assert_eq!(out, "x = - - -y\n");
        let r = format_with_debug(&out, false);
        assert!(!r.has_errors, "formatted output does not re-parse:\n{out}");
        assert_eq!(out, r.output, "formatter not idempotent:\n{out}");
    }

    #[test]
    fn unary_minus_before_non_minus_stays_tight() {
        // A `-` whose operand does not itself lead with `-` must not gain a
        // spurious space — only the `--` collision needs separating.
        assert_eq!(fmt("x = -y\n"), "x = -y\n");
        assert_eq!(fmt("x = -f(y)\n"), "x = -f(y)\n");
        assert_eq!(fmt("x = -y.z\n"), "x = -y.z\n");
    }

    #[test]
    fn double_not_stays_adjacent() {
        // `!!x` lexes as two `!` tokens and parses fine, so it must stay tight;
        // the spacing fix is specific to `-`.
        let out = fmt("x = !!y\n");
        assert_eq!(out, "x = !!y\n");
        let r = format_with_debug(&out, false);
        assert!(!r.has_errors, "formatted output does not re-parse:\n{out}");
    }
}
