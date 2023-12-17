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

	return error('Expected ${kind}, got ${p.current_token.kind}')
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
			dump(p.current_token)
			panic('Unhandled ${p.current_token.kind} at ${p.current_token.line}:${p.current_token.column}')
		}
	}

	return result
}

fn (mut p Parser) parse_export_statement() !ast.Statement {
	mut statement := ast.ExportStatement{
		ident: ''
	}

	p.eat(.kw_export)!

	statement.declaration = p.parse_declaration()!

	return statement
}

fn (mut p Parser) parse_declaration() !ast.Statement {
	result := match p.current_token.kind {
		.kw_const {
			p.parse_const_statement()!
		}
		else {
			return error('Unhandled ${p.current_token.kind} at ${p.current_token.line}:${p.current_token.column}')
		}
	}

	return result
}

fn (mut p Parser) parse_import_statement() !ast.Statement {
	mut declaration := ast.ImportDeclaration{
		path: './path-to-module.al'
		specifiers: []
	}

	p.eat(.kw_from)!
	str := p.eat(.literal_string)!

	if unwrapped := str.literal {
		declaration.path = unwrapped
	} else {
		panic('Expected string literal')
	}

	p.eat(.kw_import)!

	p.parse_import_specifiers(mut &declaration.specifiers)!

	return declaration
}

fn (mut p Parser) parse_import_specifiers(mut specifiers []ast.ImportSpecifier) ! {
	current := p.eat(.ident)!

	if unwrapped := current.literal {
		specifiers << ast.ImportSpecifier{
			ident: ast.Identifier{
				name: unwrapped
			}
		}
	} else {
		panic('Expected identifier')
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

	current := p.eat(.ident)!

	if unwrapped := current.literal {
		statement.ident = unwrapped
	} else {
		panic('Expected identifier')
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
		else {
			panic('Unhandled ${p.current_token.kind} at ${p.current_token.line}:${p.current_token.column}')
		}
	}

	return result
}

fn (mut p Parser) parse_string_expression() !ast.Expression {
	mut expression := ast.StringLiteral{
		value: ''
	}

	current := p.eat(.literal_string)!

	if unwrapped := current.literal {
		expression.value = unwrapped
	} else {
		panic('Expected string literal')
	}

	return expression
}
