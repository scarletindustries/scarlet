use crate::ast;
use crate::diagnostic::{self, Diagnostic, DiagnosticCode};
use crate::scanner::Scanner;
use crate::span::Span;
use crate::token::{Keyword, Kind, Token, Trivia, is_type_name};

type PResult<T> = Result<T, String>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseContext {
    TopLevel,
    Block,
    FunctionParams,
    Array,
    TypeDef,
    MatchArms,
}

pub struct ParseResult {
    pub ast: ast::BlockExpression,
    pub diagnostics: Vec<Diagnostic>,
}

pub struct Parser {
    tokens: Vec<Token>,
    index: usize,
    diagnostics: Vec<Diagnostic>,
    context_stack: Vec<ParseContext>,
    prev_token_end_line: i32,
    prev_token_end_column: i32,
    sync_iterations: i32,
    depth: u32,
}

pub fn new_parser(s: &mut Scanner) -> Parser {
    let tokens = s.scan_all();
    let diags = s.take_diagnostics();
    new_parser_from_tokens(tokens, diags)
}

pub fn new_parser_from_tokens(tokens: Vec<Token>, scanner_diagnostics: Vec<Diagnostic>) -> Parser {
    Parser {
        tokens,
        index: 0,
        diagnostics: scanner_diagnostics,
        context_stack: vec![ParseContext::TopLevel],
        prev_token_end_line: 0,
        prev_token_end_column: 0,
        sync_iterations: 0,
        depth: 0,
    }
}

// Checks whether a token begins a type annotation in let/const binding position.
// Lowercase identifiers are NOT type-starts here: free type variables are
// meaningless in let/const annotations. Function param and return positions
// DO admit lowercase type variables and use `at_loose_type_start` instead.
fn is_type_start_token(tok: &Token) -> bool {
    match tok.kind {
        Kind::Keyword(Keyword::Fn) => true,
        Kind::PuncOpenParen => true,
        Kind::Identifier => tok.literal.as_deref().is_some_and(is_type_name),
        _ => false,
    }
}

// Bounds recursive-descent depth so that pathologically nested input
// (`((((…`, `!!!!…`, `(T,(T,…` nested types, `S(S(S(…`) becomes a normal parse error
// instead of overflowing the native stack and aborting the process. Since
// the parser is the sole AST producer, bounding it here transitively bounds
// every downstream AST walker (compiler, formatter, inferencer).
//
// 128 mirrors serde_json's RECURSION_LIMIT: it must be small enough that the
// deepest native-frame path (an expression level descends ~7 frames through
// the precedence chain per depth unit) cannot overflow the smallest stack
// the parser runs on — notably the ~2 MiB libtest thread — before the guard
// trips. It is still far above any real program (examples peak at bracket
// depth ~5; the effective paren cap is ~42 levels).
const MAX_PARSE_DEPTH: u32 = 128;

impl Parser {
    // Runs `f` one recursion level deeper, rejecting input that nests past
    // MAX_PARSE_DEPTH. The depth counter is decremented unconditionally after
    // `f` returns (no `?` between the increment and decrement), so it stays
    // exactly balanced on both the Ok and Err paths — error recovery
    // (synchronize() then continue) always resumes at the correct depth.
    fn with_recursion_guard<T>(&mut self, f: impl FnOnce(&mut Self) -> PResult<T>) -> PResult<T> {
        self.depth += 1;
        if self.depth > MAX_PARSE_DEPTH {
            self.depth -= 1;
            return Err("expression or type nesting too deep".to_string());
        }
        let r = f(self);
        self.depth -= 1;
        r
    }

    fn push_context(&mut self, ctx: ParseContext) {
        self.context_stack.push(ctx);
    }

    fn pop_context(&mut self) {
        debug_assert!(
            self.context_stack.len() > 1,
            "context stack underflow — synchronize double-popped?"
        );
        if self.context_stack.len() > 1 {
            self.context_stack.pop();
        }
    }

    // Runs `f` with `ctx` on the context stack, popping unconditionally on
    // both the Ok and Err paths so a `?` inside `f` cannot leave a stale
    // context behind (which would misdirect every later `synchronize()`).
    // Use this for constructs that propagate errors via `?`; constructs that
    // recover in place (block/array/match) still use push/pop directly
    // because their `synchronize()` may consume the closer and pop early.
    fn with_context<T>(
        &mut self,
        ctx: ParseContext,
        f: impl FnOnce(&mut Self) -> PResult<T>,
    ) -> PResult<T> {
        self.context_stack.push(ctx);
        let r = f(self);
        debug_assert_eq!(self.context_stack.last(), Some(&ctx));
        self.context_stack.pop();
        r
    }

    fn current_context(&self) -> ParseContext {
        *self.context_stack.last().unwrap_or(&ParseContext::TopLevel)
    }

    /// Single emit path for parser diagnostics at an arbitrary span.
    fn error_at(&mut self, span: Span, message: impl Into<String>) {
        self.diagnostics.push(Diagnostic::error(
            span,
            DiagnosticCode::ParseError,
            message.into(),
        ));
    }

    fn add_error(&mut self, message: String) {
        // Every parse error funnels through here after a failed `eat`; when the
        // parser is stuck on the EOF token the error is by construction an
        // unexpected-EOF, so tag it structurally rather than by message text.
        let code = if self.kind() == Kind::Eof {
            DiagnosticCode::UnexpectedEof
        } else {
            DiagnosticCode::ParseError
        };
        let sp = self.cur().span;
        self.diagnostics.push(Diagnostic::error(
            Span::point(sp.start_line, sp.start_column),
            code,
            message,
        ));
    }

    fn current_span(&self) -> Span {
        self.cur().span
    }

    fn span_from(&self, start: Span) -> Span {
        Span {
            start_line: start.start_line,
            start_column: start.start_column,
            end_line: self.prev_token_end_line,
            end_column: self.prev_token_end_column,
        }
    }

    fn cur(&self) -> &Token {
        &self.tokens[self.index]
    }

    fn kind(&self) -> Kind {
        self.tokens[self.index].kind
    }

    fn save_token_end(&mut self) {
        let sp = self.tokens[self.index].span;
        self.prev_token_end_line = sp.end_line;
        self.prev_token_end_column = sp.end_column;
    }

    fn synchronize(&mut self) {
        let ctx = self.current_context();
        self.sync_iterations = 0;

        while self.kind() != Kind::Eof {
            self.sync_iterations += 1;
            if self.sync_iterations > 1000 {
                self.add_error(
                    "Parser recovery failed: too many recovery attempts. This is a bug in the parser."
                        .to_string(),
                );
                while self.kind() != Kind::Eof {
                    self.advance();
                }
                return;
            }
            let delim: Option<(Kind, &[Kind])> = match ctx {
                ParseContext::TopLevel => {
                    if matches!(
                        self.kind(),
                        Kind::Keyword(Keyword::Fn)
                            | Kind::Keyword(Keyword::Type)
                            | Kind::Keyword(Keyword::Const)
                            | Kind::Keyword(Keyword::Import)
                            | Kind::Keyword(Keyword::Pub)
                            | Kind::PuncAt
                            | Kind::Identifier
                    ) {
                        return;
                    }
                    None
                }
                ParseContext::Block => {
                    if self.kind() == Kind::PuncCloseBrace {
                        self.advance();
                        self.pop_context();
                        return;
                    }
                    if matches!(
                        self.kind(),
                        Kind::Keyword(Keyword::If)
                            | Kind::Keyword(Keyword::Match)
                            | Kind::Keyword(Keyword::Fn)
                            | Kind::Identifier
                    ) {
                        return;
                    }
                    None
                }
                ParseContext::FunctionParams => {
                    Some((Kind::PuncCloseParen, &[Kind::PuncOpenBrace]))
                }
                ParseContext::Array => Some((Kind::PuncCloseBracket, &[])),
                ParseContext::TypeDef => Some((Kind::PuncCloseBrace, &[])),
                ParseContext::MatchArms => Some((Kind::PuncCloseBrace, &[Kind::PuncArrow])),
            };

            if let Some((close, extra_stops)) = delim {
                if self.kind() == close {
                    self.advance();
                    self.pop_context();
                    return;
                }
                if extra_stops.contains(&self.kind()) {
                    return;
                }
                if self.kind() == Kind::PuncComma {
                    self.advance();
                    return;
                }
            }

            self.advance();
        }
    }

    // Shared error-recovery step for top-level and block node loops: record the
    // diagnostic, skip to a synchronization point, and produce an error node
    // standing in for the unparseable region.
    fn recover_node(&mut self, err: String, span: Span) -> ast::Node {
        self.add_error(err.clone());
        self.synchronize();
        ast::Node::Expression(ast::Expression::ErrorNode(ast::ErrorNode {
            message: err,
            span,
        }))
    }

    fn advance(&mut self) {
        if self.index + 1 < self.tokens.len() {
            self.save_token_end();
            self.index += 1;
        }
    }

    // Moves the consumed token out of the buffer instead of cloning it. The
    // parser only ever scans forward (index never decreases; lookahead reads
    // index+1), so a vacated slot is never read again — replacing it with a
    // cheap allocation-free sentinel is sound and keeps `eat` zero-alloc.
    fn eat(&mut self, kind: Kind) -> PResult<Token> {
        if self.tokens[self.index].kind == kind {
            self.save_token_end();
            let old = std::mem::replace(
                &mut self.tokens[self.index],
                Token {
                    kind: Kind::Eof,
                    literal: None,
                    span: Span::DUMMY,
                    leading_trivia: Vec::new(),
                },
            );
            self.index += 1;
            return Ok(old);
        }

        Err(format!("Expected '{}', got '{}'", kind, self.cur()))
    }

    fn eat_msg(&mut self, kind: Kind, message: &str) -> PResult<Token> {
        match self.eat(kind) {
            Ok(t) => Ok(t),
            Err(_) => Err(format!("{}, got '{}'", message, self.cur())),
        }
    }

    fn eat_token_literal(&mut self, kind: Kind, message: &str) -> PResult<String> {
        let eaten = self.eat_msg(kind, message)?;

        if let Some(unwrapped) = eaten.literal {
            return Ok(unwrapped);
        }

        Err(format!("Expected {}", message))
    }

    fn eat_identifier(&mut self, message: &str) -> PResult<ast::Identifier> {
        let span = self.current_span();
        let name = self.eat_token_literal(Kind::Identifier, message)?;
        Ok(ast::Identifier { name, span })
    }

    fn peek_next(&self) -> Option<Kind> {
        self.tokens.get(self.index + 1).map(|t| t.kind)
    }

    fn has_newline_before_current(&self) -> bool {
        self.cur()
            .leading_trivia
            .iter()
            .any(|t| matches!(t, Trivia::Newline))
    }

    fn extract_doc_comment(&self) -> Option<String> {
        for trivia in &self.cur().leading_trivia {
            if let Trivia::DocComment(text) = trivia {
                return Some(text.clone());
            }
        }
        None
    }

    // Parses `open` then zero or more comma-separated items then `close`.
    // Allows a trailing comma.
    fn parse_comma_list<T>(
        &mut self,
        open: Kind,
        close: Kind,
        mut item: impl FnMut(&mut Self) -> PResult<T>,
    ) -> PResult<Vec<T>> {
        self.eat(open)?;
        let mut items = Vec::new();
        while self.kind() != close && self.kind() != Kind::Eof {
            items.push(item(self)?);
            if self.kind() == Kind::PuncComma {
                self.eat(Kind::PuncComma)?;
            } else {
                break;
            }
        }
        self.eat(close)?;
        Ok(items)
    }

    // Parses zero or more items separated only by whitespace/newlines until
    // `close` (or Eof). A comma at a separator position is a hard error: the
    // items are self-delimiting, so the comma carried no information and the
    // grammar now rejects it outright. Currently used only for constructor
    // fields.
    fn parse_separated_list<T>(
        &mut self,
        close: Kind,
        mut item: impl FnMut(&mut Self) -> PResult<T>,
    ) -> PResult<Vec<T>> {
        let mut items = Vec::new();
        while self.kind() != close && self.kind() != Kind::Eof {
            items.push(item(self)?);
            if self.kind() == Kind::PuncComma {
                return Err(
                    "unexpected `,` — items here are separated by spaces or newlines, not commas"
                        .to_string(),
                );
            }
        }
        Ok(items)
    }

    fn is_type_start(&self) -> bool {
        is_type_start_token(self.cur())
    }

    // Scan forward from the token after current for a depth-0 `=` before a
    // depth-0 newline. Distinguishes `x Type = ...` / `x = ...` (binding) from
    // `xs[0]` / `Ctor(args)` (expression) without the brittle type-start
    // heuristic that misfires on uppercase constructors and `(` tuple types.
    fn is_binding_ahead(&self) -> bool {
        let mut depth = 0i32;
        let mut i = self.index + 1;
        while i < self.tokens.len() {
            let tok = &self.tokens[i];
            if depth == 0
                && tok
                    .leading_trivia
                    .iter()
                    .any(|t| matches!(t, Trivia::Newline))
            {
                return false;
            }
            match tok.kind {
                Kind::PuncOpenParen | Kind::PuncOpenBracket | Kind::PuncOpenBrace => depth += 1,
                Kind::PuncCloseParen | Kind::PuncCloseBracket | Kind::PuncCloseBrace => depth -= 1,
                Kind::PuncEquals if depth == 0 => return true,
                Kind::PuncEqualsComparator if depth == 0 => return false,
                Kind::Eof => return false,
                _ => {}
            }
            i += 1;
        }
        false
    }

    pub fn parse_program(&mut self) -> ParseResult {
        let program_span = self.current_span();
        let mut body: Vec<ast::Node> = Vec::new();
        let mut seen_non_import = false;

        while self.kind() != Kind::Eof {
            let span = self.current_span();
            match self.parse_node() {
                Ok(node) => {
                    let is_import = matches!(
                        &node,
                        ast::Node::Statement(s) if matches!(**s, ast::Statement::ImportDeclaration(_))
                    );
                    if is_import && seen_non_import {
                        self.diagnostics.push(
                            Diagnostic::error(
                                node.span(),
                                "Imports must precede all other declarations".to_string(),
                            )
                            .with_code(DiagnosticCode::ParseError),
                        );
                    }
                    if !is_import {
                        seen_non_import = true;
                    }
                    body.push(node)
                }
                Err(err) => {
                    let node = self.recover_node(err, span);
                    body.push(node);
                    continue;
                }
            }
        }

        ParseResult {
            ast: ast::BlockExpression {
                body,
                span: self.span_from(program_span),
            },
            diagnostics: std::mem::take(&mut self.diagnostics),
        }
    }

    fn parse_node(&mut self) -> PResult<ast::Node> {
        match self.kind() {
            Kind::PuncAt => {
                let doc = self.extract_doc_comment();
                let attrs = self.parse_attributes()?;
                return Ok(ast::Node::Statement(Box::new(
                    self.parse_attributed_declaration(doc, attrs)?,
                )));
            }
            Kind::Keyword(Keyword::Const) => {
                let doc = self.extract_doc_comment();
                let decl = self.parse_const_binding(doc)?;
                return Ok(ast::Node::Statement(Box::new(
                    ast::Statement::Declaration {
                        decl: Box::new(decl),
                        public: false,
                    },
                )));
            }
            Kind::Keyword(Keyword::Fn) => {
                return self.parse_function();
            }
            Kind::Keyword(Keyword::Type) => {
                let doc = self.extract_doc_comment();
                let decl = self.parse_type_declaration(doc, Vec::new(), false)?;
                return Ok(ast::Node::Statement(Box::new(
                    ast::Statement::Declaration {
                        decl: Box::new(decl),
                        public: false,
                    },
                )));
            }
            Kind::Keyword(Keyword::Import) => {
                return Ok(ast::Node::Statement(Box::new(
                    self.parse_import_declaration()?,
                )));
            }
            Kind::Keyword(Keyword::Pub) => {
                let doc = self.extract_doc_comment();
                return Ok(ast::Node::Statement(Box::new(
                    self.parse_attributed_declaration(doc, Vec::new())?,
                )));
            }
            Kind::PuncOpenParen => {
                if self.is_tuple_destructuring() {
                    return Ok(ast::Node::Statement(Box::new(
                        self.parse_tuple_destructuring()?,
                    )));
                }
            }
            Kind::Identifier => {
                if let Some(name) = &self.cur().literal
                    && self.is_binding_ahead()
                {
                    if is_type_name(name) {
                        // Uppercase ident with a depth-0 `=` ahead. Only commit
                        // to the statement forms when the token immediately
                        // after the name is `=` (TypedDiscard) or `(` (ctor
                        // destructure); anything else (e.g. `Foo.Bar = ..`)
                        // falls through to expression parsing.
                        match self.peek_next() {
                            Some(Kind::PuncEquals) => {
                                return Ok(ast::Node::Statement(Box::new(
                                    self.parse_typed_discard()?,
                                )));
                            }
                            Some(Kind::PuncOpenParen) => {
                                return Ok(ast::Node::Statement(Box::new(
                                    self.parse_ctor_destructuring()?,
                                )));
                            }
                            _ => {}
                        }
                    } else {
                        return Ok(ast::Node::Statement(Box::new(self.parse_binding()?)));
                    }
                }
            }
            _ => {}
        }
        Ok(ast::Node::Expression(self.parse_expression()?))
    }

    fn parse_expression(&mut self) -> PResult<ast::Expression> {
        self.with_recursion_guard(Self::parse_or_expression)
    }

    fn parse_or_expression(&mut self) -> PResult<ast::Expression> {
        let left = self.parse_binary_expression()?;

        if self.kind() == Kind::Keyword(Keyword::Or) {
            self.eat(Kind::Keyword(Keyword::Or))?;

            let mut receiver: Option<ast::Identifier> = None;

            if self.kind() == Kind::Identifier && self.peek_next() == Some(Kind::PuncArrow) {
                receiver = Some(self.eat_identifier("Expected identifier for or receiver")?);
                self.eat(Kind::PuncArrow)?;
            }

            let body = self.parse_expression()?;

            let span = self.span_from(left.span());
            return Ok(ast::Expression::OrExpression(ast::OrExpression {
                expression: Box::new(left),
                receiver,
                body: Box::new(body),
                span,
            }));
        }

        Ok(left)
    }

    // Binary-operator precedence ladder, loosest first. Each level is parsed
    // as a left-associative chain over the next-tighter level. Range is spliced
    // between comparison (3) and additive (4); past the table we hit unary.
    const PRECEDENCE: &[&[(Kind, ast::BinaryOp)]] = &[
        &[(Kind::LogicalOr, ast::BinaryOp::Or)],
        &[(Kind::LogicalAnd, ast::BinaryOp::And)],
        &[
            (Kind::PuncEqualsComparator, ast::BinaryOp::Eq),
            (Kind::PuncNotEqual, ast::BinaryOp::Ne),
        ],
        &[
            (Kind::PuncLt, ast::BinaryOp::Lt),
            (Kind::PuncGt, ast::BinaryOp::Gt),
            (Kind::PuncLte, ast::BinaryOp::Le),
            (Kind::PuncGte, ast::BinaryOp::Ge),
        ],
        &[
            (Kind::PuncPlus, ast::BinaryOp::Add),
            (Kind::PuncMinus, ast::BinaryOp::Sub),
        ],
        &[
            (Kind::PuncMul, ast::BinaryOp::Mul),
            (Kind::PuncDiv, ast::BinaryOp::Div),
            (Kind::PuncMod, ast::BinaryOp::Mod),
        ],
    ];

    // Range is spliced into the precedence chain between PRECEDENCE[RANGE_LEVEL]
    // (comparison) and PRECEDENCE[RANGE_LEVEL + 1] (additive); see parse_range.
    const RANGE_LEVEL: usize = 3;

    fn parse_binary_expression(&mut self) -> PResult<ast::Expression> {
        self.parse_level(0)
    }

    fn parse_level(&mut self, lvl: usize) -> PResult<ast::Expression> {
        if lvl == Self::PRECEDENCE.len() {
            return self.parse_unary_expression();
        }
        let next = |p: &mut Self| {
            if lvl == Self::RANGE_LEVEL {
                p.parse_range()
            } else {
                p.parse_level(lvl + 1)
            }
        };
        let mut left = next(self)?;
        while let Some(&(tok, op)) = Self::PRECEDENCE[lvl]
            .iter()
            .find(|(k, _)| *k == self.kind())
        {
            // A `-` on a fresh line is the start of a new unary expression, not
            // a continuation of an additive chain (P4). Only level 4 has
            // PuncMinus, so this guard is inert elsewhere.
            if tok == Kind::PuncMinus && self.has_newline_before_current() {
                break;
            }
            self.eat(tok)?;
            let right = next(self)?;
            let span = self.span_from(left.span());
            left = ast::Expression::BinaryExpression(ast::BinaryExpression {
                left: Box::new(left),
                right: Box::new(right),
                op,
                span,
            });
        }
        Ok(left)
    }

    // Range sits between comparison and additive so that `-5..5` parses as
    // `(-5)..(5)` (unary binds tighter via additive→multiplicative→unary) and
    // `a+b..c+d` parses as `(a+b)..(c+d)`.
    fn parse_range(&mut self) -> PResult<ast::Expression> {
        let left = self.parse_level(Self::RANGE_LEVEL + 1)?;

        if self.kind() == Kind::PuncDotdot {
            let start = left.span();
            self.eat(Kind::PuncDotdot)?;
            let end = self.parse_level(Self::RANGE_LEVEL + 1)?;
            let span = self.span_from(start);
            return Ok(ast::Expression::RangeExpression(ast::RangeExpression {
                start: Box::new(left),
                end: Box::new(end),
                span,
            }));
        }

        Ok(left)
    }

    fn parse_unary_expression(&mut self) -> PResult<ast::Expression> {
        self.with_recursion_guard(Self::parse_unary_expression_inner)
    }

    fn parse_unary_expression_inner(&mut self) -> PResult<ast::Expression> {
        let kind = self.kind();
        let op = match kind {
            Kind::PuncExclamationMark => ast::UnaryOp::Not,
            Kind::PuncMinus => ast::UnaryOp::Neg,
            _ => return self.parse_postfix_expression(),
        };
        let start = self.current_span();
        self.eat(kind)?;
        let inner = self.parse_unary_expression()?;

        Ok(ast::Expression::UnaryExpression(ast::UnaryExpression {
            expression: Box::new(inner),
            op,
            span: self.span_from(start),
        }))
    }

    fn parse_postfix_expression(&mut self) -> PResult<ast::Expression> {
        let mut expr = self.parse_primary_expression()?;

        loop {
            match self.kind() {
                Kind::PuncDot => {
                    expr = self.parse_dot_expression(expr)?;
                }
                Kind::PuncOpenParen => {
                    // Newline before `(` terminates the postfix chain so that
                    //   f
                    //   (a, b)
                    // is two expressions, not a call (P7).
                    if self.has_newline_before_current() {
                        break;
                    }
                    let start = expr.span();
                    let arguments = self.parse_call_args()?;
                    let span = self.span_from(start);
                    expr = ast::Expression::FunctionCallExpression(ast::FunctionCallExpression {
                        callee: Box::new(expr),
                        arguments,
                        span,
                    });
                }
                Kind::PuncOpenBracket => {
                    if self.has_newline_before_current() {
                        break;
                    }
                    let start = expr.span();
                    self.eat(Kind::PuncOpenBracket)?;
                    let index = self.parse_expression()?;
                    self.eat(Kind::PuncCloseBracket)?;
                    let span = self.span_from(start);
                    expr = ast::Expression::ArrayIndexExpression(ast::ArrayIndexExpression {
                        expression: Box::new(expr),
                        index: Box::new(index),
                        span,
                    });
                }
                Kind::PuncMinusminus => {
                    return Err(
                        "Decrement operator (--) is not supported. Values are immutable in AL - use `x = x - 1` with shadowing instead."
                            .to_string(),
                    );
                }
                _ => {
                    break;
                }
            }
        }

        Ok(expr)
    }

    fn parse_call_args(&mut self) -> PResult<Vec<ast::CallArg>> {
        self.parse_comma_list(Kind::PuncOpenParen, Kind::PuncCloseParen, |p| {
            if p.kind() == Kind::PuncDotdot {
                p.eat(Kind::PuncDotdot)?;
                let expr = p.parse_expression()?;
                Ok(ast::CallArg::Spread(expr))
            } else if p.kind() == Kind::Identifier
                && p.cur().literal.as_deref().is_some_and(|n| !is_type_name(n))
                && p.peek_next() == Some(Kind::PuncColon)
            {
                let label = p.eat_identifier("Expected argument label")?;
                p.eat(Kind::PuncColon)?;
                let value = p.parse_expression()?;
                Ok(ast::CallArg::Labeled { label, value })
            } else {
                Ok(ast::CallArg::Positional(p.parse_expression()?))
            }
        })
    }

    fn parse_primary_expression(&mut self) -> PResult<ast::Expression> {
        self.with_recursion_guard(Self::parse_primary_expression_inner)
    }

    fn parse_primary_expression_inner(&mut self) -> PResult<ast::Expression> {
        let expr = match self.kind() {
            Kind::LiteralString => self.parse_string_expression()?,
            Kind::InterpStringStart => self.parse_interpolated_string()?,
            Kind::LiteralNumber => self.parse_number_expression()?,
            Kind::Identifier => {
                ast::Expression::Identifier(self.eat_identifier("Expected identifier")?)
            }
            Kind::PuncOpenParen => self.parse_tuple()?,
            Kind::PuncOpenBrace => self.parse_block_expression()?,
            Kind::PuncOpenBracket => self.parse_array_expression()?,
            Kind::BinOpen => self.parse_binary_literal()?,
            Kind::Keyword(Keyword::If) => self.parse_if_expression()?,
            Kind::Keyword(Keyword::Match) => self.parse_match_expression()?,
            Kind::Keyword(Keyword::Fn) => self.parse_function_expression()?,
            Kind::Error => {
                let span = self.current_span();
                self.advance();
                ast::Expression::ErrorNode(ast::ErrorNode {
                    message: "Scanner error".to_string(),
                    span,
                })
            }
            _ => {
                return Err(format!("Unexpected '{}'", self.cur()));
            }
        };

        Ok(expr)
    }

    fn parse_block_expression(&mut self) -> PResult<ast::Expression> {
        let block_span = self.current_span();
        self.eat(Kind::PuncOpenBrace)?;
        self.push_context(ParseContext::Block);

        let mut body: Vec<ast::Node> = Vec::new();

        while self.kind() != Kind::PuncCloseBrace && self.kind() != Kind::Eof {
            let span = self.current_span();
            match self.parse_node() {
                Ok(node) => body.push(node),
                Err(err) => {
                    let node = self.recover_node(err, span);
                    body.push(node);
                    continue;
                }
            }
        }

        self.pop_context();
        self.eat(Kind::PuncCloseBrace)?;

        Ok(ast::Expression::BlockExpression(ast::BlockExpression {
            body,
            span: self.span_from(block_span),
        }))
    }

    fn parse_array_expression(&mut self) -> PResult<ast::Expression> {
        let span = self.current_span();
        self.eat(Kind::PuncOpenBracket)?;
        self.push_context(ParseContext::Array);

        let mut elements: Vec<ast::ArrayElement> = Vec::new();

        while self.kind() != Kind::PuncCloseBracket && self.kind() != Kind::Eof {
            let elem_span = self.current_span();

            let elem_result = if self.kind() == Kind::PuncDotdot {
                let spread_span = self.current_span();
                self.eat(Kind::PuncDotdot)?;

                // Bare `..` (followed by `]`, `,` or `else`) is a parse error
                // in an array literal — spreads there always carry a value.
                if matches!(
                    self.kind(),
                    Kind::PuncCloseBracket | Kind::PuncComma | Kind::Keyword(Keyword::Else)
                ) {
                    if self.kind() == Kind::Keyword(Keyword::Else) {
                        self.advance(); // skip the erroneous 'else' token
                    }
                    let msg = "Expected expression after `..` in array literal".to_string();
                    self.diagnostics.push(
                        Diagnostic::error(spread_span, msg.clone())
                            .with_code(DiagnosticCode::ParseError),
                    );
                    Ok(ast::ArrayElement::SpreadElement(ast::SpreadElement {
                        expression: ast::Expression::ErrorNode(ast::ErrorNode {
                            message: msg,
                            span: spread_span,
                        }),
                        span: spread_span,
                    }))
                } else {
                    self.parse_expression().map(|inner| {
                        ast::ArrayElement::SpreadElement(ast::SpreadElement {
                            expression: inner,
                            span: self.span_from(spread_span),
                        })
                    })
                }
            } else {
                self.parse_expression().map(ast::ArrayElement::Expression)
            };

            match elem_result {
                Ok(elem) => elements.push(elem),
                Err(err) => {
                    self.add_error(err.clone());
                    self.synchronize();
                    if self.current_context() != ParseContext::Array {
                        // synchronize consumed the ] and popped context
                        return Ok(ast::Expression::ArrayExpression(ast::ArrayExpression {
                            elements,
                            span: self.span_from(span),
                        }));
                    }
                    elements.push(ast::ArrayElement::Expression(ast::Expression::ErrorNode(
                        ast::ErrorNode {
                            message: err,
                            span: elem_span,
                        },
                    )));
                    continue;
                }
            }

            if self.kind() == Kind::PuncComma {
                self.eat(Kind::PuncComma)?;
            } else {
                break;
            }
        }

        self.pop_context();
        self.eat(Kind::PuncCloseBracket)?;

        Ok(ast::Expression::ArrayExpression(ast::ArrayExpression {
            elements,
            span: self.span_from(span),
        }))
    }

    // `<<seg, seg, ..>>` bit-string literal. Each segment is a value expression
    // optionally followed by `:` and a size spec; a bare segment is an 8-bit
    // Int (so `<<1, 2, 3>>` is three bytes), except a bare string which is its
    // UTF-8 bytes (so `<<'GET '>>` needs no `:utf8`).
    fn parse_binary_literal(&mut self) -> PResult<ast::Expression> {
        let start = self.current_span();
        let segments = self.parse_comma_list(Kind::BinOpen, Kind::BinClose, |p| {
            let seg_start = p.current_span();
            let value = p.parse_expression()?;
            let (size, unit, mut kind) = p.parse_bin_size_spec()?;
            if size.is_none()
                && kind == ast::BinKind::Int
                && matches!(
                    value,
                    ast::Expression::StringLiteral(_) | ast::Expression::InterpolatedString(_)
                )
            {
                kind = ast::BinKind::Utf8;
            }
            Ok(ast::BinSegment {
                value,
                size,
                unit,
                kind,
                span: p.span_from(seg_start),
            })
        })?;
        Ok(ast::Expression::BinaryLiteral(ast::BinaryLiteral {
            segments,
            span: self.span_from(start),
        }))
    }

    // Parses the optional `: size_spec` suffix of a `<<>>` segment. Grammar:
    //   :N          -> N bits, Int
    //   :size(expr) -> expr bits, Int (dynamic)
    //   :bytes(expr)-> expr bytes, Binary slice
    //   :binary     -> remaining bytes, Binary
    //   :utf8       -> string literal: its UTF-8 bytes; otherwise one
    //                  codepoint (pattern) / whole string (expr), Utf8
    // Absent spec -> default 8-bit Int (size left None; codegen supplies 8),
    // except string-literal segments which default to Utf8 (callers patch).
    fn parse_bin_size_spec(
        &mut self,
    ) -> PResult<(Option<ast::Expression>, ast::BinUnit, ast::BinKind)> {
        if self.kind() != Kind::PuncColon {
            return Ok((None, ast::BinUnit::Bits, ast::BinKind::Int));
        }
        self.eat(Kind::PuncColon)?;

        if self.kind() == Kind::LiteralNumber {
            let span = self.current_span();
            let value = self.eat_token_literal(Kind::LiteralNumber, "Expected bit width")?;
            let n = ast::Expression::NumberLiteral(ast::NumberLiteral { value, span });
            return Ok((Some(n), ast::BinUnit::Bits, ast::BinKind::Int));
        }

        if self.kind() == Kind::Identifier {
            let kw = self.cur().literal.clone().unwrap_or_default();
            match kw.as_str() {
                "binary" => {
                    self.advance();
                    return Ok((None, ast::BinUnit::Bytes, ast::BinKind::Binary));
                }
                "utf8" => {
                    self.advance();
                    return Ok((None, ast::BinUnit::Bits, ast::BinKind::Utf8));
                }
                "bytes" => {
                    self.advance();
                    self.eat(Kind::PuncOpenParen)?;
                    let size = self.parse_expression()?;
                    self.eat(Kind::PuncCloseParen)?;
                    return Ok((Some(size), ast::BinUnit::Bytes, ast::BinKind::Binary));
                }
                "size" => {
                    self.advance();
                    self.eat(Kind::PuncOpenParen)?;
                    let size = self.parse_expression()?;
                    self.eat(Kind::PuncCloseParen)?;
                    return Ok((Some(size), ast::BinUnit::Bits, ast::BinKind::Int));
                }
                _ => {}
            }
        }

        Err(format!(
            "Expected segment size spec (an integer, `bytes(..)`, `size(..)`, `binary`, or `utf8`), got '{}'",
            self.cur()
        ))
    }

    fn parse_tuple(&mut self) -> PResult<ast::Expression> {
        let start = self.current_span();
        self.eat(Kind::PuncOpenParen)?;

        if self.kind() == Kind::PuncCloseParen {
            return Err(
                "tuples need 2+ elements; for unit use 'Nil', for grouping use '{expr}'"
                    .to_string(),
            );
        }

        let mut elements: Vec<ast::Expression> = Vec::new();
        elements.push(self.parse_expression()?);

        if self.kind() == Kind::PuncCloseParen {
            return Err("single-element parens not allowed; use '{expr}' for grouping".to_string());
        }

        while self.kind() == Kind::PuncComma {
            self.eat(Kind::PuncComma)?;
            if self.kind() == Kind::PuncCloseParen {
                break;
            }
            elements.push(self.parse_expression()?);
        }

        self.eat(Kind::PuncCloseParen)?;
        Ok(ast::Expression::TupleExpression(ast::TupleExpression {
            elements,
            span: self.span_from(start),
        }))
    }

    fn parse_if_expression(&mut self) -> PResult<ast::Expression> {
        self.with_recursion_guard(Self::parse_if_expression_inner)
    }

    fn parse_if_expression_inner(&mut self) -> PResult<ast::Expression> {
        let span = self.current_span();
        self.eat(Kind::Keyword(Keyword::If))?;

        let condition = self.parse_expression()?;
        let body = self.parse_braced_body("'if' branch")?;

        if self.kind() != Kind::Keyword(Keyword::Else) {
            return Err("'if' requires an 'else' branch".to_string());
        }
        self.eat(Kind::Keyword(Keyword::Else))?;
        // Recurse through the guarded wrapper so each `else if` re-enters the
        // depth guard — otherwise the chain recurses at constant depth and a
        // long `else if` ladder overflows the native stack (uncatchable).
        let else_body = if self.kind() == Kind::Keyword(Keyword::If) {
            self.parse_if_expression()?
        } else {
            self.parse_braced_body("'else' branch")?
        };

        Ok(ast::Expression::IfExpression(ast::IfExpression {
            condition: Box::new(condition),
            body: Box::new(body),
            span: self.span_from(span),
            else_body: Box::new(else_body),
        }))
    }

    fn parse_braced_body(&mut self, what: &str) -> PResult<ast::Expression> {
        if self.kind() != Kind::PuncOpenBrace {
            return Err(format!("{what} must be a block `{{ ... }}`"));
        }
        self.parse_expression()
    }

    fn parse_match_expression(&mut self) -> PResult<ast::Expression> {
        let match_span = self.current_span();
        self.eat(Kind::Keyword(Keyword::Match))?;

        let subject = self.parse_expression()?;

        self.eat(Kind::PuncOpenBrace)?;
        self.push_context(ParseContext::MatchArms);

        let mut arms: Vec<ast::MatchArm> = Vec::new();

        'arms: while self.kind() != Kind::PuncCloseBrace && self.kind() != Kind::Eof {
            let pattern = match self.parse_pattern() {
                Ok(p) => p,
                Err(err) => {
                    let err_span = self.current_span();
                    self.add_error(err);
                    self.synchronize();
                    match self.kind() {
                        Kind::PuncCloseBrace => break,
                        // Recovery stopped at this arm's `->`: fall through with
                        // a placeholder so the body still parses and the loop
                        // advances instead of re-erroring on the same token.
                        Kind::PuncArrow => ast::Pattern::Wildcard { span: err_span },
                        _ => continue,
                    }
                }
            };

            let guard = if self.kind() == Kind::Keyword(Keyword::If) {
                self.eat(Kind::Keyword(Keyword::If))?;
                match self.parse_expression() {
                    Ok(e) => Some(e),
                    Err(err) => {
                        self.add_error(err);
                        self.synchronize();
                        match self.kind() {
                            Kind::PuncCloseBrace => break,
                            Kind::PuncArrow => None,
                            _ => continue,
                        }
                    }
                }
            } else {
                None
            };

            if let Err(err) = self.eat(Kind::PuncArrow) {
                self.add_error(err);
                self.synchronize();
                match self.kind() {
                    Kind::PuncCloseBrace => break,
                    Kind::PuncArrow => self.advance(),
                    _ => continue,
                }
            }

            let body_span = self.current_span();
            let body = match self.parse_expression() {
                Ok(e) => e,
                Err(err) => {
                    self.add_error(err.clone());
                    self.synchronize();
                    if self.kind() == Kind::PuncCloseBrace {
                        break 'arms;
                    }
                    ast::Expression::ErrorNode(ast::ErrorNode {
                        message: err,
                        span: body_span,
                    })
                }
            };

            arms.push(ast::MatchArm {
                pattern,
                guard,
                body,
            });

            if self.kind() == Kind::PuncComma {
                self.add_error(
                    "unexpected `,` — match arms are separated by newlines, not commas".to_string(),
                );
                self.eat(Kind::PuncComma)?;
            }
        }

        self.pop_context();
        self.eat(Kind::PuncCloseBrace)?;

        Ok(ast::Expression::MatchExpression(ast::MatchExpression {
            subject: Box::new(subject),
            arms,
            span: self.span_from(match_span),
        }))
    }

    // ------------------------------------------------------------------------
    // Patterns
    // ------------------------------------------------------------------------

    fn parse_pattern(&mut self) -> PResult<ast::Pattern> {
        let start = self.current_span();
        let first = self.parse_pattern_range()?;

        if self.kind() == Kind::BitwiseOr {
            let mut patterns = vec![first];
            while self.kind() == Kind::BitwiseOr {
                self.eat(Kind::BitwiseOr)?;
                patterns.push(self.parse_pattern_range()?);
            }
            return Ok(ast::Pattern::Or {
                patterns,
                span: self.span_from(start),
            });
        }

        Ok(first)
    }

    fn parse_pattern_range(&mut self) -> PResult<ast::Pattern> {
        let start = self.current_span();
        let first = self.parse_pattern_atom()?;

        if self.kind() == Kind::PuncDotdot {
            self.eat(Kind::PuncDotdot)?;
            let end = self.parse_pattern_atom()?;
            let span = self.span_from(start);
            return Ok(ast::Pattern::Range {
                start: self.require_number_bound(first),
                end: self.require_number_bound(end),
                span,
            });
        }

        Ok(first)
    }

    /// Range pattern bounds must be number literals; anything else is
    /// diagnosed and replaced with a `0` placeholder for recovery.
    fn require_number_bound(&mut self, p: ast::Pattern) -> ast::NumberLiteral {
        match p {
            ast::Pattern::Literal(ast::PatternLiteral::Number(n)) => n,
            other => {
                let span = other.span();
                self.diagnostics.push(
                    Diagnostic::error(
                        span,
                        "Range pattern bounds must be number literals".to_string(),
                    )
                    .with_code(DiagnosticCode::ParseError),
                );
                ast::NumberLiteral {
                    value: "0".to_string(),
                    span,
                }
            }
        }
    }

    fn parse_pattern_atom(&mut self) -> PResult<ast::Pattern> {
        self.with_recursion_guard(Self::parse_pattern_atom_inner)
    }

    fn parse_pattern_atom_inner(&mut self) -> PResult<ast::Pattern> {
        let start = self.current_span();
        match self.kind() {
            Kind::Keyword(Keyword::Else) => {
                self.eat(Kind::Keyword(Keyword::Else))?;
                Ok(ast::Pattern::Wildcard { span: start })
            }
            Kind::Identifier => {
                let id_span = self.current_span();
                let name = self.eat_token_literal(Kind::Identifier, "Expected pattern")?;
                if name == "_" {
                    return Ok(ast::Pattern::Wildcard { span: id_span });
                }
                if is_type_name(&name) {
                    let mut args: Vec<ast::PatternArg> = Vec::new();
                    let mut rest = false;
                    if self.kind() == Kind::PuncOpenParen && !self.has_newline_before_current() {
                        self.eat(Kind::PuncOpenParen)?;
                        let (parsed_args, parsed_rest) = self.parse_pattern_args()?;
                        args = parsed_args;
                        rest = parsed_rest;
                        self.eat(Kind::PuncCloseParen)?;
                    }
                    return Ok(ast::Pattern::Constructor {
                        name: ast::Identifier {
                            name,
                            span: id_span,
                        },
                        args,
                        rest,
                        span: self.span_from(start),
                    });
                }
                Ok(ast::Pattern::Var {
                    name: ast::Identifier {
                        name,
                        span: id_span,
                    },
                })
            }
            Kind::LiteralNumber => {
                let span = self.current_span();
                let value = self.eat_token_literal(Kind::LiteralNumber, "Expected number")?;
                Ok(ast::Pattern::Literal(ast::PatternLiteral::Number(
                    ast::NumberLiteral { value, span },
                )))
            }
            Kind::PuncMinus => {
                self.eat(Kind::PuncMinus)?;
                let value =
                    self.eat_token_literal(Kind::LiteralNumber, "Expected number after `-`")?;
                Ok(ast::Pattern::Literal(ast::PatternLiteral::Number(
                    ast::NumberLiteral {
                        value: format!("-{value}"),
                        span: self.span_from(start),
                    },
                )))
            }
            Kind::LiteralString => {
                let span = self.current_span();
                let value = self.eat_token_literal(Kind::LiteralString, "Expected string")?;
                Ok(ast::Pattern::Literal(ast::PatternLiteral::String(
                    ast::StringLiteral { value, span },
                )))
            }
            Kind::PuncOpenParen => {
                let elements =
                    self.parse_comma_list(Kind::PuncOpenParen, Kind::PuncCloseParen, |p| {
                        p.parse_pattern()
                    })?;
                if elements.len() < 2 {
                    return Err("tuple patterns need 2+ elements".to_string());
                }
                Ok(ast::Pattern::Tuple {
                    elements,
                    span: self.span_from(start),
                })
            }
            Kind::PuncOpenBracket => {
                let elements =
                    self.parse_comma_list(Kind::PuncOpenBracket, Kind::PuncCloseBracket, |p| {
                        if p.kind() == Kind::PuncDotdot {
                            let spread_span = p.current_span();
                            p.eat(Kind::PuncDotdot)?;
                            let mut binding: Option<ast::Identifier> = None;
                            if p.kind() == Kind::Identifier {
                                binding = Some(p.eat_identifier("Expected binding after `..`")?);
                            }
                            Ok(ast::ArrayPatternElement::Spread {
                                binding,
                                span: p.span_from(spread_span),
                            })
                        } else {
                            Ok(ast::ArrayPatternElement::Pattern(p.parse_pattern()?))
                        }
                    })?;
                Ok(ast::Pattern::Array {
                    elements,
                    span: self.span_from(start),
                })
            }
            Kind::BinOpen => self.parse_binary_pattern(),
            _ => Err(format!("Unexpected '{}' in pattern", self.cur())),
        }
    }

    // `<<seg, seg, .., rest:binary>>` bit-string pattern. Segment values are
    // restricted to atom patterns (binder / literal / `_`); the size spec is
    // shared with the expression form. A trailing `..ident` / `..` captures the
    // remaining bytes (alias for a final `ident:binary` segment) and must be
    // last.
    fn parse_binary_pattern(&mut self) -> PResult<ast::Pattern> {
        let start = self.current_span();
        self.eat(Kind::BinOpen)?;

        let mut segments: Vec<ast::BinSegmentPat> = Vec::new();
        let mut rest: Option<ast::BinaryPatternRest> = None;

        while self.kind() != Kind::BinClose && self.kind() != Kind::Eof {
            if self.kind() == Kind::PuncDotdot {
                let rest_span = self.current_span();
                self.eat(Kind::PuncDotdot)?;
                let binding = if self.kind() == Kind::Identifier {
                    Some(self.eat_identifier("Expected binding after `..`")?)
                } else {
                    None
                };
                rest = Some(ast::BinaryPatternRest {
                    binding,
                    span: self.span_from(rest_span),
                });
                if self.kind() == Kind::PuncComma {
                    return Err(
                        "`..` rest must be the last segment in a `<<>>` pattern".to_string()
                    );
                }
                break;
            }

            let seg_start = self.current_span();
            let value = self.parse_pattern_atom()?;
            let (size, unit, mut kind) = self.parse_bin_size_spec()?;
            // A bare string-literal segment matches its UTF-8 bytes as a
            // prefix (`<<'GET ', ..rest>>`), not a single 8-bit Int.
            if size.is_none()
                && kind == ast::BinKind::Int
                && matches!(value, ast::Pattern::Literal(ast::PatternLiteral::String(_)))
            {
                kind = ast::BinKind::Utf8;
            }
            segments.push(ast::BinSegmentPat {
                value,
                size,
                unit,
                kind,
                span: self.span_from(seg_start),
            });

            if self.kind() == Kind::PuncComma {
                self.eat(Kind::PuncComma)?;
            } else {
                break;
            }
        }

        self.eat(Kind::BinClose)?;
        Ok(ast::Pattern::Binary {
            segments,
            rest,
            span: self.span_from(start),
        })
    }

    fn parse_pattern_args(&mut self) -> PResult<(Vec<ast::PatternArg>, bool)> {
        let mut args: Vec<ast::PatternArg> = Vec::new();
        let mut rest = false;

        while self.kind() != Kind::PuncCloseParen && self.kind() != Kind::Eof {
            if self.kind() == Kind::PuncDotdot {
                self.eat(Kind::PuncDotdot)?;
                rest = true;
                break;
            }

            if self.kind() == Kind::Identifier
                && self
                    .cur()
                    .literal
                    .as_deref()
                    .is_some_and(|n| !is_type_name(n))
                && self.peek_next() == Some(Kind::PuncColon)
            {
                let label = self.eat_identifier("Expected pattern label")?;
                self.eat(Kind::PuncColon)?;
                let pattern = self.parse_pattern_range()?;
                args.push(ast::PatternArg::Labeled { label, pattern });
            } else {
                let pattern = self.parse_pattern_range()?;
                args.push(ast::PatternArg::Positional(pattern));
            }

            if self.kind() == Kind::PuncComma {
                self.eat(Kind::PuncComma)?;
            } else {
                break;
            }
        }

        Ok((args, rest))
    }

    // ------------------------------------------------------------------------
    // Functions
    // ------------------------------------------------------------------------

    fn parse_function(&mut self) -> PResult<ast::Node> {
        if self.peek_next() == Some(Kind::Identifier) {
            let doc = self.extract_doc_comment();
            let decl = self.parse_function_declaration(doc, Vec::new())?;
            return Ok(ast::Node::Statement(Box::new(
                ast::Statement::Declaration {
                    decl: Box::new(decl),
                    public: false,
                },
            )));
        }
        Ok(ast::Node::Expression(self.parse_function_expression()?))
    }

    fn parse_function_declaration(
        &mut self,
        doc: Option<String>,
        attributes: Vec<ast::Attribute>,
    ) -> PResult<ast::Declaration> {
        let fn_span = self.current_span();
        self.eat(Kind::Keyword(Keyword::Fn))?;

        let identifier = self.eat_identifier("Expected function name")?;

        let params = self.parse_parameters()?;
        let return_type = self.parse_function_return_types()?;

        let vm_attr = attributes.iter().find(|a| a.name.name == "vm");
        let body = if let Some(vm_attr) = vm_attr {
            if return_type.is_none() {
                return Err("@vm functions must declare a return type".to_string());
            }
            if self.kind() == Kind::PuncOpenBrace {
                return Err("@vm functions cannot have a body".to_string());
            }
            let op = vm_attr
                .args
                .first()
                .cloned()
                .ok_or_else(|| "@vm requires an op name, e.g. @vm(add)".to_string())?;
            ast::FnBody::Vm(op)
        } else {
            ast::FnBody::Block(self.parse_braced_body("Function body")?)
        };

        Ok(ast::Declaration::Function(ast::FunctionDeclaration {
            doc,
            attributes,
            identifier,
            params,
            return_type,
            body,
            span: self.span_from(fn_span),
        }))
    }

    fn parse_function_expression(&mut self) -> PResult<ast::Expression> {
        let fn_span = self.current_span();
        self.eat(Kind::Keyword(Keyword::Fn))?;

        let params = self.parse_parameters()?;
        // Lambdas have no return-type slot — body starts immediately after `)`.
        // This makes `fn(x) x * 3` unambiguous.
        let body = self.parse_expression()?;

        Ok(ast::Expression::FunctionExpression(
            ast::FunctionExpression {
                params,
                return_type: None,
                body: Box::new(body),
                span: self.span_from(fn_span),
            },
        ))
    }

    // Checks whether a token begins a type in param/return position. Unlike
    // `is_type_start_token`, any identifier qualifies: these positions admit
    // lowercase type variables.
    fn at_loose_type_start(&self) -> bool {
        matches!(
            self.kind(),
            Kind::Identifier | Kind::Keyword(Keyword::Fn) | Kind::PuncOpenParen
        )
    }

    fn parse_function_return_types(&mut self) -> PResult<Option<ast::TypeIdentifier>> {
        if self.at_loose_type_start() {
            return Ok(Some(self.parse_type_identifier()?));
        }
        Ok(None)
    }

    fn parse_parameters(&mut self) -> PResult<Vec<ast::FunctionParameter>> {
        self.with_context(ParseContext::FunctionParams, |p| {
            p.parse_comma_list(Kind::PuncOpenParen, Kind::PuncCloseParen, |p| {
                p.parse_parameter()
            })
        })
    }

    fn parse_parameter(&mut self) -> PResult<ast::FunctionParameter> {
        let identifier = self.eat_identifier("Expected parameter name")?;

        let mut typ: Option<ast::TypeIdentifier> = None;

        if self.at_loose_type_start() {
            typ = Some(self.parse_type_identifier()?);
        }

        Ok(ast::FunctionParameter { typ, identifier })
    }

    // ------------------------------------------------------------------------
    // Types
    // ------------------------------------------------------------------------

    fn parse_type_identifier(&mut self) -> PResult<ast::TypeIdentifier> {
        self.with_recursion_guard(Self::parse_type_identifier_inner)
    }

    fn parse_type_identifier_inner(&mut self) -> PResult<ast::TypeIdentifier> {
        if self.kind() == Kind::PuncOpenParen {
            let span = self.current_span();
            let elements =
                self.parse_comma_list(Kind::PuncOpenParen, Kind::PuncCloseParen, |p| {
                    p.parse_type_identifier()
                })?;
            if elements.len() < 2 {
                return Err("tuple types need 2+ elements".to_string());
            }
            return Ok(ast::TypeIdentifier {
                kind: ast::TypeKind::TupleType(ast::TupleType { elements }),
                span: self.span_from(span),
            });
        }

        if self.kind() == Kind::Keyword(Keyword::Fn) {
            return self.parse_function_type();
        }

        let span = self.current_span();
        let name = self.eat_token_literal(Kind::Identifier, "Expected type name")?;

        let mut type_args: Vec<ast::TypeIdentifier> = Vec::new();
        if self.kind() == Kind::PuncOpenParen && !self.has_newline_before_current() {
            type_args = self.parse_comma_list(Kind::PuncOpenParen, Kind::PuncCloseParen, |p| {
                p.parse_type_identifier()
            })?;
        }

        Ok(ast::TypeIdentifier {
            kind: ast::TypeKind::NamedType(ast::NamedType {
                identifier: ast::Identifier { name, span },
                type_args,
            }),
            span: self.span_from(span),
        })
    }

    fn parse_function_type(&mut self) -> PResult<ast::TypeIdentifier> {
        let span = self.current_span();
        self.eat(Kind::Keyword(Keyword::Fn))?;
        let param_types =
            self.parse_comma_list(Kind::PuncOpenParen, Kind::PuncCloseParen, |p| {
                p.parse_type_identifier()
            })?;

        let mut return_type: Option<Box<ast::TypeIdentifier>> = None;

        if self.at_loose_type_start() {
            let ret = self.parse_type_identifier()?;
            return_type = Some(Box::new(ret));
        }

        Ok(ast::TypeIdentifier {
            kind: ast::TypeKind::FunctionType(ast::FunctionType {
                params: param_types,
                return_type,
            }),
            span: self.span_from(span),
        })
    }

    fn parse_type_params(&mut self) -> PResult<Vec<ast::Identifier>> {
        if self.kind() != Kind::PuncOpenParen {
            return Ok(Vec::new());
        }
        self.parse_comma_list(Kind::PuncOpenParen, Kind::PuncCloseParen, |p| {
            p.eat_identifier("Expected type parameter name")
        })
    }

    fn parse_type_declaration(
        &mut self,
        doc: Option<String>,
        attributes: Vec<ast::Attribute>,
        opaque: bool,
    ) -> PResult<ast::Declaration> {
        let start = self.current_span();
        self.eat(Kind::Keyword(Keyword::Type))?;

        let identifier = self.eat_identifier("Expected type name after `type`")?;
        if !is_type_name(&identifier.name) {
            return Err(format!(
                "Type name '{}' must start with an uppercase letter",
                identifier.name
            ));
        }
        let type_params = self.parse_type_params()?;

        let body = if self.kind() == Kind::PuncEquals {
            if opaque {
                return Err("`opaque` cannot be applied to a type alias".to_string());
            }
            self.eat(Kind::PuncEquals)?;
            ast::TypeBody::Alias(self.parse_type_identifier()?)
        } else if self.kind() != Kind::PuncOpenBrace {
            if opaque {
                return Err("`opaque` type must have constructors to hide".to_string());
            }
            ast::TypeBody::External
        } else {
            self.eat(Kind::PuncOpenBrace)?;
            let variants = self.with_context(ParseContext::TypeDef, |p| {
                if p.kind() == Kind::Identifier
                    && p.cur().literal.as_deref().is_some_and(|s| !is_type_name(s))
                {
                    // Single-constructor shorthand: `type T { field Type ... }`
                    // desugars to `type T { T(field Type ...) }`. Fields are
                    // separated by newlines/spaces; commas are rejected.
                    let fields = p.parse_separated_list(Kind::PuncCloseBrace, |q| {
                        q.parse_constructor_field()
                    })?;
                    Ok(vec![ast::Constructor {
                        doc: None,
                        identifier: identifier.clone(),
                        fields,
                        span: identifier.span,
                    }])
                } else {
                    let mut variants = Vec::new();
                    while p.kind() != Kind::PuncCloseBrace && p.kind() != Kind::Eof {
                        variants.push(p.parse_constructor()?);
                    }
                    Ok(variants)
                }
            })?;
            self.eat(Kind::PuncCloseBrace)?;
            if variants.is_empty() {
                return Err("Type definition must have at least one constructor".to_string());
            }
            ast::TypeBody::Variants {
                ctors: variants,
                opaque,
            }
        };

        Ok(ast::Declaration::Type(ast::TypeDeclaration {
            doc,
            attributes,
            identifier,
            type_params,
            body,
            span: self.span_from(start),
        }))
    }

    fn parse_constructor(&mut self) -> PResult<ast::Constructor> {
        let doc = self.extract_doc_comment();
        let start = self.current_span();
        let name = self.eat_token_literal(Kind::Identifier, "Expected constructor name")?;
        if !is_type_name(&name) {
            return Err(format!(
                "Constructor name '{name}' must start with an uppercase letter"
            ));
        }
        let mut fields = Vec::new();
        if self.kind() == Kind::PuncOpenParen {
            self.eat(Kind::PuncOpenParen)?;
            fields =
                self.parse_separated_list(Kind::PuncCloseParen, |p| p.parse_constructor_field())?;
            self.eat(Kind::PuncCloseParen)?;
        }
        Ok(ast::Constructor {
            doc,
            identifier: ast::Identifier { name, span: start },
            fields,
            span: self.span_from(start),
        })
    }

    fn parse_constructor_field(&mut self) -> PResult<ast::ConstructorField> {
        let start = self.current_span();

        // Detect a bare type with no label and produce the spec'd error.
        if self.is_type_start() {
            return Err("constructor fields must be labeled: write 'label Type'".to_string());
        }

        let label = self.eat_identifier("Expected field label")?;
        let typ = self.parse_type_identifier()?;

        Ok(ast::ConstructorField {
            label,
            typ,
            span: self.span_from(start),
        })
    }

    // ------------------------------------------------------------------------
    // Bindings
    // ------------------------------------------------------------------------

    fn parse_const_binding(&mut self, doc: Option<String>) -> PResult<ast::Declaration> {
        let span = self.current_span();
        self.eat(Kind::Keyword(Keyword::Const))?;

        let identifier = self.eat_identifier("Expected const name")?;

        let mut typ: Option<ast::TypeIdentifier> = None;
        if self.is_type_start() {
            typ = Some(self.parse_type_identifier()?);
        }

        self.eat(Kind::PuncEquals)?;

        let init = self.parse_expression()?;

        Ok(ast::Declaration::Const(ast::ConstBinding {
            doc,
            identifier,
            typ,
            init,
            span: self.span_from(span),
        }))
    }

    fn is_tuple_destructuring(&self) -> bool {
        let mut depth = 0;
        let mut i = self.index;

        while i < self.tokens.len() {
            let tok = &self.tokens[i];
            if tok.kind == Kind::PuncOpenParen {
                depth += 1;
            } else if tok.kind == Kind::PuncCloseParen {
                depth -= 1;
                if depth == 0 {
                    if i + 1 < self.tokens.len() && self.tokens[i + 1].kind == Kind::PuncEquals {
                        return true;
                    }
                    return false;
                }
            } else if tok.kind == Kind::Eof {
                return false;
            }
            i += 1;
        }
        false
    }

    fn parse_tuple_destructuring(&mut self) -> PResult<ast::Statement> {
        let span = self.current_span();
        let patterns = self.parse_comma_list(Kind::PuncOpenParen, Kind::PuncCloseParen, |p| {
            p.parse_pattern_atom()
        })?;
        self.eat(Kind::PuncEquals)?;
        let init = self.parse_expression()?;

        Ok(ast::Statement::TupleDestructuringBinding(
            ast::TupleDestructuringBinding {
                patterns,
                init,
                span: self.span_from(span),
            },
        ))
    }

    fn parse_typed_discard(&mut self) -> PResult<ast::Statement> {
        let span = self.current_span();
        let ty_name = self.eat_identifier("Expected type name")?;
        self.eat(Kind::PuncEquals)?;
        let init = self.parse_expression()?;
        Ok(ast::Statement::TypedDiscard(ast::TypedDiscard {
            ty_name,
            init,
            span: self.span_from(span),
        }))
    }

    fn parse_ctor_destructuring(&mut self) -> PResult<ast::Statement> {
        let span = self.current_span();
        // The dispatch in parse_node guarantees an uppercase identifier followed
        // by `(`, so parse the constructor head directly.
        let name = self.eat_identifier("Expected constructor name")?;
        self.eat(Kind::PuncOpenParen)?;
        let (args, rest) = self.parse_pattern_args()?;
        self.eat(Kind::PuncCloseParen)?;
        let pattern_span = self.span_from(span);
        self.eat(Kind::PuncEquals)?;
        let init = self.parse_expression()?;
        Ok(ast::Statement::CtorDestructuringBinding(
            ast::CtorDestructuringBinding {
                name,
                args,
                rest,
                pattern_span,
                init,
                span: self.span_from(span),
            },
        ))
    }

    fn parse_binding(&mut self) -> PResult<ast::Statement> {
        let doc = self.extract_doc_comment();
        let span = self.current_span();
        let name = self.eat_token_literal(Kind::Identifier, "Expected identifier")?;

        let mut typ: Option<ast::TypeIdentifier> = None;
        if self.is_type_start() {
            typ = Some(self.parse_type_identifier()?);
        }

        self.eat(Kind::PuncEquals)?;
        let init = self.parse_expression()?;

        Ok(ast::Statement::VariableBinding(ast::VariableBinding {
            doc,
            identifier: ast::Identifier { name, span },
            typ,
            init,
            span: self.span_from(span),
        }))
    }

    fn parse_attributed_declaration(
        &mut self,
        doc: Option<String>,
        attrs: Vec<ast::Attribute>,
    ) -> PResult<ast::Statement> {
        let start = attrs.first().map(|a| a.span).unwrap_or(self.current_span());
        let is_pub = self.kind() == Kind::Keyword(Keyword::Pub);
        if is_pub {
            self.eat(Kind::Keyword(Keyword::Pub))?;
        }
        let opaque = is_pub && self.kind() == Kind::Keyword(Keyword::Opaque);
        if opaque {
            self.eat(Kind::Keyword(Keyword::Opaque))?;
        }
        let mut decl = self.parse_declaration_inner(doc, attrs, opaque)?;
        let s = decl.span_mut();
        s.start_line = start.start_line;
        s.start_column = start.start_column;
        Ok(ast::Statement::Declaration {
            decl: Box::new(decl),
            public: is_pub,
        })
    }

    fn parse_declaration_inner(
        &mut self,
        doc: Option<String>,
        attrs: Vec<ast::Attribute>,
        opaque: bool,
    ) -> PResult<ast::Declaration> {
        if opaque && self.kind() != Kind::Keyword(Keyword::Type) {
            return Err("`opaque` may only be applied to `type` declarations".to_string());
        }
        match self.kind() {
            Kind::Keyword(Keyword::Fn) => self.parse_function_declaration(doc, attrs),
            Kind::Keyword(Keyword::Type) => self.parse_type_declaration(doc, attrs, opaque),
            Kind::Keyword(Keyword::Const) => {
                if !attrs.is_empty() {
                    return Err("Attributes are not allowed on `const` declarations".to_string());
                }
                self.parse_const_binding(doc)
            }
            other => Err(format!(
                "Expected `fn`, `type`, or `const` after `pub`, got '{other}'"
            )),
        }
    }

    fn parse_attributes(&mut self) -> PResult<Vec<ast::Attribute>> {
        let mut attrs = Vec::new();
        while self.kind() == Kind::PuncAt {
            attrs.push(self.parse_attribute()?);
        }
        Ok(attrs)
    }

    fn parse_attribute(&mut self) -> PResult<ast::Attribute> {
        let start = self.current_span();
        self.eat(Kind::PuncAt)?;
        let name = self.eat_identifier("Expected attribute name after '@'")?;
        let mut args = Vec::new();
        if self.kind() == Kind::PuncOpenParen {
            args = self.parse_comma_list(Kind::PuncOpenParen, Kind::PuncCloseParen, |p| {
                p.eat_identifier("Expected attribute argument")
            })?;
        }
        Ok(ast::Attribute {
            name,
            args,
            span: self.span_from(start),
        })
    }

    fn parse_import_declaration(&mut self) -> PResult<ast::Statement> {
        let import_span = self.current_span();
        self.eat(Kind::Keyword(Keyword::Import))?;

        let mut path: Vec<String> = Vec::new();

        // Leading `.` / `..` segments for relative imports. The literal strings
        // are the resolver's contract (module::resolve matches on "." / "..");
        // do not derive them from Kind's Display.
        while matches!(self.kind(), Kind::PuncDot | Kind::PuncDotdot) {
            let seg = if self.kind() == Kind::PuncDot {
                "."
            } else {
                ".."
            };
            path.push(seg.to_string());
            self.advance();
            if self.kind() != Kind::PuncDiv {
                return Err("Expected `/` after relative import segment".to_string());
            }
            self.eat(Kind::PuncDiv)?;
        }

        // Track the span of the final module-name segment; the last write wins.
        let mut path_span = self.current_span();
        let first =
            self.eat_token_literal(Kind::Identifier, "Expected module name after `import`")?;
        path.push(first);

        while self.kind() == Kind::PuncDiv {
            self.eat(Kind::PuncDiv)?;
            path_span = self.current_span();
            let seg = self.eat_token_literal(Kind::Identifier, "Expected module name after `/`")?;
            path.push(seg);
        }

        let mut alias: Option<ast::Identifier> = None;
        let mut items: Vec<ast::ImportItem> = Vec::new();

        if self.kind() == Kind::Keyword(Keyword::As) {
            self.eat(Kind::Keyword(Keyword::As))?;
            alias = Some(self.eat_identifier("Expected alias after `as`")?);
        }

        // Selective imports: `.{a, B, c as d}`
        if self.kind() == Kind::PuncDot && self.peek_next() == Some(Kind::PuncOpenBrace) {
            self.eat(Kind::PuncDot)?;
            items = self.parse_comma_list(Kind::PuncOpenBrace, Kind::PuncCloseBrace, |p| {
                let name = p.eat_identifier("Expected import item")?;
                let mut item_alias: Option<ast::Identifier> = None;
                if p.kind() == Kind::Keyword(Keyword::As) {
                    p.eat(Kind::Keyword(Keyword::As))?;
                    item_alias = Some(p.eat_identifier("Expected alias after `as`")?);
                }
                Ok(ast::ImportItem {
                    name,
                    alias: item_alias,
                })
            })?;
        }

        Ok(ast::Statement::ImportDeclaration(ast::ImportDeclaration {
            path,
            alias,
            items,
            path_span,
            span: self.span_from(import_span),
        }))
    }

    fn parse_dot_expression(&mut self, left: ast::Expression) -> PResult<ast::Expression> {
        let start = left.span();
        self.eat(Kind::PuncDot)?;

        let span = self.current_span();

        if self.kind() == Kind::LiteralNumber {
            let num_str = self.eat_token_literal(Kind::LiteralNumber, "Expected tuple index")?;
            let mut result = left;
            for part in num_str.split('.') {
                result = ast::Expression::PropertyAccessExpression(ast::PropertyAccessExpression {
                    left: Box::new(result),
                    right: ast::PropertyKey::TupleIndex(ast::NumberLiteral {
                        value: part.to_string(),
                        span,
                    }),
                    span: self.span_from(start),
                });
            }
            return Ok(result);
        }

        let property = self.eat_token_literal(Kind::Identifier, "Expected property name")?;

        Ok(ast::Expression::PropertyAccessExpression(
            ast::PropertyAccessExpression {
                left: Box::new(left),
                right: ast::PropertyKey::Field(ast::Identifier {
                    name: property,
                    span,
                }),
                span: self.span_from(start),
            },
        ))
    }

    fn parse_string_expression(&mut self) -> PResult<ast::Expression> {
        let span = self.current_span();
        Ok(ast::Expression::StringLiteral(ast::StringLiteral {
            value: self.eat_token_literal(Kind::LiteralString, "Expected string")?,
            span,
        }))
    }

    fn parse_interpolated_string(&mut self) -> PResult<ast::Expression> {
        let span = self.current_span();
        self.eat(Kind::InterpStringStart)?;

        let mut parts: Vec<ast::InterpPart> = Vec::new();

        loop {
            match self.kind() {
                Kind::InterpStringPart => {
                    let part_span = self.current_span();
                    let value =
                        self.eat_token_literal(Kind::InterpStringPart, "Expected string part")?;
                    parts.push(ast::InterpPart::Literal(ast::StringLiteral {
                        value,
                        span: part_span,
                    }));
                }
                Kind::InterpStringEnd => {
                    self.eat(Kind::InterpStringEnd)?;
                    break;
                }
                Kind::PuncOpenBrace => {
                    self.eat(Kind::PuncOpenBrace)?;
                    let expr = self.parse_expression()?;
                    parts.push(ast::InterpPart::Expr(Box::new(expr)));
                    self.eat(Kind::PuncCloseBrace)?;
                }
                Kind::Identifier => {
                    let ident = self.eat_identifier("Expected identifier")?;
                    parts.push(ast::InterpPart::Expr(Box::new(
                        ast::Expression::Identifier(ident),
                    )));
                }
                _ => {
                    return Err(format!(
                        "Unexpected token in interpolated string: {}",
                        self.kind()
                    ));
                }
            }
        }

        Ok(ast::Expression::InterpolatedString(
            ast::InterpolatedString {
                parts,
                span: self.span_from(span),
            },
        ))
    }

    fn parse_number_expression(&mut self) -> PResult<ast::Expression> {
        let span = self.current_span();
        Ok(ast::Expression::NumberLiteral(ast::NumberLiteral {
            value: self.eat_token_literal(Kind::LiteralNumber, "Expected number")?,
            span,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scanner::new_scanner;

    fn parse(src: &str) -> ParseResult {
        let mut s = new_scanner(src.to_string());
        let mut p = new_parser(&mut s);
        p.parse_program()
    }

    fn assert_no_errors(src: &str) -> ParseResult {
        let result = parse(src);
        let errors: Vec<_> = result
            .diagnostics
            .iter()
            .filter(|d| d.severity == diagnostic::Severity::Error)
            .collect();
        assert!(
            errors.is_empty(),
            "expected no parse errors for:\n{}\ngot: {:#?}",
            src,
            errors
        );
        result
    }

    fn assert_has_error(src: &str, snippet: &str) {
        let result = parse(src);
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.severity == diagnostic::Severity::Error && d.message.contains(snippet)),
            "expected error containing '{}' for:\n{}\ngot: {:#?}",
            snippet,
            src,
            result.diagnostics
        );
    }

    fn assert_single_error(src: &str, snippet: &str) {
        let result = parse(src);
        let errors: Vec<_> = result
            .diagnostics
            .iter()
            .filter(|d| d.severity == diagnostic::Severity::Error)
            .collect();
        assert_eq!(
            errors.len(),
            1,
            "expected exactly one error for:\n{src}\ngot: {errors:#?}"
        );
        assert!(
            errors[0].message.contains(snippet),
            "expected sole error to contain '{snippet}', got: {:?}",
            errors[0].message
        );
    }

    #[test]
    fn test_hello_al() {
        let src = include_str!("../../../../examples/hello.al");
        let result = assert_no_errors(src);
        assert!(!result.ast.body.is_empty());
    }

    #[test]
    fn test_fizzbuzz_al() {
        let src = include_str!("../../../../examples/fizzbuzz.al");
        assert_no_errors(src);
    }

    #[test]
    fn test_factorial_al() {
        let src = include_str!("../../../../examples/factorial.al");
        assert_no_errors(src);
    }

    #[test]
    fn test_shapes_al() {
        let src = include_str!("../../../../examples/shapes.al");
        assert_no_errors(src);
    }

    #[test]
    fn test_fibonacci_al() {
        let src = include_str!("../../../../examples/fibonacci.al");
        assert_no_errors(src);
    }

    #[test]
    fn test_all_language_features_al() {
        let src = include_str!("../../../../crates/al/tests/programs/all_language_features.al");
        assert_no_errors(src);
    }

    #[test]
    fn test_trying_out_tuples_al() {
        let src = include_str!("../../../../crates/al/tests/programs/trying_out_tuples.al");
        assert_no_errors(src);
    }

    #[test]
    fn test_generic_structs_and_enums_al() {
        let src = include_str!(
            "../../../../crates/al/tests/programs/trying_out_generic_structs_and_enums.al"
        );
        assert_no_errors(src);
    }

    #[test]
    fn test_match_patterns_al() {
        let src = include_str!("../../../../crates/al/tests/programs/match_patterns_test.al");
        assert_no_errors(src);
    }

    #[test]
    fn test_literals() {
        assert_no_errors("123");
        assert_no_errors("'hello'");
        assert_no_errors("true");
        assert_no_errors("false");
    }

    #[test]
    fn test_binary_precedence() {
        let result = parse("1 + 2 * 3");
        assert!(result.diagnostics.is_empty());
        // Should be 1 + (2 * 3): top level is + with right being *
        if let ast::Node::Expression(ast::Expression::BinaryExpression(b)) = &result.ast.body[0] {
            assert_eq!(b.op, ast::BinaryOp::Add);
            assert!(matches!(*b.right, ast::Expression::BinaryExpression(_)));
        } else {
            panic!("expected binary expression");
        }
    }

    #[test]
    fn test_range_precedence() {
        // -5..5 should be RangeExpression(Unary(-5), 5)
        let result = parse("-5..5");
        assert!(result.diagnostics.is_empty());
        if let ast::Node::Expression(ast::Expression::RangeExpression(r)) = &result.ast.body[0] {
            assert!(matches!(*r.start, ast::Expression::UnaryExpression(_)));
            assert!(matches!(*r.end, ast::Expression::NumberLiteral(_)));
        } else {
            panic!("expected range expression, got {:#?}", result.ast.body[0]);
        }
        // a+b..c+d should be (a+b)..(c+d)
        let result = parse("1+2..3+4");
        if let ast::Node::Expression(ast::Expression::RangeExpression(r)) = &result.ast.body[0] {
            assert!(matches!(*r.start, ast::Expression::BinaryExpression(_)));
            assert!(matches!(*r.end, ast::Expression::BinaryExpression(_)));
        } else {
            panic!("expected range expression");
        }
    }

    #[test]
    fn test_variable_binding() {
        assert_no_errors("x = 5");
        assert_no_errors("x Int = 5");
        assert_no_errors("const PI = 3.14");
    }

    #[test]
    fn test_typed_discard() {
        let r = parse("Nil = println('x')");
        assert!(r.diagnostics.is_empty(), "{:#?}", r.diagnostics);
        let ast::Node::Statement(s) = &r.ast.body[0] else {
            panic!("expected statement, got {:#?}", r.ast.body[0])
        };
        let ast::Statement::TypedDiscard(td) = s.as_ref() else {
            panic!("expected TypedDiscard, got {:#?}", s)
        };
        assert_eq!(td.ty_name.name, "Nil");

        assert_no_errors("Int = 5");
        assert_no_errors("String = 'hi'");
    }

    #[test]
    fn test_ctor_destructuring() {
        let r = parse("Some(x) = Some(1)");
        assert!(r.diagnostics.is_empty(), "{:#?}", r.diagnostics);
        let ast::Node::Statement(s) = &r.ast.body[0] else {
            panic!("expected statement, got {:#?}", r.ast.body[0])
        };
        let ast::Statement::CtorDestructuringBinding(cd) = s.as_ref() else {
            panic!("expected CtorDestructuringBinding, got {:#?}", s)
        };
        assert_eq!(cd.name.name, "Some");
        assert_eq!(cd.args.len(), 1);

        assert_no_errors("Point(x, y) = origin");
        assert_no_errors("Wrapper(a, b, c) = make()");
    }

    #[test]
    fn test_uppercase_ident_still_expression() {
        // No `=` ahead → constructor expression, not a statement.
        let r = parse("Some(1)");
        assert!(r.diagnostics.is_empty(), "{:#?}", r.diagnostics);
        assert!(matches!(r.ast.body[0], ast::Node::Expression(_)));

        // `==` is comparison, not binding.
        let r = parse("Nil == x");
        assert!(r.diagnostics.is_empty(), "{:#?}", r.diagnostics);
        assert!(matches!(r.ast.body[0], ast::Node::Expression(_)));
    }

    #[test]
    fn test_function_decl() {
        assert_no_errors("fn add(a Int, b Int) Int { a + b }");
        assert_no_errors("fn noop() { Nil }");
        assert_no_errors("fn id(x a) a { x }");
        assert_no_errors("fn pair() (Int, Int) { (1, 2) }");
    }

    #[test]
    fn test_type_decl() {
        assert_no_errors("type Point { Point(x Int y Int) }");
        assert_no_errors("type Box(t) { Box(value t) }");
        assert_no_errors("type Color { Red\n Green\n Blue }");
        assert_no_errors("type Option(t) { Some(value t)\n None }");
        assert_no_errors("type IntList = Array(Int)");
    }

    #[test]
    fn test_type_decl_errors() {
        assert_has_error("type Foo {}", "at least one constructor");
        assert_has_error("type foo { Foo }", "uppercase");
        assert_has_error("type Foo { Foo(Int) }", "must be labeled");
    }

    #[test]
    fn test_type_decl_shorthand() {
        assert_no_errors("type User { name String age Int }");
        assert_no_errors("type Box(A) { value A }");
        assert_no_errors("type User {\n  name String\n  age Int\n}");
        assert_has_error("type Foo { bar }", "Expected type");
    }

    #[test]
    fn test_if_expression() {
        assert_no_errors("if x > 0 { 1 } else { 2 }");
        assert_has_error("if x > 0 { 1 }", "requires an 'else'");
    }

    #[test]
    fn test_match_expression() {
        assert_no_errors("match x { 1 -> 'one'\n 2 -> 'two'\n _ -> 'other' }");
        assert_no_errors("match x { 1 | 2 -> 'low'\n _ -> 'high' }");
        assert_no_errors("match x { Some(v) -> v\n None -> 0 }");
        assert_no_errors("match p { Point(x: a, y: b) -> a + b }");
        assert_no_errors("match p { Point(x: a, ..) -> a }");
        assert_no_errors("match xs { [a, b, ..rest] -> a\n [] -> 0 }");
        assert_no_errors("match t { (a, b) -> a + b }");
        assert_no_errors("match n { 1..5 -> 'low'\n _ -> 'hi' }");
    }

    #[test]
    fn test_array_and_tuple() {
        assert_no_errors("[1, 2, 3]");
        assert_no_errors("(1, 2, 3)");
        assert_no_errors("[..xs, 1, ..ys]");
        assert_has_error("[..xs, 1, ..]", "Expected expression after `..`");
        assert_has_error("()", "tuples need 2+ elements");
        assert_has_error("(1)", "single-element parens");
    }

    #[test]
    fn test_tuple_destructuring() {
        assert_no_errors("(a, b) = (1, 2)");
    }

    #[test]
    fn test_postfix() {
        assert_no_errors("a.b.c");
        assert_no_errors("x = arr[0]");
        assert_no_errors("f(1, 2)");
        assert_no_errors("a.f(1).g");
        assert_no_errors("get_fn()(1, 2)");
    }

    #[test]
    fn test_call_args() {
        assert_no_errors("f(1, 2, 3)");
        assert_no_errors("f(a: 1, b: 2)");
        assert_no_errors("f(..xs)");
        assert_no_errors("Some(1)");
        assert_no_errors("Point(x: 1, y: 2)");
    }

    #[test]
    fn test_import_pub() {
        assert_no_errors("import al/json");
        assert_no_errors("import al/json as j");
        assert_no_errors("import al/json.{a, B, c as d}");
        assert_no_errors("import ./helper");
        assert_no_errors("import ../shared/auth");
        assert_no_errors("pub fn f() { 1 }");
        assert_no_errors("pub type S { S(a Int) }");
        assert_no_errors("pub const x = 1");

        let result = parse("fn f() { 1 }\nimport al/json");
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.message.contains("precede"))
        );
    }

    #[test]
    fn test_error_recovery() {
        let result = parse("fn ??? bad");
        assert!(!result.diagnostics.is_empty());
    }

    #[test]
    fn test_block_body_diagnostics() {
        // `if`/`else` branches and function bodies must be `{ ... }` blocks.
        assert_has_error("x = if 1 < 2 5 else 6", "'if' branch must be a block");
        assert_has_error("x = if 1 < 2 { 5 } else 6", "'else' branch must be a block");
        assert_has_error("fn f() Int = 1", "Function body must be a block");
        // The well-formed shapes still parse cleanly.
        assert_no_errors("x = if 1 < 2 { 5 } else { 6 }");
        assert_no_errors("fn f() Int { 1 }");
    }

    #[test]
    fn test_tuple_arity_and_pattern_diagnostics() {
        // A parenthesized single type/pattern is not a tuple.
        assert_has_error("fn f(x (Int)) Int { 1 }", "tuple types need 2+ elements");
        assert_has_error(
            "match (1, 2) { (x) -> x }",
            "tuple patterns need 2+ elements",
        );
        // A binary operator cannot begin a pattern.
        assert_has_error("match 1 { + -> 0 }", "Unexpected '+' in pattern");
        // Recovery from a bad pattern/guard/arrow that synchronizes to the
        // arm's `->` must consume it and parse the body, not loop forever
        // re-erroring on the same token.
        assert_single_error("match x { net.V6(ip) -> 1 }", "Expected '->', got '.'");
        assert_single_error("match x { a if + -> 1 }", "Unexpected '+'");
        assert_single_error("match x { -> 1 }", "Unexpected '->' in pattern");
        // Constructor fields are space/newline separated, never comma separated.
        assert_has_error(
            "type P { P(a Int, b Int) }",
            "separated by spaces or newlines, not commas",
        );
        // The two-element forms remain valid.
        assert_no_errors("fn f(x (Int, Int)) Int { 1 }");
        assert_no_errors("match (1, 2) { (a, b) -> a }");
    }

    #[test]
    fn test_declaration_guard_diagnostics() {
        // `@vm` ops are bodyless intrinsics.
        assert_has_error(
            "@vm(add)\nfn f(a Int) Int { a }",
            "@vm functions cannot have a body",
        );
        // `opaque` requires a body and only modifies `type`.
        assert_has_error(
            "pub opaque type T",
            "`opaque` type must have constructors to hide",
        );
        assert_has_error(
            "pub opaque type T = Int",
            "`opaque` cannot be applied to a type alias",
        );
        assert_has_error(
            "pub opaque fn f() Nil { Nil }",
            "`opaque` may only be applied to `type`",
        );
        // Attributes are not allowed on `const`.
        assert_has_error(
            "@vm(x)\nconst PI = 3",
            "Attributes are not allowed on `const`",
        );
        // `pub` must be followed by a declaration keyword.
        assert_has_error("pub x = 1", "after `pub`");
        // A relative import segment must be followed by `/`.
        assert_has_error("import .foo", "Expected `/` after relative import segment");
    }

    #[test]
    fn test_decrement_operator_rejected() {
        assert_has_error("y = 5\nz = y--", "Decrement operator");
    }

    #[test]
    fn test_is_type_name() {
        assert!(is_type_name("Int"));
        assert!(is_type_name("String"));
        assert!(!is_type_name("foo"));
        assert!(!is_type_name(""));
        assert!(!is_type_name("_Foo"));
    }

    #[test]
    fn test_deep_nesting_does_not_overflow() {
        // Each of these would drive unbounded native recursion and abort the
        // process (SIGABRT, exit 134) before the recursion guard existed —
        // this test itself would take down the whole test binary. With the
        // guard they become an ordinary "too deep" parse error.
        let n = 5000;

        assert_has_error(&format!("x = {}true", "!".repeat(n)), "too deep");
        assert_has_error(&format!("{}1{}", "(".repeat(n), ")".repeat(n)), "too deep");
        assert_has_error(&format!("{}1{}", "[".repeat(n), "]".repeat(n)), "too deep");
        assert_has_error(
            &format!("type T = {}Int{}", "(Int, ".repeat(n), ")".repeat(n)),
            "too deep",
        );
        assert_has_error(
            &format!(
                "match x {{ {}y{} -> 1, else -> 2 }}",
                "S(".repeat(n),
                ")".repeat(n)
            ),
            "too deep",
        );
    }

    #[test]
    fn test_reasonable_nesting_ok() {
        // Realistic nesting must never trip the limit. The worst multiplier
        // is the array/paren path (~3 guard hits per source level via
        // parse_expression + parse_unary + parse_primary), so depth 30 ≈ 90,
        // well under MAX_PARSE_DEPTH (128).
        let n = 30;

        assert_no_errors(&format!("x = {}true", "!".repeat(n)));
        assert_no_errors(&format!("x = {}1{}", "[".repeat(n), "]".repeat(n)));
        assert_no_errors(&format!(
            "type T = {}Int{}",
            "(Int, ".repeat(n),
            ")".repeat(n)
        ));
    }

    #[test]
    fn test_binary_literal_expr() {
        assert_no_errors("x = <<>>");
        assert_no_errors("x = <<1, 2, 3>>");
        assert_no_errors("x = <<x:4>>");
        assert_no_errors("x = <<1:4, 2:4>>");
        assert_no_errors("x = <<body:bytes(len)>>");
        assert_no_errors("x = <<n:size(w)>>");
        assert_no_errors("x = <<rest:binary>>");
        assert_no_errors("x = <<s:utf8>>");
        assert_no_errors("x = <<a + b, head:binary, 0>>");
    }

    #[test]
    fn test_binary_literal_ast_shape() {
        let result = parse("x = <<1, n:4, b:bytes(len), r:binary, s:utf8>>");
        assert!(result.diagnostics.is_empty(), "{:#?}", result.diagnostics);
        let ast::Node::Statement(stmt) = &result.ast.body[0] else {
            panic!("expected statement")
        };
        let ast::Statement::VariableBinding(vb) = stmt.as_ref() else {
            panic!("expected variable binding")
        };
        let ast::Expression::BinaryLiteral(bl) = &vb.init else {
            panic!("expected BinaryLiteral, got {:?}", vb.init)
        };
        assert_eq!(bl.segments.len(), 5);
        assert_eq!(bl.segments[0].kind, ast::BinKind::Int);
        assert!(bl.segments[0].size.is_none());
        assert_eq!(bl.segments[1].kind, ast::BinKind::Int);
        assert_eq!(bl.segments[1].unit, ast::BinUnit::Bits);
        assert!(bl.segments[1].size.is_some());
        assert_eq!(bl.segments[2].kind, ast::BinKind::Binary);
        assert_eq!(bl.segments[2].unit, ast::BinUnit::Bytes);
        assert!(bl.segments[2].size.is_some());
        assert_eq!(bl.segments[3].kind, ast::BinKind::Binary);
        assert!(bl.segments[3].size.is_none());
        assert_eq!(bl.segments[4].kind, ast::BinKind::Utf8);
    }

    #[test]
    fn test_binary_literal_bad_spec() {
        assert_has_error("x = <<1:foo>>", "segment size spec");
        assert_has_error("x = <<1:>>", "segment size spec");
    }

    #[test]
    fn test_binary_pattern() {
        assert_no_errors("match b { <<a, b>> -> a + b\n else -> 0 }");
        assert_no_errors("match b { <<x:4, y:4>> -> x\n else -> 0 }");
        assert_no_errors("match b { <<_:8, body:bytes(n), rest:binary>> -> body\n else -> b }");
        assert_no_errors("match b { <<1, 2, ..rest>> -> rest\n else -> b }");
        assert_no_errors("match b { <<1, 2, ..>> -> 0\n else -> 0 }");
        assert_no_errors("match b { <<>> -> 0\n else -> 1 }");
    }

    #[test]
    fn test_binary_pattern_rest_must_be_last() {
        assert_has_error("match b { <<..r, 1>> -> r\n else -> b }", "last segment");
    }
}
