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
mut:
	scanner       &scanner.Scanner
	current_token compiler.Token
}

pub fn new_parser(mut s scanner.Scanner) Parser {
	return Parser{
		scanner: s
		current_token: s.scan_next()
	}
}

fn (mut p Parser) eat(kind token.Kind) !compiler.Token {
	if p.current_token.kind == kind {
		current := p.current_token
		p.current_token = p.scanner.scan_next()
		return current
	}

	return error('[eat] Expected ${kind}, got ${p.current_token.kind} at ${p.current_token.line}:${p.current_token.column}')
}

fn (mut p Parser) eat_msg(kind token.Kind, message string) !compiler.Token {
	return p.eat(kind) or {
		return error('[eat] ${message} [got .${p.current_token.kind} @ ${p.current_token.line}:${p.current_token.column}]')
	}
}

fn (mut p Parser) get_token_literal(kind token.Kind) !string {
	eaten := p.eat(kind)!

	if unwrapped := eaten.literal {
		return unwrapped
	}

	return error('Expected token literal for \'${p.current_token}\' ${p.current_token.line}:${p.current_token.column}')
}

pub fn (mut p Parser) parse_program() !ast.Block {
	mut program := ast.Block{}

	for p.current_token.kind != .eof {
		statement := p.parse_statement() or {
			// println(program)
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
		.kw_or {
			p.parse_or_statement()!
		}
		.identifier {
			p.parse_expression()!
		}
		.punc_declaration {
			p.parse_declaration()!
		}
		else {
			return error('[statement] Unhandled ${p.current_token.kind} at ${p.current_token.line}:${p.current_token.column}')
		}
	}

	return result
}

fn (mut p Parser) parse_or_statement() !ast.Statement {
	p.eat(.kw_or)!

	// or statements can have an argument passed into them like this `fn() or err -> { .. }`
	// or just by passing a block `fn() or { .. }`

	mut statement := ast.OrStatement{}

	if p.current_token.kind == .identifier {
		mut current := p.eat(.identifier)!

		if p.current_token.kind == .punc_arrow {
			println('[INFO] Handling options/results with an `or {}` block does not require an arrow. You can safely remove it.')
			p.eat(.punc_arrow)!
		}

		if unwrapped := current.literal {
			statement.receiver = ast.Identifier {
				name: unwrapped
			}
		} else {
			return error('Expected a valid identifier for the or {} block\'s receiving argument')
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
		condition: condition,
		body: p.parse_block('Expected if statement to have an opening brace {')!
	}

	return statement
}

fn (mut p Parser) parse_throw_statement() !ast.Statement {
	p.eat(.kw_throw)!

	return ast.ThrowStatement{
		expression: p.parse_expression()!
	}
}

fn (mut p Parser) parse_struct_initialisation() !ast.Expression {
	mut statement := ast.StructInitialisation{}

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
		return error('Expected identifier')
	}

	p.eat_msg(.punc_colon, 'Expected colon for initial struct field value')!

	field.init = p.parse_expression()!

	p.eat(.punc_comma)!

	return field
}

fn (mut p Parser) parse_return_statement() !ast.Statement {
	p.eat(.kw_return)!

	return ast.ReturnStatement{
		expression: p.parse_expression()!
	}
}

fn (mut p Parser) parse_function_statement() !ast.Statement {
	mut statement := ast.FunctionStatement{}

	p.eat(.kw_function)!

	mut identifier := p.eat_msg(.identifier, 'Expected an identifier when declaring a function')!

	if unwrapped := identifier.literal {
		statement.identifier = ast.Identifier{
			name: unwrapped
		}
	} else {
		return error('Expected identifier')
	}

	p.parse_parameters(mut &statement.params)!

	if p.current_token.kind == .identifier || p.current_token.kind == .punc_question_mark {
		if p.current_token.kind == .punc_question_mark {
			statement.is_return_option = true
			p.eat(.punc_question_mark)!
		}

		p.eat_msg(.identifier, 'Expected an identifier when specifying the return type of a function')!

		if unwrapped := p.current_token.literal {
			statement.return_type = ast.Identifier{
				name: unwrapped
			}
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

	mut current := p.eat(.identifier)!

	if unwrapped := current.literal {
		param.identifier = ast.Identifier{
			name: unwrapped
		}
	} else {
		return error('Expected identifier')
	}

	if p.current_token.kind == .punc_colon {
		p.eat(.punc_colon)!

		current = p.eat_msg(.identifier, 'Expected identifier for function parameter')!

		if unwrapped := current.literal {
			param.typ = ast.Identifier{
				name: unwrapped
			}
		} else {
			return error('Expected identifier')
		}
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

fn (mut p Parser) parse_declaration_declaration() !ast.Statement {
	p.eat(.punc_declaration)!
	return p.parse_expression()!
}

fn (mut p Parser) parse_struct_statement() !ast.Statement {
	p.eat(.kw_struct)!

	mut statement := ast.StructDeclarationStatement{
		identifier: ast.Identifier{
			name: p.get_token_literal(.identifier)!
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

	mut current := p.eat_msg(.identifier, 'Expected identifier for struct field name')!

	if unwrapped := current.literal {
		field.identifier = ast.Identifier{
			name: unwrapped
		}
	} else {
		return error('Expected identifier')
	}

	p.eat_msg(.punc_colon, 'Expected colon for struct field type')!

	current = p.eat_msg(.identifier, 'Expected identifier for struct type')!

	if unwrapped := current.literal {
		field.typ = ast.Identifier{
			name: unwrapped
		}
	} else {
		return error('Expected identifier')
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
	current := p.eat(.identifier)!

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

	current := p.eat(.identifier)!

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
		}
	}

	mut left := p.parse_primary_expression()!

	if p.current_token.kind == .punc_open_brace {
		return p.parse_struct_initialisation()!
	}

	for p.current_token.kind in [.punc_equals_comparator, .punc_not_equal, .punc_plus, .punc_minus,
		.punc_mul, .punc_div, .punc_mod, .punc_gt, .punc_lt] {
		operator := p.current_token.kind

		p.eat(operator)!

		right := p.parse_primary_expression()!

		left = ast.BinaryExpression{
			left: left,
			right: right,
			op: ast.Operator{
				kind: operator
			},
		}
	}

	return left
}

fn (mut p Parser) parse_primary_expression() !ast.Expression {
	mut expr := match p.current_token.kind {
		.literal_string { p.parse_string_expression()! }
		.literal_number { p.parse_number_expression()! }
		.identifier { p.parse_identifier_expression()! }
		.kw_none { p.eat(.kw_none)!; ast.NoneExpression{} }
		.kw_true { p.eat(.kw_true)!; ast.BooleanLiteral{ value: true } }
		.kw_false { p.eat(.kw_false)!; ast.BooleanLiteral{ value: false } }
		else { return error('Expected primary expression at ${p.current_token.line}:${p.current_token.column}. Got ${p.current_token.kind}') }
	}

	for p.current_token.kind == .punc_dot {
		expr = p.parse_dot_expression(expr)!
	}

	return expr
}

fn (mut p Parser) parse_dot_expression(left ast.Expression) !ast.Expression {
	// Consume the dot
	p.eat(.punc_dot)!

	// The next token must be an identifier (property or method)
	property := p.get_token_literal(.identifier)!

	if p.current_token.kind == .punc_open_paren {
		return p.parse_function_call_expression(property)!
	}

	// Otherwise, it's a property access
	return ast.PropertyAccessExpression{
		expression: left
		identifier: ast.Identifier{
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
		},
		arguments: arguments,
		has_exclamation_mark: has_exclamation_mark
	}
}

fn (mut p Parser) parse_identifier_expression() !ast.Expression {
	unwrapped := p.get_token_literal(.identifier)!

	if p.current_token.kind == .punc_open_paren {
		return p.parse_function_call_expression(unwrapped)!
	}

	return ast.Identifier{
		name: unwrapped
	}
}

fn (mut p Parser) parse_string_expression() !ast.Expression {
	return ast.StringLiteral{
		value: p.get_token_literal(.literal_string)!
	}
}

fn (mut p Parser) parse_number_expression() !ast.Expression {
	return ast.NumberLiteral{
		value: p.get_token_literal(.literal_number)!
	}
}
