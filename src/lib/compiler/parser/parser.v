module parser

import lib.compiler.scanner
import lib.compiler.token
import lib.compiler.parser.ast
import lib.compiler

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

pub fn (mut p Parser) parse_program() !ast.Program {
	mut program := ast.Program{}

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
		else {
			return error('Unhandled ${p.current_token.kind} at ${p.current_token.line}:${p.current_token.column}')
		}
	}

	return result
}

fn (mut p Parser) parse_export_statement() !ast.Statement {
	mut statement := ast.ExportStatement{}

	p.eat(.kw_export)!

	statement.declaration = p.parse_declaration()!

	return statement
}

fn (mut p Parser) parse_declaration() !ast.Statement {
	result := match p.current_token.kind {
		.kw_const {
			p.parse_const_statement()!
		}
		.kw_struct {
			p.parse_struct_statement()!
		}
		else {
			return error('Unhandled ${p.current_token.kind} at ${p.current_token.line}:${p.current_token.column}')
		}
	}

	return result
}

// Structs look like this:
/*
struct Foo {
	bar: string,
	baz: string
}
*/

fn (mut p Parser) parse_struct_statement() !ast.Statement {
	mut statement := ast.StructStatement{}

	p.eat(.kw_struct)!

	current := p.eat(.identifier)!
	if unwrapped := current.literal {
		statement.identifier = ast.Identifier{
			name: unwrapped
		}
	} else {
		return error('Expected identifier')
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
	result := match p.current_token.kind {
		.literal_string {
			p.parse_string_expression()!
		}
		.literal_number {
			p.parse_number_expression()!
		}
		else {
			return error('Unhandled ${p.current_token.kind} at ${p.current_token.line}:${p.current_token.column}')
		}
	}

	return result
}

fn (mut p Parser) parse_string_expression() !ast.Expression {
	mut expression := ast.StringLiteral{}

	current := p.eat(.literal_string)!

	if unwrapped := current.literal {
		expression.value = unwrapped
	} else {
		return error('Expected string literal')
	}

	return expression
}

fn (mut p Parser) parse_number_expression() !ast.Expression {
	mut expression := ast.NumberLiteral{}

	current := p.eat(.literal_number)!

	if unwrapped := current.literal {
		expression.value = unwrapped
	} else {
		return error('Expected number literal')
	}

	return expression
}
