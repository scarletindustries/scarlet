// A tiny expression language: an Expr tree, a pretty-printer, an evaluator where
// dividing by zero is an Err, and a recursive-descent parser for strings like
// '12 + 3 * ( 4 - 1 )'.
//
// Variables make it a little language rather than a calculator: `x = 12` binds
// a name, and later lines can read it. The bindings live in an environment —
// a `Map(String, Int)` threaded through the program — so a reference is a map
// lookup and an assignment returns a new environment with the name `set`.

import al/binary.{Dec}
import al/array
import al/map
import al/result
import al/string

type Expr {
	Num(value Int)
	Var(name String)
	Add(left Expr right Expr)
	Sub(left Expr right Expr)
	Mul(left Expr right Expr)
	Div(left Expr right Expr)
}

// Print an expression with parentheses around every operation.
fn show(e Expr) String {
	match e {
		Num(n) -> '${n}'
		Var(name) -> name
		Add(l, r) -> '(${show(l)} + ${show(r)})'
		Sub(l, r) -> '(${show(l)} - ${show(r)})'
		Mul(l, r) -> '(${show(l)} * ${show(r)})'
		Div(l, r) -> '(${show(l)} / ${show(r)})'
	}
}

// Evaluate against an environment of variable bindings. A `Var` is a map
// lookup; an unbound name is an error, just like dividing by zero.
fn evaluate(e Expr, env Map(String, Int)) Result(Int, String) {
	match e {
		Num(n) -> Ok(n)
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

// Evaluate both sides of an operation, then apply `op`. The first Err wins.
fn combine(
	l Expr,
	r Expr,
	env Map(String, Int),
	op fn(Int, Int) Result(Int, String),
) Result(Int, String) {
	match (evaluate(l, env), evaluate(r, env)) {
		(Ok(a), Ok(b)) -> op(a, b)
		(Err(m), _) -> Err(m)
		(_, Err(m)) -> Err(m)
	}
}

fn divide(a Int, b Int) Result(Int, String) {
	if b == 0 {
		Err('division by zero')
	} else {
		Ok(a / b)
	}
}

// Tokens are whatever sits between spaces.
fn tokenize(input String) Array(String) {
	string.split(input, ' ')
}

// The parser is recursive descent, one function per precedence level:
//   expression = term (('+' | '-') term)*
//   term       = factor (('*' | '/') factor)*
//   factor     = number | name | '(' expression ')'
// Each step returns the Expr built so far plus the unconsumed tokens.
type Parsed = Result((Expr, Array(String)), String)

fn expression(tokens Array(String)) Parsed {
	result.then(term(tokens), fn(s) more_terms(s.0, s.1))
}

fn more_terms(left Expr, tokens Array(String)) Parsed {
	match tokens {
		['+', ..rest] -> result.then(term(rest), fn(s) more_terms(Add(left, s.0), s.1))
		['-', ..rest] -> result.then(term(rest), fn(s) more_terms(Sub(left, s.0), s.1))
		else -> Ok((left, tokens))
	}
}

fn term(tokens Array(String)) Parsed {
	result.then(factor(tokens), fn(s) more_factors(s.0, s.1))
}

fn more_factors(left Expr, tokens Array(String)) Parsed {
	match tokens {
		['*', ..rest] -> result.then(factor(rest), fn(s) more_factors(Mul(left, s.0), s.1))
		['/', ..rest] -> result.then(factor(rest), fn(s) more_factors(Div(left, s.0), s.1))
		else -> Ok((left, tokens))
	}
}

fn factor(tokens Array(String)) Parsed {
	match tokens {
		[] -> Err('unexpected end of input')
		['(', ..rest] -> match expression(rest) {
			Ok((inner, [')', ..after])) -> Ok((inner, after))
			Ok(_) -> Err('missing closing )')
			Err(m) -> Err(m)
		}
		// A number is a literal; any other word is a variable reference.
		[t, ..rest] -> match binary.parse_int(binary.from_string(t), Dec) {
			Some(n) -> Ok((Num(n), rest))
			None -> Ok((Var(t), rest))
		}
	}
}

fn parse(input String) Result(Expr, String) {
	match expression(tokenize(input)) {
		Ok((e, [])) -> Ok(e)
		Ok((_, [t, ..])) -> Err("unexpected '${t}' after the expression")
		Err(m) -> Err(m)
	}
}

// Run one line against the current environment, returning the next environment.
// `name = expr` binds (or rebinds) a variable; any other line is an expression
// to evaluate and print. The environment is never mutated — `set` returns a new
// map and we thread it to the next line.
fn run_line(env Map(String, Int), line String) Map(String, Int) {
	match tokenize(line) {
		[name, '=', ..rest] -> assign(env, name, rest)
		else -> {
			print_expr(env, line)
			env
		}
	}
}

fn assign(env Map(String, Int), name String, rhs Array(String)) Map(String, Int) {
	match expression(rhs) {
		Ok((e, [])) -> match evaluate(e, env) {
			Ok(v) -> {
				println('${name} = ${v}')
				map.set(env, name, v)
			}
			Err(m) -> {
				println('${name} = <error: ${m}>')
				env
			}
		}
		Ok((_, [t, ..])) -> {
			println("cannot assign ${name}: unexpected '${t}'")
			env
		}
		Err(m) -> {
			println('cannot assign ${name}: ${m}')
			env
		}
	}
}

// Parse and evaluate an expression line, printing '<expr> = <result>'.
// Evaluation errors (division by zero, unbound names) become the result text.
fn print_expr(env Map(String, Int), input String) {
	match parse(input) {
		Ok(e) -> {
			value = result.map(evaluate(e, env), fn(v) '${v}') or err -> err
			println('${show(e)} = ${value}')
		}
		Err(m) -> println("cannot parse '${input}': ${m}")
	}
}

// A whole session: fold the lines through a fresh, empty environment.
fn run_session(lines Array(String)) {
	_ = array.fold(lines, map.new(), run_line)
	Nil
}

run_session(
	[
		'x = 12',
		'y = x + 3',
		'x * y',
		'z = ( x + y ) * 2',
		'x = x + 100',
		'x - z',
		'z / ( y - y )',
		'a + 1',
	],
)
