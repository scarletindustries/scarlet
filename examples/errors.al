// Absence versus failure. Two types, two jobs:
//
//   Option(a)     — the value may not be there. Nothing went wrong.
//   Result(a, e)  — the operation could fail, and `e` says why.
//
// The stdlib convention: a fallible operation returns `Result(_, Nil)` even
// when the error carries no data, because `Result` means "this can fail" and
// chains with `result.then`. `Option` is reserved for "may be absent" — a
// lookup that finds nothing, an index past the end.

import al/array
import al/binary.{Dec}
import al/io.{NotFound}
import al/option
import al/result
import al/string

users = ['ada', 'grace', 'alan']

// Absence: find, indexing, and lookups all hand back an Option.
found = array.find(users, fn(u) string.contains(u, 'ra'))
missing = array.find(users, fn(u) u == 'linus')

println('is_some   ${option.is_some(found)} ${option.is_some(missing)}')
println('is_none   ${option.is_none(found)} ${option.is_none(missing)}')
println('map       ${option.map(found, string.length)}')
println('unwrap    ${option.unwrap(found, 'nobody')} ${option.unwrap(missing, 'nobody')}')

// then chains a lookup that may itself find nothing (flat_map in other
// languages); or_else supplies a whole alternative Option, lazily.
initial = option.then(found, fn(u) string.split(u, '')[0])
println('then      ${option.unwrap(initial, '?')}')
println('or_else   ${option.unwrap(option.or_else(missing, fn() Some('anonymous')), '?')}')

// `or` is the shorthand for "this Option, or that value": it unwraps or falls
// back in one expression.
println('or        ${users[9] or 'out of range'}')
println('')

// Failure: a domain error type. Give the variants payloads — the caller wants
// to report the bad input, not just that "something broke".
type ParseError {
	Empty
	NotANumber(text String)
}

// binary.parse_int is the stdlib's fallible primitive, so it returns
// Result(Int, Nil): "it failed", with nothing more to say. Lift that anonymous
// failure into the domain error with result.map_err — that is the whole job of
// map_err, and it is why Nil errors are fine at the bottom of a stack.
fn parse_score(s String) Result(Int, ParseError) {
	trimmed = string.trim(s)
	if string.length(trimmed) == 0 {
		Err(Empty)
	} else {
		result.map_err(
			binary.parse_int(binary.from_string(trimmed), Dec),
			fn(_) NotANumber(trimmed),
		)
	}
}

fn show(r Result(Int, ParseError)) String {
	match r {
		Ok(n) -> 'Ok(${n})'
		Err(Empty) -> 'Err(Empty)'
		Err(NotANumber(t)) -> 'Err(NotANumber(${t}))'
	}
}

array.each([' 42 ', '', 'abc'], fn(s) println('parse     ${show(parse_score(s))}'))

good = parse_score('42')
bad = parse_score('abc')

println('is_ok     ${result.is_ok(good)} ${result.is_ok(bad)}')
println('is_err    ${result.is_err(good)} ${result.is_err(bad)}')
println('map       ${show(result.map(good, fn(n) n * 2))}')
println('unwrap    ${result.unwrap(good, 0)} ${result.unwrap(bad, 0)}')

// then chains the *next* fallible step, short-circuiting on the first Err —
// validate only ever sees a number that parsed.
fn validate(n Int) Result(Int, ParseError) {
	if n >= 0 && n <= 100 {
		Ok(n)
	} else {
		Err(NotANumber('${n} out of range'))
	}
}

println('then      ${show(result.then(good, validate))}')
println('then      ${show(result.then(parse_score('900'), validate))}')
println('then      ${show(result.then(bad, validate))}')

// `or` works on Result too, and its receiver form binds the error payload, so
// recovery can look at *why* it failed. The body may be a block.
score = parse_score('oops') or e -> {
	match e {
		Empty -> println('recover   nothing to parse')
		NotANumber(t) -> println('recover   ${t} is not a number, using 0')
	}
	0
}

println('score     ${score}')
println('')

// Errors from the stdlib are typed enums, not strings — al/io reports the path
// it could not read, so the handler does not have to reconstruct it.
match io.read_text('no-such-file.txt') {
	Ok(contents) -> println('read ${string.length(contents)} chars')
	Err(NotFound(path)) -> println('io        no such file: ${path}')
	Err(other) -> println('io        ${string.inspect(other)}')
}
