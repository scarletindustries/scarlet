module parser

import compiler.scanner
import compiler.token
import compiler.ast
import compiler
import compiler.diagnostic

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
	tokens []compiler.Token
mut:
	index         int
	current_token compiler.Token
	diagnostics   []diagnostic.Diagnostic
	context_stack []ParseContext
}

pub fn new_parser(mut s scanner.Scanner) Parser {
	tokens := s.scan_all()

	return Parser{
		tokens:        tokens
		index:         0
		current_token: tokens[0]
		diagnostics:   []diagnostic.Diagnostic{}
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
	p.diagnostics << diagnostic.error_at(p.current_token.line, p.current_token.column, message)
}

fn (mut p Parser) add_warning(message string) {
	p.diagnostics << diagnostic.warning_at(p.current_token.line, p.current_token.column, message)
}

fn (mut p Parser) synchronize() {
	ctx := p.current_context()
	mut iterations := 0

	for p.current_token.kind != .eof {
		iterations++
		if iterations > 1000 {
			p.add_error('Parser recovery failed: likely infinite loop detected. This is a bug in the parser.')
			// Skip to EOF to prevent cascading issues
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
		p.index++
		p.current_token = p.tokens[p.index]
	}
}

fn (mut p Parser) eat(kind token.Kind) !compiler.Token {
	if p.current_token.kind == kind {
		old := p.current_token

		p.index = p.index + 1
		p.current_token = p.tokens[p.index]

		return old
	}

	return error("Expected '${kind}', got '${p.current_token}'")
}

fn (mut p Parser) eat_msg(kind token.Kind, message string) !compiler.Token {
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
	mut program := ast.BlockExpression{}

	for p.current_token.kind != .eof {
		expr := p.parse_expression() or {
			// Record the error as a diagnostic
			p.add_error(err.msg())
			// Synchronize to find a recovery point
			p.synchronize()
			// Add an error node to mark the failed parse
			program.body << ast.ErrorNode{
				message: err.msg()
			}
			continue
		}

		program.body << expr
	}

	return ParseResult{
		ast:         program
		diagnostics: p.diagnostics
	}
}

fn (mut p Parser) peek_next() ?compiler.Token {
	if p.index + 1 < p.tokens.len {
		return p.tokens[p.index + 1]
	}

	return none
}

fn (mut p Parser) peek_ahead(distance int) ?compiler.Token {
	if p.index + distance < p.tokens.len {
		return p.tokens[p.index + distance]
	}

	return none
}

// Main expression parser - everything is an expression
fn (mut p Parser) parse_expression() !ast.Expression {
	return p.parse_or_expression()!
}

// Handle `expr or { ... }` or `expr or err => ...` - lowest precedence
fn (mut p Parser) parse_or_expression() !ast.Expression {
	mut left := p.parse_binary_expression()!

	if p.current_token.kind == .kw_or {
		p.eat(.kw_or)!

		mut receiver := ?ast.Identifier(none)

		// Check for optional receiver: `or err => body`
		if p.current_token.kind == .identifier {
			if next := p.peek_next() {
				// If next token is => then this identifier is the receiver
				if next.kind == .punc_arrow {
					name := p.eat_token_literal(.identifier, 'Expected identifier for or receiver')!
					receiver = ast.Identifier{
						name: name
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
		}
	}

	return left
}

// Precedence levels (lowest to highest):
// 1. || (logical or)
// 2. && (logical and)
// 3. ==, != (equality)
// 4. <, >, <=, >= (comparison)
// 5. +, - (additive)
// 6. *, /, % (multiplicative)
// 7. unary (!, -)
// 8. postfix (., [], !)

fn (mut p Parser) parse_binary_expression() !ast.Expression {
	return p.parse_logical_or()!
}

// Level 1: ||
fn (mut p Parser) parse_logical_or() !ast.Expression {
	mut left := p.parse_logical_and()!

	for p.current_token.kind == .logical_or {
		p.eat(.logical_or)!
		right := p.parse_logical_and()!
		left = ast.BinaryExpression{
			left:  left
			right: right
			op:    ast.Operator{
				kind: .logical_or
			}
		}
	}

	return left
}

// Level 2: &&
fn (mut p Parser) parse_logical_and() !ast.Expression {
	mut left := p.parse_equality()!

	for p.current_token.kind == .logical_and {
		p.eat(.logical_and)!
		right := p.parse_equality()!
		left = ast.BinaryExpression{
			left:  left
			right: right
			op:    ast.Operator{
				kind: .logical_and
			}
		}
	}

	return left
}

// Level 3: ==, !=
fn (mut p Parser) parse_equality() !ast.Expression {
	mut left := p.parse_comparison()!

	for p.current_token.kind in [.punc_equals_comparator, .punc_not_equal] {
		operator := p.current_token.kind
		p.eat(operator)!
		right := p.parse_comparison()!
		left = ast.BinaryExpression{
			left:  left
			right: right
			op:    ast.Operator{
				kind: operator
			}
		}
	}

	return left
}

// Level 4: <, >, <=, >=
fn (mut p Parser) parse_comparison() !ast.Expression {
	mut left := p.parse_additive()!

	for p.current_token.kind in [.punc_lt, .punc_gt, .punc_lte, .punc_gte] {
		operator := p.current_token.kind
		p.eat(operator)!
		right := p.parse_additive()!
		left = ast.BinaryExpression{
			left:  left
			right: right
			op:    ast.Operator{
				kind: operator
			}
		}
	}

	return left
}

// Level 5: +, -
fn (mut p Parser) parse_additive() !ast.Expression {
	mut left := p.parse_multiplicative()!

	for p.current_token.kind in [.punc_plus, .punc_minus] {
		operator := p.current_token.kind
		p.eat(operator)!
		right := p.parse_multiplicative()!
		left = ast.BinaryExpression{
			left:  left
			right: right
			op:    ast.Operator{
				kind: operator
			}
		}
	}

	return left
}

// Level 6: *, /, %
fn (mut p Parser) parse_multiplicative() !ast.Expression {
	mut left := p.parse_unary_expression()!

	for p.current_token.kind in [.punc_mul, .punc_div, .punc_mod] {
		operator := p.current_token.kind
		p.eat(operator)!
		right := p.parse_unary_expression()!
		left = ast.BinaryExpression{
			left:  left
			right: right
			op:    ast.Operator{
				kind: operator
			}
		}
	}

	return left
}

fn (mut p Parser) parse_unary_expression() !ast.Expression {
	if p.current_token.kind == .punc_exclamation_mark {
		p.eat(.punc_exclamation_mark)!

		return ast.UnaryExpression{
			expression: p.parse_unary_expression()!
			op:         ast.Operator{
				kind: .punc_exclamation_mark
			}
		}
	}

	if p.current_token.kind == .punc_minus {
		p.eat(.punc_minus)!

		return ast.UnaryExpression{
			expression: p.parse_unary_expression()!
			op:         ast.Operator{
				kind: .punc_minus
			}
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
				p.eat(.punc_open_bracket)!
				index := p.parse_expression()!
				p.eat(.punc_close_bracket)!
				expr = ast.ArrayIndexExpression{
					expression: expr
					index:      index
				}
			}
			.punc_exclamation_mark {
				// Postfix ! for error propagation
				p.eat(.punc_exclamation_mark)!
				expr = ast.PropagateExpression{
					expression: expr
				}
			}
			.punc_dotdot {
				p.eat(.punc_dotdot)!
				end := p.parse_expression()!
				expr = ast.RangeExpression{
					start: expr
					end:   end
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

// Primary expressions
fn (mut p Parser) parse_primary_expression() !ast.Expression {
	expr := match p.current_token.kind {
		.literal_string {
			p.parse_string_expression()!
		}
		.literal_string_interpolation {
			p.parse_interpolated_string()!
		}
		.literal_number {
			p.parse_number_expression()!
		}
		.identifier {
			p.parse_identifier_or_binding()!
		}
		.punc_open_paren {
			p.eat(.punc_open_paren)!
			inner := p.parse_expression()!
			p.eat(.punc_close_paren)!
			inner
		}
		.kw_none {
			p.eat(.kw_none)!
			ast.NoneExpression{}
		}
		.kw_true {
			p.eat(.kw_true)!
			ast.BooleanLiteral{
				value: true
			}
		}
		.kw_false {
			p.eat(.kw_false)!
			ast.BooleanLiteral{
				value: false
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
		.kw_struct {
			p.parse_struct_expression()!
		}
		.kw_enum {
			p.parse_enum_expression()!
		}
		.kw_const {
			p.parse_const_binding()!
		}
		.kw_export {
			p.parse_export_expression()!
		}
		.kw_from {
			p.parse_import_declaration()!
		}
		.kw_assert {
			p.parse_assert_expression()!
		}
		.kw_error {
			p.parse_error_expression()!
		}
		else {
			return error("Unexpected '${p.current_token}'")
		}
	}

	return expr
}

// Identifier, function call, struct init, or variable binding
fn (mut p Parser) parse_identifier_or_binding() !ast.Expression {
	name := p.eat_token_literal(.identifier, 'Expected identifier')!

	// Check if this is a variable binding: `x = expr`
	if p.current_token.kind == .punc_equals {
		p.eat(.punc_equals)!
		init := p.parse_expression()!
		return ast.VariableBinding{
			identifier: ast.Identifier{
				name: name
			}
			init:       init
		}
	}

	// Check if this is a function call: `foo(args)`
	if p.current_token.kind == .punc_open_paren {
		return p.parse_function_call_expression(name)!
	}

	// Check if this is struct instantiation: `Foo { field: value }`
	if p.current_token.kind == .punc_open_brace {
		// Try to parse as struct init, backtrack if it fails
		curr_index := p.index
		curr_token := p.current_token

		if result := p.parse_struct_init_expression(name) {
			return result
		}

		// Reset and treat as just an identifier
		p.index = curr_index
		p.current_token = curr_token
	}

	return ast.Identifier{
		name: name
	}
}

// Block expression: { expr1; expr2; expr3 }
fn (mut p Parser) parse_block_expression() !ast.Expression {
	p.eat(.punc_open_brace)!
	p.push_context(.block)

	mut body := []ast.Expression{}

	for p.current_token.kind != .punc_close_brace && p.current_token.kind != .eof {
		expr := p.parse_expression() or {
			p.add_error(err.msg())
			p.synchronize()
			body << ast.ErrorNode{
				message: err.msg()
			}
			continue
		}
		body << expr
	}

	p.pop_context()
	p.eat(.punc_close_brace)!

	return ast.BlockExpression{
		body: body
	}
}

// Array: [1, 2, 3]
fn (mut p Parser) parse_array_expression() !ast.Expression {
	p.eat(.punc_open_bracket)!
	p.push_context(.array)

	mut elements := []ast.Expression{}

	for p.current_token.kind != .punc_close_bracket && p.current_token.kind != .eof {
		expr := p.parse_expression() or {
			p.add_error(err.msg())
			p.synchronize()
			if p.current_token.kind == .punc_close_bracket {
				break
			}
			elements << ast.ErrorNode{
				message: err.msg()
			}
			continue
		}
		elements << expr

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
	}
}

// If expression: if cond expr else expr
// Body can be any expression (block, literal, etc.)
fn (mut p Parser) parse_if_expression() !ast.Expression {
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
		else_body: else_body
	}
}

// Match expression: match subject { pattern => body, ... }
fn (mut p Parser) parse_match_expression() !ast.Expression {
	p.eat(.kw_match)!

	subject := p.parse_expression()!

	p.eat(.punc_open_brace)!
	p.push_context(.match_arms)

	mut arms := []ast.MatchArm{}

	for p.current_token.kind != .punc_close_brace && p.current_token.kind != .eof {
		pattern := if p.current_token.kind == .kw_else {
			p.eat(.kw_else)!
			ast.Expression(ast.WildcardPattern{})
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

		p.eat(.punc_arrow) or {
			p.add_error(err.msg())
			p.synchronize()
			if p.current_token.kind == .punc_close_brace {
				break
			}
			continue
		}

		body := p.parse_expression() or {
			p.add_error(err.msg())
			p.synchronize()
			if p.current_token.kind == .punc_close_brace {
				break
			}
			ast.ErrorNode{
				message: err.msg()
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
	}
}

// Function expression: fn name(params) ReturnType { body }
fn (mut p Parser) parse_function_expression() !ast.Expression {
	p.eat(.kw_function)!

	// Optional name (for anonymous functions)
	mut identifier := ?ast.Identifier(none)
	if p.current_token.kind == .identifier {
		name := p.eat_token_literal(.identifier, 'Expected function name')!
		identifier = ast.Identifier{
			name: name
		}
	}

	// Parameters
	params := p.parse_parameters()!

	// Optional return type
	mut return_type := ?ast.TypeIdentifier(none)
	mut error_type := ?ast.TypeIdentifier(none)

	if p.current_token.kind == .punc_question_mark || p.current_token.kind == .identifier
		|| p.current_token.kind == .punc_open_bracket {
		return_type = p.parse_type_identifier()!
	}

	// Optional error type: !ErrorType
	if p.current_token.kind == .punc_exclamation_mark {
		p.eat(.punc_exclamation_mark)!
		error_type = p.parse_type_identifier()!
	}

	// Body
	body := p.parse_block_expression()!

	return ast.FunctionExpression{
		identifier:  identifier
		params:      params
		return_type: return_type
		error_type:  error_type
		body:        body
	}
}

// Parse function parameters
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

// Parse a single parameter
fn (mut p Parser) parse_parameter() !ast.FunctionParameter {
	name := p.eat_token_literal(.identifier, 'Expected parameter name')!

	mut typ := ?ast.TypeIdentifier(none)

	// Type is optional in some contexts
	if p.current_token.kind == .identifier || p.current_token.kind == .punc_open_bracket
		|| p.current_token.kind == .punc_question_mark {
		typ = p.parse_type_identifier()!
	}

	return ast.FunctionParameter{
		typ:        typ
		identifier: ast.Identifier{
			name: name
		}
	}
}

// Parse a type identifier: ?[]SomeType
fn (mut p Parser) parse_type_identifier() !ast.TypeIdentifier {
	mut is_option := false
	mut is_array := false

	if p.current_token.kind == .punc_question_mark {
		is_option = true
		p.eat(.punc_question_mark)!
	}

	if p.current_token.kind == .punc_open_bracket {
		is_array = true
		p.eat(.punc_open_bracket)!
		p.eat(.punc_close_bracket)!
	}

	name := p.eat_token_literal(.identifier, 'Expected type name')!

	if !token.is_type_identifier(name) {
		return error('Type identifiers must start with capital letters')
	}

	return ast.TypeIdentifier{
		is_option:  is_option
		is_array:   is_array
		identifier: ast.Identifier{
			name: name
		}
	}
}

// Struct definition: struct Name { field Type, ... }
fn (mut p Parser) parse_struct_expression() !ast.Expression {
	p.eat(.kw_struct)!

	name := p.eat_token_literal(.identifier, 'Expected struct name')!

	p.eat(.punc_open_brace)!
	p.push_context(.struct_def)

	mut fields := []ast.StructField{}

	for p.current_token.kind != .punc_close_brace && p.current_token.kind != .eof {
		field := p.parse_struct_field()!
		fields << field
	}

	p.pop_context()
	p.eat(.punc_close_brace)!

	return ast.StructExpression{
		identifier: ast.Identifier{
			name: name
		}
		fields:     fields
	}
}

// Parse struct field: name Type = default
fn (mut p Parser) parse_struct_field() !ast.StructField {
	name := p.eat_token_literal(.identifier, 'Expected field name')!

	typ := p.parse_type_identifier()!

	mut init := ?ast.Expression(none)

	if p.current_token.kind == .punc_equals {
		p.eat(.punc_equals)!
		init = p.parse_expression()!
	}

	if p.current_token.kind == .punc_comma {
		p.eat(.punc_comma)!
	}

	return ast.StructField{
		identifier: ast.Identifier{
			name: name
		}
		typ:        typ
		init:       init
	}
}

// Enum definition: enum Name { Variant, Variant(Type), ... }
fn (mut p Parser) parse_enum_expression() !ast.Expression {
	p.eat(.kw_enum)!

	name := p.eat_token_literal(.identifier, 'Expected enum name')!

	p.eat(.punc_open_brace)!
	p.push_context(.enum_def)

	mut variants := []ast.EnumVariant{}

	for p.current_token.kind != .punc_close_brace && p.current_token.kind != .eof {
		variant := p.parse_enum_variant()!
		variants << variant
	}

	p.pop_context()
	p.eat(.punc_close_brace)!

	return ast.EnumExpression{
		identifier: ast.Identifier{
			name: name
		}
		variants:   variants
	}
}

// Parse enum variant: Name or Name(Type)
fn (mut p Parser) parse_enum_variant() !ast.EnumVariant {
	name := p.eat_token_literal(.identifier, 'Expected variant name')!

	mut payload := ?ast.TypeIdentifier(none)

	// Check for payload type: Variant(Type)
	if p.current_token.kind == .punc_open_paren {
		p.eat(.punc_open_paren)!
		payload = p.parse_type_identifier()!
		p.eat(.punc_close_paren)!
	}

	if p.current_token.kind == .punc_comma {
		p.eat(.punc_comma)!
	}

	return ast.EnumVariant{
		identifier: ast.Identifier{
			name: name
		}
		payload:    payload
	}
}

// Struct instantiation: Name { field: value, ... }
fn (mut p Parser) parse_struct_init_expression(name string) !ast.Expression {
	p.eat(.punc_open_brace)!
	p.push_context(.struct_init)

	mut fields := []ast.StructInitField{}

	for p.current_token.kind != .punc_close_brace && p.current_token.kind != .eof {
		field_name := p.eat_token_literal(.identifier, 'Expected field name')!
		p.eat(.punc_colon)!
		value := p.parse_expression()!

		fields << ast.StructInitField{
			identifier: ast.Identifier{
				name: field_name
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
		}
		fields:     fields
	}
}

// Const binding: const name = expr
fn (mut p Parser) parse_const_binding() !ast.Expression {
	p.eat(.kw_const)!

	name := p.eat_token_literal(.identifier, 'Expected const name')!

	p.eat(.punc_equals)!

	init := p.parse_expression()!

	return ast.ConstBinding{
		identifier: ast.Identifier{
			name: name
		}
		init:       init
	}
}

// Export: export expr
fn (mut p Parser) parse_export_expression() !ast.Expression {
	p.eat(.kw_export)!

	expr := p.parse_expression()!

	return ast.ExportExpression{
		expression: expr
	}
}

// Import: from 'path' import a, b, c
fn (mut p Parser) parse_import_declaration() !ast.Expression {
	p.eat(.kw_from)!

	path_token := p.eat(.literal_string)!
	path := path_token.literal or { return error('Expected string literal for import path') }

	p.eat(.kw_import)!

	mut specifiers := []ast.ImportSpecifier{}
	p.parse_import_specifiers(mut specifiers)!

	return ast.ImportDeclaration{
		path:       path
		specifiers: specifiers
	}
}

fn (mut p Parser) parse_import_specifiers(mut specifiers []ast.ImportSpecifier) ! {
	name := p.eat_token_literal(.identifier, 'Expected import specifier')!

	specifiers << ast.ImportSpecifier{
		identifier: ast.Identifier{
			name: name
		}
	}

	if p.current_token.kind == .punc_comma {
		p.eat(.punc_comma)!
		p.parse_import_specifiers(mut specifiers)!
	}
}

// Assert: assert expr, message
fn (mut p Parser) parse_assert_expression() !ast.Expression {
	p.eat(.kw_assert)!

	expr := p.parse_expression()!

	p.eat(.punc_comma)!

	message := p.parse_expression()!

	return ast.AssertExpression{
		expression: expr
		message:    message
	}
}

// Error expression: error SomeError{}
fn (mut p Parser) parse_error_expression() !ast.Expression {
	p.eat(.kw_error)!

	// Use parse_unary_expression to avoid consuming 'or' at this level
	expr := p.parse_unary_expression()!

	return ast.ErrorExpression{
		expression: expr
	}
}

// Property/method access: expr.prop or expr.method()
fn (mut p Parser) parse_dot_expression(left ast.Expression) !ast.Expression {
	p.eat(.punc_dot)!

	property := p.eat_token_literal(.identifier, 'Expected property name')!

	if p.current_token.kind == .punc_open_paren {
		call := p.parse_function_call_expression(property)!
		return ast.PropertyAccessExpression{
			left:  left
			right: call
		}
	}

	return ast.PropertyAccessExpression{
		left:  left
		right: ast.Identifier{
			name: property
		}
	}
}

// Function call: name(args)
fn (mut p Parser) parse_function_call_expression(name string) !ast.Expression {
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
		}
		arguments:  arguments
	}
}

// String literal
fn (mut p Parser) parse_string_expression() !ast.Expression {
	return ast.StringLiteral{
		value: p.eat_token_literal(.literal_string, 'Expected string')!
	}
}

// Interpolated string: 'Hello, $name!' or 'Result: ${a + b}'
fn (mut p Parser) parse_interpolated_string() !ast.Expression {
	raw := p.eat_token_literal(.literal_string_interpolation, 'Expected interpolated string')!

	mut parts := []ast.Expression{}
	mut current := ''
	mut i := 0

	for i < raw.len {
		ch := raw[i]

		if ch == `$` {
			// Save accumulated string part
			if current.len > 0 {
				parts << ast.StringLiteral{
					value: current
				}
				current = ''
			}

			i++
			if i >= raw.len {
				return error('Unexpected end of interpolated string after $')
			}

			if raw[i] == `{` {
				// ${expr} form - find matching }
				i++
				mut expr_str := ''
				mut brace_depth := 1
				for i < raw.len && brace_depth > 0 {
					if raw[i] == `{` {
						brace_depth++
					} else if raw[i] == `}` {
						brace_depth--
						if brace_depth == 0 {
							break
						}
					}
					expr_str += raw[i].ascii_str()
					i++
				}
				if brace_depth != 0 {
					return error('Unclosed { in interpolated string')
				}
				i++ // skip closing }

				// Parse the expression
				mut s := scanner.new_scanner(expr_str)
				mut expr_parser := new_parser(mut s)
				expr := expr_parser.parse_expression()!
				parts << expr
			} else {
				// $name form - read identifier
				mut ident := ''
				for i < raw.len
					&& (raw[i].is_letter() || raw[i] == `_` || (ident.len > 0 && raw[i].is_digit())) {
					ident += raw[i].ascii_str()
					i++
				}
				if ident.len == 0 {
					return error('Expected identifier after $ in interpolated string')
				}
				parts << ast.Identifier{
					name: ident
				}
			}
		} else {
			current += ch.ascii_str()
			i++
		}
	}

	// Save final string part
	if current.len > 0 {
		parts << ast.StringLiteral{
			value: current
		}
	}

	return ast.InterpolatedString{
		parts: parts
	}
}

// Number literal
fn (mut p Parser) parse_number_expression() !ast.Expression {
	return ast.NumberLiteral{
		value: p.eat_token_literal(.literal_number, 'Expected number')!
	}
}
