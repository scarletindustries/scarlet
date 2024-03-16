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

	return error('Expected ${kind}, got ${p.current_token.kind} at ${p.current_token.line}:${p.current_token.column}')
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
		statement := p.parse_statement()!
		program.body << statement
		println(statement)
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
		.kw_return {
			p.parse_return_statement()!
		}
		.identifier {
			p.parse_expression()!
		}
		.punc_declaration {
			p.parse_declaration()!
		}
		.punc_open_brace {
			p.parse_struct_initialisation()!
		}
		else {
			return error('[statement] Unhandled ${p.current_token.kind} at ${p.current_token.line}:${p.current_token.column}')
		}
	}

	return result
}

fn (mut p Parser) parse_struct_initialisation() !ast.Statement {
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

	mut current := p.eat(.identifier)!

	if unwrapped := current.literal {
		field.identifier = ast.Identifier{
			name: unwrapped
		}
	} else {
		return error('Expected identifier')
	}

	p.eat(.punc_equals)!

	// field.init = p.parse_expression()!

	if p.current_token.kind == .punc_comma {
		p.eat(.punc_comma)!
	}

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

	mut identifier := p.eat(.identifier)!

	if unwrapped := identifier.literal {
		statement.identifier = ast.Identifier{
			name: unwrapped
		}
	} else {
		return error('Expected identifier')
	}

	p.parse_parameters(mut &statement.params)!

	if p.current_token.kind == .identifier {
		p.eat(.identifier)!

		if unwrapped := p.current_token.literal {
			statement.return_type = ast.Identifier{
				name: unwrapped
			}
		}
	}

	p.parse_function_body(mut &statement.body)!

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

		current = p.eat(.identifier)!

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
	p.eat(.punc_open_brace)!

	for p.current_token.kind != .punc_close_brace {
		statement := p.parse_statement()!
		body << statement
	}

	p.eat(.punc_close_brace)!
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

	mut statement := ast.StructStatement{
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

	mut current := p.eat(.identifier)!

	if unwrapped := current.literal {
		field.identifier = ast.Identifier{
			name: unwrapped
		}
	} else {
		return error('Expected identifier')
	}

	p.eat(.punc_colon)!

	current = p.eat(.identifier)!

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
	mut left := p.parse_primary_expression()!

	for p.current_token.kind in [.punc_equals_comparator, .punc_not_equal, .punc_plus, .punc_minus,
		.punc_mul, .punc_div, .punc_mod] {
		operator := p.current_token.kind

		p.eat(operator)!

		right := p.parse_primary_expression()!

		left = ast.BinaryExpression{
			op: ast.Operator{
				kind: operator
			}
			left: left
			right: right
		}
	}

	return left
}

fn (mut p Parser) parse_primary_expression() !ast.Expression {
	println(p.current_token)

	mut expr := match p.current_token.kind {
		.literal_string { p.parse_string_expression()! }
		.literal_number { p.parse_number_expression()! }
		.identifier { p.parse_identifier_expression()! }
		else { return error('Expected primary expression') }
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

	return ast.FunctionCallExpression{
		identifier: ast.Identifier{
			name: name
		}
		arguments: arguments
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
