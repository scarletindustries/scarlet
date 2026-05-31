type User {
	id Int
	name String
}

type DivisionError {
	message String
}

type AppError {
	message String
}

type ValidationError {
	code Int
}

type NetworkError {
	status Int
}

type Person {
	name String
	age Int
}

type Config {
	debug Bool
}

type Outcome {
	Good(value String)
	Bad(value String)
}

// ============================================================================
// Generic Structs and Enums
// ============================================================================

type GenericResult(t, e) {
	Success(value t)
	Failure(error e)
}

type Maybe(t) {
	Just(value t)
	Nothing
}

type Pair(a, b) {
	first a
	second b
}

type Box(t) {
	value t
}

const app_name = 'my app'

x = 10
x = x + 1

person = Person(name: 'alistair', age: 18)
fn add(a Int, b Int) Int {
	a + b
}
fn greet(name String) {
	name
}

fn add_generic(a, b) {
	a + b
}
_added_int = add(5, 3)
_added_str = add_generic('Hello, ', 'world!')

_callback = fn(x Int) x * 2
fn apply(x Int, f fn(Int) Int) Int {
	f(x)
}
fn apply_generic(x a, f fn(a) a) a {
	f(x)
}

double = fn(n Int) n * 2

triple = fn(n Int) n * 3

fn find_user(id Int) Option(User) {
	if id == 0 {
		None
	} else {
		Some(User(id: id, name: 'found'))
	}
}

fn divide(a Int, b Int) Result(Int, DivisionError) {
	if b == 0 {
		Err(DivisionError(message: 'Cannot divide by zero'))
	} else {
		Ok(a / b)
	}
}

fn validate(x Int) Result(Nil, ValidationError) {
	if x < 0 {
		Err(ValidationError(code: 1))
	} else {
		Ok(Nil)
	}
}

fn check_positive(x Int) Result(Int, AppError) {
	if x <= 0 {
		Err(AppError(message: 'must be positive'))
	} else {
		Ok(x * 2)
	}
}

fn max(a Int, b Int) Int {
	if a > b { a } else { b }
}

fn classify(n Int) String {
	if n < 0 {
		'negative'
	} else if n == 0 {
		'zero'
	} else {
		'positive'
	}
}

fn g() {
	'Hello!'
}

_g_result = g()

fn describe(x Int) String {
	match x {
		0 -> 'zero'
		1 -> 'one'
		else -> 'many'
	}
}

fn grade(score Int) Result(String, AppError) {
	match score {
		0..60 -> Ok('F')
		60..70 -> Ok('D')
		70..80 -> Ok('C')
		80..90 -> Ok('B')
		90..101 -> Ok('A')
		else -> Err(AppError(message: 'score must be 0-100'))
	}
}

fn handle_result(r Outcome) String {
	match r {
		Good(value) -> 'Got: ${value}'
		Bad(e) -> 'Error: ${e}'
	}
}

fn match_literal(r Outcome) String {
	match r {
		Good('special') -> 'matched special'
		Good(other) -> 'other: ${other}'
		Bad(e) -> 'error: ${e}'
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
_first_num = numbers[0] or 0

_range = 0..10

_person_name = person.name
_person_age = person.age

yes = True
no = False

_nothing = None
_greeting = 'Hello, ${app_name}!'
_complex = 'Result: ${1 + 2}'

_sum = 1 + 2
_diff = 5 - 3
_prod = 4 * 2
_quot = 10 / 2
_rem = 10 % 3

a = 5
b = 10
_eq = a == b
_neq = a != b
_lt = a < b
_gt = a > b
_lte = a <= b
_gte = a >= b

_and_result = yes && no
_or_result = yes || no
_not_result = !yes

add_result = add(5, 3)
max_result = max(10, 20)
classify_result = classify(5)
describe_result = describe(1)
grade_result = grade(85) or 'error'
example_result = example()
enum_result = handle_result(Good('success'))

error_result = divide(10, 0) or 0
_error_with_receiver = divide(10, 0) or _err -> 0

option_result = find_user(0) or User(id: 0, name: 'default')

positive_pass = check_positive(5)
positive_fail = check_positive(-1) or _err -> -1

literal_match1 = match_literal(Good('special'))
literal_match2 = match_literal(Bad('danger'))
literal_match3 = match_literal(Good('something else'))

type G {
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
println(grade_result)
println(example_result)
println(enum_result)
println(error_result)
println(option_result)
println(positive_pass)
println(positive_fail)
println(literal_match1)
println(literal_match2)
println(literal_match3)

apply_result = apply(5, double)
apply_generic_result = apply_generic(5, triple)
println(apply_result)
println(apply_generic_result)

fn inferred_double(x) {
	x * 2
}
fn inferred_add(a, b) {
	a + b
}
fn inferred_greet(name) {
	'Hello, ' + name
}
fn inferred_is_positive(n) {
	n > 0
}
fn inferred_identity(x) {
	x
}

println(inferred_double(21))
println(inferred_add(10, 5))
println(inferred_greet('World'))
println(inferred_is_positive(42))
println(inferred_identity('polymorphic'))

countdown = fn(n) if n > 0 {
	println(n)
	countdown(n - 1)
} else {
	Nil
}
countdown(3)

fn format_person(name, age) {
	'${name} is ${age} years old'
}
println(format_person('Alice', 30))

println(inferred_double(inferred_double(3)))

fn apply_twice(x, f) {
	f(f(x))
}
println(apply_twice(5, fn(n) n + 1))

// ============================================================================
// Tuples
// ============================================================================

// Tuple literals
pair = (42, 'hello')
trio = (True, 100, 'world')

// Tuple access
println(pair.0)
println(pair.1)
println(trio.2)

// Nested tuple access
nested = ((1, 2), 'outer')
println(nested.0.0)
println(nested.0.1)

// Tuple destructuring with variable binding
(ta, tb) = pair
println('a=${ta} b=${tb}')

(_, _, name3) = trio
println('name=${name3}')

// Tuple pattern matching
test_pair = (1, 'hello')
result = match test_pair {
	(0, msg) -> 'zero: ${msg}'
	(1, msg) -> 'one: ${msg}'
	else -> 'other'
}
println(result)

// ============================================================================
// Generic Types Usage
// ============================================================================

int_pair = Pair(first: 1, second: 2)
mixed_pair = Pair(first: 'age', second: 30)

println('int_pair.first: ${int_pair.first}')
println('mixed_pair: ${mixed_pair.first} = ${mixed_pair.second}')

fn safe_divide(a, b) GenericResult(Int, String) {
	if b == 0 {
		Failure('division by zero')
	} else {
		Success(a / b)
	}
}

fn unwrap_generic_result(r GenericResult(Int, String), default Int) Int {
	match r {
		Success(v) -> v
		Failure(_) -> default
	}
}

div_ok = safe_divide(10, 2)
div_err = safe_divide(10, 0)
println('10/2 = ${unwrap_generic_result(div_ok, -1)}')
println('10/0 = ${unwrap_generic_result(div_err, -1)}')

boxed_int = Box(value: 42)
boxed_str = Box(value: 'hello')
println('boxed int: ${boxed_int.value}')
println('boxed str: ${boxed_str.value}')

fn make_pair(x a, y b) Pair(a, b) {
	Pair(first: x, second: y)
}

auto_pair = make_pair(100, 'hundred')
println('auto_pair: ${auto_pair.first}, ${auto_pair.second}')

xo = Some(0)

xo
