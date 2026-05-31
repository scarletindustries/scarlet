// A tiny expression language: an Expr tree, a pretty-printer, an evaluator where
// dividing by zero is an Err, and a recursive-descent parser for strings like
// '12 + 3 * ( 4 - 1 )'.

import al/binary
import al/result
import al/string

type Expr {
	Num(value Int)
	Add(left Expr right Expr)
	Sub(left Expr right Expr)
	Mul(left Expr right Expr)
	Div(left Expr right Expr)
}

// Print an expression with parentheses around every operation.
fn show(e Expr) String {
	match e {
		Num(n) -> '${n}'
		Add(l, r) -> '(${show(l)} + ${show(r)})'
		Sub(l, r) -> '(${show(l)} - ${show(r)})'
		Mul(l, r) -> '(${show(l)} * ${show(r)})'
		Div(l, r) -> '(${show(l)} / ${show(r)})'
	}
}

fn evaluate(e Expr) Result(Int, String) {
	match e {
		Num(n) -> Ok(n)
		Add(l, r) -> combine(l, r, fn(a, b) Ok(a + b))
		Sub(l, r) -> combine(l, r, fn(a, b) Ok(a - b))
		Mul(l, r) -> combine(l, r, fn(a, b) Ok(a * b))
		Div(l, r) -> combine(l, r, divide)
	}
}

// Evaluate both sides of an operation, then apply `op`. The first Err wins.
fn combine(l Expr, r Expr, op fn(Int, Int) Result(Int, String)) Result(Int, String) {
	match (evaluate(l), evaluate(r)) {
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
//   factor     = number | '(' expression ')'
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
		[t, ..rest] -> match binary.parse_int(binary.from_string(t), 10) {
			Some(n) -> Ok((Num(n), rest))
			None -> Err("'${t}' is not a number")
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

// Parse and evaluate one input, printing '<expr> = <result>'.
// Evaluation errors like division by zero become the result text via `or`.
fn run(input String) {
	match parse(input) {
		Ok(e) -> {
			value = result.map(evaluate(e), fn(v) '${v}') or err -> err
			println('${show(e)} = ${value}')
		}
		Err(m) -> println("cannot parse '${input}': ${m}")
	}
}

run('1 + 2 * 3')
run('( 1 + 2 ) * 3')
run('12 + 3 * ( 4 - 1 )')
run('100 / 4 - 5')
run('7 / ( 3 - 3 )')
run('( 8 + 2')
