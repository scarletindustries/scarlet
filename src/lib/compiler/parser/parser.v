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
		p.parse_statement()!
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

		else {
			dump(p.current_token)
			panic('Unhandled ${p.current_token.kind} at ${p.current_token.line}:${p.current_token.column}')
		}
	}

	return result
}


fn (mut p Parser) parse_import_statement() !ast.Statement {
	mut statement := ast.ImportStatement{
		path: './path-to-module.al'
		declarations: []
	}

	p.eat(.kw_from)!
	str := p.eat(.literal_string)!

	if unwrapped := str.literal {
		statement.path = unwrapped
	} else {
		panic('Expected string literal')
	}

	p.eat(.kw_import)!

	p.parse_ident_list(mut &statement.declarations)!

	println(statement)

	return statement
}

fn (mut p Parser) parse_ident_list(mut list []ast.Identifier) ! {
	current := p.eat(.ident)!

	if unwrapped := current.literal {
		list << ast.Identifier{
			name: unwrapped
		}
	} else {
		panic('Expected identifier')
	}

	if p.current_token.kind == .punc_comma {
		p.eat(.punc_comma)!
		p.parse_ident_list(mut list)!
	}

	return
}

fn (mut p Parser) parse_const_statement() !ast.Statement {
	mut statement := ast.ConstStatement{
		
	}

	p.eat(.kw_const)!

	current := p.eat(.ident)!

	if unwrapped := current.literal {
		statement.ident = unwrapped
	} else {
		panic('Expected identifier')
	}

	p.eat(.punc_equals)!

	statement.init = p.parse_expression()!

	println(statement)

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
