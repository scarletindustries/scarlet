module parser

import scanner
import token
import ast
import diagnostic
import span as sp

pub enum ParseContext {
	top_level
	block
	function_params
	array
	struct_init
	struct_def
	enum_def
	match_arms
}

pub struct ParseResult {
pub:
	ast         ast.BlockExpression
	diagnostics []diagnostic.Diagnostic
}

pub struct Parser {
	tokens []token.Token
mut:
	index                 int
	current_token         token.Token
	diagnostics           []diagnostic.Diagnostic
	context_stack         []ParseContext
	prev_token_end_line   int
	prev_token_end_column int
	sync_iterations       int
}

pub fn new_parser(mut s scanner.Scanner) Parser {
	return new_parser_from_tokens(s.scan_all(), s.get_diagnostics())
}

pub fn new_parser_from_tokens(tokens []token.Token, scanner_diagnostics []diagnostic.Diagnostic) Parser {
	return Parser{
		tokens:        tokens
		index:         0
		current_token: tokens[0]
		diagnostics:   scanner_diagnostics
		context_stack: [ParseContext.top_level]
	}
}

fn (mut p Parser) push_context(ctx ParseContext) {
	p.context_stack << ctx
}

fn (mut p Parser) pop_context() {
	if p.context_stack.len > 1 {
		p.context_stack.pop()
	}
}

fn (p Parser) current_context() ParseContext {
	if p.context_stack.len > 0 {
		return p.context_stack.last()
	}
	return .top_level
}

fn (mut p Parser) add_error(message string) {
	p.diagnostics << diagnostic.error_at(p.current_token.line, p.current_token.column,
		message)
}

fn (mut p Parser) add_warning(message string) {
	p.diagnostics << diagnostic.warning_at(p.current_token.line, p.current_token.column,
		message)
}

fn (p Parser) current_span() sp.Span {
	token_len := if lit := p.current_token.literal {
		lit.len
	} else {
		p.current_token.kind.str().len
	}
	return sp.Span{
		start_line:   p.current_token.line
		start_column: p.current_token.column
		end_line:     p.current_token.line
		end_column:   p.current_token.column + token_len
	}
}

fn (p Parser) span_from(start sp.Span) sp.Span {
	return sp.Span{
		start_line:   start.start_line
		start_column: start.start_column
		end_line:     p.prev_token_end_line
		end_column:   p.prev_token_end_column
	}
}

fn (mut p Parser) save_token_end() {
	token_len := if lit := p.current_token.literal {
		lit.len
	} else {
		p.current_token.kind.str().len
	}
	p.prev_token_end_line = p.current_token.line
	p.prev_token_end_column = p.current_token.column + token_len
}

fn (mut p Parser) synchronize() {
	ctx := p.current_context()

	for p.current_token.kind != .eof {
		p.sync_iterations++
		if p.sync_iterations > 1000 {
			p.add_error('Parser recovery failed: too many recovery attempts. This is a bug in the parser.')
			for p.current_token.kind != .eof {
				p.advance()
			}
			return
		}
		match ctx {
			.top_level {
				if p.current_token.kind in [.kw_function, .kw_struct, .kw_enum, .kw_const, .kw_from,
					.kw_export, .identifier] {
					return
				}
			}
			.block {
				if p.current_token.kind == .punc_close_brace {
					p.advance()
					p.pop_context()
					return
				}
				if p.current_token.kind in [.kw_if, .kw_match, .kw_function, .identifier] {
					return
				}
			}
			.function_params {
				if p.current_token.kind == .punc_close_paren {
					p.advance()
					p.pop_context()
					return
				}
				if p.current_token.kind == .punc_open_brace {
					return
				}
				if p.current_token.kind == .punc_comma {
					p.advance()
					return
				}
			}
			.array {
				if p.current_token.kind == .punc_close_bracket {
					p.advance()
					p.pop_context()
					return
				}
				if p.current_token.kind == .punc_comma {
					p.advance()
					return
				}
			}
			.struct_init, .struct_def {
				if p.current_token.kind == .punc_close_brace {
					p.advance()
					p.pop_context()
					return
				}
				if p.current_token.kind == .punc_comma {
					p.advance()
					return
				}
			}
			.enum_def {
				if p.current_token.kind == .punc_close_brace {
					p.advance()
					p.pop_context()
					return
				}
				if p.current_token.kind == .punc_comma {
					p.advance()
					return
				}
			}
			.match_arms {
				if p.current_token.kind == .punc_close_brace {
					p.advance()
					p.pop_context()
					return
				}
				if p.current_token.kind == .punc_arrow {
					return
				}
				if p.current_token.kind == .punc_comma {
					p.advance()
					return
				}
			}
		}

		p.advance()
	}
}

fn (mut p Parser) advance() {
	if p.index + 1 < p.tokens.len {
		p.save_token_end()
		p.index++
		p.current_token = p.tokens[p.index]
	}
}

fn (mut p Parser) eat(kind token.Kind) !token.Token {
	if p.current_token.kind == kind {
		old := p.current_token
		p.save_token_end()

		p.index = p.index + 1
		p.current_token = p.tokens[p.index]

		return old
	}

	return error("Expected '${kind}', got '${p.current_token}'")
}

fn (mut p Parser) eat_msg(kind token.Kind, message string) !token.Token {
	return p.eat(kind) or { return error("${message}, got '${p.current_token}'") }
}

fn (mut p Parser) eat_token_literal(kind token.Kind, message string) !string {
	eaten := p.eat_msg(kind, message)!

	if unwrapped := eaten.literal {
		return unwrapped
	}

	return error('Expected ${message}')
}

pub fn (mut p Parser) parse_program() ParseResult {
	program_span := p.current_span()
	mut body := []ast.Node{}

	for p.current_token.kind != .eof {
		span := p.current_span()
		node := p.parse_node() or {
			p.add_error(err.msg())
			p.synchronize()

			err_expr := ast.Expression(ast.ErrorNode{
				message: err.msg()
				span:    span
			})
			body << ast.Node(err_expr)
			continue
		}

		body << node
	}

	return ParseResult{
		ast:         ast.BlockExpression{
			body: body
			span: p.span_from(program_span)
		}
		diagnostics: p.diagnostics
	}
}

fn (mut p Parser) parse_node() !ast.Node {
	// Try to parse as a statement first (keywords)
	match p.current_token.kind {
		.kw_const {
			return ast.Node(p.parse_const_binding()!)
		}
		.kw_function {
			return p.parse_function()!
		}
		.kw_struct {
			return ast.Node(p.parse_struct_declaration()!)
		}
		.kw_enum {
			return ast.Node(p.parse_enum_declaration()!)
		}
		.kw_import {
			return ast.Node(p.parse_import_declaration()!)
		}
		.kw_export {
			return ast.Node(p.parse_export_declaration()!)
		}
		.punc_open_paren {
			// Could be tuple destructuring: (a, b) = expr
			if p.is_tuple_destructuring() {
				return ast.Node(p.parse_tuple_destructuring()!)
			}
		}
		.identifier {
			// Check for binding patterns: identifier = or identifier Type =
			if next := p.peek_next() {
				if next.kind == .punc_equals {
					return ast.Node(p.parse_binding()!)
				}
				// Check if it's an identifier followed by a type annotation
				if p.is_type_start_at_next() {
					return ast.Node(p.parse_binding()!)
				}
			}
		}
		else {}
	}
	// Otherwise parse as expression
	return ast.Node(p.parse_expression()!)
}

fn (mut p Parser) peek_next() ?token.Token {
	if p.index + 1 < p.tokens.len {
		return p.tokens[p.index + 1]
	}

	return none
}

fn (mut p Parser) peek_ahead(distance int) ?token.Token {
	if p.index + distance < p.tokens.len {
		return p.tokens[p.index + distance]
	}

	return none
}

fn (mut p Parser) parse_expression() !ast.Expression {
	return p.parse_or_expression()!
}

fn (mut p Parser) parse_or_expression() !ast.Expression {
	mut left := p.parse_binary_expression()!

	if p.current_token.kind == .kw_or {
		or_span := p.current_span()
		p.eat(.kw_or)!

		mut receiver := ?ast.Identifier(none)

		if p.current_token.kind == .identifier {
			if next := p.peek_next() {
				if next.kind == .punc_arrow {
					span := p.current_span()
					name := p.eat_token_literal(.identifier, 'Expected identifier for or receiver')!
					receiver = ast.Identifier{
						name: name
						span: span
					}
					p.eat(.punc_arrow)!
				}
			}
		}

		body := p.parse_expression()!

		return ast.OrExpression{
			expression: left
			receiver:   receiver
			body:       body
			span:       or_span
		}
	}

	return left
}

fn (mut p Parser) parse_binary_expression() !ast.Expression {
	return p.parse_logical_or()!
}

fn (mut p Parser) parse_logical_or() !ast.Expression {
	mut left := p.parse_logical_and()!

	for p.current_token.kind == .logical_or {
		span := p.current_span()
		p.eat(.logical_or)!
		right := p.parse_logical_and()!
		left = ast.BinaryExpression{
			left:  left
			right: right
			op:    ast.Operator{
				kind: .logical_or
			}
			span:  span
		}
	}

	return left
}

fn (mut p Parser) parse_logical_and() !ast.Expression {
	mut left := p.parse_equality()!

	for p.current_token.kind == .logical_and {
		span := p.current_span()
		p.eat(.logical_and)!
		right := p.parse_equality()!
		left = ast.BinaryExpression{
			left:  left
			right: right
			op:    ast.Operator{
				kind: .logical_and
			}
			span:  span
		}
	}

	return left
}

fn (mut p Parser) parse_equality() !ast.Expression {
	mut left := p.parse_comparison()!

	for p.current_token.kind in [.punc_equals_comparator, .punc_not_equal] {
		span := p.current_span()
		operator := p.current_token.kind
		p.eat(operator)!
		right := p.parse_comparison()!
		left = ast.BinaryExpression{
			left:  left
			right: right
			op:    ast.Operator{
				kind: operator
			}
			span:  span
		}
	}

	return left
}

fn (mut p Parser) parse_comparison() !ast.Expression {
	mut left := p.parse_additive()!

	for p.current_token.kind in [.punc_lt, .punc_gt, .punc_lte, .punc_gte] {
		span := p.current_span()
		operator := p.current_token.kind
		p.eat(operator)!
		right := p.parse_additive()!
		left = ast.BinaryExpression{
			left:  left
			right: right
			op:    ast.Operator{
				kind: operator
			}
			span:  span
		}
	}

	return left
}

fn (mut p Parser) parse_additive() !ast.Expression {
	mut left := p.parse_multiplicative()!

	for p.current_token.kind in [.punc_plus, .punc_minus] {
		span := p.current_span()
		operator := p.current_token.kind
		p.eat(operator)!
		right := p.parse_multiplicative()!
		left = ast.BinaryExpression{
			left:  left
			right: right
			op:    ast.Operator{
				kind: operator
			}
			span:  span
		}
	}

	return left
}

fn (mut p Parser) parse_multiplicative() !ast.Expression {
	mut left := p.parse_unary_expression()!

	for p.current_token.kind in [.punc_mul, .punc_div, .punc_mod] {
		span := p.current_span()
		operator := p.current_token.kind
		p.eat(operator)!
		right := p.parse_unary_expression()!
		left = ast.BinaryExpression{
			left:  left
			right: right
			op:    ast.Operator{
				kind: operator
			}
			span:  span
		}
	}

	return left
}

fn (mut p Parser) parse_unary_expression() !ast.Expression {
	if p.current_token.kind == .punc_exclamation_mark {
		start := p.current_span()
		p.eat(.punc_exclamation_mark)!
		inner := p.parse_unary_expression()!

		return ast.UnaryExpression{
			expression: inner
			op:         ast.Operator{
				kind: .punc_exclamation_mark
			}
			span:       p.span_from(start)
		}
	}

	if p.current_token.kind == .punc_minus {
		start := p.current_span()
		p.eat(.punc_minus)!
		inner := p.parse_unary_expression()!

		return ast.UnaryExpression{
			expression: inner
			op:         ast.Operator{
				kind: .punc_minus
			}
			span:       p.span_from(start)
		}
	}

	return p.parse_postfix_expression()!
}

fn (mut p Parser) parse_postfix_expression() !ast.Expression {
	mut expr := p.parse_primary_expression()!

	for {
		match p.current_token.kind {
			.punc_dot {
				expr = p.parse_dot_expression(expr)!
			}
			.punc_open_bracket {
				// only treat as array index if there's no whitespace before the bracket
				// this allows `arr[0]` but not `arr [0]` or `x = 1\n[1, 2]`
				if p.current_token.leading_trivia.len > 0 {
					break
				}
				start := expr.span
				p.eat(.punc_open_bracket)!
				index := p.parse_expression()!
				p.eat(.punc_close_bracket)!
				expr = ast.ArrayIndexExpression{
					expression: expr
					index:      index
					span:       p.span_from(start)
				}
			}
			.punc_dotdot {
				start := expr.span
				p.eat(.punc_dotdot)!
				end := p.parse_expression()!
				expr = ast.RangeExpression{
					start: expr
					end:   end
					span:  p.span_from(start)
				}
			}
			.punc_plusplus {
				return error('Increment operator (++) is not supported. Values are immutable in AL - use `x = x + 1` with shadowing instead.')
			}
			.punc_minusminus {
				return error('Decrement operator (--) is not supported. Values are immutable in AL - use `x = x - 1` with shadowing instead.')
			}
			else {
				break
			}
		}
	}

	return expr
}

fn (mut p Parser) parse_primary_expression() !ast.Expression {
	expr := match p.current_token.kind {
		.literal_string {
			p.parse_string_expression()!
		}
		.interp_string_start {
			p.parse_interpolated_string()!
		}
		.literal_number {
			p.parse_number_expression()!
		}
		.identifier {
			p.parse_identifier_or_call()!
		}
		.punc_open_paren {
			p.parse_tuple()!
		}
		.kw_none {
			span := p.current_span()
			p.eat(.kw_none)!
			ast.NoneExpression{
				span: span
			}
		}
		.kw_true {
			span := p.current_span()
			p.eat(.kw_true)!
			ast.BooleanLiteral{
				value: true
				span:  span
			}
		}
		.kw_false {
			span := p.current_span()
			p.eat(.kw_false)!
			ast.BooleanLiteral{
				value: false
				span:  span
			}
		}
		.punc_open_brace {
			p.parse_block_expression()!
		}
		.punc_open_bracket {
			p.parse_array_expression()!
		}
		.kw_if {
			p.parse_if_expression()!
		}
		.kw_match {
			p.parse_match_expression()!
		}
		.kw_function {
			p.parse_function_expression()!
		}
		.kw_error {
			p.parse_error_expression()!
		}
		.error {
			span := p.current_span()
			p.advance()
			ast.ErrorNode{
				message: 'Scanner error'
				span:    span
			}
		}
		else {
			return error("Unexpected '${p.current_token}'")
		}
	}

	return expr
}

fn (mut p Parser) parse_identifier_or_call() !ast.Expression {
	span := p.current_span()
	name := p.eat_token_literal(.identifier, 'Expected identifier')!

	if p.current_token.kind == .punc_open_paren {
		// Could be function call or struct init with type args like Box(Int){ ... }
		// Save position and try to parse as type args first
		saved_index := p.index
		saved_token := p.current_token

		if type_args := p.try_parse_type_args() {
			// If next token is {, it's struct init with type args
			if p.current_token.kind == .punc_open_brace {
				return p.parse_struct_init_with_type_args(name, span, type_args)!
			}
		}

		// Restore position and parse as function call
		p.index = saved_index
		p.current_token = saved_token
		return p.parse_function_call_expression(name, span)!
	}

	if p.current_token.kind == .punc_open_brace {
		curr_index := p.index
		curr_token := p.current_token

		if result := p.parse_struct_init_expression(name, span) {
			return result
		}

		p.index = curr_index
		p.current_token = curr_token
	}

	return ast.Identifier{
		name: name
		span: span
	}
}

fn (mut p Parser) parse_block_expression() !ast.Expression {
	block_span := p.current_span()
	p.eat(.punc_open_brace)!
	p.push_context(.block)

	mut body := []ast.Node{}

	for p.current_token.kind != .punc_close_brace && p.current_token.kind != .eof {
		span := p.current_span()
		node := p.parse_node() or {
			p.add_error(err.msg())
			p.synchronize()
			err_expr := ast.Expression(ast.ErrorNode{
				message: err.msg()
				span:    span
			})
			body << ast.Node(err_expr)
			continue
		}
		body << node
	}

	p.pop_context()
	p.eat(.punc_close_brace)!

	return ast.BlockExpression{
		body: body
		span: p.span_from(block_span)
	}
}

fn (mut p Parser) parse_array_expression() !ast.Expression {
	span := p.current_span()
	p.eat(.punc_open_bracket)!
	p.push_context(.array)

	mut elements := []ast.ArrayElement{}

	for p.current_token.kind != .punc_close_bracket && p.current_token.kind != .eof {
		elem_span := p.current_span()

		if p.current_token.kind == .punc_dotdot {
			spread_span := p.current_span()
			p.eat(.punc_dotdot)!

			// anonymous spread (.. followed by ] or , or else)
			if p.current_token.kind in [.punc_close_bracket, .punc_comma, .kw_else] {
				if p.current_token.kind == .kw_else {
					p.advance() // skip the erroneous 'else' token
				}
				elements << ast.SpreadElement{
					expression: none
					span:       spread_span
				}
			} else {
				inner := p.parse_expression() or {
					p.add_error(err.msg())
					p.synchronize()
					if p.current_context() != .array {
						// synchronize consumed the ] and popped context
						return ast.ArrayExpression{
							elements: elements
							span:     span
						}
					}
					elements << ast.Expression(ast.ErrorNode{
						message: err.msg()
						span:    elem_span
					})
					continue
				}
				elements << ast.SpreadElement{
					expression: inner
					span:       spread_span
				}
			}
		} else {
			expr := p.parse_expression() or {
				p.add_error(err.msg())
				p.synchronize()
				if p.current_context() != .array {
					// synchronize consumed the ] and popped context
					return ast.ArrayExpression{
						elements: elements
						span:     span
					}
				}
				elements << ast.Expression(ast.ErrorNode{
					message: err.msg()
					span:    elem_span
				})
				continue
			}
			elements << expr
		}

		if p.current_token.kind == .punc_comma {
			p.eat(.punc_comma)!
		} else {
			break
		}
	}

	p.pop_context()
	p.eat(.punc_close_bracket)!

	return ast.ArrayExpression{
		elements: elements
		span:     span
	}
}

fn (mut p Parser) parse_tuple() !ast.Expression {
	start := p.current_span()
	p.eat(.punc_open_paren)!

	if p.current_token.kind == .punc_close_paren {
		return error('Empty tuple () is not allowed. Use `none` instead.')
	}

	mut elements := []ast.Expression{}
	elements << p.parse_expression()!

	for p.current_token.kind == .punc_comma {
		p.eat(.punc_comma)!
		if p.current_token.kind == .punc_close_paren {
			break
		}
		elements << p.parse_expression()!
	}

	p.eat(.punc_close_paren)!
	return ast.TupleExpression{
		elements: elements
		span:     p.span_from(start)
	}
}

fn (mut p Parser) parse_if_expression() !ast.Expression {
	span := p.current_span()
	p.eat(.kw_if)!

	condition := p.parse_expression()!
	body := p.parse_expression()!

	mut else_body := ?ast.Expression(none)

	if p.current_token.kind == .kw_else {
		p.eat(.kw_else)!
		else_body = p.parse_expression()!
	}

	return ast.IfExpression{
		condition: condition
		body:      body
		span:      span
		else_body: else_body
	}
}

fn (mut p Parser) parse_match_expression() !ast.Expression {
	match_span := p.current_span()
	p.eat(.kw_match)!

	subject := p.parse_expression()!

	p.eat(.punc_open_brace)!
	p.push_context(.match_arms)

	mut arms := []ast.MatchArm{}

	for p.current_token.kind != .punc_close_brace && p.current_token.kind != .eof {
		first_span := p.current_span()
		first_pattern := if p.current_token.kind == .kw_else {
			span := p.current_span()
			p.eat(.kw_else)!
			ast.Expression(ast.WildcardPattern{
				span: span
			})
		} else {
			p.parse_expression() or {
				p.add_error(err.msg())
				p.synchronize()
				if p.current_token.kind == .punc_close_brace {
					break
				}
				continue
			}
		}

		pattern := if p.current_token.kind == .bitwise_or {
			mut patterns := [first_pattern]
			for p.current_token.kind == .bitwise_or {
				p.eat(.bitwise_or)!
				next_pattern := p.parse_expression() or {
					p.add_error(err.msg())
					p.synchronize()
					break
				}
				patterns << next_pattern
			}
			ast.Expression(ast.OrPattern{
				patterns: patterns
				span:     first_span
			})
		} else {
			first_pattern
		}

		p.eat(.punc_arrow) or {
			p.add_error(err.msg())
			p.synchronize()
			if p.current_token.kind == .punc_close_brace {
				break
			}
			continue
		}

		body_span := p.current_span()
		body := p.parse_expression() or {
			p.add_error(err.msg())
			p.synchronize()
			if p.current_token.kind == .punc_close_brace {
				break
			}
			ast.ErrorNode{
				message: err.msg()
				span:    body_span
			}
		}

		arms << ast.MatchArm{
			pattern: pattern
			body:    body
		}

		if p.current_token.kind == .punc_comma {
			p.eat(.punc_comma)!
		}
	}

	p.pop_context()
	p.eat(.punc_close_brace)!

	return ast.MatchExpression{
		subject: subject
		arms:    arms
		span:    p.span_from(match_span)
	}
}

fn (p Parser) extract_doc_comment() ?string {
	for trivia in p.current_token.leading_trivia {
		if trivia.kind == .doc_comment {
			return trivia.text
		}
	}

	return none
}

fn (mut p Parser) parse_function() !ast.Node {
	// Check if next token is identifier (declaration) or paren (expression)
	if next := p.peek_next() {
		if next.kind == .identifier {
			return ast.Node(p.parse_function_declaration()!)
		}
	}
	return ast.Node(p.parse_function_expression()!)
}

fn (mut p Parser) parse_function_declaration() !ast.Statement {
	doc := p.extract_doc_comment()

	fn_span := p.current_span()
	p.eat(.kw_function)!

	id_span := p.current_span()
	name := p.eat_token_literal(.identifier, 'Expected function name')!

	params := p.parse_parameters()!
	return_type, error_type := p.parse_function_return_types()!
	body := p.parse_block_expression()!

	return ast.FunctionDeclaration{
		doc:         doc
		identifier:  ast.Identifier{
			name: name
			span: id_span
		}
		params:      params
		return_type: return_type
		error_type:  error_type
		body:        body
		span:        fn_span
	}
}

fn (mut p Parser) parse_function_expression() !ast.Expression {
	fn_span := p.current_span()
	p.eat(.kw_function)!

	params := p.parse_parameters()!
	return_type, error_type := p.parse_function_return_types()!
	body := p.parse_block_expression()!

	return ast.FunctionExpression{
		params:      params
		return_type: return_type
		error_type:  error_type
		body:        body
		span:        fn_span
	}
}

fn (mut p Parser) parse_function_return_types() !(?ast.TypeIdentifier, ?ast.TypeIdentifier) {
	mut return_type := ?ast.TypeIdentifier(none)
	mut error_type := ?ast.TypeIdentifier(none)

	if p.current_token.kind == .punc_question_mark {
		span := p.current_span()
		p.eat(.punc_question_mark)!
		if p.current_token.kind == .punc_open_brace {
			return_type = ast.TypeIdentifier{
				is_option:  true
				identifier: ast.Identifier{
					name: 'None'
					span: span
				}
				span:       span
			}
		} else {
			inner := p.parse_type_identifier()!
			return_type = ast.TypeIdentifier{
				is_option:   true
				is_array:    inner.is_array
				is_function: inner.is_function
				identifier:  inner.identifier
				param_types: inner.param_types
				return_type: inner.return_type
				error_type:  inner.error_type
				span:        inner.span
			}
		}
	} else if p.current_token.kind == .identifier || p.current_token.kind == .punc_open_bracket {
		return_type = p.parse_type_identifier()!
	}

	if p.current_token.kind == .punc_exclamation_mark {
		p.eat(.punc_exclamation_mark)!
		error_type = p.parse_type_identifier()!
	}

	return return_type, error_type
}

fn (mut p Parser) parse_parameters() ![]ast.FunctionParameter {
	p.eat(.punc_open_paren)!
	p.push_context(.function_params)

	mut params := []ast.FunctionParameter{}

	for p.current_token.kind != .punc_close_paren && p.current_token.kind != .eof {
		param := p.parse_parameter()!
		params << param

		if p.current_token.kind == .punc_comma {
			p.eat(.punc_comma)!
		}
	}

	p.pop_context()
	p.eat(.punc_close_paren)!

	return params
}

fn (mut p Parser) parse_parameter() !ast.FunctionParameter {
	span := p.current_span()
	name := p.eat_token_literal(.identifier, 'Expected parameter name')!

	mut typ := ?ast.TypeIdentifier(none)

	if p.current_token.kind == .identifier || p.current_token.kind == .punc_open_bracket
		|| p.current_token.kind == .punc_question_mark || p.current_token.kind == .kw_function {
		typ = p.parse_type_identifier()!
	}

	return ast.FunctionParameter{
		typ:        typ
		identifier: ast.Identifier{
			name: name
			span: span
		}
	}
}

fn (mut p Parser) is_type_start() bool {
	if p.current_token.kind == .punc_question_mark {
		return true
	}

	if p.current_token.kind == .punc_open_bracket {
		if next := p.peek_next() {
			return next.kind == .punc_close_bracket
		}
		return false
	}

	if p.current_token.kind == .identifier {
		if name := p.current_token.literal {
			return name.len > 0 && name[0] >= `A` && name[0] <= `Z`
		}
	}

	return false
}

fn (mut p Parser) is_type_start_at_next() bool {
	next := p.peek_next() or { return false }

	if next.kind == .punc_question_mark {
		return true
	}

	if next.kind == .punc_open_bracket {
		return true
	}

	if next.kind == .identifier {
		if name := next.literal {
			return name.len > 0 && name[0] >= `A` && name[0] <= `Z`
		}
	}

	return false
}

fn (mut p Parser) parse_type_identifier() !ast.TypeIdentifier {
	mut is_option := false

	if p.current_token.kind == .punc_question_mark {
		is_option = true
		p.eat(.punc_question_mark)!
	}

	// Array type: []T where T can itself be an array type
	if p.current_token.kind == .punc_open_bracket {
		start_span := p.current_span()
		p.eat(.punc_open_bracket)!
		p.eat(.punc_close_bracket)!
		inner := p.parse_type_identifier()!
		return ast.TypeIdentifier{
			is_option:    is_option
			is_array:     true
			element_type: &inner
			identifier:   ast.Identifier{
				span: start_span
			}
			span:         p.span_from(start_span)
		}
	}

	if p.current_token.kind == .kw_function {
		return p.parse_function_type(is_option)!
	}

	span := p.current_span()
	name := p.eat_token_literal(.identifier, 'Expected type name')!

	mut type_args := []ast.TypeIdentifier{}
	if p.current_token.kind == .punc_open_paren {
		p.eat(.punc_open_paren)!
		for p.current_token.kind != .punc_close_paren && p.current_token.kind != .eof {
			arg := p.parse_type_identifier()!
			type_args << arg
			if p.current_token.kind == .punc_comma {
				p.eat(.punc_comma)!
			}
		}
		p.eat(.punc_close_paren)!
	}

	return ast.TypeIdentifier{
		is_option:  is_option
		is_array:   false
		identifier: ast.Identifier{
			name: name
			span: span
		}
		type_args:  type_args
		span:       p.span_from(span)
	}
}

fn (mut p Parser) parse_function_type(is_option bool) !ast.TypeIdentifier {
	span := p.current_span()
	p.eat(.kw_function)!
	p.eat(.punc_open_paren)!

	mut param_types := []ast.TypeIdentifier{}

	for p.current_token.kind != .punc_close_paren && p.current_token.kind != .eof {
		param_type := p.parse_type_identifier()!
		param_types << param_type

		if p.current_token.kind == .punc_comma {
			p.eat(.punc_comma)!
		}
	}

	p.eat(.punc_close_paren)!

	mut return_type := ?&ast.TypeIdentifier(none)
	mut error_type := ?&ast.TypeIdentifier(none)

	if p.current_token.kind == .identifier || p.current_token.kind == .punc_open_bracket
		|| p.current_token.kind == .punc_question_mark || p.current_token.kind == .kw_function {
		mut ret := p.parse_type_identifier()!
		return_type = &ret

		if p.current_token.kind == .punc_exclamation_mark {
			p.eat(.punc_exclamation_mark)!
			mut err := p.parse_type_identifier()!
			error_type = &err
		}
	}

	return ast.TypeIdentifier{
		is_option:   is_option
		is_function: true
		identifier:  ast.Identifier{
			name: 'fn'
			span: span
		}
		param_types: param_types
		return_type: return_type
		error_type:  error_type
		span:        p.span_from(span)
	}
}

fn (mut p Parser) parse_type_params() ![]ast.Identifier {
	mut params := []ast.Identifier{}

	if p.current_token.kind != .punc_open_paren {
		return params
	}

	p.eat(.punc_open_paren)!

	for p.current_token.kind != .punc_close_paren && p.current_token.kind != .eof {
		param_span := p.current_span()
		param_name := p.eat_token_literal(.identifier, 'Expected type parameter name')!

		params << ast.Identifier{
			name: param_name
			span: param_span
		}

		if p.current_token.kind == .punc_comma {
			p.eat(.punc_comma)!
		}
	}

	p.eat(.punc_close_paren)!

	return params
}

fn (mut p Parser) parse_struct_declaration() !ast.Statement {
	doc := p.extract_doc_comment()

	struct_span := p.current_span()
	p.eat(.kw_struct)!

	id_span := p.current_span()
	name := p.eat_token_literal(.identifier, 'Expected struct name')!

	type_params := p.parse_type_params()!

	p.eat(.punc_open_brace)!
	p.push_context(.struct_def)

	mut fields := []ast.StructField{}

	for p.current_token.kind != .punc_close_brace && p.current_token.kind != .eof {
		field := p.parse_struct_field()!
		fields << field
	}

	p.pop_context()
	p.eat(.punc_close_brace)!

	return ast.StructDeclaration{
		doc:         doc
		identifier:  ast.Identifier{
			name: name
			span: id_span
		}
		type_params: type_params
		fields:      fields
		span:        p.span_from(struct_span)
	}
}

fn (mut p Parser) parse_struct_field() !ast.StructField {
	doc := p.extract_doc_comment()

	span := p.current_span()
	name := p.eat_token_literal(.identifier, 'Expected field name')!

	typ := p.parse_type_identifier()!

	mut init := ?ast.Expression(none)

	if p.current_token.kind == .punc_equals {
		p.eat(.punc_equals)!
		init = p.parse_expression()!
	}

	if p.current_token.kind == .punc_comma {
		p.add_warning('Trailing comma after struct field is deprecated')
		p.eat(.punc_comma)!
	}

	return ast.StructField{
		doc:        doc
		identifier: ast.Identifier{
			name: name
			span: span
		}
		typ:        typ
		init:       init
	}
}

fn (mut p Parser) parse_enum_declaration() !ast.Statement {
	doc := p.extract_doc_comment()

	enum_span := p.current_span()
	p.eat(.kw_enum)!

	id_span := p.current_span()
	name := p.eat_token_literal(.identifier, 'Expected enum name')!

	type_params := p.parse_type_params()!

	p.eat(.punc_open_brace)!
	p.push_context(.enum_def)

	mut variants := []ast.EnumVariant{}

	for p.current_token.kind != .punc_close_brace && p.current_token.kind != .eof {
		variant := p.parse_enum_variant()!
		variants << variant
	}

	p.pop_context()
	p.eat(.punc_close_brace)!

	return ast.EnumDeclaration{
		doc:         doc
		identifier:  ast.Identifier{
			name: name
			span: id_span
		}
		type_params: type_params
		variants:    variants
		span:        p.span_from(enum_span)
	}
}

fn (mut p Parser) parse_enum_variant() !ast.EnumVariant {
	doc := p.extract_doc_comment()

	span := p.current_span()
	name := p.eat_token_literal(.identifier, 'Expected variant name')!

	mut payload := []ast.TypeIdentifier{}

	if p.current_token.kind == .punc_open_paren {
		p.eat(.punc_open_paren)!
		for p.current_token.kind != .punc_close_paren {
			payload << p.parse_type_identifier()!
			if p.current_token.kind == .punc_comma {
				p.eat(.punc_comma)!
			} else {
				break
			}
		}
		p.eat(.punc_close_paren)!
	}

	if p.current_token.kind == .punc_comma {
		p.add_warning('Trailing comma after enum variant is deprecated')
		p.eat(.punc_comma)!
	}

	return ast.EnumVariant{
		doc:        doc
		identifier: ast.Identifier{
			name: name
			span: span
		}
		payload:    payload
	}
}

fn (mut p Parser) parse_struct_init_expression(name string, name_span sp.Span) !ast.Expression {
	p.eat(.punc_open_brace)!
	p.push_context(.struct_init)

	mut fields := []ast.StructInitField{}

	for p.current_token.kind != .punc_close_brace && p.current_token.kind != .eof {
		field_span := p.current_span()
		field_name := p.eat_token_literal(.identifier, 'Expected field name')!
		p.eat(.punc_colon)!
		value := p.parse_expression()!

		fields << ast.StructInitField{
			identifier: ast.Identifier{
				name: field_name
				span: field_span
			}
			init:       value
		}

		if p.current_token.kind == .punc_comma {
			p.eat(.punc_comma)!
		}
	}

	p.pop_context()
	p.eat(.punc_close_brace)!

	return ast.StructInitExpression{
		identifier: ast.Identifier{
			name: name
			span: name_span
		}
		fields:     fields
		span:       p.span_from(name_span)
	}
}

// Try to parse type args like (Int, String). Returns none if not valid type args.
fn (mut p Parser) try_parse_type_args() ?[]ast.TypeIdentifier {
	if p.current_token.kind != .punc_open_paren {
		return none
	}
	p.eat(.punc_open_paren) or { return none }

	mut type_args := []ast.TypeIdentifier{}
	for p.current_token.kind != .punc_close_paren && p.current_token.kind != .eof {
		// Type args should be type identifiers (uppercase or type syntax)
		if !p.is_type_start() {
			return none
		}
		type_arg := p.parse_type_identifier() or { return none }
		type_args << type_arg
		if p.current_token.kind == .punc_comma {
			p.eat(.punc_comma) or { return none }
		}
	}
	p.eat(.punc_close_paren) or { return none }
	return type_args
}

fn (mut p Parser) parse_struct_init_with_type_args(name string, name_span sp.Span, type_args []ast.TypeIdentifier) !ast.Expression {
	p.eat(.punc_open_brace)!
	p.push_context(.struct_init)

	mut fields := []ast.StructInitField{}

	for p.current_token.kind != .punc_close_brace && p.current_token.kind != .eof {
		field_span := p.current_span()
		field_name := p.eat_token_literal(.identifier, 'Expected field name')!
		p.eat(.punc_colon)!
		value := p.parse_expression()!

		fields << ast.StructInitField{
			identifier: ast.Identifier{
				name: field_name
				span: field_span
			}
			init:       value
		}

		if p.current_token.kind == .punc_comma {
			p.eat(.punc_comma)!
		}
	}

	p.pop_context()
	p.eat(.punc_close_brace)!

	return ast.StructInitExpression{
		identifier: ast.Identifier{
			name: name
			span: name_span
		}
		type_args:  type_args
		fields:     fields
		span:       p.span_from(name_span)
	}
}

fn (mut p Parser) parse_const_binding() !ast.Statement {
	doc := p.extract_doc_comment()
	span := p.current_span()
	p.eat(.kw_const)!

	name_span := p.current_span()
	name := p.eat_token_literal(.identifier, 'Expected const name')!

	mut typ := ?ast.TypeIdentifier(none)
	if p.is_type_start() {
		typ = p.parse_type_identifier()!
	}

	p.eat(.punc_equals)!

	init := p.parse_expression()!

	return ast.ConstBinding{
		doc:        doc
		identifier: ast.Identifier{
			name: name
			span: name_span
		}
		typ:        typ
		init:       init
		span:       span
	}
}

fn (p Parser) is_tuple_destructuring() bool {
	mut depth := 0
	mut i := p.index

	for i < p.tokens.len {
		tok := p.tokens[i]
		if tok.kind == .punc_open_paren {
			depth++
		} else if tok.kind == .punc_close_paren {
			depth--
			if depth == 0 {
				if i + 1 < p.tokens.len && p.tokens[i + 1].kind == .punc_equals {
					return true
				}
				return false
			}
		} else if tok.kind == .eof {
			return false
		}
		i++
	}
	return false
}

fn (mut p Parser) parse_tuple_destructuring() !ast.Statement {
	span := p.current_span()
	p.eat(.punc_open_paren)!

	mut patterns := []ast.Expression{}

	for p.current_token.kind != .punc_close_paren && p.current_token.kind != .eof {
		pattern_span := p.current_span()

		if p.current_token.kind == .identifier {
			name := p.eat_token_literal(.identifier, 'Expected identifier')!

			if name.len > 0 && name[0] >= `A` && name[0] <= `Z` {
				patterns << ast.TypeIdentifier{
					identifier: ast.Identifier{
						name: name
						span: pattern_span
					}
					span:       pattern_span
				}
			} else {
				patterns << ast.Identifier{
					name: name
					span: pattern_span
				}
			}
		} else {
			return error('Expected identifier in tuple destructuring pattern')
		}

		if p.current_token.kind == .punc_comma {
			p.eat(.punc_comma)!
		} else {
			break
		}
	}

	p.eat(.punc_close_paren)!
	p.eat(.punc_equals)!
	init := p.parse_expression()!

	return ast.TupleDestructuringBinding{
		patterns: patterns
		init:     init
		span:     p.span_from(span)
	}
}

fn (mut p Parser) parse_binding() !ast.Statement {
	doc := p.extract_doc_comment()
	span := p.current_span()
	name := p.eat_token_literal(.identifier, 'Expected identifier')!

	// Check for type annotation
	mut typ := ?ast.TypeIdentifier(none)
	if p.is_type_start() {
		typ = p.parse_type_identifier()!
	}

	p.eat(.punc_equals)!
	init := p.parse_expression()!

	// Uppercase name followed by = is a type pattern binding
	if name.len > 0 && name[0] >= `A` && name[0] <= `Z` && typ == none {
		return ast.TypePatternBinding{
			typ:  ast.TypeIdentifier{
				identifier: ast.Identifier{
					name: name
					span: span
				}
				span:       span
			}
			init: init
			span: p.span_from(span)
		}
	}

	return ast.VariableBinding{
		doc:        doc
		identifier: ast.Identifier{
			name: name
			span: span
		}
		typ:        typ
		init:       init
		span:       p.span_from(span)
	}
}

fn (mut p Parser) parse_export_declaration() !ast.Statement {
	span := p.current_span()
	p.eat(.kw_export)!

	// Export must be followed by a statement (function, struct, enum, const, or binding)
	decl := match p.current_token.kind {
		.kw_function { p.parse_function_declaration()! }
		.kw_struct { p.parse_struct_declaration()! }
		.kw_enum { p.parse_enum_declaration()! }
		.kw_const { p.parse_const_binding()! }
		.identifier { p.parse_binding()! }
		else { return error('Expected declaration after export') }
	}

	return ast.ExportDeclaration{
		declaration: decl
		span:        span
	}
}

fn (mut p Parser) parse_import_declaration() !ast.Statement {
	import_span := p.current_span()
	p.eat(.kw_from)!

	path_token := p.eat(.literal_string)!
	path := path_token.literal or { return error('Expected string literal for import path') }

	p.eat(.kw_import)!

	mut specifiers := []ast.ImportSpecifier{}
	p.parse_import_specifiers(mut specifiers)!

	return ast.ImportDeclaration{
		path:       path
		specifiers: specifiers
		span:       import_span
	}
}

fn (mut p Parser) parse_import_specifiers(mut specifiers []ast.ImportSpecifier) ! {
	span := p.current_span()
	name := p.eat_token_literal(.identifier, 'Expected import specifier')!

	specifiers << ast.ImportSpecifier{
		identifier: ast.Identifier{
			name: name
			span: span
		}
	}

	if p.current_token.kind == .punc_comma {
		p.eat(.punc_comma)!
		p.parse_import_specifiers(mut specifiers)!
	}
}

fn (mut p Parser) parse_error_expression() !ast.Expression {
	start := p.current_span()
	p.eat(.kw_error)!

	expr := p.parse_unary_expression()!

	return ast.ErrorExpression{
		expression: expr
		span:       p.span_from(start)
	}
}

fn (mut p Parser) parse_dot_expression(left ast.Expression) !ast.Expression {
	start := left.span
	p.eat(.punc_dot)!

	span := p.current_span()

	if p.current_token.kind == .literal_number {
		num_str := p.eat_token_literal(.literal_number, 'Expected tuple index')!

		if num_str.contains('.') {
			parts := num_str.split('.')
			mut result := ast.Expression(ast.PropertyAccessExpression{
				left:  left
				right: ast.NumberLiteral{
					value: parts[0]
					span:  span
				}
				span:  p.span_from(start)
			})
			for i := 1; i < parts.len; i++ {
				result = ast.PropertyAccessExpression{
					left:  result
					right: ast.NumberLiteral{
						value: parts[i]
						span:  span
					}
					span:  p.span_from(start)
				}
			}
			return result
		}

		return ast.PropertyAccessExpression{
			left:  left
			right: ast.NumberLiteral{
				value: num_str
				span:  span
			}
			span:  p.span_from(start)
		}
	}

	property := p.eat_token_literal(.identifier, 'Expected property name')!

	if p.current_token.kind == .punc_open_paren {
		call := p.parse_function_call_expression(property, span)!
		return ast.PropertyAccessExpression{
			left:  left
			right: call
			span:  p.span_from(start)
		}
	}

	return ast.PropertyAccessExpression{
		left:  left
		right: ast.Identifier{
			name: property
			span: span
		}
		span:  p.span_from(start)
	}
}

fn (mut p Parser) parse_function_call_expression(name string, name_span sp.Span) !ast.Expression {
	p.eat(.punc_open_paren)!

	mut arguments := []ast.Expression{}

	for p.current_token.kind != .punc_close_paren {
		arguments << p.parse_expression()!

		if p.current_token.kind == .punc_comma {
			p.eat(.punc_comma)!
		}
	}

	p.eat(.punc_close_paren)!

	return ast.FunctionCallExpression{
		identifier: ast.Identifier{
			name: name
			span: name_span
		}
		arguments:  arguments
		span:       p.span_from(name_span)
	}
}

fn (mut p Parser) parse_string_expression() !ast.Expression {
	span := p.current_span()
	return ast.StringLiteral{
		value: p.eat_token_literal(.literal_string, 'Expected string')!
		span:  span
	}
}

fn (mut p Parser) parse_interpolated_string() !ast.Expression {
	span := p.current_span()
	p.eat(.interp_string_start)!

	mut parts := []ast.Expression{}

	for {
		match p.current_token.kind {
			.interp_string_part {
				part_span := p.current_span()
				value := p.current_token.literal or { '' }
				p.eat(.interp_string_part)!
				parts << ast.StringLiteral{
					value: value
					span:  part_span
				}
			}
			.interp_string_end {
				p.eat(.interp_string_end)!
				break
			}
			.punc_open_brace {
				p.eat(.punc_open_brace)!
				expr := p.parse_expression()!
				parts << expr
				p.eat(.punc_close_brace)!
			}
			.identifier {
				ident_span := p.current_span()
				name := p.current_token.literal or { '' }
				p.eat(.identifier)!
				parts << ast.Identifier{
					name: name
					span: ident_span
				}
			}
			else {
				return error('Unexpected token in interpolated string: ${p.current_token.kind}')
			}
		}
	}

	return ast.InterpolatedString{
		parts: parts
		span:  span
	}
}

fn (mut p Parser) parse_number_expression() !ast.Expression {
	span := p.current_span()
	return ast.NumberLiteral{
		value: p.eat_token_literal(.literal_number, 'Expected number')!
		span:  span
	}
}
