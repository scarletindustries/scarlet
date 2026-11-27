use std::collections::{BTreeSet, HashMap};

use crate::ast;
use crate::d;
use crate::diagnostic::{Diagnostic, Severity};
use crate::parser::{self, SyntaxError};
use crate::scanner;
use crate::span::Span;
use crate::token::{Kind, Token, Trivia};

pub mod doc;
mod verify;

use doc::{
    Doc, block, delimited, delimited_hug, delimited_hug_list, delimited_no_trailing, group,
    group_willing, hard_braces, hard_list, hardline, hardlines, join, line, nest, nil, text,
};
pub use verify::Mismatch;

const MAX_WIDTH: isize = 100;

/// A constructor with this many fields always breaks one-per-line.
const FIELDS_PER_LINE: usize = 4;

pub enum FormatResult {
    Formatted {
        output: String,
    },
    ParseFailed {
        errors: Vec<Diagnostic<SyntaxError>>,
    },
    /// A formatter bug: layout dropped an input comment. No output; callers
    /// must leave the source untouched. `comment` is one of the lost ones.
    CommentsLost {
        comment: String,
    },
    /// A formatter bug: the output is not the same program as the input. No
    /// output; callers must leave the source untouched.
    OutputInvalid(FormatBug),
}

/// How the formatter's output fails to be the input's program. Each is a
/// formatter bug.
#[derive(Debug)]
pub enum FormatBug {
    /// The input parsed with no error, yet holds a node the parser made for
    /// one. That is a parser bug, and there is nothing to lay out there.
    ErrorNode(Span),
    /// Keeping breaks flat never stopped layout adding the same line end.
    LineEndLoop,
    Layout(verify::Mismatch),
    /// The output has an error the input didn't.
    DoesNotReparse(Diagnostic<SyntaxError>),
    TopLevelCount {
        output: usize,
        input: usize,
    },
}

impl std::fmt::Display for FormatBug {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FormatBug::ErrorNode(s) => write!(
                f,
                "the parser left an error node at {}:{} without reporting an error",
                s.start_line, s.start_column
            ),
            FormatBug::LineEndLoop => write!(f, "layout keeps adding the same line end"),
            FormatBug::Layout(m) => write!(f, "{m}"),
            FormatBug::DoesNotReparse(d) => write!(
                f,
                "output does not re-parse: {} at {}:{}",
                d.problem, d.span.start_line, d.span.start_column
            ),
            FormatBug::TopLevelCount { output, input } => write!(
                f,
                "output re-parses to {output} top-level nodes, input had {input}"
            ),
        }
    }
}

/// Every comment in the tokens' leading trivia, keyed by trimmed text with its
/// count. The comment-loss check compares an input census to an output census,
/// so both must key identically: they share this one function.
fn comment_census(tokens: &[Token]) -> HashMap<String, usize> {
    let mut census: HashMap<String, usize> = HashMap::new();
    for tok in tokens {
        for t in &tok.leading_trivia {
            if let Some(c) = t.comment_text() {
                let n = census.entry(c.trim_end().to_string()).or_default();
                *n = n.saturating_add(1);
            }
        }
    }
    census
}

pub fn format(input: &str) -> FormatResult {
    format_at(input, MAX_WIDTH)
}

/// `format` with lines up to `width` columns. Tests use narrow widths to make
/// layout take every line break it can.
fn format_at(input: &str, width: isize) -> FormatResult {
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
        if let [dotdot, next] = pair
            && dotdot.kind == Kind::PuncDotdot
        {
            dotdot_before.insert(Pos::start_of(&next.span), Pos::start_of(&dotdot.span));
        }
    }

    let p = parser::new_parser_from_tokens(scanned_tokens, scanner_diagnostics);
    let result = p.parse_program();

    let errors: Vec<Diagnostic<SyntaxError>> = result
        .diagnostics
        .into_iter()
        .filter(|d| d.severity() == Severity::Error && !d.problem.is_single_quoted_string())
        .collect();

    if !errors.is_empty() {
        return FormatResult::ParseFailed { errors };
    }

    let f = Formatter {
        trivia_map,
        dotdot_before,
    };
    let body = match f.program(&result.ast, &eof_trivia) {
        Ok(body) => body,
        Err(ErrorNodeLeft(span)) => return FormatResult::OutputInvalid(FormatBug::ErrorNode(span)),
    };

    // Lay out, then read the output back with the real scanner and compare
    // it to the doc's meaning (see `verify`). A line end that layout added is
    // undone by keeping the breaks that made it flat. Each round keeps at
    // least one more break flat, so this ends.
    let reference = doc::layout_keeping_flat(&body, width, &doc::KeepFlat::All);
    let (reference_tokens, _) = scanner::new_scanner(reference.text).scan_all();
    let mut keep_flat = BTreeSet::new();
    let (output, rescanned_tokens, rescan_diagnostics) = loop {
        let layout =
            doc::layout_keeping_flat(&body, width, &doc::KeepFlat::These(keep_flat.clone()));
        let mut output = layout.text.clone();
        while output.ends_with('\n') {
            output.pop();
        }
        output.push('\n');
        let mut rescan = scanner::new_scanner(output.clone());
        let (tokens, diagnostics) = rescan.scan_all();
        match verify::compare(&reference_tokens, &tokens, &layout) {
            verify::Verdict::Same => break (output, tokens, diagnostics),
            verify::Verdict::KeepFlat(ids) => {
                let before = keep_flat.len();
                keep_flat.extend(ids);
                if keep_flat.len() == before {
                    return FormatResult::OutputInvalid(FormatBug::LineEndLoop);
                }
            }
            verify::Verdict::Different(m) => {
                return FormatResult::OutputInvalid(FormatBug::Layout(m));
            }
        }
    };

    // Postcondition: every input comment made it into the output. Comparing
    // trivia to trivia, never raw substrings, so a string literal that looks
    // like a comment cannot mask a real loss.
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

    // `verify` proved the output means what the doc means. The doc is built
    // from the input's AST, and this checks that it means a program at all,
    // with the same top-level items. `- -x` rendering as the rejected `--`
    // token was a real bug of this shape.
    let rp = parser::new_parser_from_tokens(rescanned_tokens, rescan_diagnostics);
    let reparsed = rp.parse_program();
    if let Some(d) = reparsed
        .diagnostics
        .iter()
        .find(|d| d.severity() == Severity::Error)
    {
        return FormatResult::OutputInvalid(FormatBug::DoesNotReparse(d.clone()));
    }
    if reparsed.ast.body.len() != result.ast.body.len() {
        return FormatResult::OutputInvalid(FormatBug::TopLevelCount {
            output: reparsed.ast.body.len(),
            input: result.ast.body.len(),
        });
    }

    FormatResult::Formatted { output }
}

/// A scanner line and column, used to key trivia by the token it belongs to.
/// It's a named type so that swapping line and column is a compile error, not a
/// silent lookup miss.
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
            column: span.end_column.saturating_sub(1),
        }
    }
}

/// Building the doc met an error node, at this span, in input that parsed
/// with no error.
struct ErrorNodeLeft(Span);

type Fmt<T> = Result<T, ErrorNodeLeft>;

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
                    parts.push(hardlines(blanks.saturating_add(1)));
                }
                parts.push(text(c.trim_end().to_string()));
                emitted_any = true;
                newlines = 0;
            } else {
                newlines = newlines.saturating_add(1);
            }
        }
        let blanks = if emitted_any {
            newlines.saturating_sub(1).min(max_blanks)
        } else {
            0
        };
        if emitted_any || !first {
            parts.push(hardlines(blanks.saturating_add(1)));
        }
        doc::concat(parts)
    }

    /// Comments attached to a block's closing brace. `after_content` says
    /// whether block content precedes them: the first comment then needs a
    /// separating hardline of its own. In an otherwise-empty block the
    /// enclosing brace layout already supplies the newline, and a leading
    /// hardline here would render as a blank indented line under the `{`.
    fn trailing_comments(&self, s: Span, after_content: bool) -> Doc {
        let Some(trivia) = self.closing_brace_trivia(s) else {
            return nil();
        };
        let mut parts = Vec::new();
        for t in trivia {
            if let Some(c) = t.comment_text() {
                if after_content || !parts.is_empty() {
                    parts.push(hardline());
                }
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

    fn program(&self, block: &ast::BlockExpression, eof_trivia: &[Trivia]) -> Fmt<Doc> {
        Ok(d![
            self.nodes_with_trivia(&block.body, 1)?,
            self.trivia_doc(eof_trivia, block.body.is_empty(), 1),
        ])
    }

    fn node(&self, n: &ast::Node) -> Fmt<Doc> {
        match n {
            ast::Node::Statement(s) => self.statement(s),
            ast::Node::Expression(e) => self.expr_at_line_start(e),
        }
    }

    /// Emit each node preceded by its leading trivia.
    fn nodes_with_trivia(&self, nodes: &[ast::Node], max_blanks: usize) -> Fmt<Doc> {
        let mut parts = Vec::new();
        for (i, n) in nodes.iter().enumerate() {
            parts.push(self.leading_trivia(n.span(), i == 0, max_blanks));
            parts.push(self.node(n)?);
        }
        Ok(doc::concat(parts))
    }

    fn statement(&self, stmt: &ast::Statement) -> Fmt<Doc> {
        Ok(match stmt {
            ast::Statement::Binding(s) => {
                let ty = match &s.typ {
                    Some(t) => d![text(" "), self.type_(t)],
                    None => nil(),
                };
                d![self.pattern(&s.pattern)?, ty, self.bound(" =", &s.init)?]
            }
            ast::Statement::Backpass(s) => {
                let mut parts = Vec::new();
                for (i, b) in s.binders.iter().enumerate() {
                    if i > 0 {
                        parts.push(text(", "));
                    }
                    parts.push(self.pattern(b)?);
                }
                parts.push(text(" <- "));
                parts.push(self.expr(&s.call)?);
                doc::concat(parts)
            }
            ast::Statement::Declaration { decl, public } => self.declaration(decl, *public)?,
            ast::Statement::ImportDeclaration(s) => {
                let mut out = vec![text("import "), text(s.path.to_string())];
                if let Some(a) = &s.alias {
                    out.push(text(" as "));
                    out.push(text(a.name.clone()));
                }
                doc::concat(out)
            }
        })
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

    fn declaration(&self, dcl: &ast::Declaration, is_public: bool) -> Fmt<Doc> {
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
                    self.bound(" =", &s.init)?
                ]
            }
            ast::Declaration::Function(func) => self.fn_decl(func)?,
            ast::Declaration::Type(s) => self.type_decl(s),
        };
        Ok(d![attrs, prefix, body])
    }

    /// A named-function parameter list the author broke after `(` stays broken.
    /// Width alone is the wrong signal: a signature can fit and still read
    /// worse than one-per-line, and reflowing discards the author's choice.
    ///
    /// Lambdas do not consult this. A hard-broken `fn(\n x,\n)` injects
    /// HardLines that make an enclosing call hug, so the next pass measures a
    /// different document and is not a fixed point.
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
        author_header_line: Option<i32>,
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
        let list =
            if author_header_line.is_some_and(|line| self.params_broken_by_author(line, params)) {
                hard_list("(", ps, ")")
            } else if name.is_none() {
                // A lambda's parameters stay on one line. They are short, and
                // breaking them buys a few columns at the cost of burying the
                // `fn` that says what the argument is.
                d![text("("), join(ps, text(", ")), text(")")]
            } else {
                delimited("(", ps, ")")
            };
        d![head, list, r]
    }

    fn fn_decl(&self, f: &ast::FunctionDeclaration) -> Fmt<Doc> {
        let header = self.fn_header(
            Some(&f.identifier.name),
            &f.params,
            &f.return_type,
            Some(f.identifier.span.start_line),
        );
        Ok(match &f.body {
            ast::FnBody::Block(b) => d![header, text(" "), self.fn_body(b)?],
            ast::FnBody::Vm(_) => header,
        })
    }

    fn fn_expr(&self, f: &ast::FunctionExpression) -> Fmt<Doc> {
        Ok(d![
            self.fn_header(None, &f.params, &None, None),
            text(" "),
            self.lambda_body(&f.body)?,
        ])
    }

    /// A named `fn` body is always a hard multi-line block, never collapsed
    /// onto the signature line. Lambdas go through `lambda_body` instead.
    fn fn_body(&self, body: &ast::Expression) -> Fmt<Doc> {
        if let ast::Expression::BlockExpression(b) = body {
            let trail = self.trailing_comments(b.span, true);
            if b.body.is_empty() && trail.is_nil() {
                return Ok(text("{}"));
            }
        }
        self.body_as_hard_block(body)
    }

    fn lambda_body(&self, body: &ast::Expression) -> Fmt<Doc> {
        if let ast::Expression::BlockExpression(b) = body
            && let [ast::Node::Expression(inner)] = b.body.as_slice()
            && !self.has_comment_at(inner.span())
            && self.trailing_comments(b.span, true).is_nil()
        {
            return Ok(group(self.expr(inner)?));
        }
        if !matches!(body, ast::Expression::BlockExpression(_)) && !self.has_comment_at(body.span())
        {
            return Ok(group(self.expr(body)?));
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
            ast::TypeBody::Variants { opaque: false, .. }
            | ast::TypeBody::Alias(_)
            | ast::TypeBody::External => nil(),
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
                if let [c] = ctors.as_slice()
                    && c.identifier.name == s.identifier.name
                    && !c.fields.is_empty()
                {
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
        // fit under MAX_WIDTH and still be unreadable.
        let list = if fields.len() >= FIELDS_PER_LINE {
            hard_list("(", fields, ")")
        } else {
            delimited("(", fields, ")")
        };
        d![text(c.identifier.name.clone()), list]
    }

    fn expr(&self, e: &ast::Expression) -> Fmt<Doc> {
        use ast::Expression as E;
        Ok(match e {
            E::StringLiteral(s) => text(quoted(&s.value)),
            E::InterpolatedString(s) => {
                let mut parts: Vec<Doc> = vec![text("\"")];
                for part in &s.parts {
                    match part {
                        ast::InterpPart::Literal(sl) => parts.push(text(escape_string(&sl.value))),
                        // A real Doc, not a pre-rendered string, so hard breaks
                        // carry the enclosing indent. Newlines inside `${…}` are
                        // ordinary tokens and never change the string's value.
                        ast::InterpPart::Expr(e) => {
                            parts.push(text("${"));
                            parts.push(group(self.expr(e)?));
                            parts.push(text("}"));
                        }
                    }
                }
                parts.push(text("\""));
                doc::concat(parts)
            }
            E::NumberLiteral(n) => text(n.value.clone()),
            E::Identifier(id) => text(id.name.clone()),
            E::BinaryExpression(b) => self.binary_chain(b)?,
            E::UnaryExpression(u) => {
                // `- -x` must not collapse into `--x`: the scanner relexes
                // `--` as the rejected decrement token, so the formatted
                // program would no longer parse.
                let gap = if u.op == ast::UnaryOp::Neg && starts_with_minus(&u.expression) {
                    text(" ")
                } else {
                    nil()
                };
                d![text(u.op.symbol()), gap, self.expr(&u.expression)?]
            }
            E::BlockExpression(b) => self.block_expr(b)?,
            E::IfExpression(_) => self.if_chain(e)?,
            E::MatchExpression(m) => self.match_expr(m)?,
            E::OrExpression(o) => {
                let recv = match &o.receiver {
                    Some(r) => d![text(r.name.clone()), text(" -> ")],
                    None => nil(),
                };
                d![
                    self.expr(&o.expression)?,
                    text(" or "),
                    recv,
                    self.expr(&o.body)?
                ]
            }
            E::PipeExpression(p) => self.pipeline(p, HeadAt::MidLine)?,
            E::FunctionExpression(f) => self.fn_expr(f)?,
            E::FunctionCallExpression(c) => {
                let args: Vec<Doc> = c
                    .arguments
                    .iter()
                    .map(|a| self.call_arg(a))
                    .collect::<Fmt<_>>()?;
                // A block-shaped final argument hugs the parentheses,
                // `f(a, fn() { … })`, instead of one argument per line, and so
                // does a final list, `Object([ … ])`, unless an earlier
                // argument is a list too.
                let parens = match c.arguments.split_last() {
                    Some((last, _)) if arg_can_hug(last) => delimited_hug("(", args, ")"),
                    Some((last, init)) if arg_is_list(last) && !init.iter().any(arg_is_list) => {
                        delimited_hug_list("(", args, ")")
                    }
                    _ => delimited("(", args, ")"),
                };
                d![self.expr(&c.callee)?, parens]
            }
            E::ModuleAccessExpression(m) => text(format!("{}::{}", m.module.name, m.member.name)),
            E::PropertyAccessExpression(p) => {
                let key = match &p.right {
                    ast::PropertyKey::Field(id) => text(id.name.clone()),
                    ast::PropertyKey::TupleIndex(n) => text(n.value.clone()),
                };
                d![self.expr(&p.left)?, text("."), key]
            }
            E::ArrayExpression(a) => {
                let items: Vec<Doc> = a
                    .elements
                    .iter()
                    .map(|e| self.array_elem(e))
                    .collect::<Fmt<_>>()?;
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
                        Ok(d![
                            self.comments_before(s.span),
                            self.expr(&s.value)?,
                            self.bin_size_spec(&s.spec, is_string)?
                        ])
                    })
                    .collect::<Fmt<_>>()?;
                delimited("<<", segs, ">>")
            }
            E::TupleExpression(t) => {
                let items: Vec<Doc> = t
                    .elements
                    .iter()
                    .map(|e| Ok(d![self.comments_before(e.span()), self.expr(e)?]))
                    .collect::<Fmt<_>>()?;
                delimited("(", items, ")")
            }
            E::ArrayIndexExpression(a) => {
                d![
                    self.expr(&a.expression)?,
                    text("["),
                    self.expr(&a.index)?,
                    text("]")
                ]
            }
            E::RangeExpression(r) => d![self.expr(&r.start)?, text(".."), self.expr(&r.end)?],
            // `format()` returns `ParseFailed` before building any Doc when
            // there are errors, so reaching here means a parser path built an
            // ErrorNode without recording one.
            E::ErrorNode(n) => return Err(ErrorNodeLeft(n.span)),
        })
    }

    /// `a |> b |> c` parses as `(a |> b) |> c`. The chain is laid out as one
    /// group, so it either fits on one line or puts every `|>` on its own
    /// line. A line that starts with `|>` continues the one before, so
    /// breaking there never ends a statement.
    ///
    /// When `a` starts its own line, the `|>` lines line up under it, as in
    /// Gleam. When something comes before `a` on its line, like `fn(x)` or a
    /// binding that `bound` didn't move down, they go one level in instead,
    /// so they don't sit under whatever that was.
    fn pipeline(&self, p: &ast::PipeExpression, head_at: HeadAt) -> Fmt<Doc> {
        let mut stages = vec![&*p.right];
        let mut head = &*p.left;
        while let ast::Expression::PipeExpression(inner) = head {
            stages.push(&inner.right);
            head = &inner.left;
        }
        let mut rest = Vec::new();
        for stage in stages.into_iter().rev() {
            rest.push(line());
            rest.push(text("|> "));
            rest.push(self.expr(stage)?);
        }
        let indent = match head_at {
            HeadAt::LineStart => 0,
            HeadAt::MidLine => 1,
        };
        Ok(group(d![self.expr(head)?, nest(indent, doc::concat(rest))]))
    }

    /// `expr` for an expression that starts its own line: a statement, an
    /// arm body after a broken `->`, a call argument, or a pipeline `bound`
    /// moved down.
    fn expr_at_line_start(&self, e: &ast::Expression) -> Fmt<Doc> {
        match e {
            ast::Expression::PipeExpression(p) => self.pipeline(p, HeadAt::LineStart),
            _ => self.expr(e),
        }
    }

    /// What follows `=` or a label's `:`. A pipeline that doesn't fit
    /// there moves to the next line, one level in, so its `|>` lines sit under
    /// its first value and not under the name being bound. Anything else
    /// stays on the binder's line.
    fn bound(&self, binder: &'static str, e: &ast::Expression) -> Fmt<Doc> {
        Ok(match e {
            ast::Expression::PipeExpression(_) => {
                d![
                    text(binder),
                    group(nest(1, d![line(), self.expr_at_line_start(e)?]))
                ]
            }
            _ => d![text(binder), text(" "), self.expr(e)?],
        })
    }

    fn call_arg(&self, a: &ast::CallArg) -> Fmt<Doc> {
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
            ast::CallArg::Positional(_) | ast::CallArg::Labeled { .. } => {
                self.comments_before(a.span())
            }
        };
        let arg = match a {
            ast::CallArg::Positional(e) => self.expr_at_line_start(e)?,
            // A punned argument keeps its shorthand. If the formatter expanded
            // `label:` to `label: label`, the shorthand would be gone the first
            // time a file got formatted.
            ast::CallArg::Labeled {
                punned: true,
                label,
                ..
            } => d![text(label.name.clone()), text(":")],
            ast::CallArg::Labeled { label, value, .. } => {
                d![text(label.name.clone()), self.bound(":", value)?]
            }
            ast::CallArg::Spread(e) => d![text(".."), self.expr(e)?],
        };
        Ok(d![comments, arg])
    }

    /// The `:spec` suffix of a `<<>>` segment, the reverse of `parse_bin_size_spec`.
    /// A string-literal segment's Utf8 kind is the parser default, so its
    /// `:utf8` is omitted.
    fn bin_size_spec(&self, spec: &ast::BinSpec, value_is_string: bool) -> Fmt<Doc> {
        Ok(match spec {
            ast::BinSpec::Int { bits: None } => nil(),
            ast::BinSpec::Int {
                bits: Some(ast::Expression::NumberLiteral(n)),
            } => d![text(":"), text(n.value.clone())],
            ast::BinSpec::Int { bits: Some(e) } => d![text(":size("), self.expr(e)?, text(")")],
            ast::BinSpec::Binary { bytes: None } => text(":binary"),
            ast::BinSpec::Binary { bytes: Some(e) } => {
                d![text(":bytes("), self.expr(e)?, text(")")]
            }
            ast::BinSpec::Utf8 if value_is_string => nil(),
            ast::BinSpec::Utf8 => text(":utf8"),
        })
    }

    fn array_elem(&self, e: &ast::ArrayElement) -> Fmt<Doc> {
        Ok(match e {
            ast::ArrayElement::Expression(e) => {
                d![self.comments_before(e.span()), self.expr(e)?]
            }
            ast::ArrayElement::SpreadElement(se) => d![
                self.comments_before(se.span),
                text(".."),
                self.expr(&se.expression)?
            ],
        })
    }

    fn body_as_block(&self, body: &ast::Expression) -> Fmt<Doc> {
        match body {
            ast::Expression::BlockExpression(b) => self.block_expr(b),
            other => Ok(block(self.expr(other)?)),
        }
    }

    fn block_expr(&self, b: &ast::BlockExpression) -> Fmt<Doc> {
        let trail = self.trailing_comments(b.span, !b.body.is_empty());
        if b.body.is_empty() && trail.is_nil() {
            return Ok(text("{}"));
        }
        let hard = !trail.is_nil() || b.body.iter().any(|n| self.has_comment_at(n.span()));
        let body = d![self.nodes_with_trivia(&b.body, 1)?, trail];
        Ok(if b.body.len() <= 1 && !hard {
            block(body)
        } else {
            hard_braces(body)
        })
    }

    /// A chain of operators on one precedence level, `a || b || c`, is one
    /// group: on one line, or every operand on its own line indented under
    /// the first. The operator ends its line, since a line that starts with
    /// `-` would parse as a new statement.
    fn binary_chain(&self, b: &ast::BinaryExpression) -> Fmt<Doc> {
        let level = parser::precedence_level(b.op);
        let mut rest = Vec::new();
        let mut cur = b;
        let first = loop {
            rest.push((cur.op, &*cur.right));
            match &*cur.left {
                ast::Expression::BinaryExpression(l) if parser::precedence_level(l.op) == level => {
                    cur = l
                }
                left => break left,
            }
        };
        let rest: Vec<Doc> = rest
            .into_iter()
            .rev()
            .map(|(op, right)| {
                Ok(d![
                    text(format!(" {}", op.symbol())),
                    line(),
                    self.expr(right)?
                ])
            })
            .collect::<Fmt<_>>()?;
        Ok(group(d![self.expr(first)?, nest(1, doc::concat(rest))]))
    }

    fn if_chain(&self, e: &ast::Expression) -> Fmt<Doc> {
        // Unroll an if/else-if/else chain into a flat clause list so they share
        // one group and break together.
        struct Clause<'a> {
            /// Where a comment between `else` and `if` attaches.
            if_span: Span,
            cond: &'a ast::Expression,
            /// Written `then body`. `then { .. }` says no more than the
            /// block alone, so it is written back as one.
            then_keyword: bool,
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
                        then_keyword: i.then_keyword
                            && !matches!(*i.body, ast::Expression::BlockExpression(_)),
                        body: &i.body,
                    });
                    cur = &i.else_body;
                }
                other => break other,
            }
        };
        let cond_comment = clauses.iter().any(|c| self.has_comment_at(c.cond.span()));
        let else_is_block = matches!(else_body, ast::Expression::BlockExpression(_));
        let kw = |i: usize, clause: &Clause| {
            // A comment between `else` and `if` attaches to the nested `if`
            // token. The chain head's own trivia comes from the statement list.
            if i == 0 {
                text("if ")
            } else {
                d![
                    text("else "),
                    self.comments_before(clause.if_span),
                    text("if ")
                ]
            }
        };
        // A bare branch after `then` or `else`. A comment above it puts it on
        // a line of its own, indented under the keyword.
        let bare = |word: &'static str, b: &ast::Expression| -> Fmt<Doc> {
            Ok(if self.has_comment_at(b.span()) {
                d![
                    text(word),
                    nest(
                        1,
                        d![hardline(), self.comments_before(b.span()), self.expr(b)?]
                    )
                ]
            } else {
                d![text(word), text(" "), self.expr(b)?]
            })
        };
        // `if a then x else if b then y else z`: on one line when it fits,
        // else a clause a line, each after the first indented under it.
        if clauses.iter().all(|c| c.then_keyword) && !else_is_block && !cond_comment {
            let mut docs: Vec<Doc> = Vec::new();
            for (i, clause) in clauses.iter().enumerate() {
                docs.push(d![
                    kw(i, clause),
                    self.expr(clause.cond)?,
                    text(" "),
                    bare("then", clause.body)?,
                ]);
            }
            let last = bare("else", else_body)?;
            let rest: Vec<Doc> = docs
                .drain(1..)
                .chain([last])
                .map(|d| d![line(), d])
                .collect();
            let first = docs.pop().unwrap_or_else(nil);
            return Ok(group(d![first, nest(1, doc::concat(rest))]));
        }
        // Only a ternary-shaped if/else stays on one line: no chain, no
        // comments, every branch a bare atom. Anything heavier breaks every
        // clause, so the whole reads symmetrically like `match`.
        let is_chain = clauses.len() > 1;
        // A comment before a branch's `{` attaches to the brace token, and
        // such a branch cannot stay flat.
        let brace_comment = clauses.iter().any(|c| self.has_comment_at(c.body.span()))
            || self.has_comment_at(else_body.span());
        let trivial = !is_chain
            && !cond_comment
            && !brace_comment
            && clauses
                .iter()
                .all(|c| !c.then_keyword && self.is_trivial_branch(c.body))
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
            let branch = if clause.then_keyword {
                bare("then", clause.body)?
            } else {
                d![self.comments_before(clause.body.span()), body(clause.body)?]
            };
            docs.push(d![
                kw(i, clause),
                self.comments_before(clause.cond.span()),
                self.expr(clause.cond)?,
                text(" "),
                branch,
            ]);
        }
        docs.push(if else_is_block {
            d![
                text("else "),
                self.comments_before(else_body.span()),
                body(else_body)?
            ]
        } else {
            bare("else", else_body)?
        });
        Ok(if trivial {
            group(join(docs, line()))
        } else {
            join(docs, text(" "))
        })
    }

    /// A branch is trivial when it is one comment-free bare atom. Only these
    /// may keep an `if` on one line.
    fn is_trivial_branch(&self, b: &ast::Expression) -> bool {
        let ast::Expression::BlockExpression(blk) = b else {
            return false;
        };
        let [ast::Node::Expression(inner)] = blk.body.as_slice() else {
            return false;
        };
        if !self.trailing_comments(blk.span, true).is_nil() || self.has_comment_at(inner.span()) {
            return false;
        }
        matches!(
            inner,
            ast::Expression::Identifier(_)
                | ast::Expression::NumberLiteral(_)
                | ast::Expression::StringLiteral(_)
        )
    }

    fn body_as_hard_block(&self, body: &ast::Expression) -> Fmt<Doc> {
        let inner = match body {
            ast::Expression::BlockExpression(b) => {
                d![
                    self.nodes_with_trivia(&b.body, 1)?,
                    self.trailing_comments(b.span, !b.body.is_empty())
                ]
            }
            other => self.expr(other)?,
        };
        Ok(hard_braces(inner))
    }

    fn match_expr(&self, m: &ast::MatchExpression) -> Fmt<Doc> {
        let mut arms: Vec<Doc> = Vec::new();
        for (i, arm) in m.arms.iter().enumerate() {
            arms.push(self.leading_trivia(arm.pattern.span(), i == 0, 1));
            let guard = match &arm.guard {
                Some(g) => d![text(" if "), self.expr(g)?],
                None => nil(),
            };
            let head = self.pattern(&arm.pattern)?;
            let body = self.expr(&arm.body)?;
            // A body that ends the line itself hugs the arrow. Anything else
            // gets a break point after the arrow, so an overflowing arm wraps
            // there rather than inside the pattern.
            let tail = if doc::ends_line(&body) {
                d![text(" -> "), body]
            } else {
                let body = self.expr_at_line_start(&arm.body)?;
                group_willing(nest(1, d![text(" ->"), line(), body]))
            };
            arms.push(d![head, guard, tail]);
        }
        let trail = self.trailing_comments(m.span, true);
        Ok(d![
            text("match "),
            self.expr(&m.subject)?,
            text(" "),
            hard_braces(d![doc::concat(arms), trail]),
        ])
    }

    fn pattern(&self, p: &ast::Pattern) -> Fmt<Doc> {
        use ast::Pattern as P;
        Ok(match p {
            P::Var { name } => text(name.name.clone()),
            P::Literal(ast::PatternLiteral::Number(n)) => text(n.value.clone()),
            P::Literal(ast::PatternLiteral::String(s)) => text(quoted(&s.value)),
            P::Tuple { elements, .. } => {
                let es: Vec<Doc> = elements
                    .iter()
                    .map(|e| self.pattern(e))
                    .collect::<Fmt<_>>()?;
                delimited("(", es, ")")
            }
            P::Array { elements, .. } => {
                let es: Vec<Doc> = elements
                    .iter()
                    .map(|e| match e {
                        ast::ArrayPatternElement::Pattern(p) => self.pattern(p),
                        ast::ArrayPatternElement::Spread { binding, .. } => Ok(match binding {
                            Some(b) => d![text(".."), text(b.name.clone())],
                            None => text(".."),
                        }),
                    })
                    .collect::<Fmt<_>>()?;
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
                        Ok(d![
                            self.comments_before(s.span),
                            self.pattern(&s.value)?,
                            self.bin_size_spec(&s.spec, is_string)?
                        ])
                    })
                    .collect::<Fmt<_>>()?;
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
                // Dropping the qualifier changes the program: `io::NotFound`
                // resolves through the module, `NotFound` needs an import.
                let head = match qualifier {
                    Some(q) => format!("{}::{}", q.name, name.name),
                    None => name.name.clone(),
                };
                if args.is_empty() && !*rest {
                    return Ok(text(head));
                }
                let mut items: Vec<Doc> = args
                    .iter()
                    .map(|a| {
                        Ok(match a {
                            ast::PatternArg::Positional(p) => self.pattern(p)?,
                            ast::PatternArg::Labeled {
                                label,
                                punned: true,
                                ..
                            } => text(format!("{}:", label.name)),
                            ast::PatternArg::Labeled { label, pattern, .. } => {
                                d![text(label.name.clone()), text(": "), self.pattern(pattern)?]
                            }
                        })
                    })
                    .collect::<Fmt<_>>()?;
                if *rest {
                    items.push(text(".."));
                    // No trailing comma: the parser rejects `..,`, so a wrapped
                    // pattern must end `..\n)`.
                    return Ok(d![text(head), delimited_no_trailing("(", items, ")")]);
                }
                d![text(head), delimited("(", items, ")")]
            }
            P::Or { first, rest, .. } => {
                // Keep it flat while it fits: `A | B | C`. When it's too wide,
                // put one alternative per line, each after the first led by
                // its pipe at the arm's indent. Never break inside an
                // alternative's own parens.
                let mut parts = vec![self.pattern(first)?];
                for p in rest {
                    parts.push(d![line(), text("| "), self.pattern(p)?]);
                }
                group(doc::concat(parts))
            }
            P::Range { start, end, .. } => {
                d![
                    text(start.value.clone()),
                    text(".."),
                    text(end.value.clone())
                ]
            }
        })
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
                let name = match &n.qualifier {
                    Some(q) => format!("{}::{}", q.name, n.identifier.name),
                    None => n.identifier.name.clone(),
                };
                if n.type_args.is_empty() {
                    text(name)
                } else {
                    let args: Vec<Doc> = n.type_args.iter().map(|a| self.type_(a)).collect();
                    d![text(name), delimited("(", args, ")")]
                }
            }
        }
    }
}

fn quoted(s: &str) -> String {
    format!("\"{}\"", escape_string(s))
}

fn escape_string(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            '\\' => result.push_str("\\\\"),
            '\0' => result.push_str("\\0"),
            '$' => result.push_str("\\$"),
            '"' => result.push_str("\\\""),
            other => result.push(other),
        }
    }
    result
}

/// The expression a call argument passes, whatever form the argument takes.
fn arg_value(a: &ast::CallArg) -> &ast::Expression {
    match a {
        ast::CallArg::Positional(e) | ast::CallArg::Spread(e) => e,
        ast::CallArg::Labeled { value, .. } => value,
    }
}

/// Whether a final call argument may hug the call's parentheses: its own last
/// line is a closing brace, so the closing paren can follow it. Everything
/// else keeps the one-argument-per-line fallback.
fn arg_can_hug(a: &ast::CallArg) -> bool {
    ends_in_a_block(arg_value(a))
}

fn ends_in_a_block(e: &ast::Expression) -> bool {
    match e {
        ast::Expression::FunctionExpression(_)
        | ast::Expression::MatchExpression(_)
        | ast::Expression::BlockExpression(_)
        | ast::Expression::IfExpression(_) => true,
        // A call that hugs ends where its own last argument does, so the
        // caller's paren may follow that one's: `Timer(waiting: spawn(fn() {
        // … }))` closes three at once.
        ast::Expression::FunctionCallExpression(c) => c.arguments.last().is_some_and(arg_can_hug),
        _ => false,
    }
}

/// Whether a call argument is written between a pair of delimiters, which may
/// hug the call's parentheses when it comes last.
fn arg_is_list(a: &ast::CallArg) -> bool {
    matches!(
        arg_value(a),
        ast::Expression::ArrayExpression(_)
            | ast::Expression::TupleExpression(_)
            | ast::Expression::BinaryLiteral(_)
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
        E::PipeExpression(p) => starts_with_minus(&p.left),
        E::RangeExpression(r) => starts_with_minus(&r.start),
        _ => false,
    }
}

/// Where a pipeline's first value sits on its line. See `Formatter::pipeline`.
#[derive(Clone, Copy)]
enum HeadAt {
    LineStart,
    MidLine,
}

#[cfg(test)]
mod tests;
