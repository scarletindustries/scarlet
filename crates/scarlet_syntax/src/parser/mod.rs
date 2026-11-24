use crate::ast;
use crate::diagnostic::Diagnostic;
use crate::scanner::ScanError;
use crate::scanner::Scanner;
use crate::span::Span;
use crate::token::{self, Keyword, Kind, Token, Trivia, is_type_name};

mod error;

pub use error::{DeclarationLead, Expected, LineItem, ParseError, SyntaxError};

// The error is boxed so each frame of the recursive descent only carries a
// pointer. Without the box, `test_deep_nesting_does_not_overflow` overflows.
type PResult<T> = Result<T, Box<ParseError>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParseContext {
    TopLevel,
    Block,
    FunctionParams,
    Array,
    TypeDef,
    MatchArms,
}

/// Where `synchronize()` stopped. On `ConsumedCloser` the caller must pop its
/// own context and skip its trailing `eat(close)`.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SyncOutcome {
    AtItem,
    ConsumedCloser,
}

/// Where [`Parser::recover_in_arm`] left the parser. `ConsumedCloser` means it
/// ate the match's `}`, so the caller must stop and skip its own `eat`.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArmRecovery {
    ConsumedCloser,
    AtCloseBrace,
    AtArrow,
    Resume,
}

/// A parsed source file: its top-level body, its module doc comment, and
/// every diagnostic from the scanner and the parser.
pub struct ParseResult {
    pub ast: ast::BlockExpression,
    pub doc: Option<String>,
    pub diagnostics: Vec<Diagnostic<SyntaxError>>,
}

pub struct Parser {
    /// Every token before the end of input. The Eof token lives apart in
    /// `eof`, so the stream can never run out: any position at or past
    /// `tokens.len()` is the end of input.
    tokens: Vec<Token>,
    eof: Token,
    /// At most `tokens.len()`, which is the Eof token.
    index: usize,
    diagnostics: Vec<Diagnostic<SyntaxError>>,
    context_stack: Vec<ParseContext>,
    prev_token_end_line: i32,
    prev_token_end_column: i32,
    depth: u32,
    /// Token whose leading trivia gave up the module doc comment, so
    /// [`Parser::extract_doc_comment`] skips it instead of re-attaching it to
    /// the first declaration.
    module_doc_token: Option<usize>,
}

pub fn new_parser(s: &mut Scanner) -> Parser {
    let (tokens, diags) = s.scan_all();
    new_parser_from_tokens(tokens, diags)
}

/// Where `op` sits on the precedence ladder, loosest first. Two operators on
/// one level chain left to right: `a - b + c` is `(a - b) + c`.
pub(crate) fn precedence_level(op: ast::BinaryOp) -> usize {
    Parser::PRECEDENCE
        .iter()
        .position(|level| level.iter().any(|(_, o)| *o == op))
        .unwrap_or(Parser::PRECEDENCE.len())
}

pub(crate) fn new_parser_from_tokens(
    tokens: Vec<Token>,
    scanner_diagnostics: Vec<Diagnostic<ScanError>>,
) -> Parser {
    // The scanner always ends the stream with Eof. A hand-built stream that
    // forgot it gets one at the end of its last token.
    let mut tokens = token::insert_line_ends(tokens);
    let eof = match tokens.pop() {
        Some(last) if last.kind == Kind::Eof => last,
        last => {
            let span = last.as_ref().map_or(Span::DUMMY, |t| {
                Span::point(t.span.end_line, t.span.end_column)
            });
            tokens.extend(last);
            Token {
                kind: Kind::Eof,
                span,
                leading_trivia: Vec::new(),
            }
        }
    };
    Parser {
        tokens,
        eof,
        index: 0,
        diagnostics: scanner_diagnostics
            .into_iter()
            .map(|d| d.map(SyntaxError::Scan))
            .collect(),
        context_stack: vec![ParseContext::TopLevel],
        prev_token_end_line: 0,
        prev_token_end_column: 0,
        depth: 0,
        module_doc_token: None,
    }
}

// Whether a token starts a type annotation on a binding or a `const`. A
// lowercase name doesn't count, because a free type variable means nothing
// there. Parameter and return positions allow them and use
// `at_loose_type_start`.
fn is_type_start_token(tok: &Token) -> bool {
    match &tok.kind {
        Kind::Keyword(Keyword::Fn) => true,
        Kind::PuncOpenParen => true,
        Kind::Identifier(name) => is_type_name(name),
        _ => false,
    }
}

// Limits recursive-descent depth, so deeply nested input is a parse error
// rather than a native stack overflow. The parser is the only thing that
// builds an AST, so this also limits every AST walker that runs after it.
//
// 128 must stay small enough that the deepest native-frame path (~7 frames per
// depth unit through the precedence chain) cannot overflow the ~2 MiB libtest
// thread before the guard trips. Real programs peak around bracket depth 5.
const MAX_PARSE_DEPTH: u32 = 128;

impl Parser {
    // Runs `f` one level deeper. There's no `?` between setting the depth and
    // restoring it, so it stays balanced on the Err path and error recovery
    // resumes at the right depth.
    fn with_recursion_guard<T>(&mut self, f: impl FnOnce(&mut Self) -> PResult<T>) -> PResult<T> {
        let outer = self.depth;
        let Some(depth) = outer.checked_add(1).filter(|d| *d <= MAX_PARSE_DEPTH) else {
            return Err(Box::new(ParseError::NestingTooDeep));
        };
        self.depth = depth;
        let r = f(self);
        self.depth = outer;
        r
    }

    fn push_context(&mut self, ctx: ParseContext) {
        self.context_stack.push(ctx);
    }

    fn pop_context(&mut self) {
        debug_assert!(self.context_stack.len() > 1, "context stack underflow");
        if self.context_stack.len() > 1 {
            self.context_stack.pop();
        }
    }

    // Pops on both the Ok and Err paths, so a `?` inside `f` can't leave a
    // stale context behind to mislead a later `synchronize()`. Blocks, arrays
    // and matches recover in place, so they still push and pop directly: they
    // have to act on their own `SyncOutcome`.
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

    /// Reports a parse error at `span`. [`Parser::add_error`] reports one at
    /// the current token instead.
    fn error_at(&mut self, span: Span, error: ParseError) {
        self.diagnostics.push(Diagnostic::new(
            span,
            SyntaxError::Parse {
                error,
                at_end: false,
            },
        ));
    }

    fn add_error(&mut self, error: &ParseError) {
        // An error at EOF is an unexpected EOF by construction, so it says so
        // from the token rather than from the message text.
        let at_end = self.kind() == Kind::Eof;
        let sp = self.cur().span;
        self.diagnostics.push(Diagnostic::new(
            Span::point(sp.start_line, sp.start_column),
            SyntaxError::Parse {
                error: error.clone(),
                at_end,
            },
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

    /// The token at `i`. Every position at or past the end is the Eof token.
    fn token_at(&self, i: usize) -> &Token {
        self.tokens.get(i).unwrap_or(&self.eof)
    }

    /// The token `k` places after the current one, or `None` past the Eof token.
    fn token_ahead(&self, k: usize) -> Option<&Token> {
        self.tokens_from(self.index).nth(k)
    }

    /// The tokens from `i` on, through the Eof token.
    fn tokens_from(&self, i: usize) -> impl Iterator<Item = &Token> {
        self.tokens
            .get(i..)
            .unwrap_or_default()
            .iter()
            .chain(std::iter::once(&self.eof))
    }

    fn cur(&self) -> &Token {
        self.token_at(self.index)
    }

    fn kind(&self) -> Kind {
        self.cur().kind.clone()
    }

    fn save_token_end(&mut self) {
        let sp = self.cur().span;
        self.prev_token_end_line = sp.end_line;
        self.prev_token_end_column = sp.end_column;
    }

    /// Moves past the current token. The Eof token is never moved past.
    fn bump(&mut self) {
        if self.index < self.tokens.len() {
            self.save_token_end();
            self.index = self.index.saturating_add(1);
        }
    }

    // Skips tokens until a good place to resume in the current context. It
    // never touches the context stack, because the frame that pushed a context
    // always pops it. It always ends: each pass either returns or reaches the
    // `advance()` at the bottom, so it moves one token per pass and reaches Eof
    // within as many passes as there are tokens.
    fn synchronize(&mut self) -> SyncOutcome {
        let ctx = self.current_context();

        while self.kind() != Kind::Eof {
            // A line end in a list of lines closes the broken item, so the next
            // one starts right after it.
            if self.kind() == Kind::LineEnd
                && matches!(
                    ctx,
                    ParseContext::TopLevel
                        | ParseContext::Block
                        | ParseContext::TypeDef
                        | ParseContext::MatchArms
                )
            {
                self.advance();
                return SyncOutcome::AtItem;
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
                            | Kind::Identifier(_)
                    ) {
                        return SyncOutcome::AtItem;
                    }
                    None
                }
                ParseContext::Block => {
                    if self.kind() == Kind::PuncCloseBrace {
                        self.advance();
                        return SyncOutcome::ConsumedCloser;
                    }
                    if matches!(
                        self.kind(),
                        Kind::Keyword(Keyword::If)
                            | Kind::Keyword(Keyword::Match)
                            | Kind::Keyword(Keyword::Fn)
                            | Kind::Identifier(_)
                    ) {
                        return SyncOutcome::AtItem;
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
                    return SyncOutcome::ConsumedCloser;
                }
                if extra_stops.contains(&self.kind()) {
                    return SyncOutcome::AtItem;
                }
                if self.kind() == Kind::PuncComma {
                    self.advance();
                    return SyncOutcome::AtItem;
                }
            }

            self.advance();
        }
        SyncOutcome::AtItem
    }

    // Recovers from an error inside a `match` arm: reports it, synchronises,
    // and says where recovery stopped.
    fn recover_in_arm(&mut self, err: &ParseError) -> ArmRecovery {
        self.add_error(err);
        if self.synchronize() == SyncOutcome::ConsumedCloser {
            return ArmRecovery::ConsumedCloser;
        }
        match self.kind() {
            Kind::PuncCloseBrace => ArmRecovery::AtCloseBrace,
            Kind::PuncArrow => ArmRecovery::AtArrow,
            _ => ArmRecovery::Resume,
        }
    }

    // Recovers from an error in a top-level or block node loop: reports it,
    // synchronises, and builds an error node for the region it couldn't parse.
    fn recover_node(&mut self, err: Box<ParseError>, span: Span) -> (ast::Node, SyncOutcome) {
        self.add_error(&err);
        let outcome = self.synchronize();
        let node = ast::Node::Expression(ast::Expression::ErrorNode(ast::ErrorNode {
            error: *err,
            span,
        }));
        (node, outcome)
    }

    fn advance(&mut self) {
        self.bump();
    }

    fn eat(&mut self, kind: Kind) -> PResult<()> {
        if self.cur().kind == kind {
            self.bump();
            return Ok(());
        }

        Err(Box::new(ParseError::ExpectedToken {
            expected: kind,
            got: self.kind(),
        }))
    }

    // Consumes the current token and returns its text when `extract` matches
    // its kind.
    fn eat_payload(
        &mut self,
        what: Expected,
        extract: impl FnOnce(&Kind) -> Option<String>,
    ) -> PResult<String> {
        match extract(&self.cur().kind) {
            Some(text) => {
                self.bump();
                Ok(text)
            }
            None => Err(Box::new(ParseError::Expected {
                what,
                got: self.kind(),
            })),
        }
    }

    fn eat_name(&mut self, what: Expected) -> PResult<String> {
        self.eat_payload(what, |k| match k {
            Kind::Identifier(name) => Some(name.to_string()),
            _ => None,
        })
    }

    fn eat_number(&mut self, what: Expected) -> PResult<String> {
        self.eat_payload(what, |k| match k {
            Kind::LiteralNumber(text) => Some(text.to_string()),
            _ => None,
        })
    }

    fn eat_string(&mut self, what: Expected) -> PResult<String> {
        self.eat_payload(what, |k| match k {
            Kind::LiteralString(text) => Some(text.to_string()),
            _ => None,
        })
    }

    fn eat_interp_part(&mut self, what: Expected) -> PResult<String> {
        self.eat_payload(what, |k| match k {
            Kind::InterpStringPart(text) => Some(text.to_string()),
            _ => None,
        })
    }

    fn eat_identifier(&mut self, what: Expected) -> PResult<ast::Identifier> {
        let span = self.current_span();
        let name = self.eat_name(what)?;
        Ok(ast::Identifier { name, span })
    }

    fn peek_next(&self) -> Option<Kind> {
        self.token_ahead(1).map(|t| t.kind.clone())
    }

    /// The first `/** */` in the current token's leading trivia. If that token
    /// also donated the module doc, the declaration takes the next one, so a
    /// file opening with both keeps both.
    fn extract_doc_comment(&self) -> Option<String> {
        let taken = usize::from(self.module_doc_token == Some(self.index));
        self.cur()
            .leading_trivia
            .iter()
            .filter_map(|t| match t {
                Trivia::DocComment(text) => Some(text),
                _ => None,
            })
            .nth(taken)
            .cloned()
    }

    /// The module doc comment: a `/** */` block starting on line 0. Trivia
    /// carries no span, but the scanner emits a `Newline` per line break and
    /// drops only horizontal whitespace, so "first in the leading trivia" is
    /// exactly "starts on line 0".
    fn take_module_doc(&mut self) -> Option<String> {
        let doc = match self.cur().leading_trivia.first() {
            Some(Trivia::DocComment(text)) => text.clone(),
            _ => return None,
        };
        self.module_doc_token = Some(self.index);
        Some(doc)
    }

    // Parses `open`, then zero or more items separated by commas, then
    // `close`. A trailing comma is allowed.
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

    /// Parses the items in a type body, one per line. `noun` names them in
    /// the error.
    ///
    /// The line end is the separator, so source can't drift from what
    /// `scarlet fmt` writes. A comma between items is an error, because each
    /// item already shows where it ends.
    fn parse_line_separated_list<T>(
        &mut self,
        close: Kind,
        noun: LineItem,
        mut item: impl FnMut(&mut Self) -> PResult<T>,
    ) -> PResult<Vec<T>> {
        let mut items = Vec::new();
        while self.kind() != close && self.kind() != Kind::Eof {
            if self.kind() == Kind::PuncComma {
                return Err(Box::new(ParseError::CommaBetweenLines(noun)));
            }
            items.push(item(self)?);
            self.end_line_item(&close, noun)?;
        }
        Ok(items)
    }

    /// After an item of a `{ }` list or the file: a line end must come next,
    /// unless the list closes here.
    fn end_line_item(&mut self, close: &Kind, noun: LineItem) -> PResult<()> {
        match self.kind() {
            Kind::LineEnd => {
                self.eat(Kind::LineEnd)?;
                Ok(())
            }
            Kind::Eof => Ok(()),
            k if k == *close => Ok(()),
            _ => Err(Box::new(ParseError::SameLine(noun))),
        }
    }

    /// Constructor fields, separated by commas. Line breaks inside the
    /// brackets are only spacing, so they can't separate fields:
    ///
    /// ```text
    /// Normie(username String, age Int)
    /// Done(
    ///     method Binary,
    ///     target Binary,
    /// )
    /// ```
    fn parse_field_list<T>(
        &mut self,
        close: Kind,
        mut item: impl FnMut(&mut Self) -> PResult<T>,
    ) -> PResult<Vec<T>> {
        let mut items = Vec::new();
        while self.kind() != close && self.kind() != Kind::Eof {
            items.push(item(self)?);
            if self.kind() == Kind::PuncComma {
                self.eat(Kind::PuncComma)?;
                continue;
            }
            if self.kind() == close {
                break;
            }
            return Err(Box::new(ParseError::FieldsWithoutCommas));
        }
        Ok(items)
    }

    fn is_type_start(&self) -> bool {
        is_type_start_token(self.cur())
    }

    // Looks ahead for an `=` at depth 0 before a line end at depth 0. That
    // tells a binding (`x Type =`, `x =`) from an expression (`xs[0]`,
    // `Ctor(args)`). Guessing from whether a type starts here would misfire on
    // uppercase constructors and `(` tuple types.
    fn is_binding_ahead(&self) -> bool {
        let mut depth = 0usize;
        for tok in self.tokens_from(self.index).skip(1) {
            if depth == 0 && tok.kind == Kind::LineEnd {
                return false;
            }
            match tok.kind {
                Kind::PuncOpenParen | Kind::PuncOpenBracket | Kind::PuncOpenBrace => {
                    depth = depth.saturating_add(1);
                }
                // A closer with nothing open ends the enclosing block, and the
                // statement with it.
                Kind::PuncCloseParen | Kind::PuncCloseBracket | Kind::PuncCloseBrace => {
                    let Some(d) = depth.checked_sub(1) else {
                        return false;
                    };
                    depth = d;
                }
                Kind::PuncEquals if depth == 0 => return true,
                Kind::PuncEqualsComparator if depth == 0 => return false,
                Kind::Eof => return false,
                _ => {}
            }
        }
        false
    }

    // Looks ahead on this line for a `<-` outside any brackets, which makes
    // the statement a backpass: its binders are patterns, like the left of
    // `=`. An `=`, a block, or the end of the line first means it is
    // something else: a binding, a call, a comparison.
    fn is_backpass_ahead(&self) -> bool {
        let mut depth = 0usize;
        for tok in self.tokens_from(self.index) {
            match tok.kind {
                Kind::PuncOpenParen | Kind::PuncOpenBracket | Kind::BinOpen => {
                    depth = depth.saturating_add(1);
                }
                Kind::PuncCloseParen | Kind::PuncCloseBracket | Kind::BinClose => {
                    let Some(d) = depth.checked_sub(1) else {
                        return false;
                    };
                    depth = d;
                }
                Kind::PuncBackArrow if depth == 0 => return true,
                Kind::PuncEquals | Kind::PuncOpenBrace | Kind::LineEnd | Kind::Eof => return false,
                _ => {}
            }
        }
        false
    }

    // Takes the parser by value. Parsing moves the diagnostics out, so a
    // second run would silently report an empty program.
    pub fn parse_program(mut self) -> ParseResult {
        let program_span = self.current_span();
        let doc = self.take_module_doc();
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
                        self.error_at(node.span(), ParseError::ImportAfterDeclaration);
                    }
                    if !is_import {
                        seen_non_import = true;
                    }
                    body.push(node);
                    if let Err(err) = self.end_line_item(&Kind::Eof, LineItem::Declaration) {
                        self.add_error(&err);
                    }
                }
                Err(err) => {
                    // TopLevel has no closing delimiter, so the outcome is always AtItem.
                    let (node, _) = self.recover_node(err, span);
                    body.push(node);
                    continue;
                }
            }
        }

        let span = self.span_from(program_span);
        ParseResult {
            ast: ast::BlockExpression { body, span },
            doc,
            diagnostics: self.diagnostics,
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
            _ if self.is_backpass_ahead() => {
                return Ok(ast::Node::Statement(Box::new(self.parse_backpass()?)));
            }
            Kind::PuncOpenParen if self.is_tuple_destructuring() => {
                return Ok(ast::Node::Statement(Box::new(self.parse_binding()?)));
            }
            Kind::Identifier(_) if self.is_qualified_ctor_destructuring() => {
                return Ok(ast::Node::Statement(Box::new(self.parse_binding()?)));
            }
            // An uppercase name only starts a binding as `Ctor = e` or
            // `Ctor(..) = e`; anything else falls through to an expression.
            Kind::Identifier(name)
                if self.is_binding_ahead()
                    && (!is_type_name(&name)
                        || matches!(
                            self.peek_next(),
                            Some(Kind::PuncEquals | Kind::PuncOpenParen)
                        )) =>
            {
                return Ok(ast::Node::Statement(Box::new(self.parse_binding()?)));
            }
            _ => {}
        }
        Ok(ast::Node::Expression(self.parse_expression()?))
    }

    fn parse_expression(&mut self) -> PResult<ast::Expression> {
        self.with_recursion_guard(Self::parse_or_expression)
    }

    fn parse_or_expression(&mut self) -> PResult<ast::Expression> {
        let left = self.parse_pipe_expression()?;

        if self.kind() == Kind::Keyword(Keyword::Or) {
            self.eat(Kind::Keyword(Keyword::Or))?;

            let mut receiver: Option<ast::Identifier> = None;

            if matches!(self.kind(), Kind::Identifier(_))
                && self.peek_next() == Some(Kind::PuncArrow)
            {
                receiver = Some(self.eat_identifier(Expected::OrReceiver)?);
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

    // `left |> right` is sugar for `right(left, ...)`, and `crate::desugar`
    // rewrites it before type checking (`ast::PipeExpression` explains why).
    // It's left-associative and looser than every binary operator, so a whole
    // arithmetic, comparison or boolean expression is one pipeline stage. This
    // matches Gleam, where `|>` is the loosest operator.
    fn parse_pipe_expression(&mut self) -> PResult<ast::Expression> {
        let mut left = self.parse_binary_expression()?;

        while self.kind() == Kind::PuncPipe {
            self.eat(Kind::PuncPipe)?;
            let right = self.parse_binary_expression()?;
            let span = self.span_from(left.span());
            left = ast::Expression::PipeExpression(ast::PipeExpression {
                left: Box::new(left),
                right: Box::new(right),
                span,
            });
        }

        Ok(left)
    }

    // The binary operators by precedence, loosest first. Each level is a
    // left-associative chain over the next. Ranges sit between RANGE_LEVEL and
    // the level after it, and unary operators come after the last level.
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

    const RANGE_LEVEL: usize = 3;

    fn parse_binary_expression(&mut self) -> PResult<ast::Expression> {
        self.parse_level(0)
    }

    fn parse_level(&mut self, lvl: usize) -> PResult<ast::Expression> {
        let Some(ops) = Self::PRECEDENCE.get(lvl) else {
            return self.parse_unary_expression();
        };
        let next = |p: &mut Self| {
            if lvl == Self::RANGE_LEVEL {
                p.parse_range()
            } else {
                p.parse_level(lvl.saturating_add(1))
            }
        };
        let mut left = next(self)?;
        while let Some((tok, op)) = ops
            .iter()
            .find(|(k, _)| *k == self.kind())
            .map(|(k, op)| (k.clone(), *op))
        {
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

    // Range sits between comparison and additive so `-5..5` parses as
    // `(-5)..(5)` and `a+b..c+d` as `(a+b)..(c+d)`.
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
                // A bare module name before `::` was taken by the primary
                // expression, so anything reaching here is not one.
                Kind::PuncColonColon => {
                    return Err(Box::new(ParseError::QualifierNotModule));
                }
                Kind::PuncOpenParen => {
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
                    return Err(Box::new(ParseError::Decrement));
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
            } else if matches!(&p.cur().kind, Kind::Identifier(n) if !is_type_name(n))
                && p.peek_next() == Some(Kind::PuncColon)
            {
                let label = p.eat_identifier(Expected::ArgumentLabel)?;
                p.eat(Kind::PuncColon)?;
                // `label:` with nothing before the next `,` or `)` is
                // shorthand for `label: label`. Expand it here so every later
                // pass sees an ordinary labelled argument, and record that it
                // was punned so the formatter can write it back the same way.
                if matches!(p.kind(), Kind::PuncComma | Kind::PuncCloseParen) {
                    let value = ast::Expression::Identifier(label.clone());
                    Ok(ast::CallArg::Labeled {
                        label,
                        value,
                        punned: true,
                    })
                } else {
                    let value = p.parse_expression()?;
                    Ok(ast::CallArg::Labeled {
                        label,
                        value,
                        punned: false,
                    })
                }
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
            Kind::LiteralString(_) => self.parse_string_expression()?,
            Kind::InterpStringStart => self.parse_interpolated_string()?,
            Kind::LiteralNumber(_) => self.parse_number_expression()?,
            Kind::Identifier(_) => {
                let id = self.eat_identifier(Expected::Identifier)?;
                if self.kind() == Kind::PuncColonColon {
                    self.parse_module_access(id)?
                } else {
                    ast::Expression::Identifier(id)
                }
            }
            Kind::PuncOpenParen => self.parse_tuple()?,
            Kind::PuncOpenBrace => self.parse_block_expression()?,
            Kind::PuncOpenBracket => self.parse_array_expression()?,
            Kind::BinOpen => self.parse_binary_literal()?,
            Kind::Keyword(Keyword::If) => self.parse_if_expression()?,
            Kind::Keyword(Keyword::Match) => self.parse_match_expression()?,
            Kind::Keyword(Keyword::Fn) => self.parse_function_expression()?,
            Kind::Error(_) => {
                let span = self.current_span();
                self.advance();
                ast::Expression::ErrorNode(ast::ErrorNode {
                    error: ParseError::ScannerError,
                    span,
                })
            }
            _ => {
                return Err(Box::new(ParseError::Unexpected(self.kind())));
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
                Ok(node) => {
                    body.push(node);
                    if let Err(err) = self.end_line_item(&Kind::PuncCloseBrace, LineItem::Statement)
                    {
                        self.add_error(&err);
                    }
                }
                Err(err) => {
                    let (node, outcome) = self.recover_node(err, span);
                    body.push(node);
                    if outcome == SyncOutcome::ConsumedCloser {
                        self.pop_context();
                        return Ok(ast::Expression::BlockExpression(ast::BlockExpression {
                            body,
                            span: self.span_from(block_span),
                        }));
                    }
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

                // A spread in an array literal always carries a value, so a
                // bare `..` is an error.
                if matches!(
                    self.kind(),
                    Kind::PuncCloseBracket | Kind::PuncComma | Kind::Keyword(Keyword::Else)
                ) {
                    if self.kind() == Kind::Keyword(Keyword::Else) {
                        self.advance();
                    }
                    self.error_at(spread_span, ParseError::SpreadWithoutValue);
                    Ok(ast::ArrayElement::SpreadElement(ast::SpreadElement {
                        expression: ast::Expression::ErrorNode(ast::ErrorNode {
                            error: ParseError::SpreadWithoutValue,
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
                    self.add_error(&err);
                    if self.synchronize() == SyncOutcome::ConsumedCloser {
                        self.pop_context();
                        return Ok(ast::Expression::ArrayExpression(ast::ArrayExpression {
                            elements,
                            span: self.span_from(span),
                        }));
                    }
                    elements.push(ast::ArrayElement::Expression(ast::Expression::ErrorNode(
                        ast::ErrorNode {
                            error: *err,
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

    // Parses a `<<seg, seg, ..>>` bit-string literal. A segment with no size
    // is an 8-bit Int, except a string, which is its UTF-8 bytes.
    fn parse_binary_literal(&mut self) -> PResult<ast::Expression> {
        let start = self.current_span();
        let segments = self.parse_comma_list(Kind::BinOpen, Kind::BinClose, |p| {
            let seg_start = p.current_span();
            let value = p.parse_expression()?;
            let spec = p.parse_bin_size_spec(matches!(
                value,
                ast::Expression::StringLiteral(_) | ast::Expression::InterpolatedString(_)
            ))?;
            Ok(ast::BinSegment {
                value,
                spec,
                span: p.span_from(seg_start),
            })
        })?;
        Ok(ast::Expression::BinaryLiteral(ast::BinaryLiteral {
            segments,
            span: self.span_from(start),
        }))
    }

    // The optional `: size_spec` suffix of a `<<>>` segment:
    //   :N          -> N bits, Int
    //   :size(expr) -> expr bits, Int (dynamic)
    //   :bytes(expr)-> expr bytes, Binary slice
    //   :binary     -> remaining bytes, Binary
    //   :utf8       -> UTF-8 bytes
    // With no suffix it's an 8-bit Int with the size left as None (codegen
    // fills in 8), unless `value_is_string`, which defaults to Utf8.
    fn parse_bin_size_spec(&mut self, value_is_string: bool) -> PResult<ast::BinSpec> {
        if self.kind() != Kind::PuncColon {
            return Ok(if value_is_string {
                ast::BinSpec::Utf8
            } else {
                ast::BinSpec::Int { bits: None }
            });
        }
        self.eat(Kind::PuncColon)?;

        if matches!(self.kind(), Kind::LiteralNumber(_)) {
            let span = self.current_span();
            let value = self.eat_number(Expected::BitWidth)?;
            let n = ast::Expression::NumberLiteral(ast::NumberLiteral { value, span });
            return Ok(ast::BinSpec::Int { bits: Some(n) });
        }

        if let Kind::Identifier(kw) = self.kind() {
            match kw.as_ref() {
                "binary" => {
                    self.advance();
                    return Ok(ast::BinSpec::Binary { bytes: None });
                }
                "utf8" => {
                    self.advance();
                    return Ok(ast::BinSpec::Utf8);
                }
                "bytes" => {
                    self.advance();
                    self.eat(Kind::PuncOpenParen)?;
                    let size = self.parse_expression()?;
                    self.eat(Kind::PuncCloseParen)?;
                    return Ok(ast::BinSpec::Binary { bytes: Some(size) });
                }
                "size" => {
                    self.advance();
                    self.eat(Kind::PuncOpenParen)?;
                    let size = self.parse_expression()?;
                    self.eat(Kind::PuncCloseParen)?;
                    return Ok(ast::BinSpec::Int { bits: Some(size) });
                }
                _ => {}
            }
        }

        Err(Box::new(ParseError::SegmentSpec { got: self.kind() }))
    }

    fn parse_tuple(&mut self) -> PResult<ast::Expression> {
        let start = self.current_span();
        self.eat(Kind::PuncOpenParen)?;

        if self.kind() == Kind::PuncCloseParen {
            return Err(Box::new(ParseError::EmptyTuple));
        }

        let mut elements: Vec<ast::Expression> = Vec::new();
        elements.push(self.parse_expression()?);

        if self.kind() == Kind::PuncCloseParen {
            return Err(Box::new(ParseError::SingleElementParens));
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
        // `then` is a keyword only here, right after the condition. An
        // expression can never continue with a name, so this is the only
        // place `then` can mean anything, and `result.then` stays a function.
        let then_keyword =
            matches!(self.kind(), Kind::Identifier(ref name) if &**name == token::THEN);
        let body = if then_keyword {
            self.advance();
            // `then` is a name to the line-end rule, which can't know it is a
            // keyword here, so a branch on the next line gets a line end
            // before it. The branch belongs to the `if` all the same.
            if self.kind() == Kind::LineEnd {
                self.advance();
            }
            self.parse_expression()?
        } else if self.kind() == Kind::PuncOpenBrace {
            self.parse_expression()?
        } else {
            return Err(Box::new(ParseError::IfWithoutBody));
        };

        if self.kind() != Kind::Keyword(Keyword::Else) {
            return Err(Box::new(ParseError::IfWithoutElse));
        }
        self.eat(Kind::Keyword(Keyword::Else))?;
        // Any expression can follow: `else { .. }`, `else if ..` or
        // `else not_found()`. An `else if` goes through the guarded wrapper so
        // each step counts towards the depth limit. Otherwise a long chain
        // would recurse without raising the count and overflow the native
        // stack.
        let else_body = if self.kind() == Kind::Keyword(Keyword::If) {
            self.parse_if_expression()?
        } else {
            self.parse_expression()?
        };

        Ok(ast::Expression::IfExpression(ast::IfExpression {
            condition: Box::new(condition),
            then_keyword,
            body: Box::new(body),
            span: self.span_from(span),
            else_body: Box::new(else_body),
        }))
    }

    fn parse_function_body(&mut self) -> PResult<ast::Expression> {
        if self.kind() != Kind::PuncOpenBrace {
            return Err(Box::new(ParseError::FunctionBodyNotBlock));
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
        let mut closed = false;

        'arms: while self.kind() != Kind::PuncCloseBrace && self.kind() != Kind::Eof {
            let pattern = match self.parse_pattern() {
                Ok(p) => p,
                Err(err) => {
                    let err_span = self.current_span();
                    match self.recover_in_arm(&err) {
                        ArmRecovery::ConsumedCloser => {
                            closed = true;
                            break 'arms;
                        }
                        ArmRecovery::AtCloseBrace => break,
                        // Stopped at this arm's `->`: use a placeholder so the
                        // body still parses and the loop advances instead of
                        // re-erroring on the same token.
                        ArmRecovery::AtArrow => ast::Pattern::Var {
                            name: ast::Identifier {
                                name: "_".to_string(),
                                span: err_span,
                            },
                        },
                        ArmRecovery::Resume => continue,
                    }
                }
            };

            let guard = if self.kind() == Kind::Keyword(Keyword::If) {
                self.eat(Kind::Keyword(Keyword::If))?;
                match self.parse_expression() {
                    Ok(e) => Some(e),
                    Err(err) => match self.recover_in_arm(&err) {
                        ArmRecovery::ConsumedCloser => {
                            closed = true;
                            break 'arms;
                        }
                        ArmRecovery::AtCloseBrace => break,
                        ArmRecovery::AtArrow => None,
                        ArmRecovery::Resume => continue,
                    },
                }
            } else {
                None
            };

            if let Err(err) = self.eat(Kind::PuncArrow) {
                match self.recover_in_arm(&err) {
                    ArmRecovery::ConsumedCloser => {
                        closed = true;
                        break 'arms;
                    }
                    ArmRecovery::AtCloseBrace => break,
                    ArmRecovery::AtArrow => self.advance(),
                    ArmRecovery::Resume => continue,
                }
            }

            let body_span = self.current_span();
            let body = match self.parse_expression() {
                Ok(e) => e,
                Err(err) => match self.recover_in_arm(&err) {
                    ArmRecovery::ConsumedCloser => {
                        closed = true;
                        break 'arms;
                    }
                    ArmRecovery::AtCloseBrace => break 'arms,
                    ArmRecovery::AtArrow | ArmRecovery::Resume => {
                        ast::Expression::ErrorNode(ast::ErrorNode {
                            error: *err,
                            span: body_span,
                        })
                    }
                },
            };

            arms.push(ast::MatchArm {
                pattern,
                guard,
                body,
            });

            if self.kind() == Kind::PuncComma {
                self.add_error(&ParseError::CommaBetweenArms);
                self.eat(Kind::PuncComma)?;
            } else if let Err(err) = self.end_line_item(&Kind::PuncCloseBrace, LineItem::Arm) {
                self.add_error(&err);
            }
        }

        self.pop_context();
        if !closed {
            self.eat(Kind::PuncCloseBrace)?;
        }

        Ok(ast::Expression::MatchExpression(ast::MatchExpression {
            subject: Box::new(subject),
            arms,
            span: self.span_from(match_span),
        }))
    }

    fn parse_pattern(&mut self) -> PResult<ast::Pattern> {
        let start = self.current_span();
        // Allow a leading `|`, as TypeScript unions do: `| A | B` means
        // `A | B`. The formatter never writes it, but accepting it lets every
        // alternative of a hand-wrapped or-pattern start its line with a pipe.
        if self.kind() == Kind::BitwiseOr {
            self.eat(Kind::BitwiseOr)?;
        }
        let first = self.parse_pattern_range()?;

        if self.kind() == Kind::BitwiseOr {
            let mut rest = Vec::new();
            while self.kind() == Kind::BitwiseOr {
                self.eat(Kind::BitwiseOr)?;
                rest.push(self.parse_pattern_range()?);
            }
            return Ok(ast::Pattern::Or {
                first: Box::new(first),
                rest,
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

    /// Range pattern bounds must be number literals; anything else errors and
    /// becomes a `0` placeholder for recovery.
    // Every non-number pattern, current or future, is the same recovery error.
    #[allow(unknown_lints, wildcard_over_own_enum)]
    fn require_number_bound(&mut self, p: ast::Pattern) -> ast::NumberLiteral {
        match p {
            ast::Pattern::Literal(ast::PatternLiteral::Number(n)) => n,
            other => {
                let span = other.span();
                self.error_at(span, ParseError::RangeBoundNotNumber);
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

    /// The shared tail of a constructor pattern: the optional argument list.
    fn finish_ctor_pattern(
        &mut self,
        qualifier: Option<ast::Identifier>,
        name: String,
        name_span: Span,
        start: Span,
    ) -> PResult<ast::Pattern> {
        if !is_type_name(&name) {
            return Err(Box::new(ParseError::LowercaseConstructor { name }));
        }
        let mut args: Vec<ast::PatternArg> = Vec::new();
        let mut rest = false;
        if self.kind() == Kind::PuncOpenParen {
            self.eat(Kind::PuncOpenParen)?;
            let (parsed_args, parsed_rest) = self.parse_pattern_args()?;
            args = parsed_args;
            rest = parsed_rest;
            self.eat(Kind::PuncCloseParen)?;
        }
        Ok(ast::Pattern::Constructor {
            qualifier,
            name: ast::Identifier {
                name,
                span: name_span,
            },
            args,
            rest,
            span: self.span_from(start),
        })
    }

    fn parse_pattern_atom_inner(&mut self) -> PResult<ast::Pattern> {
        let start = self.current_span();
        match self.kind() {
            Kind::Keyword(Keyword::Else) => Err(Box::new(ParseError::ElsePattern)),
            Kind::Identifier(_) => {
                let id_span = self.current_span();
                let name = self.eat_name(Expected::Pattern)?;
                // `io::NotFound(path)` reaches a constructor through its
                // module's name.
                if self.eat_module_separator(&name)? {
                    let q = ast::Identifier {
                        name,
                        span: id_span,
                    };
                    let member_span = self.current_span();
                    let member = self.eat_name(Expected::ConstructorName)?;
                    return self.finish_ctor_pattern(Some(q), member, member_span, start);
                }
                if is_type_name(&name) {
                    return self.finish_ctor_pattern(None, name, id_span, start);
                }
                Ok(ast::Pattern::Var {
                    name: ast::Identifier {
                        name,
                        span: id_span,
                    },
                })
            }
            Kind::LiteralNumber(_) => {
                let span = self.current_span();
                let value = self.eat_number(Expected::Number)?;
                Ok(ast::Pattern::Literal(ast::PatternLiteral::Number(
                    ast::NumberLiteral { value, span },
                )))
            }
            Kind::PuncMinus => {
                self.eat(Kind::PuncMinus)?;
                let value = self.eat_number(Expected::NumberAfterMinus)?;
                Ok(ast::Pattern::Literal(ast::PatternLiteral::Number(
                    ast::NumberLiteral {
                        value: format!("-{value}"),
                        span: self.span_from(start),
                    },
                )))
            }
            Kind::LiteralString(_) => {
                let span = self.current_span();
                let value = self.eat_string(Expected::String)?;
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
                    return Err(Box::new(ParseError::TuplePatternTooShort));
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
                            if matches!(p.kind(), Kind::Identifier(_)) {
                                binding = Some(p.eat_identifier(Expected::SpreadBinding)?);
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
            _ => Err(Box::new(ParseError::UnexpectedInPattern(self.kind()))),
        }
    }

    // Parses a `<<seg, seg, .., rest:binary>>` bit-string pattern. A segment's
    // value can only be an atom pattern, and its size spec is the same as in
    // the expression form. A trailing `..ident` or `..` captures the rest and
    // must come last.
    fn parse_binary_pattern(&mut self) -> PResult<ast::Pattern> {
        let start = self.current_span();
        self.eat(Kind::BinOpen)?;

        let mut segments: Vec<ast::BinSegmentPat> = Vec::new();
        let mut rest: Option<ast::BinaryPatternRest> = None;

        while self.kind() != Kind::BinClose && self.kind() != Kind::Eof {
            if self.kind() == Kind::PuncDotdot {
                self.eat(Kind::PuncDotdot)?;
                let binding = if matches!(self.kind(), Kind::Identifier(_)) {
                    Some(self.eat_identifier(Expected::SpreadBinding)?)
                } else {
                    None
                };
                rest = Some(ast::BinaryPatternRest { binding });
                if self.kind() == Kind::PuncComma {
                    return Err(Box::new(ParseError::RestNotLast));
                }
                break;
            }

            let seg_start = self.current_span();
            let value = self.parse_pattern_atom()?;
            // A bare string segment matches its UTF-8 bytes as a prefix
            // (`<<'GET ', ..rest>>`), not a single 8-bit Int.
            let spec = self.parse_bin_size_spec(matches!(
                value,
                ast::Pattern::Literal(ast::PatternLiteral::String(_))
            ))?;
            segments.push(ast::BinSegmentPat {
                value,
                spec,
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

            if matches!(&self.cur().kind, Kind::Identifier(n) if !is_type_name(n))
                && self.peek_next() == Some(Kind::PuncColon)
            {
                let label = self.eat_identifier(Expected::PatternLabel)?;
                self.eat(Kind::PuncColon)?;
                // `name:` with nothing after it binds the field to `name`.
                let punned = matches!(self.kind(), Kind::PuncComma | Kind::PuncCloseParen);
                let pattern = if punned {
                    ast::Pattern::Var {
                        name: label.clone(),
                    }
                } else {
                    self.parse_pattern()?
                };
                args.push(ast::PatternArg::Labeled {
                    label,
                    pattern,
                    punned,
                });
            } else {
                let pattern = self.parse_pattern()?;
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

    fn parse_function(&mut self) -> PResult<ast::Node> {
        if matches!(self.peek_next(), Some(Kind::Identifier(_))) {
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

        let identifier = self.eat_identifier(Expected::FunctionName)?;

        let params = self.parse_parameters()?;
        let return_type = self.parse_function_return_type()?;

        let vm_attr = attributes.iter().find(|a| a.name.name == "vm");
        let body = if let Some(vm_attr) = vm_attr {
            if return_type.is_none() {
                return Err(Box::new(ParseError::VmWithoutReturnType));
            }
            if self.kind() == Kind::PuncOpenBrace {
                return Err(Box::new(ParseError::VmWithBody));
            }
            // Check the argument count here, once, because everything after
            // this assumes `FnBody::Vm` carries exactly the op key.
            let op = match vm_attr.args.as_slice() {
                [op] => op.clone(),
                _ => {
                    return Err(Box::new(ParseError::VmArity));
                }
            };
            ast::FnBody::Vm(op)
        } else {
            ast::FnBody::Block(self.parse_function_body()?)
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
        // Lambdas have no return-type slot: the body starts right after `)`,
        // which makes `fn(x) x * 3` unambiguous.
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

    // Whether the current token starts a type in a parameter or return
    // position. Any name counts, because these positions allow lowercase type
    // variables.
    fn at_loose_type_start(&self) -> bool {
        matches!(
            self.kind(),
            Kind::Identifier(_) | Kind::Keyword(Keyword::Fn) | Kind::PuncOpenParen
        )
    }

    fn parse_function_return_type(&mut self) -> PResult<Option<ast::TypeIdentifier>> {
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
        let identifier = self.eat_identifier(Expected::ParameterName)?;

        let mut typ: Option<ast::TypeIdentifier> = None;

        if self.at_loose_type_start() {
            typ = Some(self.parse_type_identifier()?);
        }

        Ok(ast::FunctionParameter { typ, identifier })
    }

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
                return Err(Box::new(ParseError::TupleTypeTooShort));
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
        let name = self.eat_name(Expected::TypeName)?;

        // In `module::Type`, the first name is the imported module and the
        // second names the type inside it.
        let mut qualifier: Option<ast::Identifier> = None;
        let mut identifier = ast::Identifier { name, span };
        if self.eat_module_separator(&identifier.name)? {
            let member_span = self.current_span();
            let member = self.eat_name(Expected::MemberTypeName)?;
            qualifier = Some(identifier);
            identifier = ast::Identifier {
                name: member,
                span: member_span,
            };
        }

        let mut type_args: Vec<ast::TypeIdentifier> = Vec::new();
        if self.kind() == Kind::PuncOpenParen {
            type_args = self.parse_comma_list(Kind::PuncOpenParen, Kind::PuncCloseParen, |p| {
                p.parse_type_identifier()
            })?;
        }

        Ok(ast::TypeIdentifier {
            kind: ast::TypeKind::NamedType(ast::NamedType {
                qualifier,
                identifier,
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
            p.eat_identifier(Expected::TypeParameterName)
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

        let identifier = self.eat_identifier(Expected::DeclaredTypeName)?;
        if !is_type_name(&identifier.name) {
            return Err(Box::new(ParseError::LowercaseTypeName {
                name: identifier.name,
            }));
        }
        let type_params = self.parse_type_params()?;

        let body = if self.kind() == Kind::PuncEquals {
            if opaque {
                return Err(Box::new(ParseError::OpaqueAlias));
            }
            self.eat(Kind::PuncEquals)?;
            ast::TypeBody::Alias(self.parse_type_identifier()?)
        } else if self.kind() != Kind::PuncOpenBrace {
            if opaque {
                return Err(Box::new(ParseError::OpaqueWithoutConstructors));
            }
            ast::TypeBody::External
        } else {
            self.eat(Kind::PuncOpenBrace)?;
            let variants = self.with_context(ParseContext::TypeDef, |p| {
                if matches!(&p.cur().kind, Kind::Identifier(s) if !is_type_name(s)) {
                    // Single-constructor shorthand: `type T { field Type ... }`
                    // desugars to `type T { T(field Type ...) }`.
                    let fields =
                        p.parse_line_separated_list(Kind::PuncCloseBrace, LineItem::Field, |q| {
                            q.parse_constructor_field()
                        })?;
                    Ok(vec![ast::Constructor {
                        doc: None,
                        identifier: identifier.clone(),
                        fields,
                        span: identifier.span,
                    }])
                } else {
                    p.parse_line_separated_list(Kind::PuncCloseBrace, LineItem::Constructor, |q| {
                        q.parse_constructor()
                    })
                }
            })?;
            self.eat(Kind::PuncCloseBrace)?;
            if variants.is_empty() {
                return Err(Box::new(ParseError::EmptyTypeDefinition));
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
        let name = self.eat_name(Expected::ConstructorName)?;
        if !is_type_name(&name) {
            return Err(Box::new(ParseError::LowercaseConstructor { name }));
        }
        let mut fields = Vec::new();
        if self.kind() == Kind::PuncOpenParen {
            self.eat(Kind::PuncOpenParen)?;
            fields =
                self.parse_field_list(Kind::PuncCloseParen, |p| p.parse_constructor_field())?;
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

        if self.is_type_start() {
            return Err(Box::new(ParseError::UnlabeledField));
        }

        let label = self.eat_identifier(Expected::FieldLabel)?;
        let typ = self.parse_type_identifier()?;

        Ok(ast::ConstructorField {
            label,
            typ,
            span: self.span_from(start),
        })
    }

    fn parse_const_binding(&mut self, doc: Option<String>) -> PResult<ast::Declaration> {
        let span = self.current_span();
        self.eat(Kind::Keyword(Keyword::Const))?;

        let identifier = self.eat_identifier(Expected::ConstName)?;

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

    // Whether an `=` follows the matching `)`.
    fn is_tuple_destructuring(&self) -> bool {
        let mut depth = 0usize;
        let mut rest = self.tokens_from(self.index);
        while let Some(tok) = rest.next() {
            if tok.kind == Kind::PuncOpenParen {
                depth = depth.saturating_add(1);
            } else if tok.kind == Kind::PuncCloseParen {
                let Some(d) = depth.checked_sub(1) else {
                    return false;
                };
                depth = d;
                if depth == 0 {
                    return rest
                        .next()
                        .is_some_and(|next| next.kind == Kind::PuncEquals);
                }
            } else if tok.kind == Kind::Eof {
                return false;
            }
        }
        false
    }

    /// Whether a `module::Ctor(..) = e` statement starts here: a module name,
    /// `::`, a constructor name and its `(`, then an `=` after the matching `)`.
    /// Without the `=` it is a call, `http::Fixed(1, b)`.
    fn is_qualified_ctor_destructuring(&self) -> bool {
        let kind = |i: usize| self.token_ahead(i).map(|t| &t.kind);
        let (Some(Kind::Identifier(_)), Some(Kind::PuncColonColon), Some(Kind::Identifier(ctor))) =
            (kind(0), kind(1), kind(2))
        else {
            return false;
        };
        if !is_type_name(ctor) || kind(3) != Some(&Kind::PuncOpenParen) {
            return false;
        }
        let mut depth = 0usize;
        let mut rest = self.tokens_from(self.index).skip(3);
        while let Some(tok) = rest.next() {
            match tok.kind {
                Kind::PuncOpenParen => depth = depth.saturating_add(1),
                Kind::PuncCloseParen => {
                    let Some(d) = depth.checked_sub(1) else {
                        return false;
                    };
                    depth = d;
                    if depth == 0 {
                        return rest
                            .next()
                            .is_some_and(|next| next.kind == Kind::PuncEquals);
                    }
                }
                Kind::Eof => return false,
                _ => {}
            }
        }
        false
    }

    fn parse_backpass(&mut self) -> PResult<ast::Statement> {
        let span = self.current_span();
        let mut binders = vec![self.parse_backpass_binder()?];
        while self.kind() == Kind::PuncComma {
            self.eat(Kind::PuncComma)?;
            binders.push(self.parse_backpass_binder()?);
        }
        self.eat(Kind::PuncBackArrow)?;

        let call = self.parse_expression()?;
        if matches!(call, ast::Expression::OrExpression(_)) {
            return Err(Box::new(ParseError::BackpassOr));
        }
        if !matches!(call, ast::Expression::FunctionCallExpression(_)) {
            return Err(Box::new(ParseError::BackpassNotCall));
        }
        // Check this last, so the whole statement is consumed either way.
        // Returning early would leave recovery stuck on the first binder.
        if self.current_context() != ParseContext::Block {
            return Err(Box::new(ParseError::BackpassAtTopLevel));
        }

        Ok(ast::Statement::Backpass(ast::BackpassBinding {
            binders,
            call,
            span: self.span_from(span),
        }))
    }

    fn parse_backpass_binder(&mut self) -> PResult<ast::Pattern> {
        self.parse_pattern()
    }

    /// `pattern [Type] = init`. A plain name may carry a type, `x Int = 5`;
    /// any other pattern takes the type from its constructor or its parts.
    fn parse_binding(&mut self) -> PResult<ast::Statement> {
        let doc = self.extract_doc_comment();
        let span = self.current_span();
        let pattern = self.parse_pattern()?;

        let mut typ: Option<ast::TypeIdentifier> = None;
        if matches!(pattern, ast::Pattern::Var { .. }) && self.is_type_start() {
            typ = Some(self.parse_type_identifier()?);
        }

        self.eat(Kind::PuncEquals)?;
        let init = self.parse_expression()?;

        Ok(ast::Statement::Binding(Box::new(ast::Binding {
            doc,
            pattern,
            typ,
            init,
            span: self.span_from(span),
        })))
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
        // Callers only get here after `pub` or at least one attribute, so the
        // error can always name what came before.
        let preceded_by = if is_pub {
            DeclarationLead::Pub
        } else {
            DeclarationLead::Attributes
        };
        let mut decl = self.parse_declaration_inner(doc, attrs, opaque, preceded_by)?;
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
        preceded_by: DeclarationLead,
    ) -> PResult<ast::Declaration> {
        if opaque && self.kind() != Kind::Keyword(Keyword::Type) {
            return Err(Box::new(ParseError::OpaqueNotType));
        }
        match self.kind() {
            Kind::Keyword(Keyword::Fn) => self.parse_function_declaration(doc, attrs),
            Kind::Keyword(Keyword::Type) => self.parse_type_declaration(doc, attrs, opaque),
            Kind::Keyword(Keyword::Const) => {
                if !attrs.is_empty() {
                    return Err(Box::new(ParseError::AttributesOnConst));
                }
                self.parse_const_binding(doc)
            }
            other => Err(Box::new(ParseError::ExpectedDeclaration {
                after: preceded_by,
                got: other,
            })),
        }
    }

    fn parse_attributes(&mut self) -> PResult<Vec<ast::Attribute>> {
        let mut attrs = Vec::new();
        while self.kind() == Kind::PuncAt {
            attrs.push(self.parse_attribute()?);
            if self.kind() == Kind::LineEnd {
                self.eat(Kind::LineEnd)?;
            }
        }
        Ok(attrs)
    }

    fn parse_attribute(&mut self) -> PResult<ast::Attribute> {
        let start = self.current_span();
        self.eat(Kind::PuncAt)?;
        let name = self.eat_identifier(Expected::AttributeName)?;
        let mut args = Vec::new();
        if self.kind() == Kind::PuncOpenParen {
            args = self.parse_comma_list(Kind::PuncOpenParen, Kind::PuncCloseParen, |p| {
                p.eat_identifier(Expected::AttributeArgument)
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

        // The `.` and `..` of a relative import. They can only come before the
        // first module name, so they never mix into `names`.
        let mut leading: Vec<ast::RelSeg> = Vec::new();
        while matches!(self.kind(), Kind::PuncDot | Kind::PuncDotdot) {
            leading.push(if self.kind() == Kind::PuncDot {
                ast::RelSeg::CurrentDir
            } else {
                ast::RelSeg::ParentDir
            });
            self.advance();
            if self.kind() != Kind::PuncDiv {
                return Err(Box::new(ParseError::RelativeImportWithoutSlash));
            }
            self.eat(Kind::PuncDiv)?;
        }

        // Span of the final module-name segment; the last write wins.
        let mut path_span = self.current_span();
        let first = self.eat_name(Expected::ImportedModule)?;
        let mut names: Vec<String> = vec![first];

        while self.kind() == Kind::PuncDiv {
            self.eat(Kind::PuncDiv)?;
            path_span = self.current_span();
            let seg = self.eat_name(Expected::ModuleSegment)?;
            names.push(seg);
        }

        let mut alias: Option<ast::Identifier> = None;
        if self.kind() == Kind::Keyword(Keyword::As) {
            self.eat(Kind::Keyword(Keyword::As))?;
            alias = Some(self.eat_identifier(Expected::Alias)?);
        }
        let path = ast::ImportPath { leading, names };

        // Everything a module exports is reached through its name, so there
        // is no `import m.{x}`. Say what to write instead, then skip the list
        // and keep the import, so the error is the only one.
        if self.kind() == Kind::PuncDot && self.peek_next() == Some(Kind::PuncOpenBrace) {
            let list_start = self.current_span();
            self.eat(Kind::PuncDot)?;
            self.eat(Kind::PuncOpenBrace)?;
            let qualifier = match &alias {
                Some(a) => a.name.clone(),
                None => path.names.last().cloned().unwrap_or_default(),
            };
            let item = match self.kind() {
                Kind::Identifier(name) => name.to_string(),
                _ => "name".to_string(),
            };
            while !matches!(self.kind(), Kind::PuncCloseBrace | Kind::Eof) {
                self.advance();
            }
            if self.kind() == Kind::PuncCloseBrace {
                self.advance();
            }
            let list = self.span_from(list_start);
            self.error_at(
                list,
                ParseError::ImportList {
                    path: path.clone(),
                    qualifier,
                    item,
                },
            );
        }

        Ok(ast::Statement::ImportDeclaration(ast::ImportDeclaration {
            path,
            alias,
            path_span,
            span: self.span_from(import_span),
        }))
    }

    /// Eats the `::` after a module name in a type or a constructor pattern.
    /// There `http.Method` can only mean `http::Method`, because there's no
    /// value for `.` to read a field of. So we report it and read it as `::`,
    /// rather than fail on the next token.
    fn eat_module_separator(&mut self, module: &str) -> PResult<bool> {
        match self.kind() {
            Kind::PuncColonColon => {
                self.eat(Kind::PuncColonColon)?;
                Ok(true)
            }
            Kind::PuncDot => {
                let next = self.token_ahead(1).map(|t| &t.kind);
                let Some(Kind::Identifier(member)) = next else {
                    return Ok(false);
                };
                if !is_type_name(member) {
                    return Ok(false);
                }
                let error = ParseError::DotBeforeModuleMember {
                    module: module.to_string(),
                    member: member.to_string(),
                };
                let span = self.current_span();
                self.error_at(span, error);
                self.eat(Kind::PuncDot)?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    fn parse_module_access(&mut self, module: ast::Identifier) -> PResult<ast::Expression> {
        self.eat(Kind::PuncColonColon)?;
        let span = self.current_span();
        let name = self.eat_name(Expected::ModuleMember)?;
        Ok(ast::Expression::ModuleAccessExpression(
            ast::ModuleAccessExpression {
                span: module.span.union(&span),
                module,
                member: ast::Identifier { name, span },
            },
        ))
    }

    fn parse_dot_expression(&mut self, left: ast::Expression) -> PResult<ast::Expression> {
        let start = left.span();
        self.eat(Kind::PuncDot)?;

        let span = self.current_span();

        if matches!(self.kind(), Kind::LiteralNumber(_)) {
            let num_str = self.eat_number(Expected::TupleIndex)?;
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

        let property = self.eat_name(Expected::PropertyName)?;

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
            value: self.eat_string(Expected::String)?,
            span,
        }))
    }

    fn parse_interpolated_string(&mut self) -> PResult<ast::Expression> {
        let span = self.current_span();
        self.eat(Kind::InterpStringStart)?;

        let mut parts: Vec<ast::InterpPart> = Vec::new();

        loop {
            match self.kind() {
                Kind::InterpStringPart(_) => {
                    let part_span = self.current_span();
                    let value = self.eat_interp_part(Expected::StringPart)?;
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
                Kind::Identifier(_) => {
                    let ident = self.eat_identifier(Expected::Identifier)?;
                    parts.push(ast::InterpPart::Expr(Box::new(
                        ast::Expression::Identifier(ident),
                    )));
                }
                _ => {
                    return Err(Box::new(ParseError::UnexpectedInInterpolation(self.kind())));
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
            value: self.eat_number(Expected::Number)?,
            span,
        }))
    }
}

#[cfg(test)]
mod tests;
