use crate::ast;
use crate::diagnostic::{self, Diagnostic};
use crate::scanner::Scanner;
use crate::span::Span;
use crate::token::{Kind, Token, TriviaKind};

type PResult<T> = Result<T, String>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseContext {
    TopLevel,
    Block,
    FunctionParams,
    Array,
    StructInit,
    StructDef,
    EnumDef,
    MatchArms,
}

pub struct ParseResult {
    pub ast: ast::BlockExpression,
    pub diagnostics: Vec<Diagnostic>,
}

pub struct Parser {
    tokens: Vec<Token>,
    index: usize,
    current_token: Token,
    diagnostics: Vec<Diagnostic>,
    context_stack: Vec<ParseContext>,
    prev_token_end_line: i32,
    prev_token_end_column: i32,
    sync_iterations: i32,
}

pub fn new_parser(s: &mut Scanner) -> Parser {
    let tokens = s.scan_all();
    let diags = s.get_diagnostics();
    new_parser_from_tokens(tokens, diags)
}

pub fn new_parser_from_tokens(tokens: Vec<Token>, scanner_diagnostics: Vec<Diagnostic>) -> Parser {
    let current_token = tokens[0].clone();
    Parser {
        tokens,
        index: 0,
        current_token,
        diagnostics: scanner_diagnostics,
        context_stack: vec![ParseContext::TopLevel],
        prev_token_end_line: 0,
        prev_token_end_column: 0,
        sync_iterations: 0,
    }
}

fn is_type_name(name: &str) -> bool {
    name.as_bytes()
        .first()
        .is_some_and(|b| b.is_ascii_uppercase())
}

// Checks whether a token begins a type identifier. `lookahead` is the token
// AFTER tok — used to distinguish `[]T` (array type) from `[expr]` (array lit).
// When lookahead is None, `[` is treated leniently as a possible type start —
// callers in contexts where array literals are impossible (binding annotations)
// rely on this.
fn is_type_start_token(tok: &Token, lookahead: Option<&Token>) -> bool {
    match tok.kind {
        Kind::PuncQuestionMark => true,
        Kind::PuncOpenBracket => {
            if let Some(next) = lookahead {
                next.kind == Kind::PuncCloseBracket
            } else {
                true
            }
        }
        Kind::Identifier => {
            if let Some(name) = &tok.literal {
                is_type_name(name)
            } else {
                false
            }
        }
        _ => false,
    }
}

impl Parser {
    fn push_context(&mut self, ctx: ParseContext) {
        self.context_stack.push(ctx);
    }

    fn pop_context(&mut self) {
        if self.context_stack.len() > 1 {
            self.context_stack.pop();
        }
    }

    fn current_context(&self) -> ParseContext {
        *self.context_stack.last().unwrap_or(&ParseContext::TopLevel)
    }

    fn add_error(&mut self, message: String) {
        self.diagnostics.push(diagnostic::error_at(
            self.current_token.line,
            self.current_token.column,
            message,
        ));
    }

    fn add_warning(&mut self, message: String) {
        self.diagnostics.push(diagnostic::warning_at(
            self.current_token.line,
            self.current_token.column,
            message,
        ));
    }

    fn current_span(&self) -> Span {
        Span {
            start_line: self.current_token.line,
            start_column: self.current_token.column,
            end_line: self.current_token.line,
            end_column: self.current_token.column + self.current_token.length,
        }
    }

    fn span_from(&self, start: Span) -> Span {
        Span {
            start_line: start.start_line,
            start_column: start.start_column,
            end_line: self.prev_token_end_line,
            end_column: self.prev_token_end_column,
        }
    }

    fn save_token_end(&mut self) {
        self.prev_token_end_line = self.current_token.line;
        self.prev_token_end_column = self.current_token.column + self.current_token.length;
    }

    fn synchronize(&mut self) {
        let ctx = self.current_context();

        while self.current_token.kind != Kind::Eof {
            self.sync_iterations += 1;
            if self.sync_iterations > 1000 {
                self.add_error(
                    "Parser recovery failed: too many recovery attempts. This is a bug in the parser."
                        .to_string(),
                );
                while self.current_token.kind != Kind::Eof {
                    self.advance();
                }
                return;
            }
            match ctx {
                ParseContext::TopLevel => {
                    if matches!(
                        self.current_token.kind,
                        Kind::KwFunction
                            | Kind::KwStruct
                            | Kind::KwEnum
                            | Kind::KwConst
                            | Kind::KwFrom
                            | Kind::KwExport
                            | Kind::Identifier
                    ) {
                        return;
                    }
                }
                ParseContext::Block => {
                    if self.current_token.kind == Kind::PuncCloseBrace {
                        self.advance();
                        self.pop_context();
                        return;
                    }
                    if matches!(
                        self.current_token.kind,
                        Kind::KwIf | Kind::KwMatch | Kind::KwFunction | Kind::Identifier
                    ) {
                        return;
                    }
                }
                ParseContext::FunctionParams => {
                    if self.current_token.kind == Kind::PuncCloseParen {
                        self.advance();
                        self.pop_context();
                        return;
                    }
                    if self.current_token.kind == Kind::PuncOpenBrace {
                        return;
                    }
                    if self.current_token.kind == Kind::PuncComma {
                        self.advance();
                        return;
                    }
                }
                ParseContext::Array => {
                    if self.current_token.kind == Kind::PuncCloseBracket {
                        self.advance();
                        self.pop_context();
                        return;
                    }
                    if self.current_token.kind == Kind::PuncComma {
                        self.advance();
                        return;
                    }
                }
                ParseContext::StructInit | ParseContext::StructDef => {
                    if self.current_token.kind == Kind::PuncCloseBrace {
                        self.advance();
                        self.pop_context();
                        return;
                    }
                    if self.current_token.kind == Kind::PuncComma {
                        self.advance();
                        return;
                    }
                }
                ParseContext::EnumDef => {
                    if self.current_token.kind == Kind::PuncCloseBrace {
                        self.advance();
                        self.pop_context();
                        return;
                    }
                    if self.current_token.kind == Kind::PuncComma {
                        self.advance();
                        return;
                    }
                }
                ParseContext::MatchArms => {
                    if self.current_token.kind == Kind::PuncCloseBrace {
                        self.advance();
                        self.pop_context();
                        return;
                    }
                    if self.current_token.kind == Kind::PuncArrow {
                        return;
                    }
                    if self.current_token.kind == Kind::PuncComma {
                        self.advance();
                        return;
                    }
                }
            }

            self.advance();
        }
    }

    fn advance(&mut self) {
        if self.index + 1 < self.tokens.len() {
            self.save_token_end();
            self.index += 1;
            self.current_token = self.tokens[self.index].clone();
        }
    }

    fn eat(&mut self, kind: Kind) -> PResult<Token> {
        if self.current_token.kind == kind {
            let old = self.current_token.clone();
            self.save_token_end();

            self.index += 1;
            self.current_token = self.tokens[self.index].clone();

            return Ok(old);
        }

        Err(format!("Expected '{}', got '{}'", kind, self.current_token))
    }

    fn eat_msg(&mut self, kind: Kind, message: &str) -> PResult<Token> {
        match self.eat(kind) {
            Ok(t) => Ok(t),
            Err(_) => Err(format!("{}, got '{}'", message, self.current_token)),
        }
    }

    fn eat_token_literal(&mut self, kind: Kind, message: &str) -> PResult<String> {
        let eaten = self.eat_msg(kind, message)?;

        if let Some(unwrapped) = eaten.literal {
            return Ok(unwrapped);
        }

        Err(format!("Expected {}", message))
    }

    fn peek_next(&self) -> Option<Token> {
        if self.index + 1 < self.tokens.len() {
            return Some(self.tokens[self.index + 1].clone());
        }
        None
    }

    fn extract_doc_comment(&self) -> Option<String> {
        for trivia in &self.current_token.leading_trivia {
            if trivia.kind == TriviaKind::DocComment {
                return Some(trivia.text.clone());
            }
        }
        None
    }

    fn is_type_start(&self) -> bool {
        let next = self.peek_next();
        is_type_start_token(&self.current_token, next.as_ref())
    }

    pub fn parse_program(&mut self) -> ParseResult {
        let program_span = self.current_span();
        let mut body: Vec<ast::Node> = Vec::new();

        while self.current_token.kind != Kind::Eof {
            let span = self.current_span();
            match self.parse_node() {
                Ok(node) => body.push(node),
                Err(err) => {
                    self.add_error(err.clone());
                    self.synchronize();

                    let err_expr =
                        ast::Expression::ErrorNode(ast::ErrorNode { message: err, span });
                    body.push(ast::Node::Expression(err_expr));
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
        match self.current_token.kind {
            Kind::KwConst => {
                return Ok(ast::Node::Statement(Box::new(self.parse_const_binding()?)));
            }
            Kind::KwFunction => {
                return self.parse_function();
            }
            Kind::KwStruct => {
                return Ok(ast::Node::Statement(Box::new(
                    self.parse_struct_declaration()?,
                )));
            }
            Kind::KwEnum => {
                return Ok(ast::Node::Statement(Box::new(
                    self.parse_enum_declaration()?,
                )));
            }
            Kind::KwFrom => {
                return Ok(ast::Node::Statement(Box::new(
                    self.parse_import_declaration()?,
                )));
            }
            Kind::KwExport => {
                return Ok(ast::Node::Statement(Box::new(
                    self.parse_export_declaration()?,
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
                if let Some(next) = self.peek_next() {
                    if next.kind == Kind::PuncEquals {
                        return Ok(ast::Node::Statement(Box::new(self.parse_binding()?)));
                    }
                    // Lenient on `[` (no 2-ahead lookahead) — parse_binding handles
                    // the misparse gracefully if it's an array literal.
                    if is_type_start_token(&next, None) {
                        return Ok(ast::Node::Statement(Box::new(self.parse_binding()?)));
                    }
                }
            }
            _ => {}
        }
        Ok(ast::Node::Expression(self.parse_expression()?))
    }

    fn parse_expression(&mut self) -> PResult<ast::Expression> {
        self.parse_or_expression()
    }

    fn parse_or_expression(&mut self) -> PResult<ast::Expression> {
        let left = self.parse_binary_expression()?;

        if self.current_token.kind == Kind::KwOr {
            self.eat(Kind::KwOr)?;

            let mut receiver: Option<ast::Identifier> = None;

            if self.current_token.kind == Kind::Identifier
                && let Some(next) = self.peek_next()
                && next.kind == Kind::PuncArrow
            {
                let span = self.current_span();
                let name = self
                    .eat_token_literal(Kind::Identifier, "Expected identifier for or receiver")?;
                receiver = Some(ast::Identifier { name, span });
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

    fn parse_binary_expression(&mut self) -> PResult<ast::Expression> {
        self.parse_logical_or()
    }

    fn parse_logical_or(&mut self) -> PResult<ast::Expression> {
        let mut left = self.parse_logical_and()?;

        while self.current_token.kind == Kind::LogicalOr {
            self.eat(Kind::LogicalOr)?;
            let right = self.parse_logical_and()?;
            let span = self.span_from(left.span());
            left = ast::Expression::BinaryExpression(ast::BinaryExpression {
                left: Box::new(left),
                right: Box::new(right),
                op: ast::Operator {
                    kind: Kind::LogicalOr,
                },
                span,
            });
        }

        Ok(left)
    }

    fn parse_logical_and(&mut self) -> PResult<ast::Expression> {
        let mut left = self.parse_equality()?;

        while self.current_token.kind == Kind::LogicalAnd {
            self.eat(Kind::LogicalAnd)?;
            let right = self.parse_equality()?;
            let span = self.span_from(left.span());
            left = ast::Expression::BinaryExpression(ast::BinaryExpression {
                left: Box::new(left),
                right: Box::new(right),
                op: ast::Operator {
                    kind: Kind::LogicalAnd,
                },
                span,
            });
        }

        Ok(left)
    }

    fn parse_equality(&mut self) -> PResult<ast::Expression> {
        let mut left = self.parse_comparison()?;

        while matches!(
            self.current_token.kind,
            Kind::PuncEqualsComparator | Kind::PuncNotEqual
        ) {
            let operator = self.current_token.kind;
            self.eat(operator)?;
            let right = self.parse_comparison()?;
            let span = self.span_from(left.span());
            left = ast::Expression::BinaryExpression(ast::BinaryExpression {
                left: Box::new(left),
                right: Box::new(right),
                op: ast::Operator { kind: operator },
                span,
            });
        }

        Ok(left)
    }

    fn parse_comparison(&mut self) -> PResult<ast::Expression> {
        let mut left = self.parse_additive()?;

        while matches!(
            self.current_token.kind,
            Kind::PuncLt | Kind::PuncGt | Kind::PuncLte | Kind::PuncGte
        ) {
            let operator = self.current_token.kind;
            self.eat(operator)?;
            let right = self.parse_additive()?;
            let span = self.span_from(left.span());
            left = ast::Expression::BinaryExpression(ast::BinaryExpression {
                left: Box::new(left),
                right: Box::new(right),
                op: ast::Operator { kind: operator },
                span,
            });
        }

        Ok(left)
    }

    fn parse_additive(&mut self) -> PResult<ast::Expression> {
        let mut left = self.parse_multiplicative()?;

        while matches!(self.current_token.kind, Kind::PuncPlus | Kind::PuncMinus) {
            let operator = self.current_token.kind;
            self.eat(operator)?;
            let right = self.parse_multiplicative()?;
            let span = self.span_from(left.span());
            left = ast::Expression::BinaryExpression(ast::BinaryExpression {
                left: Box::new(left),
                right: Box::new(right),
                op: ast::Operator { kind: operator },
                span,
            });
        }

        Ok(left)
    }

    fn parse_multiplicative(&mut self) -> PResult<ast::Expression> {
        let mut left = self.parse_unary_expression()?;

        while matches!(
            self.current_token.kind,
            Kind::PuncMul | Kind::PuncDiv | Kind::PuncMod
        ) {
            let operator = self.current_token.kind;
            self.eat(operator)?;
            let right = self.parse_unary_expression()?;
            let span = self.span_from(left.span());
            left = ast::Expression::BinaryExpression(ast::BinaryExpression {
                left: Box::new(left),
                right: Box::new(right),
                op: ast::Operator { kind: operator },
                span,
            });
        }

        Ok(left)
    }

    fn parse_unary_expression(&mut self) -> PResult<ast::Expression> {
        if self.current_token.kind == Kind::PuncExclamationMark {
            let start = self.current_span();
            self.eat(Kind::PuncExclamationMark)?;
            let inner = self.parse_unary_expression()?;

            return Ok(ast::Expression::UnaryExpression(ast::UnaryExpression {
                expression: Box::new(inner),
                op: ast::Operator {
                    kind: Kind::PuncExclamationMark,
                },
                span: self.span_from(start),
            }));
        }

        if self.current_token.kind == Kind::PuncMinus {
            let start = self.current_span();
            self.eat(Kind::PuncMinus)?;
            let inner = self.parse_unary_expression()?;

            return Ok(ast::Expression::UnaryExpression(ast::UnaryExpression {
                expression: Box::new(inner),
                op: ast::Operator {
                    kind: Kind::PuncMinus,
                },
                span: self.span_from(start),
            }));
        }

        self.parse_postfix_expression()
    }

    fn parse_postfix_expression(&mut self) -> PResult<ast::Expression> {
        let mut expr = self.parse_primary_expression()?;

        loop {
            match self.current_token.kind {
                Kind::PuncDot => {
                    expr = self.parse_dot_expression(expr)?;
                }
                Kind::PuncOpenBracket => {
                    // only treat as array index if there's no whitespace before the bracket
                    // this allows `arr[0]` but not `arr [0]` or `x = 1\n[1, 2]`
                    if !self.current_token.leading_trivia.is_empty() {
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
                Kind::PuncDotdot => {
                    let start = expr.span();
                    self.eat(Kind::PuncDotdot)?;
                    let end = self.parse_expression()?;
                    let span = self.span_from(start);
                    expr = ast::Expression::RangeExpression(ast::RangeExpression {
                        start: Box::new(expr),
                        end: Box::new(end),
                        span,
                    });
                }
                Kind::PuncPlusplus => {
                    return Err(
                        "Increment operator (++) is not supported. Values are immutable in AL - use `x = x + 1` with shadowing instead."
                            .to_string(),
                    );
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

    fn parse_primary_expression(&mut self) -> PResult<ast::Expression> {
        let expr = match self.current_token.kind {
            Kind::LiteralString => self.parse_string_expression()?,
            Kind::InterpStringStart => self.parse_interpolated_string()?,
            Kind::LiteralNumber => self.parse_number_expression()?,
            Kind::Identifier => self.parse_identifier_or_call()?,
            Kind::PuncOpenParen => self.parse_tuple()?,
            Kind::KwNone => {
                let span = self.current_span();
                self.eat(Kind::KwNone)?;
                ast::Expression::NoneExpression(ast::NoneExpression { span })
            }
            Kind::KwTrue => {
                let span = self.current_span();
                self.eat(Kind::KwTrue)?;
                ast::Expression::BooleanLiteral(ast::BooleanLiteral { value: true, span })
            }
            Kind::KwFalse => {
                let span = self.current_span();
                self.eat(Kind::KwFalse)?;
                ast::Expression::BooleanLiteral(ast::BooleanLiteral { value: false, span })
            }
            Kind::PuncOpenBrace => self.parse_block_expression()?,
            Kind::PuncOpenBracket => self.parse_array_expression()?,
            Kind::KwIf => self.parse_if_expression()?,
            Kind::KwMatch => self.parse_match_expression()?,
            Kind::KwFunction => self.parse_function_expression()?,
            Kind::KwError => self.parse_error_expression()?,
            Kind::Error => {
                let span = self.current_span();
                self.advance();
                ast::Expression::ErrorNode(ast::ErrorNode {
                    message: "Scanner error".to_string(),
                    span,
                })
            }
            _ => {
                return Err(format!("Unexpected '{}'", self.current_token));
            }
        };

        Ok(expr)
    }

    fn parse_identifier_or_call(&mut self) -> PResult<ast::Expression> {
        let span = self.current_span();
        let name = self.eat_token_literal(Kind::Identifier, "Expected identifier")?;

        if self.current_token.kind == Kind::PuncOpenParen {
            // Could be function call or struct init with type args like Box(Int){ ... }
            // Save position and try to parse as type args first
            let saved_index = self.index;
            let saved_token = self.current_token.clone();

            if let Some(type_args) = self.try_parse_type_args() {
                // If next token is {, it's struct init with type args
                if self.current_token.kind == Kind::PuncOpenBrace {
                    return self.parse_struct_init_with_type_args(name, span, type_args);
                }
            }

            // Restore position and parse as function call
            self.index = saved_index;
            self.current_token = saved_token;
            return self.parse_function_call_expression(name, span);
        }

        if self.current_token.kind == Kind::PuncOpenBrace {
            let curr_index = self.index;
            let curr_token = self.current_token.clone();

            if let Ok(result) = self.parse_struct_init_expression(name.clone(), span) {
                return Ok(result);
            }

            self.index = curr_index;
            self.current_token = curr_token;
        }

        Ok(ast::Expression::Identifier(ast::Identifier { name, span }))
    }

    fn parse_block_expression(&mut self) -> PResult<ast::Expression> {
        let block_span = self.current_span();
        self.eat(Kind::PuncOpenBrace)?;
        self.push_context(ParseContext::Block);

        let mut body: Vec<ast::Node> = Vec::new();

        while self.current_token.kind != Kind::PuncCloseBrace
            && self.current_token.kind != Kind::Eof
        {
            let span = self.current_span();
            match self.parse_node() {
                Ok(node) => body.push(node),
                Err(err) => {
                    self.add_error(err.clone());
                    self.synchronize();
                    let err_expr =
                        ast::Expression::ErrorNode(ast::ErrorNode { message: err, span });
                    body.push(ast::Node::Expression(err_expr));
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

        while self.current_token.kind != Kind::PuncCloseBracket
            && self.current_token.kind != Kind::Eof
        {
            let elem_span = self.current_span();

            if self.current_token.kind == Kind::PuncDotdot {
                let spread_span = self.current_span();
                self.eat(Kind::PuncDotdot)?;

                // anonymous spread (.. followed by ] or , or else)
                if matches!(
                    self.current_token.kind,
                    Kind::PuncCloseBracket | Kind::PuncComma | Kind::KwElse
                ) {
                    if self.current_token.kind == Kind::KwElse {
                        self.advance(); // skip the erroneous 'else' token
                    }
                    elements.push(ast::ArrayElement::SpreadElement(ast::SpreadElement {
                        expression: None,
                        span: spread_span,
                    }));
                } else {
                    match self.parse_expression() {
                        Ok(inner) => {
                            elements.push(ast::ArrayElement::SpreadElement(ast::SpreadElement {
                                expression: Some(inner),
                                span: self.span_from(spread_span),
                            }));
                        }
                        Err(err) => {
                            self.add_error(err.clone());
                            self.synchronize();
                            if self.current_context() != ParseContext::Array {
                                // synchronize consumed the ] and popped context
                                return Ok(ast::Expression::ArrayExpression(
                                    ast::ArrayExpression {
                                        elements,
                                        span: self.span_from(span),
                                    },
                                ));
                            }
                            elements.push(ast::ArrayElement::Expression(
                                ast::Expression::ErrorNode(ast::ErrorNode {
                                    message: err,
                                    span: elem_span,
                                }),
                            ));
                            continue;
                        }
                    }
                }
            } else {
                match self.parse_expression() {
                    Ok(expr) => {
                        elements.push(ast::ArrayElement::Expression(expr));
                    }
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
            }

            if self.current_token.kind == Kind::PuncComma {
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

    fn parse_tuple(&mut self) -> PResult<ast::Expression> {
        let start = self.current_span();
        self.eat(Kind::PuncOpenParen)?;

        if self.current_token.kind == Kind::PuncCloseParen {
            return Err("Empty tuple () is not allowed. Use `none` instead.".to_string());
        }

        let mut elements: Vec<ast::Expression> = Vec::new();
        elements.push(self.parse_expression()?);

        while self.current_token.kind == Kind::PuncComma {
            self.eat(Kind::PuncComma)?;
            if self.current_token.kind == Kind::PuncCloseParen {
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
        let span = self.current_span();
        self.eat(Kind::KwIf)?;

        let condition = self.parse_expression()?;
        let body = self.parse_expression()?;

        let mut else_body: Option<Box<ast::Expression>> = None;

        if self.current_token.kind == Kind::KwElse {
            self.eat(Kind::KwElse)?;
            else_body = Some(Box::new(self.parse_expression()?));
        }

        Ok(ast::Expression::IfExpression(ast::IfExpression {
            condition: Box::new(condition),
            body: Box::new(body),
            span: self.span_from(span),
            else_body,
        }))
    }

    fn parse_match_expression(&mut self) -> PResult<ast::Expression> {
        let match_span = self.current_span();
        self.eat(Kind::KwMatch)?;

        let subject = self.parse_expression()?;

        self.eat(Kind::PuncOpenBrace)?;
        self.push_context(ParseContext::MatchArms);

        let mut arms: Vec<ast::MatchArm> = Vec::new();

        'arms: while self.current_token.kind != Kind::PuncCloseBrace
            && self.current_token.kind != Kind::Eof
        {
            let first_span = self.current_span();
            let first_pattern = if self.current_token.kind == Kind::KwElse {
                let span = self.current_span();
                self.eat(Kind::KwElse)?;
                ast::Expression::WildcardPattern(ast::WildcardPattern { span })
            } else {
                match self.parse_expression() {
                    Ok(e) => e,
                    Err(err) => {
                        self.add_error(err);
                        self.synchronize();
                        if self.current_token.kind == Kind::PuncCloseBrace {
                            break;
                        }
                        continue;
                    }
                }
            };

            let pattern = if self.current_token.kind == Kind::BitwiseOr {
                let mut patterns = vec![first_pattern];
                while self.current_token.kind == Kind::BitwiseOr {
                    self.eat(Kind::BitwiseOr)?;
                    match self.parse_expression() {
                        Ok(next_pattern) => patterns.push(next_pattern),
                        Err(err) => {
                            self.add_error(err);
                            self.synchronize();
                            break;
                        }
                    }
                }
                ast::Expression::OrPattern(ast::OrPattern {
                    patterns,
                    span: self.span_from(first_span),
                })
            } else {
                first_pattern
            };

            if let Err(err) = self.eat(Kind::PuncArrow) {
                self.add_error(err);
                self.synchronize();
                if self.current_token.kind == Kind::PuncCloseBrace {
                    break;
                }
                continue;
            }

            let body_span = self.current_span();
            let body = match self.parse_expression() {
                Ok(e) => e,
                Err(err) => {
                    self.add_error(err.clone());
                    self.synchronize();
                    if self.current_token.kind == Kind::PuncCloseBrace {
                        break 'arms;
                    }
                    ast::Expression::ErrorNode(ast::ErrorNode {
                        message: err,
                        span: body_span,
                    })
                }
            };

            arms.push(ast::MatchArm { pattern, body });

            if self.current_token.kind == Kind::PuncComma {
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

    fn parse_function(&mut self) -> PResult<ast::Node> {
        if let Some(next) = self.peek_next()
            && next.kind == Kind::Identifier
        {
            return Ok(ast::Node::Statement(Box::new(
                self.parse_function_declaration()?,
            )));
        }
        Ok(ast::Node::Expression(self.parse_function_expression()?))
    }

    fn parse_function_declaration(&mut self) -> PResult<ast::Statement> {
        let doc = self.extract_doc_comment();

        let fn_span = self.current_span();
        self.eat(Kind::KwFunction)?;

        let id_span = self.current_span();
        let name = self.eat_token_literal(Kind::Identifier, "Expected function name")?;

        let params = self.parse_parameters()?;
        let (return_type, error_type) = self.parse_function_return_types()?;
        let body = self.parse_function_body()?;

        Ok(ast::Statement::FunctionDeclaration(
            ast::FunctionDeclaration {
                doc,
                identifier: ast::Identifier {
                    name,
                    span: id_span,
                },
                params,
                return_type,
                error_type,
                body,
                span: self.span_from(fn_span),
            },
        ))
    }

    fn parse_function_expression(&mut self) -> PResult<ast::Expression> {
        let fn_span = self.current_span();
        self.eat(Kind::KwFunction)?;

        let params = self.parse_parameters()?;
        let (return_type, error_type) = self.parse_function_return_types()?;
        let body = self.parse_function_body()?;

        Ok(ast::Expression::FunctionExpression(
            ast::FunctionExpression {
                params,
                return_type,
                error_type,
                body: Box::new(body),
                span: self.span_from(fn_span),
            },
        ))
    }

    fn parse_function_body(&mut self) -> PResult<ast::Expression> {
        self.parse_expression()
    }

    fn parse_function_return_types(
        &mut self,
    ) -> PResult<(Option<ast::TypeIdentifier>, Option<ast::TypeIdentifier>)> {
        let mut return_type: Option<ast::TypeIdentifier> = None;
        let mut error_type: Option<ast::TypeIdentifier> = None;

        if self.current_token.kind == Kind::PuncQuestionMark {
            let span = self.current_span();
            self.eat(Kind::PuncQuestionMark)?;
            if self.current_token.kind == Kind::PuncOpenBrace {
                // Bare `?` before `{` — optional None (e.g. `fn f() ? { ... }`)
                let inner = ast::TypeIdentifier {
                    kind: ast::TypeKind::NamedType(ast::NamedType {
                        identifier: ast::Identifier {
                            name: "None".to_string(),
                            span,
                        },
                        type_args: Vec::new(),
                    }),
                    span,
                };
                return_type = Some(ast::TypeIdentifier {
                    kind: ast::TypeKind::OptionType(ast::OptionType {
                        inner: Box::new(inner),
                    }),
                    span,
                });
            } else {
                let inner = self.parse_type_identifier()?;
                return_type = Some(ast::TypeIdentifier {
                    kind: ast::TypeKind::OptionType(ast::OptionType {
                        inner: Box::new(inner),
                    }),
                    span: self.span_from(span),
                });
            }
        } else if self.is_type_start() {
            return_type = Some(self.parse_type_identifier()?);
        }

        if self.current_token.kind == Kind::PuncExclamationMark {
            // !Type is an error type; !expr is unary-not body. Disambiguate by
            // peeking past the ! for a type start.
            if let Some(next) = self.peek_next()
                && is_type_start_token(&next, None)
            {
                self.eat(Kind::PuncExclamationMark)?;
                error_type = Some(self.parse_type_identifier()?);
            }
        }

        Ok((return_type, error_type))
    }

    fn parse_parameters(&mut self) -> PResult<Vec<ast::FunctionParameter>> {
        self.eat(Kind::PuncOpenParen)?;
        self.push_context(ParseContext::FunctionParams);

        let mut params: Vec<ast::FunctionParameter> = Vec::new();

        while self.current_token.kind != Kind::PuncCloseParen
            && self.current_token.kind != Kind::Eof
        {
            let param = self.parse_parameter()?;
            params.push(param);

            if self.current_token.kind == Kind::PuncComma {
                self.eat(Kind::PuncComma)?;
            }
        }

        self.pop_context();
        self.eat(Kind::PuncCloseParen)?;

        Ok(params)
    }

    fn parse_parameter(&mut self) -> PResult<ast::FunctionParameter> {
        let span = self.current_span();
        let name = self.eat_token_literal(Kind::Identifier, "Expected parameter name")?;

        let mut typ: Option<ast::TypeIdentifier> = None;

        if self.current_token.kind == Kind::Identifier
            || self.current_token.kind == Kind::PuncOpenBracket
            || self.current_token.kind == Kind::PuncQuestionMark
            || self.current_token.kind == Kind::KwFunction
        {
            typ = Some(self.parse_type_identifier()?);
        }

        Ok(ast::FunctionParameter {
            typ,
            identifier: ast::Identifier { name, span },
        })
    }

    fn parse_type_identifier(&mut self) -> PResult<ast::TypeIdentifier> {
        if self.current_token.kind == Kind::PuncQuestionMark {
            let span = self.current_span();
            self.eat(Kind::PuncQuestionMark)?;
            let inner = self.parse_type_identifier()?;
            return Ok(ast::TypeIdentifier {
                kind: ast::TypeKind::OptionType(ast::OptionType {
                    inner: Box::new(inner),
                }),
                span: self.span_from(span),
            });
        }

        if self.current_token.kind == Kind::PuncOpenBracket {
            let span = self.current_span();
            self.eat(Kind::PuncOpenBracket)?;
            self.eat(Kind::PuncCloseBracket)?;
            let inner = self.parse_type_identifier()?;
            return Ok(ast::TypeIdentifier {
                kind: ast::TypeKind::ArrayType(ast::ArrayType {
                    element: Box::new(inner),
                }),
                span: self.span_from(span),
            });
        }

        if self.current_token.kind == Kind::KwFunction {
            return self.parse_function_type();
        }

        let span = self.current_span();
        let name = self.eat_token_literal(Kind::Identifier, "Expected type name")?;

        let mut type_args: Vec<ast::TypeIdentifier> = Vec::new();
        if self.current_token.kind == Kind::PuncOpenParen {
            self.eat(Kind::PuncOpenParen)?;
            while self.current_token.kind != Kind::PuncCloseParen
                && self.current_token.kind != Kind::Eof
            {
                let arg = self.parse_type_identifier()?;
                type_args.push(arg);
                if self.current_token.kind == Kind::PuncComma {
                    self.eat(Kind::PuncComma)?;
                }
            }
            self.eat(Kind::PuncCloseParen)?;
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
        self.eat(Kind::KwFunction)?;
        self.eat(Kind::PuncOpenParen)?;

        let mut param_types: Vec<ast::TypeIdentifier> = Vec::new();
        while self.current_token.kind != Kind::PuncCloseParen
            && self.current_token.kind != Kind::Eof
        {
            let param_type = self.parse_type_identifier()?;
            param_types.push(param_type);
            if self.current_token.kind == Kind::PuncComma {
                self.eat(Kind::PuncComma)?;
            }
        }
        self.eat(Kind::PuncCloseParen)?;

        let mut return_type: Option<Box<ast::TypeIdentifier>> = None;
        let mut error_type: Option<Box<ast::TypeIdentifier>> = None;

        if self.current_token.kind == Kind::Identifier
            || self.current_token.kind == Kind::PuncOpenBracket
            || self.current_token.kind == Kind::PuncQuestionMark
            || self.current_token.kind == Kind::KwFunction
        {
            let ret = self.parse_type_identifier()?;
            return_type = Some(Box::new(ret));

            if self.current_token.kind == Kind::PuncExclamationMark {
                self.eat(Kind::PuncExclamationMark)?;
                let err = self.parse_type_identifier()?;
                error_type = Some(Box::new(err));
            }
        }

        Ok(ast::TypeIdentifier {
            kind: ast::TypeKind::FunctionType(ast::FunctionType {
                params: param_types,
                return_type,
                error_type,
            }),
            span: self.span_from(span),
        })
    }

    fn parse_type_params(&mut self) -> PResult<Vec<ast::Identifier>> {
        let mut params: Vec<ast::Identifier> = Vec::new();

        if self.current_token.kind != Kind::PuncOpenParen {
            return Ok(params);
        }

        self.eat(Kind::PuncOpenParen)?;

        while self.current_token.kind != Kind::PuncCloseParen
            && self.current_token.kind != Kind::Eof
        {
            let param_span = self.current_span();
            let param_name =
                self.eat_token_literal(Kind::Identifier, "Expected type parameter name")?;

            params.push(ast::Identifier {
                name: param_name,
                span: param_span,
            });

            if self.current_token.kind == Kind::PuncComma {
                self.eat(Kind::PuncComma)?;
            }
        }

        self.eat(Kind::PuncCloseParen)?;

        Ok(params)
    }

    fn parse_struct_declaration(&mut self) -> PResult<ast::Statement> {
        let doc = self.extract_doc_comment();

        let struct_span = self.current_span();
        self.eat(Kind::KwStruct)?;

        let id_span = self.current_span();
        let name = self.eat_token_literal(Kind::Identifier, "Expected struct name")?;

        let type_params = self.parse_type_params()?;

        self.eat(Kind::PuncOpenBrace)?;
        self.push_context(ParseContext::StructDef);

        let mut fields: Vec<ast::StructField> = Vec::new();

        while self.current_token.kind != Kind::PuncCloseBrace
            && self.current_token.kind != Kind::Eof
        {
            let field = self.parse_struct_field()?;
            fields.push(field);
        }

        self.pop_context();
        self.eat(Kind::PuncCloseBrace)?;

        Ok(ast::Statement::StructDeclaration(ast::StructDeclaration {
            doc,
            identifier: ast::Identifier {
                name,
                span: id_span,
            },
            type_params,
            fields,
            span: self.span_from(struct_span),
        }))
    }

    fn parse_struct_field(&mut self) -> PResult<ast::StructField> {
        let doc = self.extract_doc_comment();

        let span = self.current_span();
        let name = self.eat_token_literal(Kind::Identifier, "Expected field name")?;

        let typ = self.parse_type_identifier()?;

        let mut init: Option<ast::Expression> = None;

        if self.current_token.kind == Kind::PuncEquals {
            self.eat(Kind::PuncEquals)?;
            init = Some(self.parse_expression()?);
        }

        if self.current_token.kind == Kind::PuncComma {
            self.add_warning("Trailing comma after struct field is deprecated".to_string());
            self.eat(Kind::PuncComma)?;
        }

        Ok(ast::StructField {
            doc,
            identifier: ast::Identifier { name, span },
            typ,
            init,
        })
    }

    fn parse_enum_declaration(&mut self) -> PResult<ast::Statement> {
        let doc = self.extract_doc_comment();

        let enum_span = self.current_span();
        self.eat(Kind::KwEnum)?;

        let id_span = self.current_span();
        let name = self.eat_token_literal(Kind::Identifier, "Expected enum name")?;

        let type_params = self.parse_type_params()?;

        self.eat(Kind::PuncOpenBrace)?;
        self.push_context(ParseContext::EnumDef);

        let mut variants: Vec<ast::EnumVariant> = Vec::new();

        while self.current_token.kind != Kind::PuncCloseBrace
            && self.current_token.kind != Kind::Eof
        {
            let variant = self.parse_enum_variant()?;
            variants.push(variant);
        }

        self.pop_context();
        self.eat(Kind::PuncCloseBrace)?;

        Ok(ast::Statement::EnumDeclaration(ast::EnumDeclaration {
            doc,
            identifier: ast::Identifier {
                name,
                span: id_span,
            },
            type_params,
            variants,
            span: self.span_from(enum_span),
        }))
    }

    fn parse_enum_variant(&mut self) -> PResult<ast::EnumVariant> {
        let doc = self.extract_doc_comment();

        let span = self.current_span();
        let name = self.eat_token_literal(Kind::Identifier, "Expected variant name")?;

        let mut payload: Vec<ast::TypeIdentifier> = Vec::new();

        if self.current_token.kind == Kind::PuncOpenParen {
            self.eat(Kind::PuncOpenParen)?;
            while self.current_token.kind != Kind::PuncCloseParen {
                payload.push(self.parse_type_identifier()?);
                if self.current_token.kind == Kind::PuncComma {
                    self.eat(Kind::PuncComma)?;
                } else {
                    break;
                }
            }
            self.eat(Kind::PuncCloseParen)?;
        }

        if self.current_token.kind == Kind::PuncComma {
            self.add_warning("Trailing comma after enum variant is deprecated".to_string());
            self.eat(Kind::PuncComma)?;
        }

        Ok(ast::EnumVariant {
            doc,
            identifier: ast::Identifier { name, span },
            payload,
        })
    }

    fn parse_struct_init_expression(
        &mut self,
        name: String,
        name_span: Span,
    ) -> PResult<ast::Expression> {
        self.eat(Kind::PuncOpenBrace)?;
        self.push_context(ParseContext::StructInit);

        let mut fields: Vec<ast::StructInitField> = Vec::new();

        while self.current_token.kind != Kind::PuncCloseBrace
            && self.current_token.kind != Kind::Eof
        {
            let field_span = self.current_span();
            let field_name = self.eat_token_literal(Kind::Identifier, "Expected field name")?;
            self.eat(Kind::PuncColon)?;
            let value = self.parse_expression()?;

            fields.push(ast::StructInitField {
                identifier: ast::Identifier {
                    name: field_name,
                    span: field_span,
                },
                init: value,
            });

            if self.current_token.kind == Kind::PuncComma {
                self.eat(Kind::PuncComma)?;
            }
        }

        self.pop_context();
        self.eat(Kind::PuncCloseBrace)?;

        Ok(ast::Expression::StructInitExpression(
            ast::StructInitExpression {
                identifier: ast::Identifier {
                    name,
                    span: name_span,
                },
                type_args: Vec::new(),
                fields,
                span: self.span_from(name_span),
            },
        ))
    }

    // Try to parse type args like (Int, String). Returns None if not valid type args.
    fn try_parse_type_args(&mut self) -> Option<Vec<ast::TypeIdentifier>> {
        if self.current_token.kind != Kind::PuncOpenParen {
            return None;
        }
        self.eat(Kind::PuncOpenParen).ok()?;

        let mut type_args: Vec<ast::TypeIdentifier> = Vec::new();
        while self.current_token.kind != Kind::PuncCloseParen
            && self.current_token.kind != Kind::Eof
        {
            // Type args should be type identifiers (uppercase or type syntax)
            if !self.is_type_start() {
                return None;
            }
            let type_arg = self.parse_type_identifier().ok()?;
            type_args.push(type_arg);
            if self.current_token.kind == Kind::PuncComma {
                self.eat(Kind::PuncComma).ok()?;
            }
        }
        self.eat(Kind::PuncCloseParen).ok()?;
        Some(type_args)
    }

    fn parse_struct_init_with_type_args(
        &mut self,
        name: String,
        name_span: Span,
        type_args: Vec<ast::TypeIdentifier>,
    ) -> PResult<ast::Expression> {
        self.eat(Kind::PuncOpenBrace)?;
        self.push_context(ParseContext::StructInit);

        let mut fields: Vec<ast::StructInitField> = Vec::new();

        while self.current_token.kind != Kind::PuncCloseBrace
            && self.current_token.kind != Kind::Eof
        {
            let field_span = self.current_span();
            let field_name = self.eat_token_literal(Kind::Identifier, "Expected field name")?;
            self.eat(Kind::PuncColon)?;
            let value = self.parse_expression()?;

            fields.push(ast::StructInitField {
                identifier: ast::Identifier {
                    name: field_name,
                    span: field_span,
                },
                init: value,
            });

            if self.current_token.kind == Kind::PuncComma {
                self.eat(Kind::PuncComma)?;
            }
        }

        self.pop_context();
        self.eat(Kind::PuncCloseBrace)?;

        Ok(ast::Expression::StructInitExpression(
            ast::StructInitExpression {
                identifier: ast::Identifier {
                    name,
                    span: name_span,
                },
                type_args,
                fields,
                span: self.span_from(name_span),
            },
        ))
    }

    fn parse_const_binding(&mut self) -> PResult<ast::Statement> {
        let doc = self.extract_doc_comment();
        let span = self.current_span();
        self.eat(Kind::KwConst)?;

        let name_span = self.current_span();
        let name = self.eat_token_literal(Kind::Identifier, "Expected const name")?;

        let mut typ: Option<ast::TypeIdentifier> = None;
        if self.is_type_start() {
            typ = Some(self.parse_type_identifier()?);
        }

        self.eat(Kind::PuncEquals)?;

        let init = self.parse_expression()?;

        Ok(ast::Statement::ConstBinding(ast::ConstBinding {
            doc,
            identifier: ast::Identifier {
                name,
                span: name_span,
            },
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
        self.eat(Kind::PuncOpenParen)?;

        let mut patterns: Vec<ast::Expression> = Vec::new();

        while self.current_token.kind != Kind::PuncCloseParen
            && self.current_token.kind != Kind::Eof
        {
            let pattern_span = self.current_span();

            if self.current_token.kind == Kind::Identifier {
                let name = self.eat_token_literal(Kind::Identifier, "Expected identifier")?;

                if is_type_name(&name) {
                    patterns.push(ast::Expression::TypeIdentifier(ast::TypeIdentifier {
                        kind: ast::TypeKind::NamedType(ast::NamedType {
                            identifier: ast::Identifier {
                                name,
                                span: pattern_span,
                            },
                            type_args: Vec::new(),
                        }),
                        span: pattern_span,
                    }));
                } else {
                    patterns.push(ast::Expression::Identifier(ast::Identifier {
                        name,
                        span: pattern_span,
                    }));
                }
            } else {
                return Err("Expected identifier in tuple destructuring pattern".to_string());
            }

            if self.current_token.kind == Kind::PuncComma {
                self.eat(Kind::PuncComma)?;
            } else {
                break;
            }
        }

        self.eat(Kind::PuncCloseParen)?;
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

        // Uppercase name followed by = is a type pattern binding
        if is_type_name(&name) && typ.is_none() {
            return Ok(ast::Statement::TypePatternBinding(
                ast::TypePatternBinding {
                    typ: ast::TypeIdentifier {
                        kind: ast::TypeKind::NamedType(ast::NamedType {
                            identifier: ast::Identifier { name, span },
                            type_args: Vec::new(),
                        }),
                        span,
                    },
                    init,
                    span: self.span_from(span),
                },
            ));
        }

        Ok(ast::Statement::VariableBinding(ast::VariableBinding {
            doc,
            identifier: ast::Identifier { name, span },
            typ,
            init,
            span: self.span_from(span),
        }))
    }

    fn parse_export_declaration(&mut self) -> PResult<ast::Statement> {
        let span = self.current_span();
        self.eat(Kind::KwExport)?;

        // Export must be followed by a statement (function, struct, enum, const, or binding)
        let decl = match self.current_token.kind {
            Kind::KwFunction => self.parse_function_declaration()?,
            Kind::KwStruct => self.parse_struct_declaration()?,
            Kind::KwEnum => self.parse_enum_declaration()?,
            Kind::KwConst => self.parse_const_binding()?,
            Kind::Identifier => self.parse_binding()?,
            _ => return Err("Expected declaration after export".to_string()),
        };

        Ok(ast::Statement::ExportDeclaration(ast::ExportDeclaration {
            declaration: Box::new(decl),
            span: self.span_from(span),
        }))
    }

    fn parse_import_declaration(&mut self) -> PResult<ast::Statement> {
        let import_span = self.current_span();
        self.eat(Kind::KwFrom)?;

        let path_token = self.eat(Kind::LiteralString)?;
        let path = path_token
            .literal
            .ok_or_else(|| "Expected string literal for import path".to_string())?;

        self.eat(Kind::KwImport)?;

        let mut specifiers: Vec<ast::ImportSpecifier> = Vec::new();
        self.parse_import_specifiers(&mut specifiers)?;

        Ok(ast::Statement::ImportDeclaration(ast::ImportDeclaration {
            path,
            specifiers,
            span: self.span_from(import_span),
        }))
    }

    fn parse_import_specifiers(
        &mut self,
        specifiers: &mut Vec<ast::ImportSpecifier>,
    ) -> PResult<()> {
        let span = self.current_span();
        let name = self.eat_token_literal(Kind::Identifier, "Expected import specifier")?;

        specifiers.push(ast::ImportSpecifier {
            identifier: ast::Identifier { name, span },
        });

        if self.current_token.kind == Kind::PuncComma {
            self.eat(Kind::PuncComma)?;
            self.parse_import_specifiers(specifiers)?;
        }

        Ok(())
    }

    fn parse_error_expression(&mut self) -> PResult<ast::Expression> {
        let start = self.current_span();
        self.eat(Kind::KwError)?;

        let expr = self.parse_unary_expression()?;

        Ok(ast::Expression::ErrorExpression(ast::ErrorExpression {
            expression: Box::new(expr),
            span: self.span_from(start),
        }))
    }

    fn parse_dot_expression(&mut self, left: ast::Expression) -> PResult<ast::Expression> {
        let start = left.span();
        self.eat(Kind::PuncDot)?;

        let span = self.current_span();

        if self.current_token.kind == Kind::LiteralNumber {
            let num_str = self.eat_token_literal(Kind::LiteralNumber, "Expected tuple index")?;

            if num_str.contains('.') {
                let parts: Vec<&str> = num_str.split('.').collect();
                let mut result =
                    ast::Expression::PropertyAccessExpression(ast::PropertyAccessExpression {
                        left: Box::new(left),
                        right: Box::new(ast::Expression::NumberLiteral(ast::NumberLiteral {
                            value: parts[0].to_string(),
                            span,
                        })),
                        span: self.span_from(start),
                    });
                for part in parts.iter().skip(1) {
                    result =
                        ast::Expression::PropertyAccessExpression(ast::PropertyAccessExpression {
                            left: Box::new(result),
                            right: Box::new(ast::Expression::NumberLiteral(ast::NumberLiteral {
                                value: part.to_string(),
                                span,
                            })),
                            span: self.span_from(start),
                        });
                }
                return Ok(result);
            }

            return Ok(ast::Expression::PropertyAccessExpression(
                ast::PropertyAccessExpression {
                    left: Box::new(left),
                    right: Box::new(ast::Expression::NumberLiteral(ast::NumberLiteral {
                        value: num_str,
                        span,
                    })),
                    span: self.span_from(start),
                },
            ));
        }

        let property = self.eat_token_literal(Kind::Identifier, "Expected property name")?;

        if self.current_token.kind == Kind::PuncOpenParen {
            let call = self.parse_function_call_expression(property, span)?;
            return Ok(ast::Expression::PropertyAccessExpression(
                ast::PropertyAccessExpression {
                    left: Box::new(left),
                    right: Box::new(call),
                    span: self.span_from(start),
                },
            ));
        }

        Ok(ast::Expression::PropertyAccessExpression(
            ast::PropertyAccessExpression {
                left: Box::new(left),
                right: Box::new(ast::Expression::Identifier(ast::Identifier {
                    name: property,
                    span,
                })),
                span: self.span_from(start),
            },
        ))
    }

    fn parse_function_call_expression(
        &mut self,
        name: String,
        name_span: Span,
    ) -> PResult<ast::Expression> {
        self.eat(Kind::PuncOpenParen)?;

        let mut arguments: Vec<ast::Expression> = Vec::new();

        while self.current_token.kind != Kind::PuncCloseParen {
            arguments.push(self.parse_expression()?);

            if self.current_token.kind == Kind::PuncComma {
                self.eat(Kind::PuncComma)?;
            }
        }

        self.eat(Kind::PuncCloseParen)?;

        Ok(ast::Expression::FunctionCallExpression(
            ast::FunctionCallExpression {
                identifier: ast::Identifier {
                    name,
                    span: name_span,
                },
                arguments,
                span: self.span_from(name_span),
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

        let mut parts: Vec<ast::Expression> = Vec::new();

        loop {
            match self.current_token.kind {
                Kind::InterpStringPart => {
                    let part_span = self.current_span();
                    let value = self.current_token.literal.clone().unwrap_or_default();
                    self.eat(Kind::InterpStringPart)?;
                    parts.push(ast::Expression::StringLiteral(ast::StringLiteral {
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
                    parts.push(expr);
                    self.eat(Kind::PuncCloseBrace)?;
                }
                Kind::Identifier => {
                    let ident_span = self.current_span();
                    let name = self.current_token.literal.clone().unwrap_or_default();
                    self.eat(Kind::Identifier)?;
                    parts.push(ast::Expression::Identifier(ast::Identifier {
                        name,
                        span: ident_span,
                    }));
                }
                _ => {
                    return Err(format!(
                        "Unexpected token in interpolated string: {}",
                        self.current_token.kind
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

    fn assert_no_errors(src: &str) {
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
    }

    #[test]
    fn test_hello_al() {
        let src = include_str!("../../examples/hello.al");
        let result = parse(src);
        let errors: Vec<_> = result
            .diagnostics
            .iter()
            .filter(|d| d.severity == diagnostic::Severity::Error)
            .collect();
        assert!(errors.is_empty(), "diagnostics: {:#?}", errors);
        assert!(!result.ast.body.is_empty());
    }

    #[test]
    fn test_fizzbuzz_al() {
        let src = include_str!("../../examples/fizzbuzz.al");
        let result = parse(src);
        let errors: Vec<_> = result
            .diagnostics
            .iter()
            .filter(|d| d.severity == diagnostic::Severity::Error)
            .collect();
        assert!(errors.is_empty(), "diagnostics: {:#?}", errors);
    }

    #[test]
    fn test_factorial_al() {
        let src = include_str!("../../examples/factorial.al");
        assert_no_errors(src);
    }

    #[test]
    fn test_shapes_al() {
        let src = include_str!("../../examples/shapes.al");
        assert_no_errors(src);
    }

    #[test]
    fn test_fibonacci_al() {
        let src = include_str!("../../examples/fibonacci.al");
        assert_no_errors(src);
    }

    #[test]
    fn test_all_language_features_al() {
        let src = include_str!("../../examples/all_language_features.al");
        assert_no_errors(src);
    }

    #[test]
    fn test_trying_out_tuples_al() {
        let src = include_str!("../../examples/trying_out_tuples.al");
        assert_no_errors(src);
    }

    #[test]
    fn test_generic_structs_and_enums_al() {
        let src = include_str!("../../examples/trying_out_generic_structs_and_enums.al");
        assert_no_errors(src);
    }

    #[test]
    fn test_match_patterns_al() {
        let src = include_str!("../../examples/match_patterns_test.al");
        assert_no_errors(src);
    }

    #[test]
    fn test_literals() {
        assert_no_errors("123");
        assert_no_errors("'hello'");
        assert_no_errors("true");
        assert_no_errors("false");
        assert_no_errors("none");
    }

    #[test]
    fn test_binary_precedence() {
        let result = parse("1 + 2 * 3");
        assert!(result.diagnostics.is_empty());
        // Should be 1 + (2 * 3): top level is + with right being *
        if let ast::Node::Expression(ast::Expression::BinaryExpression(b)) = &result.ast.body[0] {
            assert_eq!(b.op.kind, Kind::PuncPlus);
            assert!(matches!(*b.right, ast::Expression::BinaryExpression(_)));
        } else {
            panic!("expected binary expression");
        }
    }

    #[test]
    fn test_variable_binding() {
        assert_no_errors("x = 5");
        assert_no_errors("x Int = 5");
        assert_no_errors("const PI = 3.14");
    }

    #[test]
    fn test_function_decl() {
        assert_no_errors("fn add(a Int, b Int) Int { a + b }");
        assert_no_errors("fn noop() { none }");
    }

    #[test]
    fn test_struct_decl() {
        assert_no_errors("struct Point { x Int\n y Int }");
        assert_no_errors("struct Box(T) { value T }");
    }

    #[test]
    fn test_enum_decl() {
        assert_no_errors("enum Color { Red\n Green\n Blue }");
        assert_no_errors("enum Option(T) { Some(T)\n None }");
    }

    #[test]
    fn test_if_expression() {
        assert_no_errors("if x > 0 { 1 } else { 2 }");
    }

    #[test]
    fn test_match_expression() {
        assert_no_errors("match x { 1 -> 'one', 2 -> 'two', else -> 'other' }");
        assert_no_errors("match x { 1 | 2 -> 'low', else -> 'high' }");
    }

    #[test]
    fn test_array_and_tuple() {
        assert_no_errors("[1, 2, 3]");
        assert_no_errors("(1, 2, 3)");
        assert_no_errors("[..xs, 1, ..]");
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
    }

    #[test]
    fn test_import_export() {
        assert_no_errors("from 'foo' import bar, baz");
        assert_no_errors("export fn f() { 1 }");
    }

    #[test]
    fn test_error_recovery() {
        let result = parse("fn ??? bad");
        assert!(!result.diagnostics.is_empty());
    }

    #[test]
    fn test_is_type_name() {
        assert!(is_type_name("Int"));
        assert!(is_type_name("String"));
        assert!(!is_type_name("foo"));
        assert!(!is_type_name(""));
        assert!(!is_type_name("_Foo"));
    }
}
