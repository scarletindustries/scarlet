use std::collections::HashMap;

use crate::ast;
use crate::d;
use crate::diagnostic::{Diagnostic, Severity};
use crate::parser;
use crate::scanner;
use crate::span::Span;
use crate::token::{Kind, Token, Trivia};

pub mod doc;
use doc::{
    Doc, block, delimited, delimited_commas_when_flat, delimited_hug, delimited_no_trailing, group,
    group_willing, hard_braces, hard_list, hard_list_bare, hardline, hardlines, join, line, nest,
    nil, text,
};

const MAX_WIDTH: isize = 100;

/// A constructor with this many fields always breaks one-per-line.
const FIELDS_PER_LINE: usize = 4;

pub enum FormatResult {
    Formatted {
        output: String,
    },
    ParseFailed {
        errors: Vec<Diagnostic>,
    },
    /// A formatter bug: layout dropped an input comment. No output; callers
    /// must leave the source untouched. `comment` is one of the lost ones.
    CommentsLost {
        comment: String,
    },
    /// A formatter bug: the output does not re-parse into a program with the
    /// same number of top-level nodes. No output; callers must leave the source
    /// untouched.
    OutputInvalid {
        detail: String,
    },
}

/// Every comment in the tokens' leading trivia, keyed by trimmed text with its
/// count. The comment-loss check compares an input census to an output census,
/// so both must key identically: they share this one function.
fn comment_census(tokens: &[Token]) -> HashMap<String, usize> {
    let mut census: HashMap<String, usize> = HashMap::new();
    for tok in tokens {
        for t in &tok.leading_trivia {
            if let Some(c) = t.comment_text() {
                *census.entry(c.trim_end().to_string()).or_default() += 1;
            }
        }
    }
    census
}

pub fn format(input: &str) -> FormatResult {
    let mut s = scanner::new_scanner(input.to_string());
    let (scanned_tokens, scanner_diagnostics) = s.scan_all();

    // Census the input so a comment the formatter fails to re-emit makes
    // `format` a no-op instead of silent data loss.
    let expected_comments = comment_census(&scanned_tokens);

    let mut trivia_map: HashMap<Pos, Vec<Trivia>> = HashMap::new();
    let mut eof_trivia: Vec<Trivia> = Vec::new();
    for tok in &scanned_tokens {
        if tok.kind == Kind::Eof {
            eof_trivia = tok.leading_trivia.clone();
        } else if !tok.leading_trivia.is_empty() {
            trivia_map.insert(Pos::start_of(&tok.span), tok.leading_trivia.clone());
        }
    }

    // A spread's `..` token precedes the spread expression's span, so a comment
    // above `..x` attaches to the `..`, not to where the AST span starts. Map
    // each token back to a preceding `..` so `call_arg` can find that trivia.
    let mut dotdot_before: HashMap<Pos, Pos> = HashMap::new();
    for pair in scanned_tokens.windows(2) {
        if pair[0].kind == Kind::PuncDotdot {
            dotdot_before.insert(Pos::start_of(&pair[1].span), Pos::start_of(&pair[0].span));
        }
    }

    let p = parser::new_parser_from_tokens(scanned_tokens, scanner_diagnostics);
    let result = p.parse_program();

    let errors: Vec<Diagnostic> = result
        .diagnostics
        .into_iter()
        .filter(|d| d.severity == Severity::Error)
        .collect();

    if !errors.is_empty() {
        return FormatResult::ParseFailed { errors };
    }

    let f = Formatter {
        trivia_map,
        dotdot_before,
    };
    let body = f.program(&result.ast, &eof_trivia);
    let mut output = doc::layout(&body, MAX_WIDTH);
    while output.ends_with('\n') {
        output.pop();
    }
    output.push('\n');

    // Postcondition: every input comment made it into the output. Comparing
    // trivia to trivia, never raw substrings, so a string literal that looks
    // like a comment cannot mask a real loss.
    let mut rescan = scanner::new_scanner(output.clone());
    let (rescanned_tokens, rescan_diagnostics) = rescan.scan_all();

    if !expected_comments.is_empty() {
        let actual_comments = comment_census(&rescanned_tokens);
        for (comment, &count) in &expected_comments {
            if actual_comments.get(comment).copied().unwrap_or(0) < count {
                return FormatResult::CommentsLost {
                    comment: comment.clone(),
                };
            }
        }
    }

    // Postcondition: the output scans and parses into the same number of
    // top-level nodes. `- -x` collapsing to the rejected `--` token was a real
    // bug of this shape, and it must never overwrite valid source in place.
    let rp = parser::new_parser_from_tokens(rescanned_tokens, rescan_diagnostics);
    let reparsed = rp.parse_program();
    if let Some(d) = reparsed
        .diagnostics
        .iter()
        .find(|d| d.severity == Severity::Error)
    {
        return FormatResult::OutputInvalid {
            detail: format!(
                "output does not re-parse: {} at {}:{}",
                d.message, d.span.start_line, d.span.start_column
            ),
        };
    }
    if reparsed.ast.body.len() != result.ast.body.len() {
        return FormatResult::OutputInvalid {
            detail: format!(
                "output re-parses to {} top-level nodes, input had {}",
                reparsed.ast.body.len(),
                result.ast.body.len()
            ),
        };
    }

    FormatResult::Formatted { output }
}

/// A scanner line/column position, keying trivia by the token it attaches to.
/// A named type so transposing line and column is a compile error, not a silent
/// lookup miss.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Pos {
    line: i32,
    column: i32,
}

impl Pos {
    fn start_of(span: &Span) -> Pos {
        Pos {
            line: span.start_line,
            column: span.start_column,
        }
    }

    /// Start position of the closing `}` ending `span`. Assumes `span` ends in
    /// a one-column `}` token, so `end_column - 1` is the brace's start.
    fn closing_brace_of(span: &Span) -> Pos {
        Pos {
            line: span.end_line,
            column: span.end_column - 1,
        }
    }
}

struct Formatter {
    trivia_map: HashMap<Pos, Vec<Trivia>>,
    /// Start position of a `..` token, keyed by the start position of the
    /// token that immediately follows it. See `format`.
    dotdot_before: HashMap<Pos, Pos>,
}

impl Formatter {
    fn trivia_at(&self, s: Span) -> Option<&[Trivia]> {
        self.trivia_map
            .get(&Pos::start_of(&s))
            .map(|v| v.as_slice())
    }

    /// Trivia on the closing `}` of `s`: comments after a block's last node.
    fn closing_brace_trivia(&self, s: Span) -> Option<&[Trivia]> {
        self.trivia_map
            .get(&Pos::closing_brace_of(&s))
            .map(|v| v.as_slice())
    }

    /// Leading trivia for a node, ending ready for the node text. Preserved
    /// blank lines are capped at `max_blanks`.
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
            if let Some(c) = t.comment_text() {
                let blanks = if emitted_any {
                    newlines.saturating_sub(1).min(max_blanks)
                } else {
                    0
                };
                if emitted_any {
                    parts.push(hardlines(1 + blanks));
                }
                parts.push(text(c.trim_end().to_string()));
                emitted_any = true;
                newlines = 0;
            } else {
                newlines += 1;
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
        let Some(trivia) = self.closing_brace_trivia(s) else {
            return nil();
        };
        let mut parts = Vec::new();
        for t in trivia {
            if let Some(c) = t.comment_text() {
                parts.push(hardline());
                parts.push(text(c.trim_end().to_string()));
            }
        }
        doc::concat(parts)
    }

    /// Comments on the token starting `s`, each on its own line above the
    /// node, or nil so flat layouts stay flat. Unlike `leading_trivia` this
    /// emits no separating newline of its own and drops blank lines between
    /// comments. For nodes inside delimited lists and `if`/`else` chains.
    fn comments_before(&self, s: Span) -> Doc {
        self.comments_before_at(Pos::start_of(&s))
    }

    fn comments_before_at(&self, pos: Pos) -> Doc {
        let Some(trivia) = self.trivia_map.get(&pos).map(|v| v.as_slice()) else {
            return nil();
        };
        let mut parts = Vec::new();
        for t in trivia {
            if let Some(c) = t.comment_text() {
                parts.push(text(c.trim_end().to_string()));
                parts.push(hardline());
            }
        }
        doc::concat(parts)
    }

    fn has_comment_at(&self, s: Span) -> bool {
        self.trivia_at(s)
            .is_some_and(|tr| tr.iter().any(|t| t.comment_text().is_some()))
    }

    fn program(&self, block: &ast::BlockExpression, eof_trivia: &[Trivia]) -> Doc {
        d![
            self.nodes_with_trivia(&block.body, 2),
            self.trivia_doc(eof_trivia, block.body.is_empty(), 2),
        ]
    }

    fn node(&self, n: &ast::Node) -> Doc {
        match n {
            ast::Node::Statement(s) => self.statement(s),
            ast::Node::Expression(e) => self.expr(e),
        }
    }

    /// Emit each node preceded by its leading trivia.
    fn nodes_with_trivia(&self, nodes: &[ast::Node], max_blanks: usize) -> Doc {
        let mut parts = Vec::new();
        for (i, n) in nodes.iter().enumerate() {
            parts.push(self.leading_trivia(n.span(), i == 0, max_blanks));
            parts.push(self.node(n));
        }
        doc::concat(parts)
    }

    fn statement(&self, stmt: &ast::Statement) -> Doc {
        match stmt {
            ast::Statement::VariableBinding(s) => {
                let ty = match &s.typ {
                    Some(t) => d![text(" "), self.type_(t)],
                    None => nil(),
                };
                d![
                    text(s.identifier.name.clone()),
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
                d![
                    text(s.ty_name.name.clone()),
                    text(" = "),
                    self.expr(&s.init)
                ]
            }
            ast::Statement::CtorDestructuringBinding(s) => {
                d![
                    self.pattern(&s.as_pattern()),
                    text(" = "),
                    self.expr(&s.init)
                ]
            }
            ast::Statement::Declaration { decl, public } => self.declaration(decl, *public),
            ast::Statement::ImportDeclaration(s) => {
                let mut out = vec![text("import "), text(s.path.to_string())];
                if let Some(a) = &s.alias {
                    out.push(text(" as "));
                    out.push(text(a.name.clone()));
                }
                if !s.items.is_empty() {
                    let items: Vec<Doc> = s
                        .items
                        .iter()
                        .map(|it| match &it.alias {
                            Some(a) => d![
                                text(it.name.name.clone()),
                                text(" as "),
                                text(a.name.clone())
                            ],
                            None => text(it.name.name.clone()),
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
            parts.push(text(a.name.name.clone()));
            if !a.args.is_empty() {
                let args: Vec<Doc> = a.args.iter().map(|id| text(id.name.clone())).collect();
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
                    text(s.identifier.name.clone()),
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

    /// A parameter list the author broke after `(` stays broken. Width alone
    /// is the wrong signal: a signature can fit and still read worse than
    /// one-per-line, and reflowing discards the author's choice.
    fn params_broken_by_author(&self, header_line: i32, params: &[ast::FunctionParameter]) -> bool {
        params
            .first()
            .is_some_and(|p| p.identifier.span.start_line > header_line)
    }

    fn fn_header(
        &self,
        name: Option<&str>,
        params: &[ast::FunctionParameter],
        ret: &Option<ast::TypeIdentifier>,
        header_line: i32,
    ) -> Doc {
        let head = match name {
            Some(n) => text(format!("fn {n}")),
            None => text("fn"),
        };
        let ps: Vec<Doc> = params
            .iter()
            .map(|p| match &p.typ {
                Some(t) => d![text(p.identifier.name.clone()), text(" "), self.type_(t)],
                None => text(p.identifier.name.clone()),
            })
            .collect();
        let r = match ret {
            Some(t) => d![text(" "), self.type_(t)],
            None => nil(),
        };
        let list = if self.params_broken_by_author(header_line, params) {
            hard_list("(", ps, ")")
        } else {
            delimited("(", ps, ")")
        };
        d![head, list, r]
    }

    fn fn_decl(&self, f: &ast::FunctionDeclaration) -> Doc {
        let header = self.fn_header(
            Some(&f.identifier.name),
            &f.params,
            &f.return_type,
            f.identifier.span.start_line,
        );
        match &f.body {
            ast::FnBody::Block(b) => d![header, text(" "), self.fn_body(b)],
            ast::FnBody::Vm(_) => header,
        }
    }

    fn fn_expr(&self, f: &ast::FunctionExpression) -> Doc {
        d![
            self.fn_header(None, &f.params, &None, f.span.start_line),
            text(" "),
            self.lambda_body(&f.body),
        ]
    }

    /// A named `fn` body is always a hard multi-line block, never collapsed
    /// onto the signature line. Lambdas go through `lambda_body` instead.
    fn fn_body(&self, body: &ast::Expression) -> Doc {
        if let ast::Expression::BlockExpression(b) = body {
            let trail = self.trailing_comments(b.span);
            if b.body.is_empty() && trail.is_nil() {
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
            && self.trailing_comments(b.span).is_nil()
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
            let ps: Vec<Doc> = s.type_params.iter().map(|p| text(p.name.clone())).collect();
            delimited("(", ps, ")")
        };
        let prefix = match &s.body {
            ast::TypeBody::Variants { opaque: true, .. } => text("opaque "),
            _ => nil(),
        };
        let head = d![
            prefix,
            text("type "),
            text(s.identifier.name.clone()),
            params
        ];
        match &s.body {
            ast::TypeBody::External => head,
            ast::TypeBody::Alias(t) => d![head, text(" = "), self.type_(t)],
            ast::TypeBody::Variants { ctors, .. } => {
                // Shorthand: one constructor named after the type, with at
                // least one field, emits the fields directly inside `{}`.
                if ctors.len() == 1
                    && ctors[0].identifier.name == s.identifier.name
                    && !ctors[0].fields.is_empty()
                {
                    let c = &ctors[0];
                    let mut parts = Vec::new();
                    for (i, f) in c.fields.iter().enumerate() {
                        // `leading_trivia` emits the separating hardline and
                        // any comment above the field. A hand-written hardline
                        // here deletes the comment.
                        parts.push(self.leading_trivia(f.span, i == 0, 1));
                        parts.push(d![
                            text(f.label.name.clone()),
                            text(" "),
                            self.type_(&f.typ)
                        ]);
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
            return text(c.identifier.name.clone());
        }
        let fields: Vec<Doc> = c
            .fields
            .iter()
            .map(|f| d![text(f.label.name.clone()), text(" "), self.type_(&f.typ)])
            .collect();
        // Four or more fields always go one per line: a wide constructor can
        // fit under MAX_WIDTH and still be unreadable. A broken list needs no
        // commas, and the parser accepts either form.
        let list = if fields.len() >= FIELDS_PER_LINE {
            hard_list_bare("(", fields, ")")
        } else {
            delimited_commas_when_flat("(", fields, ")")
        };
        d![text(c.identifier.name.clone()), list]
    }

    fn expr(&self, e: &ast::Expression) -> Doc {
        use ast::Expression as E;
        match e {
            E::StringLiteral(s) => text(quoted(&s.value)),
            E::InterpolatedString(s) => {
                let literal: String = s
                    .parts
                    .iter()
                    .filter_map(|p| match p {
                        ast::InterpPart::Literal(sl) => Some(sl.value.as_str()),
                        ast::InterpPart::Expr(_) => None,
                    })
                    .collect();
                let q = pick_quote(&literal);
                let mut parts: Vec<Doc> = vec![text(q.to_string())];
                for part in &s.parts {
                    match part {
                        ast::InterpPart::Literal(sl) => {
                            parts.push(text(escape_string(&sl.value, q)))
                        }
                        // A real Doc, not a pre-rendered string, so hard breaks
                        // carry the enclosing indent. Newlines inside `${…}` are
                        // ordinary tokens and never change the string's value.
                        ast::InterpPart::Expr(e) => {
                            parts.push(text("${"));
                            parts.push(group(self.expr(e)));
                            parts.push(text("}"));
                        }
                    }
                }
                parts.push(text(q.to_string()));
                doc::concat(parts)
            }
            E::NumberLiteral(n) => text(n.value.clone()),
            E::Identifier(id) => text(id.name.clone()),
            E::BinaryExpression(b) => group(d![
                self.expr(&b.left),
                text(format!(" {}", b.op.symbol())),
                line(),
                self.expr(&b.right),
            ]),
            E::UnaryExpression(u) => {
                // `- -x` must not collapse into `--x`: the scanner relexes
                // `--` as the rejected decrement token, so the formatted
                // program would no longer parse.
                let gap = if u.op == ast::UnaryOp::Neg && starts_with_minus(&u.expression) {
                    text(" ")
                } else {
                    nil()
                };
                d![text(u.op.symbol()), gap, self.expr(&u.expression)]
            }
            E::BlockExpression(b) => self.block_expr(b),
            E::IfExpression(_) => self.if_chain(e),
            E::MatchExpression(m) => self.match_expr(m),
            E::OrExpression(o) => {
                let recv = match &o.receiver {
                    Some(r) => d![text(r.name.clone()), text(" -> ")],
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
                // A block-shaped final argument hugs the parentheses,
                // `f(a, fn() { … })`, instead of one argument per line.
                let parens = if c.arguments.last().is_some_and(arg_can_hug) {
                    delimited_hug("(", args, ")")
                } else {
                    delimited("(", args, ")")
                };
                d![self.expr(&c.callee), parens]
            }
            E::PropertyAccessExpression(p) => {
                let key = match &p.right {
                    ast::PropertyKey::Field(id) => text(id.name.clone()),
                    ast::PropertyKey::TupleIndex(n) => text(n.value.clone()),
                };
                d![self.expr(&p.left), text("."), key]
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
                            self.comments_before(s.span),
                            self.expr(&s.value),
                            self.bin_size_spec(&s.spec, is_string)
                        ]
                    })
                    .collect();
                delimited("<<", segs, ">>")
            }
            E::TupleExpression(t) => {
                let items: Vec<Doc> = t
                    .elements
                    .iter()
                    .map(|e| d![self.comments_before(e.span()), self.expr(e)])
                    .collect();
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
            // `format()` returns `ParseFailed` before building any Doc when
            // there are errors, so reaching here means a parser path built an
            // ErrorNode without recording one.
            #[allow(clippy::unreachable)]
            E::ErrorNode(_) => unreachable!("ErrorNode reached formatter; parser emitted no Error"),
        }
    }

    fn call_arg(&self, a: &ast::CallArg) -> Doc {
        // A spread's span starts after the `..`, but a comment above the
        // argument attaches to the `..` token. Query both positions.
        let comments = match a {
            ast::CallArg::Spread(e) => {
                let start = Pos::start_of(&e.span());
                let before_dotdot = self
                    .dotdot_before
                    .get(&start)
                    .map_or(nil(), |&p| self.comments_before_at(p));
                d![before_dotdot, self.comments_before_at(start)]
            }
            _ => self.comments_before(a.span()),
        };
        let arg = match a {
            ast::CallArg::Positional(e) => self.expr(e),
            ast::CallArg::Labeled { label, value } => {
                d![text(label.name.clone()), text(": "), self.expr(value)]
            }
            ast::CallArg::Spread(e) => d![text(".."), self.expr(e)],
        };
        d![comments, arg]
    }

    /// The `:spec` suffix of a `<<>>` segment, inverting `parse_bin_size_spec`.
    /// A string-literal segment's Utf8 kind is the parser default, so its
    /// `:utf8` is omitted.
    fn bin_size_spec(&self, spec: &ast::BinSpec, value_is_string: bool) -> Doc {
        match spec {
            ast::BinSpec::Int { bits: None } => nil(),
            ast::BinSpec::Int {
                bits: Some(ast::Expression::NumberLiteral(n)),
            } => d![text(":"), text(n.value.clone())],
            ast::BinSpec::Int { bits: Some(e) } => d![text(":size("), self.expr(e), text(")")],
            ast::BinSpec::Binary { bytes: None } => text(":binary"),
            ast::BinSpec::Binary { bytes: Some(e) } => {
                d![text(":bytes("), self.expr(e), text(")")]
            }
            ast::BinSpec::Utf8 if value_is_string => nil(),
            ast::BinSpec::Utf8 => text(":utf8"),
        }
    }

    fn array_elem(&self, e: &ast::ArrayElement) -> Doc {
        match e {
            ast::ArrayElement::Expression(e) => {
                d![self.comments_before(e.span()), self.expr(e)]
            }
            ast::ArrayElement::SpreadElement(se) => d![
                self.comments_before(se.span),
                text(".."),
                self.expr(&se.expression)
            ],
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
        if b.body.is_empty() && trail.is_nil() {
            return text("{}");
        }
        let hard = !trail.is_nil() || b.body.iter().any(|n| self.has_comment_at(n.span()));
        let body = d![self.nodes_with_trivia(&b.body, 1), trail];
        if b.body.len() <= 1 && !hard {
            block(body)
        } else {
            hard_braces(body)
        }
    }

    fn if_chain(&self, e: &ast::Expression) -> Doc {
        // Unroll an if/else-if/else chain into a flat clause list so they share
        // one group and break together.
        struct Clause<'a> {
            /// Where a comment between `else` and `if` attaches.
            if_span: Span,
            cond: &'a ast::Expression,
            body: &'a ast::Expression,
        }
        let mut clauses: Vec<Clause> = Vec::new();
        let mut cur = e;
        let else_body = loop {
            match cur {
                ast::Expression::IfExpression(i) => {
                    clauses.push(Clause {
                        if_span: i.span,
                        cond: &i.condition,
                        body: &i.body,
                    });
                    cur = &i.else_body;
                }
                other => break other,
            }
        };
        // Only a ternary-shaped if/else stays on one line: no chain, no
        // comments, every branch a bare atom. Anything heavier breaks every
        // clause, so the whole reads symmetrically like `match`.
        let is_chain = clauses.len() > 1;
        let cond_comment = clauses.iter().any(|c| self.has_comment_at(c.cond.span()));
        // A comment before a branch's `{` attaches to the brace token, and
        // such a branch cannot stay flat.
        let brace_comment = clauses.iter().any(|c| self.has_comment_at(c.body.span()))
            || self.has_comment_at(else_body.span());
        let trivial = !is_chain
            && !cond_comment
            && !brace_comment
            && clauses.iter().all(|c| self.is_trivial_branch(c.body))
            && self.is_trivial_branch(else_body);
        let body = |b: &ast::Expression| {
            if trivial {
                self.body_as_block(b)
            } else {
                self.body_as_hard_block(b)
            }
        };
        let mut docs: Vec<Doc> = Vec::new();
        for (i, clause) in clauses.iter().enumerate() {
            // A comment between `else` and `if` attaches to the nested `if`
            // token. The chain head's own trivia comes from the statement list.
            let kw = if i == 0 {
                text("if ")
            } else {
                d![
                    text("else "),
                    self.comments_before(clause.if_span),
                    text("if ")
                ]
            };
            docs.push(d![
                kw,
                self.comments_before(clause.cond.span()),
                self.expr(clause.cond),
                text(" "),
                self.comments_before(clause.body.span()),
                body(clause.body),
            ]);
        }
        docs.push(d![
            text("else "),
            self.comments_before(else_body.span()),
            body(else_body)
        ]);
        if trivial {
            group(join(docs, line()))
        } else {
            join(docs, text(" "))
        }
    }

    /// A branch is trivial when it is one comment-free bare atom. Only these
    /// may keep an `if` on one line.
    fn is_trivial_branch(&self, b: &ast::Expression) -> bool {
        let ast::Expression::BlockExpression(blk) = b else {
            return false;
        };
        if blk.body.len() != 1 || !self.trailing_comments(blk.span).is_nil() {
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
                d![
                    self.nodes_with_trivia(&b.body, 1),
                    self.trailing_comments(b.span)
                ]
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
            let body = self.expr(&arm.body);
            // A body that ends the line itself hugs the arrow. Anything else
            // gets a break point after the arrow, so an overflowing arm wraps
            // there rather than inside the pattern.
            let tail = if doc::ends_line(&body) {
                d![text(" -> "), body]
            } else {
                group_willing(nest(1, d![text(" ->"), line(), body]))
            };
            arms.push(d![head, guard, tail]);
        }
        let trail = self.trailing_comments(m.span);
        d![
            text("match "),
            self.expr(&m.subject),
            text(" "),
            hard_braces(d![doc::concat(arms), trail]),
        ]
    }

    fn pattern(&self, p: &ast::Pattern) -> Doc {
        use ast::Pattern as P;
        match p {
            P::Wildcard { .. } => text("_"),
            P::Var { name } => text(name.name.clone()),
            P::Literal(ast::PatternLiteral::Number(n)) => text(n.value.clone()),
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
                            Some(b) => d![text(".."), text(b.name.clone())],
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
                            self.comments_before(s.span),
                            self.pattern(&s.value),
                            self.bin_size_spec(&s.spec, is_string)
                        ]
                    })
                    .collect();
                match rest {
                    None => delimited("<<", segs, ">>"),
                    Some(r) => {
                        segs.push(match &r.binding {
                            Some(b) => d![text(".."), text(b.name.clone())],
                            None => text(".."),
                        });
                        delimited_no_trailing("<<", segs, ">>")
                    }
                }
            }
            P::Constructor {
                qualifier,
                name,
                args,
                rest,
                ..
            } => {
                // Dropping the qualifier changes the program: `io.NotFound`
                // resolves through the module, `NotFound` needs an import.
                let head = match qualifier {
                    Some(q) => format!("{}.{}", q.name, name.name),
                    None => name.name.clone(),
                };
                if args.is_empty() && !*rest {
                    return text(head);
                }
                let mut items: Vec<Doc> = args
                    .iter()
                    .map(|a| match a {
                        ast::PatternArg::Positional(p) => self.pattern(p),
                        ast::PatternArg::Labeled { label, pattern } => {
                            d![text(label.name.clone()), text(": "), self.pattern(pattern)]
                        }
                    })
                    .collect();
                if *rest {
                    items.push(text(".."));
                    // No trailing comma: the parser rejects `..,`, so a wrapped
                    // pattern must end `..\n)`.
                    return d![text(head), delimited_no_trailing("(", items, ")")];
                }
                d![text(head), delimited("(", items, ")")]
            }
            P::Or { first, rest, .. } => {
                let ps: Vec<Doc> = std::iter::once(&**first)
                    .chain(rest.iter())
                    .map(|p| self.pattern(p))
                    .collect();
                join(ps, text(" | "))
            }
            P::Range { start, end, .. } => {
                d![
                    text(start.value.clone()),
                    text(".."),
                    text(end.value.clone())
                ]
            }
        }
    }

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
                    text(n.identifier.name.clone())
                } else {
                    let args: Vec<Doc> = n.type_args.iter().map(|a| self.type_(a)).collect();
                    d![text(n.identifier.name.clone()), delimited("(", args, ")")]
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

/// Whether a final call argument may hug the call's parentheses. Only
/// block-shaped expressions do, so the closing paren can follow their closing
/// brace. Everything else keeps the one-argument-per-line fallback.
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

/// Whether the formatted form of `e` begins with a `-` glyph. Walks the
/// leftmost spine, mirroring which glyph the formatter emits first.
fn starts_with_minus(e: &ast::Expression) -> bool {
    use ast::Expression as E;
    match e {
        E::UnaryExpression(u) => u.op == ast::UnaryOp::Neg,
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

    #[track_caller]
    fn fmt(src: &str) -> String {
        match format(src) {
            FormatResult::Formatted { output } => output,
            FormatResult::ParseFailed { errors } => panic!("parse failed: {errors:?}"),
            FormatResult::CommentsLost { comment } => {
                panic!("formatter lost the comment `{comment}` in:\n{src}")
            }
            FormatResult::OutputInvalid { detail } => {
                panic!("formatter produced invalid output ({detail}) for:\n{src}")
            }
        }
    }

    #[track_caller]
    fn assert_round_trips(out: &str) {
        match format(out) {
            FormatResult::Formatted { output } => {
                assert_eq!(out, output, "formatter not idempotent:\n{out}")
            }
            FormatResult::ParseFailed { .. } => {
                panic!("formatted output does not re-parse:\n{out}")
            }
            FormatResult::CommentsLost { comment } => {
                panic!("re-format lost the comment `{comment}`:\n{out}")
            }
            FormatResult::OutputInvalid { detail } => {
                panic!("re-format produced invalid output ({detail}):\n{out}")
            }
        }
    }

    #[test]
    fn idempotent_on_examples() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        for dir in [root.join("examples"), root.join("crates/al/tests/programs")] {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.extension().and_then(|s| s.to_str()) != Some("al") {
                    continue;
                }
                let name = path.file_name().unwrap().to_string_lossy().into_owned();
                let src = std::fs::read_to_string(&path).unwrap();
                let once = match format(&src) {
                    FormatResult::Formatted { output } => output,
                    // Unparseable source is outside the formatter's remit.
                    FormatResult::ParseFailed { .. } => continue,
                    FormatResult::CommentsLost { comment } => {
                        panic!("formatting {name} lost the comment `{comment}`")
                    }
                    FormatResult::OutputInvalid { detail } => {
                        panic!("formatting {name} produced invalid output: {detail}")
                    }
                };
                match format(&once) {
                    FormatResult::Formatted { output } => {
                        assert_eq!(once, output, "formatter not idempotent for {name}")
                    }
                    FormatResult::ParseFailed { .. } => {
                        panic!("formatted output of {name} does not re-parse:\n{once}")
                    }
                    FormatResult::CommentsLost { comment } => {
                        panic!("re-formatting {name} lost the comment `{comment}`")
                    }
                    FormatResult::OutputInvalid { detail } => {
                        panic!("re-formatting {name} produced invalid output: {detail}")
                    }
                }
            }
        }
    }

    #[test]
    fn simple_cases() {
        #[rustfmt::skip]
        let cases: &[(&str, &str, &str)] = &[
            ("fn_body_always_breaks",
             "fn f(x Int) Int { if x > 0 { 1 } else { 2 } }\n",
             "fn f(x Int) Int {\n\tif x > 0 { 1 } else { 2 }\n}\n"),
            ("trivial_if_stays_inline_in_broken_body",
             "fn max(a Int, b Int) Int { if a > b { a } else { b } }\n",
             "fn max(a Int, b Int) Int {\n\tif a > b { a } else { b }\n}\n"),
            ("non_trivial_if_branch_breaks",
             "fn fu(id Int) Option { if id == 0 { None } else { Some(id) } }\n",
             "fn fu(id Int) Option {\n\tif id == 0 {\n\t\tNone\n\t} else {\n\t\tSome(id)\n\t}\n}\n"),
            ("if_chain_breaks_even_when_branches_trivial",
             "fn c(n Int) String { if n < 0 { 'neg' } else if n == 0 { 'zero' } else { 'pos' } }\n",
             "fn c(n Int) String {\n\tif n < 0 {\n\t\t'neg'\n\t} else if n == 0 {\n\t\t'zero'\n\t} else {\n\t\t'pos'\n\t}\n}\n"),
            ("fn_body_braced",
             "fn perimeter(a Int, b Int, c Int) Int { a + b + c }\n",
             "fn perimeter(a Int, b Int, c Int) Int {\n\ta + b + c\n}\n"),
            ("type_shorthand_always_breaks",
             "type Point { Point(x Int, y Int) }\n",
             "type Point {\n\tx Int\n\ty Int\n}\n"),
            ("type_single_nullary_ctor_explicit",
             "type Nil { Nil }\n",
             "type Nil {\n\tNil\n}\n"),
            ("lambda_braceless",
             "f = fn(x Int) { x * 2 }\n",
             "f = fn(x Int) x * 2\n"),
            ("pub_preserves_blank_line",
             "pub fn a() Int { 1 }\n\npub fn b() Int { 2 }\n",
             "pub fn a() Int {\n\t1\n}\n\npub fn b() Int {\n\t2\n}\n"),
            ("type_multi_variant_explicit",
             "type Maybe(a) {\n\tJust(value A)\n\tNothing\n}\n",
             "type Maybe(a) {\n\tJust(value A)\n\tNothing\n}\n"),
            ("double_quotes_normalise_to_single",
             "x = \"hello\"\n",
             "x = 'hello'\n"),
            ("single_quote_in_content_uses_double",
             "x = \"it's fine\"\n",
             "x = \"it's fine\"\n"),
            ("single_quote_escaped_uses_double",
             "x = 'it\\'s fine'\n",
             "x = \"it's fine\"\n"),
            ("both_quotes_prefers_single_with_escape",
             "x = \"it's \\\"ok\\\"\"\n",
             "x = 'it\\'s \"ok\"'\n"),
            ("vm_attribute_on_own_line",
             "@vm(tcp_listen) pub fn listen(p Int) Result(Server, String)\n",
             "@vm(tcp_listen)\npub fn listen(p Int) Result(Server, String)\n"),
            ("external_type_body_less",
             "pub type Socket\n",
             "pub type Socket\n"),
            ("opaque_type_shorthand_round_trips",
             "pub opaque type Id {\n\tn Int\n}\n",
             "pub opaque type Id {\n\tn Int\n}\n"),
            ("opaque_type_variants_round_trips",
             "pub opaque type Maybe {\n\tYes\n\tNo\n}\n",
             "pub opaque type Maybe {\n\tYes\n\tNo\n}\n"),
            ("match_arms_newline_separated_no_commas",
             "match x { Ok(a) -> a\n Err(e) -> e }\n",
             "match x {\n\tOk(a) -> a\n\tErr(e) -> e\n}\n"),
            ("blank_line_preserved",
             "fn a() Int { 1 }\n\nfn b() Int { 2 }\n",
             "fn a() Int {\n\t1\n}\n\nfn b() Int {\n\t2\n}\n"),
            ("excess_blank_lines_collapse",
             "fn a() Int { 1 }\n\n\n\n\nfn b() Int { 2 }\n",
             "fn a() Int {\n\t1\n}\n\n\nfn b() Int {\n\t2\n}\n"),
            ("comment_preserved_above_fn",
             "// hello\nfn a() Int { 1 }\n",
             "// hello\nfn a() Int {\n\t1\n}\n"),
            ("multiline_block_comment_preserved_verbatim",
             "/* a\n   b */\nfn a() Int { 1 }\n",
             "/* a\n   b */\nfn a() Int {\n\t1\n}\n"),
        ];
        for (name, src, expected) in cases {
            let out = fmt(src);
            assert_eq!(out, *expected, "case `{name}`");
            assert_round_trips(&out);
        }
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
    fn match_top_level_wildcard_emits_else() {
        let out = fmt("fn f(x Int) String { match x { 1 -> 'one'\n _ -> 'other' } }\n");
        assert!(out.contains("else -> 'other'\n"), "got: {out}");
        let out2 = fmt("fn g(o Option(Int)) String { match o { Some(_) -> 'y'\n else -> 'n' } }\n");
        assert!(out2.contains("Some(_) -> 'y'"), "nested _: {out2}");
        assert!(out2.contains("else -> 'n'\n"), "top else: {out2}");
    }

    #[test]
    fn or_handler_overflow_breaks_subject_args() {
        // When `subject(...) or e -> handler(...)` overflows, the subject's
        // arguments break and the handler stays intact, not the reverse.
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
        // A `{ … }` handler is the natural break point, so the subject stays
        // flat and the block body breaks.
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
        // A lone multi-statement lambda hugs the parentheses.
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
        // The head line is over the limit, so every argument gets its own.
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
        // Only a final block-shaped argument hugs.
        let src = "f(fn() {\n\ta()\n\tb()\n}, other)\n";
        assert_eq!(
            fmt(src),
            "f(\n\tfn() {\n\t\ta()\n\t\tb()\n\t},\n\tother,\n)\n"
        );
    }

    #[test]
    fn hugging_call_as_argument_breaks_outer_call() {
        // A hug never leaks upward: an outer call still breaks per-argument.
        let src = "outer(inner(fn() {\n\ta()\n\tb()\n}), other)\n";
        assert_eq!(
            fmt(src),
            "outer(\n\tinner(fn() {\n\t\ta()\n\t\tb()\n\t}),\n\tother,\n)\n"
        );
    }

    #[test]
    fn hugged_lambda_body_still_wraps_wide_statements() {
        // Statements inside a hugged lambda still re-probe their own width.
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
        // A wrapped `..` rest marker must not emit `..,`: the parser rejects a
        // comma after `..`, so the output would not parse.
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
        assert_round_trips(&out);
    }

    #[test]
    fn long_match_arm_breaks_after_arrow_not_inside_pattern() {
        // An overflowing arm wraps after the `->`, one indent deeper. The
        // pattern must stay intact.
        let src = "match x {\n\tSome(value) -> quite_long_function_name(value, another_argument, \
                   and_yet_another_long_argument_here)\n\telse -> None\n}\n";
        let out = fmt(src);
        assert_eq!(
            out,
            "match x {\n\tSome(value) ->\n\t\tquite_long_function_name(value, another_argument, \
             and_yet_another_long_argument_here)\n\telse -> None\n}\n"
        );
    }

    #[test]
    fn long_guarded_match_arm_breaks_after_arrow() {
        // Pattern and guard share the head line; the body wraps after `->`.
        let src = "match x {\n\tSome(value) if value <= some_limit(value) - tolerance -> \
                   build_result(value, scale_factor, offset)\n\telse -> None\n}\n";
        let out = fmt(src);
        assert_eq!(
            out,
            "match x {\n\tSome(value) if value <= some_limit(value) - tolerance ->\n\t\t\
             build_result(value, scale_factor, offset)\n\telse -> None\n}\n"
        );
    }

    #[test]
    fn match_arm_block_body_hugs_arrow() {
        // A body that ends the line itself keeps hugging the arrow.
        let src = "match x {\n\tSome(v) -> match v {\n\t\t0 -> 'zero'\n\t\telse -> 'more'\n\t}\n\telse -> 'none'\n}\n";
        assert_eq!(fmt(src), src);
    }

    #[test]
    fn if_chain_and_struct_call_break() {
        // Inlined rather than read from examples/: an over-long if-chain and
        // struct call must each break one clause or argument per line.
        let src = "type TriangleInfo {\n\tsides String\n\ttriangle_type String\n\tperimeter Int\n\tvalid Bool\n}\n\nfn classify_triangle(a, b, c) String {\n\tif a + b <= c {\n\t\t'Invalid'\n\t} else if a == b && b == c {\n\t\t'Equilateral'\n\t} else {\n\t\t'Scalene'\n\t}\n}\n\nfn analyze_triangle(a, b, c) {\n\tTriangleInfo(sides: '${a}, ${b}, ${c}', triangle_type: classify_triangle(a, b, c), perimeter: a + b + c, valid: a + b > c)\n}\n";
        let out = fmt(src);
        assert!(
            out.contains("\t} else if a == b && b == c {\n\t\t'Equilateral'\n"),
            "if-chain should break per-clause:\n{out}"
        );
        assert!(
            out.contains("TriangleInfo(\n\t\tsides: "),
            "struct call should break per-arg:\n{out}"
        );
    }

    /// Top-level items the source parses into, to prove the formatter does not
    /// split or merge program structure.
    fn top_level_items(src: &str) -> usize {
        let mut s = scanner::new_scanner(src.to_string());
        let (toks, diags) = s.scan_all();
        let p = parser::new_parser_from_tokens(toks, diags);
        p.parse_program().ast.body.len()
    }

    #[test]
    fn wrapped_subtraction_keeps_operator_trailing() {
        // A wrapping subtraction must keep the operator at the END of the left
        // operand's line. The parser reads a leading `-` as a fresh unary
        // expression, so a wrapped operator silently re-parses as two
        // statements.
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

        // Must re-parse to one binding, not a binding plus a stray unary.
        assert_eq!(
            top_level_items(&out),
            1,
            "formatting split one subtraction into two top-level items:\n{out}"
        );
    }

    #[test]
    fn nested_unary_minus_keeps_space() {
        // `- -x` must not print as `--x`: the scanner relexes `--` as the
        // rejected decrement token, corrupting a valid file in place.
        let out = fmt("x = - -y\n");
        assert_eq!(out, "x = - -y\n");
        assert!(
            !out.contains("--"),
            "nested unary minus collapsed into the `--` decrement token:\n{out}"
        );
        assert_round_trips(&out);
    }

    #[test]
    fn deeply_nested_unary_minus_keeps_spaces() {
        let out = fmt("x = - - -y\n");
        assert_eq!(out, "x = - - -y\n");
        assert_round_trips(&out);
    }

    /// Dropping a pattern's module qualifier changes the program: `io.NotFound`
    /// resolves through the module, `NotFound` needs an import.
    #[test]
    fn a_qualified_constructor_pattern_keeps_its_qualifier() {
        let src = "match e {\n\tio.NotFound(path) -> path\n\tio.Denied -> 'no'\n\telse -> ''\n}\n";
        let out = fmt(src);
        assert!(out.contains("io.NotFound(path)"), "{out}");
        assert!(out.contains("io.Denied"), "{out}");
        assert_round_trips(&out);
    }

    /// Regression: a comment between the fields of a single-constructor
    /// shorthand used to be deleted outright.
    #[test]
    fn shorthand_fields_keep_their_comments() {
        let src = "type R {\n\ta Int\n\t// why b matters\n\tb String\n}\n";
        let out = fmt(src);
        assert!(out.contains("// why b matters"), "comment deleted:\n{out}");
        assert_round_trips(&out);
    }

    #[test]
    fn call_argument_comments_survive() {
        let src = "f(\n\t// why a\n\ta,\n\tb,\n)\n";
        let out = fmt(src);
        assert!(out.contains("// why a"), "comment deleted:\n{out}");
        assert_round_trips(&out);
    }

    #[test]
    fn labeled_and_final_call_argument_comments_survive() {
        let out = fmt("f(\n\ta,\n\t// why b\n\tlabel: b,\n)\n");
        assert!(out.contains("// why b"), "comment deleted:\n{out}");
        assert_round_trips(&out);
    }

    #[test]
    fn array_element_comments_survive() {
        let out = fmt("x = [\n\t// first\n\t1,\n\t2,\n]\n");
        assert!(out.contains("// first"), "comment deleted:\n{out}");
        assert_round_trips(&out);
    }

    #[test]
    fn array_spread_comments_survive() {
        let out = fmt("x = [\n\t1,\n\t// the rest\n\t..rest,\n]\n");
        assert!(out.contains("// the rest"), "comment deleted:\n{out}");
        assert_round_trips(&out);
    }

    #[test]
    fn call_spread_argument_comments_survive() {
        let out = fmt("f(\n\t// rest\n\t..args,\n)\n");
        assert!(out.contains("// rest"), "comment deleted:\n{out}");
        assert_round_trips(&out);
    }

    #[test]
    fn tuple_element_comments_survive() {
        let out = fmt("x = (\n\t// left\n\t1,\n\t2,\n)\n");
        assert!(out.contains("// left"), "comment deleted:\n{out}");
        assert_round_trips(&out);
    }

    #[test]
    fn binary_segment_comments_survive() {
        let out = fmt("x = <<\n\t// version byte\n\t1,\n\t2,\n>>\n");
        assert!(out.contains("// version byte"), "comment deleted:\n{out}");
        assert_round_trips(&out);
    }

    #[test]
    fn if_condition_comments_survive() {
        let out = fmt("if // check sign\nx > 0 {\n\t1\n} else {\n\t2\n}\n");
        assert!(out.contains("// check sign"), "comment deleted:\n{out}");
        assert_round_trips(&out);
    }

    #[test]
    fn else_if_gap_comments_survive() {
        // A comment between `else` and `if` attaches to the nested `if` token.
        let out = fmt("if a {\n\t1\n} else // why\nif b {\n\t2\n} else {\n\t3\n}\n");
        assert!(out.contains("// why"), "comment deleted:\n{out}");
        assert_round_trips(&out);
    }

    #[test]
    fn else_brace_gap_comments_survive() {
        // A comment between `else` and its `{` attaches to the brace token.
        let out = fmt("if a {\n\t1\n} else // fallback\n{\n\t2\n}\n");
        assert!(out.contains("// fallback"), "comment deleted:\n{out}");
        assert_round_trips(&out);
    }

    /// A comment with no emission site (before the `else` keyword, whose token
    /// has no AST span) must make `format` refuse to produce output.
    #[test]
    fn unemittable_comment_is_a_noop_not_a_deletion() {
        let src = "if a {\n\t1\n}\n// stranded before else\nelse {\n\t2\n}\n";
        match format(src) {
            FormatResult::CommentsLost { comment } => {
                assert_eq!(comment, "// stranded before else")
            }
            // If a future change learns to emit this comment, it must actually
            // be present in the output.
            FormatResult::Formatted { output } => {
                assert!(output.contains("// stranded before else"), "{output}")
            }
            FormatResult::ParseFailed { errors } => panic!("parse failed: {errors:?}"),
            FormatResult::OutputInvalid { detail } => {
                panic!("formatter produced invalid output: {detail}")
            }
        }
    }

    /// A dropped comment whose text also appears in a string literal must still
    /// be detected: the check compares trivia, never raw substrings.
    #[test]
    fn comment_loss_is_not_masked_by_string_literal() {
        let src = "s = \"// stranded before else\"\nif a {\n\t1\n}\n// stranded before else\nelse {\n\t2\n}\n";
        match format(src) {
            FormatResult::CommentsLost { comment } => {
                assert_eq!(comment, "// stranded before else")
            }
            // If a future change learns to emit this comment, it must appear
            // in addition to the string literal.
            FormatResult::Formatted { output } => {
                assert!(
                    output.matches("// stranded before else").count() >= 2,
                    "{output}"
                )
            }
            FormatResult::ParseFailed { errors } => panic!("parse failed: {errors:?}"),
            FormatResult::OutputInvalid { detail } => {
                panic!("formatter produced invalid output: {detail}")
            }
        }
    }

    /// A dropped comment that is a prefix of a surviving one must still be
    /// detected: `// TODO` vs `// TODO: fix`.
    #[test]
    fn comment_loss_is_not_masked_by_longer_comment() {
        let src = "// stranded before else and more\nif a {\n\t1\n}\n// stranded before else\nelse {\n\t2\n}\n";
        match format(src) {
            FormatResult::CommentsLost { comment } => {
                assert_eq!(comment, "// stranded before else")
            }
            FormatResult::Formatted { output } => {
                assert!(output.contains("// stranded before else\n"), "{output}")
            }
            FormatResult::ParseFailed { errors } => panic!("parse failed: {errors:?}"),
            FormatResult::OutputInvalid { detail } => {
                panic!("formatter produced invalid output: {detail}")
            }
        }
    }

    #[test]
    fn unary_minus_before_non_minus_stays_tight() {
        // Only the `--` collision needs a separating space.
        assert_eq!(fmt("x = -y\n"), "x = -y\n");
        assert_eq!(fmt("x = -f(y)\n"), "x = -f(y)\n");
        assert_eq!(fmt("x = -y.z\n"), "x = -y.z\n");
    }

    #[test]
    fn double_not_stays_adjacent() {
        // `!!x` lexes as two tokens and parses fine, so it stays tight.
        let out = fmt("x = !!y\n");
        assert_eq!(out, "x = !!y\n");
        assert!(
            matches!(format(&out), FormatResult::Formatted { .. }),
            "formatted output does not re-parse:\n{out}"
        );
    }

    #[test]
    fn interpolation_stays_flat_when_it_fits() {
        let out = fmt("x = 'value: ${a + b}'\n");
        assert_eq!(out, "x = 'value: ${a + b}'\n");
        assert_round_trips(&out);
    }

    #[test]
    fn hard_breaking_match_inside_interpolation_gets_real_layout() {
        // A `match` inside `${…}` must lay out at the enclosing indent, not be
        // spliced in as text pre-rendered at indent 0.
        let src =
            "fn f(y Int) String {\n\t'value: ${match y { 0 -> 'zero'\n else -> 'other' }}'\n}\n";
        let out = fmt(src);
        assert_eq!(
            out,
            "fn f(y Int) String {\n\t'value: ${match y {\n\t\t0 -> 'zero'\n\t\telse -> 'other'\n\t}}'\n}\n"
        );
        assert_round_trips(&out);
    }

    #[test]
    fn wide_interpolated_expression_breaks_at_enclosing_indent() {
        // The sub-expression joins the enclosing layout, so an overflow breaks
        // with real indentation instead of being measured at infinite width.
        let src = "fn f() String {\n\t'r: ${wwwwwwwwwwwwwwwwwwwwwwwwwwwwww(aaaaaaaaaaaaaaaaaaaaaaaaaa, bbbbbbbbbbbbbbbbbbbbbbbbbb, cccccc)}'\n}\n";
        let out = fmt(src);
        assert!(
            out.contains("(\n\t\taaaaaaaaaaaaaaaaaaaaaaaaaa,\n"),
            "expected the call to break per-argument at the enclosing indent, got:\n{out}"
        );
        assert_round_trips(&out);
    }
}
