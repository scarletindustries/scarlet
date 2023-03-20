module parser

import lib.compiler.scanner
import lib.compiler
import toml.token

pub struct Parser {
mut:
	scanner       &scanner.Scanner
	current_token &compiler.Token
}

pub fn new_parser(mut scanner scanner.Scanner) Parser {
	return Parser{
		scanner: scanner
		current_token: &scanner.scan_next()
	}
}

fn (mut p Parser) eat(kind token.Kind) ! {
	if p.current_token.kind == kind {
		p.current_token = &p.scanner.scan_next()
		return
	}

	return error('Expected ${kind}, got ${p.current_token.kind}')
}
