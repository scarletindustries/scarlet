struct User {
	id Int,
	name String,
}

struct DivisionError {
	message String,
}

struct Error {
	message String,
}

struct ValidationError {
	code Int,
}

struct NetworkError {
	status Int,
}

struct Person {
	name String,
	age Int,
}

struct Config {
	debug Bool,
}

enum Result {
	Ok(String)
	Err(String)
}

enum Option {
	Some(Int)
	None
}

const app_name = 'my app'

x = 10
x = x + 1

person = Person{ name: 'alistair', age: 18 }
fn add(a Int, b Int) Int { a + b }
fn greet(name String) { name }

callback = fn(x Int) Int { x * 2 }
fn apply(x Int, f fn(Int) Int) Int {
	f(x)
}
fn apply_generic(x a, f fn(a) a) a {
	f(x)
}

double = fn(n Int) Int { n * 2 }

triple = fn(n Int) Int { n * 3 }

fn find_user(id Int) ?User {
	if id == 0 { none } else {
		User{ id: id, name: 'found' }
	}
}

fn divide(a Int, b Int) Int!DivisionError {
	if b == 0 {
		error DivisionError{ message: 'Cannot divide by zero' }
	} else { a / b }
}

fn validate(x Int)!ValidationError {
	if x < 0 {
		error ValidationError{ code: 1 }
	} else { none }
}

fn check_positive(x Int) Int!Error {
	assert x > 0, Error{ message: 'must be positive' }
	x * 2
}

fn max(a Int, b Int) Int {
	if a > b { a } else { b }
}

fn classify(n Int) String {
	if n < 0 { 'negative' } else if n == 0 { 'zero' } else { 'positive' }
}

fn describe(x Int) String {
	match x {
		0 -> 'zero',
		1 -> 'one',
		else -> 'many',
	}
}

fn handle_result(r Result) String {
	match r {
		Ok(value) -> 'Got: ${value}',
		Err(e) -> 'Error: ${e}',
	}
}

fn match_literal(r Result) String {
	match r {
		Ok('special') -> 'matched special',
		Ok(other) -> 'other: ${other}',
		Err(e) -> 'error: ${e}',
	}
}

fn example() Int {
	result = {
		a = 10
		b = 20
		a + b
	}
	result * 2
}

numbers = [1, 2, 3, 4, 5]
first = numbers[0]

range = 0..10

person_name = person.name
person_age = person.age

assert x > 0, 'x must be positive'

yes = true
no = false

nothing = none

greeting = 'Hello, ${app_name}!'
complex = 'Result: ${1 + 2}'

sum = 1 + 2
diff = 5 - 3
prod = 4 * 2
quot = 10 / 2
rem = 10 % 3

a = 5
b = 10
eq = a == b
neq = a != b
lt = a < b
gt = a > b
lte = a <= b
gte = a >= b

and_result = yes && no
or_result = yes || no
not_result = !yes

add_result = add(5, 3)
max_result = max(10, 20)
classify_result = classify(5)
describe_result = describe(1)
example_result = example()
enum_result = handle_result(Ok('success'))

error_result = divide(10, 0) or 0
error_with_receiver = divide(10, 0) or err -> 0

option_result = find_user(0) or User{ id: 0, name: 'default' }

assert_pass = check_positive(5)
assert_fail = check_positive(-1) or err -> -1

literal_match1 = match_literal(Ok('special'))
literal_match2 = match_literal(Err('danger'))
literal_match3 = match_literal(Ok('something else'))

x = enum G {
	Test
	BottledIt
}
println('x is:')
println(x)
println('what')

println(add_result)
println(max_result)
println(classify_result)
println(describe_result)
println(example_result)
println(enum_result)
println(error_result)
println(option_result)
println(assert_pass)
println(assert_fail)
println(literal_match1)
println(literal_match2)
println(literal_match3)

// Function type annotation tests
apply_result = apply(5, double)
apply_generic_result = apply_generic(5, triple)
println(apply_result)
println(apply_generic_result)
