// A tiny language, end to end: a lexer that turns text into tokens, a
// recursive-descent parser that turns tokens into a tree, and an evaluator
// that folds the tree down to a number. Every piece is built from what the
// earlier examples taught — sum types, array patterns, Result, closures, and
// a Map threaded through the program as the variable environment.

import al/array
import al/binary.{Dec}
import al/map
import al/result
import al/string

type Token {
	Number(value Int)
	Name(text String)
	Plus
	Minus
	Star
	Slash
	Open
	Close
	Assign
}

// Every character that is a token on its own. Words — numbers and variable
// names — are runs of the remaining characters.
const punctuation = ['+', '-', '*', '/', '(', ')', '=']

// `split(s, '')` yields one string per Unicode scalar value, which is how the
// lexer gets a character set to test membership against.
const word_chars = string.split('abcdefghijklmnopqrstuvwxyz0123456789_', '')

fn symbol(c String) Token {
	match c {
		'+' -> Plus
		'-' -> Minus
		'*' -> Star
		'/' -> Slash
		'(' -> Open
		')' -> Close
		else -> Assign
	}
}

// The lexer matches on characters exactly the way the parser below matches on
// tokens: peel one off the front, recurse on the rest.
fn lex(source String) Result(Array(Token), String) {
	scan(string.split(source, ''))
}

fn scan(chars Array(String)) Result(Array(Token), String) {
	match chars {
		[] -> Ok([])
		[' ', ..rest] -> scan(rest)
		[c, ..rest] if array.contains(punctuation, c) -> prepend(symbol(c), scan(rest))
		[c, ..] if array.contains(word_chars, c) -> {
			(word, rest) = take_word(chars)
			result.then(word_token(word), fn(t) prepend(t, scan(rest)))
		}
		[c, ..] -> Err('unexpected character: ${c}')
	}
}

fn prepend(t Token, rest Result(Array(Token), String)) Result(Array(Token), String) {
	result.map(rest, fn(ts) [t, ..ts])
}

// The longest run of word characters, plus whatever follows it.
fn take_word(chars Array(String)) (String, Array(String)) {
	match chars {
		[c, ..rest] if array.contains(word_chars, c) -> {
			(tail, left) = take_word(rest)
			(c + tail, left)
		}
		else -> ('', chars)
	}
}

// A word that parses as an integer is a number; anything else is a name.
// A word that only *starts* like a number (`12abc`) is neither, and the lexer
// says so rather than inventing a variable called `12abc`.
fn word_token(word String) Result(Token, String) {
	match binary.parse_int(binary.from_string(word), Dec) {
		Ok(n) -> Ok(Number(n))
		Err(Nil) -> if starts_with_digit(word) {
			Err('bad number: ${word}')
		} else {
			Ok(Name(word))
		}
	}
}

fn starts_with_digit(word String) Bool {
	match string.split(word, '') {
		[first, ..] -> result.is_ok(binary.parse_int(binary.from_string(first), Dec))
		[] -> False
	}
}

type Expr {
	Lit(value Int)
	Var(name String)
	Add(left Expr, right Expr)
	Sub(left Expr, right Expr)
	Mul(left Expr, right Expr)
	Div(left Expr, right Expr)
}

// One function per precedence level, each returning the tree built so far and
// the tokens it did not consume:
//   expression = term (('+' | '-') term)*
//   term       = factor (('*' | '/') factor)*
//   factor     = number | name | '(' expression ')'
type Parsed = Result((Expr, Array(Token)), String)

fn expression(tokens Array(Token)) Parsed {
	result.then(term(tokens), fn(s) more_terms(s.0, s.1))
}

fn more_terms(left Expr, tokens Array(Token)) Parsed {
	match tokens {
		[Plus, ..rest] -> result.then(term(rest), fn(s) more_terms(Add(left, s.0), s.1))
		[Minus, ..rest] -> result.then(term(rest), fn(s) more_terms(Sub(left, s.0), s.1))
		else -> Ok((left, tokens))
	}
}

fn term(tokens Array(Token)) Parsed {
	result.then(factor(tokens), fn(s) more_factors(s.0, s.1))
}

fn more_factors(left Expr, tokens Array(Token)) Parsed {
	match tokens {
		[Star, ..rest] -> result.then(factor(rest), fn(s) more_factors(Mul(left, s.0), s.1))
		[Slash, ..rest] -> result.then(factor(rest), fn(s) more_factors(Div(left, s.0), s.1))
		else -> Ok((left, tokens))
	}
}

fn factor(tokens Array(Token)) Parsed {
	match tokens {
		[] -> Err('unexpected end of input')
		[Number(n), ..rest] -> Ok((Lit(n), rest))
		[Name(x), ..rest] -> Ok((Var(x), rest))
		[Open, ..rest] -> match expression(rest) {
			Ok((inner, [Close, ..after])) -> Ok((inner, after))
			Ok(_) -> Err('missing closing )')
			Err(m) -> Err(m)
		}
		[t, ..] -> Err('unexpected ${string.inspect(t)}')
	}
}

fn parse(tokens Array(Token)) Result(Expr, String) {
	match expression(tokens) {
		Ok((e, [])) -> Ok(e)
		Ok((_, [t, ..])) -> Err('unexpected ${string.inspect(t)} after the expression')
		Err(m) -> Err(m)
	}
}

// Evaluate against the environment. A `Var` is a map lookup, so an unbound
// name fails the same way dividing by zero does: as an Err the caller reports.
fn evaluate(e Expr, env Map(String, Int)) Result(Int, String) {
	match e {
		Lit(n) -> Ok(n)
		Var(name) -> match map.get(env, name) {
			Some(v) -> Ok(v)
			None -> Err('undefined variable: ${name}')
		}
		Add(l, r) -> combine(l, r, env, fn(a, b) Ok(a + b))
		Sub(l, r) -> combine(l, r, env, fn(a, b) Ok(a - b))
		Mul(l, r) -> combine(l, r, env, fn(a, b) Ok(a * b))
		Div(l, r) -> combine(l, r, env, divide)
	}
}

// Evaluate both operands, then apply `op`. The leftmost Err wins.
fn combine(
	l Expr,
	r Expr,
	env Map(String, Int),
	op fn(Int, Int) Result(Int, String),
) Result(Int, String) {
	result.then(evaluate(l, env), fn(a) result.then(evaluate(r, env), fn(b) op(a, b)))
}

fn divide(a Int, b Int) Result(Int, String) {
	if b == 0 {
		Err('division by zero')
	} else {
		Ok(a / b)
	}
}

// Print the tree back out, fully parenthesized, so the reader can see the
// precedence the parser chose.
fn show(e Expr) String {
	match e {
		Lit(n) -> '${n}'
		Var(name) -> name
		Add(l, r) -> '(${show(l)} + ${show(r)})'
		Sub(l, r) -> '(${show(l)} - ${show(r)})'
		Mul(l, r) -> '(${show(l)} * ${show(r)})'
		Div(l, r) -> '(${show(l)} / ${show(r)})'
	}
}

// A line is either `name = expression`, which binds a variable, or a bare
// expression, which is evaluated and printed. Binding returns a *new* map;
// the fold below threads it into the next line, so nothing is ever mutated.
fn run_line(env Map(String, Int), line String) Map(String, Int) {
	match lex(line) {
		Err(m) -> {
			report(line, Err(m))
			env
		}
		Ok([Name(x), Assign, ..rest]) -> bind(env, line, x, rest)
		Ok(tokens) -> {
			report(
				line,
				result.then(
					parse(tokens),
					fn(e) result.map(evaluate(e, env), fn(v) '${show(e)} = ${v}'),
				),
			)
			env
		}
	}
}

fn bind(env Map(String, Int), line String, name String, rest Array(Token)) Map(String, Int) {
	match result.then(parse(rest), fn(e) evaluate(e, env)) {
		Ok(v) -> {
			report(line, Ok('${name} = ${v}'))
			map.set(env, name, v)
		}
		Err(m) -> {
			report(line, Err(m))
			env
		}
	}
}

fn report(line String, outcome Result(String, String)) Nil {
	match outcome {
		Ok(text) -> println('${line}  ->  ${text}')
		Err(m) -> println('${line}  !  ${m}')
	}
}

const session = [
	'x = 12',
	'y = x + 3',
	'x*y',
	'z = (x + y) * 2',
	'x = x + 100',
	'x - z',
	'z / (y - y)',
	'a + 1',
	'2 +',
	'12abc',
	'x \$ y',
]

_ = array.fold(session, map.new(), run_line)
