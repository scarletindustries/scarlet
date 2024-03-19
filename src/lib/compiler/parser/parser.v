module parser

import lib.compiler.scanner
import lib.compiler.token
import lib.compiler.parser.ast
import lib.compiler

/*
 * Parser is responsible for parsing the tokens into an AST.
 * Some parse functions accept a mut reference to a struct to mutate
 * the struct in place. Some functions will return a new struct.
 * Just be aware of this when consuming the parser.
 */

pub struct Parser {
	tokens []compiler.Token
mut:
	scanner       &scanner.Scanner
	index         int
	current_token compiler.Token
}

pub fn new_parser(mut s scanner.Scanner) Parser {
	tokens := s.scan_all()

	return Parser{
		tokens: tokens
		scanner: s
		index: 0
		current_token: tokens[0]
	}
}

fn (mut p Parser) eat(kind token.Kind) !compiler.Token {
	if p.current_token.kind == kind {
		old := p.current_token

		p.index = p.index + 1
		p.current_token = p.tokens[p.index]

		return old
	}

	return error('[eat] Expected ${kind}, got ${p.current_token.kind} at ${p.current_token.line}:${p.current_token.column}')
}

fn (mut p Parser) eat_msg(kind token.Kind, message string) !compiler.Token {
	return p.eat(kind) or {
		return error('[eat] ${message} [got .${p.current_token.kind} @ ${p.current_token.line}:${p.current_token.column}]')
	}
}

fn (mut p Parser) get_token_literal(kind token.Kind, message string) !string {
	eaten := p.eat_msg(kind, message)!

	if unwrapped := eaten.literal {
		return unwrapped
	}

	return error('Expected token literal for \'${p.current_token}\' ${p.current_token.line}:${p.current_token.column}')
}

pub fn (mut p Parser) parse_program() !ast.BlockExpression {
	mut program := ast.BlockExpression{}

	for p.current_token.kind != .eof {
		statement := p.parse_statement() or {
			println(program)
			println('=====================Compiler Bug=====================')
			println('| The above is the program parsed up until the error |')
			println('|   Plz report this on GitHub, with your full code   |')
			println('======================================================')
			return error(err.msg())
		}

		program.body << statement
	}

	return program
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

fn (mut p Parser) parse_statement() !ast.Statement {
	result := match p.current_token.kind {
		.kw_from {
			p.parse_import_statement()!
		}
		.kw_const {
			p.parse_const_statement()!
		}
		.kw_export {
			p.parse_export_statement()!
		}
		.kw_function {
			p.parse_function_statement()!
		}
		.kw_if {
			p.parse_if_statement()!
		}
		.kw_throw {
			p.parse_throw_statement()!
		}
		.kw_return {
			p.parse_return_statement()!
		}
		.kw_for {
			p.parse_for_statement()!
		}
		.kw_or {
			p.parse_or_statement()!
		}
		.kw_continue {
			p.parse_continue()!
		}
		.kw_assert {
			p.parse_assert()!
		}
		.kw_break {
			p.parse_break()!
		}
		.kw_struct {
			p.parse_struct_statement()!
		}
		.identifier {
			if unwrapped := p.peek_next() {
				if unwrapped.kind == .punc_equals {
					return p.parse_assignment()!
				}

				if unwrapped.kind == .punc_declaration {
					identifier := p.get_token_literal(.identifier, 'Expected identifier for declaration')!

					return ast.DeclarationStatement{
						identifier: ast.Identifier{
							name: identifier
						},
						expression: p.parse_declaration_declaration()!
					}
				}
			}

			return p.parse_expression()!
		}
		else {
			return error('[statement] Unhandled ${p.current_token.kind} at ${p.current_token.line}:${p.current_token.column}')
		}
	}

	return result
}

fn (mut p Parser) parse_assert() !ast.Statement {
	p.eat(.kw_assert)!

	expression := p.parse_expression()!

	p.eat(.punc_comma)!

	message := p.parse_expression()!

	return ast.AssertStatement{
		expression: expression
		message: message
	}
}

fn (mut p Parser) parse_assignment() !ast.Statement {
	mut statement := ast.AssignmentStatement{}

	current := p.eat_msg(.identifier, 'Expected identifier for assignment')!

	if unwrapped := current.literal {
		statement.identifier = ast.Identifier{
			name: unwrapped
		}
	} else {
		return error('Expected identifier')
	}

	p.eat(.punc_equals)!

	statement.expression = p.parse_expression()!

	return statement
}

fn (mut p Parser) parse_continue() !ast.Statement {
	p.eat(.kw_continue)!
	return ast.ContinueStatement{}
}

fn (mut p Parser) parse_break() !ast.Statement {
	p.eat(.kw_break)!
	return ast.BreakStatement{}
}

fn (mut p Parser) parse_for_statement() !ast.Statement {
	p.eat(.kw_for)!

	if p.current_token.kind == .identifier {
		name := p.get_token_literal(.identifier, 'Expected identifier for `for` block variable')!

		p.eat(.kw_in)!

		expression := p.parse_expression()!
		body := p.parse_block('Expected an opening brace for the `for` block')!

		return ast.ForInStatement{
			expression: expression
			body: body
			identifier: ast.Identifier{
				name: name
			}
		}
	}

	return ast.ForStatement{
		body: p.parse_block('Expected opening brace for the `for` block')!
	}
}

fn (mut p Parser) parse_or_statement() !ast.Statement {
	p.eat(.kw_or)!

	// or statements can have an argument passed into them like this `fn() or err -> { .. }`
	// or just by passing a block `fn() or { .. }`

	mut statement := ast.OrStatement{}

	if p.current_token.kind == .identifier {
		mut current := p.eat_msg(.identifier, 'Expected identifier for `or` block receiving argument')!

		if p.current_token.kind == .punc_arrow {
			println('[INFO] Handling options/results with an `or {}` block does not require an arrow. You can safely remove it.')
			p.eat(.punc_arrow)!
		}

		if unwrapped := current.literal {
			statement.receiver = ast.Identifier{
				name: unwrapped
			}
		} else {
			return error("Expected a valid identifier for the or {} block's receiving argument")
		}
	}

	if p.current_token.kind == .punc_open_brace {
		statement.body = p.parse_block('Expected an opening brace for the `or` block')!
	} else {
		statement.body = [p.parse_expression()!]
	}

	return statement
}

fn (mut p Parser) parse_block(no_open_brace_message string) ![]ast.Statement {
	mut statements := []ast.Statement{}

	p.eat_msg(.punc_open_brace, no_open_brace_message)!

	for p.current_token.kind != .punc_close_brace {
		statements << p.parse_statement()!
	}

	p.eat(.punc_close_brace)!

	return statements
}

fn (mut p Parser) parse_if_statement() !ast.Statement {
	p.eat(.kw_if)!

	condition := p.parse_expression()!

	mut statement := ast.IfStatement{
		condition: condition
		body: p.parse_block('Expected if statement to have an opening brace {')!
	}

	if p.current_token.kind == .kw_else {
		p.eat(.kw_else)!

		if p.current_token.kind == .kw_if {
			statement.else_body = [p.parse_if_statement()!]
		} else if p.current_token.kind == .punc_open_brace {
			statement.else_body = p.parse_block('Expected else statement to have an opening brace {')!
		} else {
			statement.else_body = [p.parse_statement()!]
		}
	}

	return statement
}

fn (mut p Parser) parse_throw_statement() !ast.Statement {
	p.eat(.kw_throw)!

	return ast.ThrowStatement{
		expression: p.parse_expression()!
	}
}

fn (mut p Parser) parse_struct_initialisation(identifier ast.Identifier) !ast.Expression {
	mut statement := ast.StructInitialisation{
		identifier: identifier
	}

	p.eat(.punc_open_brace)!

	for p.current_token.kind != .punc_close_brace {
		field := p.parse_struct_init_field()!
		statement.fields << field
	}

	p.eat(.punc_close_brace)!

	return statement
}

fn (mut p Parser) parse_struct_init_field() !ast.StructInitialisationField {
	mut field := ast.StructInitialisationField{}

	mut current := p.eat_msg(.identifier, 'Expected identifier for struct field name')!

	if unwrapped := current.literal {
		field.identifier = ast.Identifier{
			name: unwrapped
		}
	} else {
		return error('Expected identifier2')
	}

	p.eat_msg(.punc_colon, 'Expected colon for initial struct field value')!

	field.init = p.parse_expression()!

	p.eat(.punc_comma)!

	return field
}

fn (mut p Parser) parse_return_statement() !ast.Statement {
	ret := p.eat(.kw_return)!

	// Values being returned must be on the same line
	if p.current_token.line == ret.line {
		return ast.ReturnStatement{
			expression: p.parse_expression()!
		}
	} else {
		return ast.ReturnStatement{}
	}
}

fn (mut p Parser) parse_function_statement() !ast.Statement {
	p.eat(.kw_function)!

	mut statement := ast.FunctionStatement{
		identifier: ast.Identifier{
			name: p.get_token_literal(.identifier, 'Expected identifier for function name')!
		}
	}

	p.parse_parameters(mut &statement.params)!

	if p.current_token.kind == .identifier || p.current_token.kind == .punc_open_bracket || p.current_token.kind == .punc_question_mark {
		if p.current_token.kind == .punc_question_mark {
			statement.is_return_option = true
			p.eat(.punc_question_mark)!
		}

		mut is_array := false

		if p.current_token.kind == .punc_open_bracket {
			is_array = true
			p.eat(.punc_open_bracket)!
			p.eat(.punc_close_bracket)!
		}

		literal := p.get_token_literal(.identifier, 'Expected an identifier when specifying the return type of a function')!

		statement.return_type = ast.TypeIdentifier{
			identifier: ast.Identifier{
				name: literal
			}
			is_array: is_array,
			is_option: false
		}
	}

	if p.current_token.kind == .punc_comma {
		p.eat(.punc_comma)!
		p.eat_msg(.identifier, 'Expected the name of an identifier for the error type')!

		if unwrapped := p.current_token.literal {
			statement.throw_type = ast.Identifier{
				name: unwrapped
			}
		}
	}

	p.eat(.punc_open_brace)!

	p.parse_function_body(mut &statement.body)!

	p.eat(.punc_close_brace)!

	return statement
}

fn (mut p Parser) parse_parameters(mut params []ast.FunctionParameter) ![]ast.FunctionParameter {
	p.eat(.punc_open_paren)!

	for p.current_token.kind != .punc_close_paren {
		param := p.parse_parameter()!
		params << param
	}

	p.eat(.punc_close_paren)!

	return params
}

fn (mut p Parser) parse_parameter() !ast.FunctionParameter {
	mut param := ast.FunctionParameter{}

	mut current := p.eat_msg(.identifier, 'Expected identifier for function parameter name')!

	if unwrapped := current.literal {
		param.identifier = ast.Identifier{
			name: unwrapped
		}
	} else {
		return error('Expected identifier 4')
	}


	mut is_array := false

	if p.current_token.kind == .punc_open_bracket {
		is_array = true
		p.eat(.punc_open_bracket)!
		p.eat(.punc_close_bracket)!
	}

	current = p.eat_msg(.identifier, 'Expected identifier for function parameter type')!

	if unwrapped := current.literal {
		param.typ = ast.TypeIdentifier{
			identifier: ast.Identifier{
				name: unwrapped
			},
			is_array: is_array
		}
	} else {
		return error('Expected identifier 5')
	}

	if p.current_token.kind == .punc_comma {
		p.eat(.punc_comma)!
	}

	return param
}

fn (mut p Parser) parse_function_body(mut body []ast.Statement) ! {
	for p.current_token.kind != .punc_close_brace {
		statement := p.parse_statement()!
		body << statement
	}
}

fn (mut p Parser) parse_export_statement() !ast.Statement {
	p.eat(.kw_export)!

	return ast.ExportStatement{
		declaration: p.parse_declaration()!
	}
}

fn (mut p Parser) parse_declaration() !ast.Statement {
	result := match p.current_token.kind {
		.kw_const {
			p.parse_const_statement()!
		}
		.kw_struct {
			p.parse_struct_statement()!
		}
		.kw_function {
			p.parse_function_statement()!
		}
		.punc_declaration {
			p.parse_declaration_declaration()!
		}
		else {
			return error('[declaration] Unhandled ${p.current_token.kind} at ${p.current_token.line}:${p.current_token.column}')
		}
	}

	return result
}

fn (mut p Parser) parse_declaration_declaration() !ast.Expression {
	p.eat(.punc_declaration)!
	return p.parse_expression()!
}

fn (mut p Parser) parse_struct_statement() !ast.Statement {
	p.eat(.kw_struct)!

	mut statement := ast.StructDeclarationStatement{
		identifier: ast.Identifier{
			name: p.get_token_literal(.identifier, 'Expected identifier for struct name')!
		}
	}

	p.eat(.punc_open_brace)!
	p.parse_struct_fields(mut &statement.fields)!
	p.eat(.punc_close_brace)!

	return statement
}

fn (mut p Parser) parse_struct_fields(mut fields []ast.StructField) ! {
	for p.current_token.kind != .punc_close_brace {
		field := p.parse_struct_field()!
		fields << field
	}
}

fn (mut p Parser) parse_struct_field() !ast.StructField {
	mut field := ast.StructField{}

	if p.current_token.kind == .kw_function {
		fn_stmt := p.parse_function_statement()!

		field.typ = ast.TypeIdentifier{
			identifier: ast.Identifier{
				name: 'Function'
			}
		}

		if fn_stmt is ast.FunctionStatement {
			field.identifier = fn_stmt.identifier
		}

		field.init = ast.BlockExpression{
			body: [fn_stmt]
		}

		return field
	}

	mut current := p.eat_msg(.identifier, 'Expected identifier for struct field name')!

	if unwrapped := current.literal {
		field.identifier = ast.Identifier{
			name: unwrapped
		}
	} else {
		return error('Expected identifier 6')
	}

	mut is_array := false

	if p.current_token.kind == .punc_open_bracket {
		is_array = true
		p.eat(.punc_open_bracket)!
		p.eat(.punc_close_bracket)!
	}

	current = p.eat_msg(.identifier, 'Expected identifier for struct type')!

	if unwrapped := current.literal {
		field.typ = ast.TypeIdentifier{
			identifier: ast.Identifier{
				name: unwrapped
			}
			is_array: is_array
		}
	} else {
		return error('Expected identifier 9')
	}

	if p.current_token.kind == .punc_equals {
		p.eat(.punc_equals)!
		field.init = p.parse_expression()!
	}

	if p.current_token.kind == .punc_comma {
		p.eat(.punc_comma)!
	}

	return field
}

fn (mut p Parser) parse_import_statement() !ast.Statement {
	mut declaration := ast.ImportDeclaration{}

	p.eat(.kw_from)!
	str := p.eat(.literal_string)!

	if unwrapped := str.literal {
		declaration.path = unwrapped
	} else {
		return error('Expected string literal')
	}

	p.eat(.kw_import)!

	p.parse_import_specifiers(mut &declaration.specifiers)!

	return declaration
}

fn (mut p Parser) parse_import_specifiers(mut specifiers []ast.ImportSpecifier) ! {
	current := p.eat_msg(.identifier, 'Expected identifier for import specifier')!

	if unwrapped := current.literal {
		specifiers << ast.ImportSpecifier{
			identifier: ast.Identifier{
				name: unwrapped
			}
		}
	} else {
		return error('Expected identifier')
	}

	if p.current_token.kind == .punc_comma {
		p.eat(.punc_comma)!
		p.parse_import_specifiers(mut specifiers)!
	}

	return
}

fn (mut p Parser) parse_const_statement() !ast.Statement {
	mut statement := ast.ConstStatement{}

	p.eat(.kw_const)!

	current := p.eat_msg(.identifier, 'Expected an identifier for const declaration')!

	if unwrapped := current.literal {
		statement.identifier = ast.Identifier{
			name: unwrapped
		}
	} else {
		return error('Expected identifier')
	}

	p.eat(.punc_equals)!

	statement.init = p.parse_expression()!

	return statement
}

fn (mut p Parser) parse_expression() !ast.Expression {
	if p.current_token.kind == .punc_exclamation_mark {
		p.eat(.punc_exclamation_mark)!

		return ast.UnaryExpression{
			expression: p.parse_expression()!
			op: ast.Operator{
				kind: .punc_exclamation_mark
			}
		}
	}

	left := p.parse_primary_expression()!

	if left is ast.Identifier {
		// possible that we're starting a struct initialisation,
		// so if the next token is an opening brace, we'll parse that.
		// Couldn't think of a better way than just trying to parse it
		// and if it fails, reset the index and current token. Can
		// look and improve this later on.
		curr_index := p.index
		curr_token := p.current_token

		match p.current_token.kind {
			.punc_open_brace {
				if result := p.parse_struct_initialisation(left) {
					return result
				} 
			}

			.punc_open_bracket {
				if result := p.parse_expression() {
					return ast.ArrayIndexExpression{
						identifier: left
						index: result
					}
				}
			}

			else {}
		}

		p.index = curr_index
		p.current_token = curr_token
	}

	if p.current_token.kind == .punc_dotdot {
		p.eat_msg(.punc_dotdot, 'Expected range punctuation')!

		right := p.parse_expression()!

		return ast.RangeExpression{
			start: left
			end: right
		}
	}

	for p.current_token.kind in [.punc_equals_comparator, .punc_not_equal, .punc_plus, .punc_minus,
		.punc_mul, .punc_div, .punc_mod, .punc_gt, .punc_lt, .punc_gte, .punc_lte, .logical_and, .logical_or] {
		operator := p.current_token.kind

		p.eat(operator)!

		right := p.parse_primary_expression()!

		return ast.BinaryExpression{
			left: left
			right: right
			op: ast.Operator{
				kind: operator
			}
		}
	}

	return left
}

fn (mut p Parser) parse_primary_expression() !ast.Expression {
	mut expr := match p.current_token.kind {
		.literal_string {
			p.parse_string_expression()!
		}
		.literal_number {
			p.parse_number_expression()!
		}
		.identifier {
			p.parse_identifier_expression()!
		}
		.punc_open_paren {
			p.eat(.punc_open_paren)!
			expression := p.parse_expression()!
			p.eat(.punc_close_paren)!
			return expression
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
			p.parse_array()!
		}
		else {
			return error('Expected primary expression at ${p.current_token.line}:${p.current_token.column}. Got ${p.current_token.kind}')
		}
	}


	for p.current_token.kind == .punc_dot {
		expr = p.parse_dot_expression(expr)!
	}

	return expr
}

fn (mut p Parser) parse_array() !ast.Expression {
	p.eat(.punc_open_bracket)!

	mut elements := []ast.Expression{}

	for p.current_token.kind != .punc_close_bracket {
		element := p.parse_expression()!
		elements << element

		if p.current_token.kind == .punc_comma {
			p.eat(.punc_comma)!
		} else {
			break
		}
	}

	p.eat(.punc_close_bracket)!

	return ast.ArrayExpression{
		elements: elements
	}
}

fn (mut p Parser) parse_block_expression() !ast.Expression {
	return ast.BlockExpression{
		body: p.parse_block('Expected an opening brace for an inline block')!
	}
}

fn (mut p Parser) parse_dot_expression(left ast.Expression) !ast.Expression {
	p.eat(.punc_dot)!

	// The next token must be an identifier (property or method)
	property := p.get_token_literal(.identifier, 'Expected an identifier for property access')!

	if p.current_token.kind == .punc_open_paren {
		return ast.PropertyAccessExpression{
			left: left
			right: p.parse_function_call_expression(property)!
		}
	}

	// Otherwise, it's a property access
	return ast.PropertyAccessExpression{
		left: left
		right: ast.Identifier{
			name: property
		}
	}
}

fn (mut p Parser) parse_function_call_expression(name string) !ast.Expression {
	p.eat(.punc_open_paren)!

	mut arguments := []ast.Expression{}

	// Parse arguments until a closing parenthesis is found
	for p.current_token.kind != .punc_close_paren {
		// Parse an expression as an argument
		argument := p.parse_expression()!
		arguments << argument

		// If the next token is a comma, consume it and continue parsing arguments
		if p.current_token.kind == .punc_comma {
			p.eat(.punc_comma)!
		}
	}

	// Consume the closing parenthesis
	p.eat(.punc_close_paren)!

	mut has_exclamation_mark := false

	if p.current_token.kind == .punc_exclamation_mark {
		has_exclamation_mark = true
		p.eat(.punc_exclamation_mark)!
	}

	return ast.FunctionCallExpression{
		identifier: ast.Identifier{
			name: name
		}
		arguments: arguments
		has_exclamation_mark: has_exclamation_mark
	}
}

fn (mut p Parser) parse_identifier_expression() !ast.Expression {
	unwrapped := p.get_token_literal(.identifier, 'Expected an identifier as part of an expression')!

	if p.current_token.kind == .punc_open_paren {
		return p.parse_function_call_expression(unwrapped)!
	}

	return ast.Identifier{
		name: unwrapped
	}
}

fn (mut p Parser) parse_string_expression() !ast.Expression {
	return ast.StringLiteral{
		value: p.get_token_literal(.literal_string, 'Expected a string')!
	}
}

fn (mut p Parser) parse_number_expression() !ast.Expression {
	return ast.NumberLiteral{
		value: p.get_token_literal(.literal_number, 'Expected a number')!
	}
}
