module main

import scanner
import parser
import bytecode
import flags { Flags }
import vm
import types

fn run(code string) !string {
	mut s := scanner.new_scanner(code)
	mut p := parser.new_parser(mut s)
	parsed := p.parse_program()
	if parsed.diagnostics.len > 0 {
		return error('Parse error')
	}
	checked := types.check(parsed.ast)
	if !checked.success {
		return error('Type error')
	}
	program := bytecode.compile(checked.typed_ast, checked.env, Flags{})!
	mut v := vm.new_vm(program, Flags{})
	result := v.run()!
	return vm.inspect(result)
}

fn expect(code string, expected string) ! {
	result := run(code)!
	assert result == expected, 'Expected "${expected}", got "${result}"'
}

fn test_integer_literals() ! {
	expect('42', '42')!
	expect('0', '0')!
	expect('123456', '123456')!
}

fn test_float_literals() ! {
	expect('3.14', '3.14')!
	expect('0.5', '0.5')!
}

fn test_string_literals() ! {
	expect("'hello'", 'hello')!
	expect("'hello world'", 'hello world')!
	expect("''", '')!
}

fn test_boolean_literals() ! {
	expect('true', 'true')!
	expect('false', 'false')!
}

fn test_none_literal() ! {
	expect('none', 'none')!
}

fn test_addition() ! {
	expect('1 + 2', '3')!
	expect('10 + 20 + 30', '60')!
	expect('1.5 + 2.5', '4.0')!
}

fn test_subtraction() ! {
	expect('10 - 3', '7')!
	expect('100 - 50 - 25', '25')!
}

fn test_multiplication() ! {
	expect('3 * 4', '12')!
	expect('2 * 3 * 4', '24')!
}

fn test_division() ! {
	expect('10 / 2', '5')!
	expect('100 / 10 / 2', '5')!
}

fn test_modulo() ! {
	expect('10 % 3', '1')!
	expect('17 % 5', '2')!
}

fn test_operator_precedence() ! {
	expect('2 + 3 * 4', '14')!
	expect('(2 + 3) * 4', '20')!
	expect('10 - 2 * 3', '4')!
}

fn test_equality() ! {
	expect('1 == 1', 'true')!
	expect('1 == 2', 'false')!
	expect("'a' == 'a'", 'true')!
	expect("'a' == 'b'", 'false')!
}

fn test_inequality() ! {
	expect('1 != 2', 'true')!
	expect('1 != 1', 'false')!
}

fn test_less_than() ! {
	expect('1 < 2', 'true')!
	expect('2 < 1', 'false')!
	expect('1 < 1', 'false')!
}

fn test_greater_than() ! {
	expect('2 > 1', 'true')!
	expect('1 > 2', 'false')!
	expect('1 > 1', 'false')!
}

fn test_less_than_or_equal() ! {
	expect('1 <= 2', 'true')!
	expect('1 <= 1', 'true')!
	expect('2 <= 1', 'false')!
}

fn test_greater_than_or_equal() ! {
	expect('2 >= 1', 'true')!
	expect('1 >= 1', 'true')!
	expect('1 >= 2', 'false')!
}

fn test_logical_and() ! {
	expect('true && true', 'true')!
	expect('true && false', 'false')!
	expect('false && true', 'false')!
	expect('false && false', 'false')!
}

fn test_logical_or() ! {
	expect('true || true', 'true')!
	expect('true || false', 'true')!
	expect('false || true', 'true')!
	expect('false || false', 'false')!
}

fn test_logical_not() ! {
	expect('!true', 'false')!
	expect('!false', 'true')!
}

fn test_variable_binding() ! {
	expect('x = 42
x', '42')!
}

fn test_variable_reassignment() ! {
	expect('x = 10
x = x + 5
x', '15')!
}

fn test_const_binding() ! {
	expect("const name = 'alice'
name", 'alice')!
}

fn test_multiple_variables() ! {
	expect('a = 1
b = 2
c = a + b
c', '3')!
}

fn test_if_true() ! {
	expect("if true { 'yes' } else { 'no' }", 'yes')!
}

fn test_if_false() ! {
	expect("if false { 'yes' } else { 'no' }", 'no')!
}

fn test_if_with_condition() ! {
	expect("x = 10
if x > 5 { 'big' } else { 'small' }", 'big')!
}

fn test_if_else_if() ! {
	expect("x = 0
if x < 0 { 'negative' } else if x == 0 { 'zero' } else { 'positive' }",
		'zero')!
}

fn test_if_returns_value() ! {
	expect('result = if true { 42 } else { 0 }
result', '42')!
}

fn test_match_basic() ! {
	expect("x = 1
match x {
    0 => 'zero',
    1 => 'one',
    else => 'other',
}",
		'one')!
}

fn test_match_else() ! {
	expect("x = 99
match x {
    0 => 'zero',
    1 => 'one',
    else => 'other',
}",
		'other')!
}

fn test_match_returns_value() ! {
	expect("x = 2
result = match x {
    1 => 'a',
    2 => 'b',
    else => 'c',
}
result",
		'b')!
}

fn test_function_definition() ! {
	expect('fn add(a int, b int) int { a + b }
add(2, 3)', '5')!
}

fn test_function_no_return_type() ! {
	expect("fn greet(name string) { 'Hello, ' + name }
greet('world')", 'Hello, world')!
}

fn test_nested_function_calls() ! {
	expect('fn double(x int) int { x * 2 }
fn add_one(x int) int { x + 1 }
double(add_one(5))',
		'12')!
}

fn test_anonymous_function() ! {
	expect('f = fn(x int) int { x * 2 }
f(21)', '42')!
}

fn test_function_as_value() ! {
	expect('fn apply(f, x int) int { f(x) }
double = fn(n int) int { n * 2 }
apply(double, 5)',
		'10')!
}

fn test_struct_definition_and_instantiation() ! {
	expect('struct Point {
    x int,
    y int,
}
p = Point{ x: 10, y: 20 }
p.x', '10')!
}

fn test_struct_field_access() ! {
	expect("struct User {
    name string,
    age int,
}
u = User{ name: 'alice', age: 30 }
u.name",
		'alice')!
}

fn test_struct_inspect() ! {
	result := run("struct Person {
    name string,
    age int,
}
p = Person{ name: 'bob', age: 25 }
inspect(p)")!
	assert result == 'Person{ name: bob, age: 25 }' || result == 'Person{ age: 25, name: bob }', 'Unexpected struct inspect: ${result}'
}

fn test_struct_in_function() ! {
	expect('struct Point {
    x int,
    y int,
}
fn get_x(p Point) int { p.x }
point = Point{ x: 42, y: 0 }
get_x(point)',
		'42')!
}

fn test_enum_no_payload() ! {
	expect('enum Color {
    Red,
    Green,
    Blue,
}
c = Color.Red
inspect(c)', 'Color.Red')!
}

fn test_enum_with_payload() ! {
	expect('enum Option {
    Some(int),
    None,
}
x = Option.Some(42)
inspect(x)',
		'Option.Some(42)')!
}

fn test_enum_match() ! {
	expect("enum Result {
    Ok(string),
    Err(string),
}
fn handle(r Result) string {
    match r {
        Ok(v) => 'success: ' + v,
        Err(e) => 'error: ' + e,
    }
}
handle(Result.Ok('done'))",
		'success: done')!
}

fn test_enum_shorthand_in_match() ! {
	expect("enum Result {
    Ok(string),
    Err(string),
}
fn handle(r Result) string {
    match r {
        Ok(v) => v,
        Err(e) => e,
    }
}
handle(Result.Ok('works'))",
		'works')!
}

fn test_enum_shorthand_in_function_call() ! {
	expect('enum Result {
    Ok(int),
    Err(string),
}
fn process(r Result) int {
    match r {
        Ok(n) => n,
        Err(e) => 0,
    }
}
process(Ok(42))',
		'42')!
}

fn test_array_literal() ! {
	expect('[1, 2, 3]', '[1, 2, 3]')!
}

fn test_array_index() ! {
	expect('arr = [10, 20, 30]
arr[1]', '20')!
}

fn test_array_first_element() ! {
	expect("arr = ['a', 'b', 'c']
arr[0]", 'a')!
}

fn test_array_mixed_types() ! {
	expect("[1, 'two', true]", '[1, two, true]')!
}

fn test_array_in_variable() ! {
	expect('numbers = [1, 2, 3, 4, 5]
numbers[4]', '5')!
}

fn test_range_expression() ! {
	expect('0..5', '[0, 1, 2, 3, 4]')!
}

fn test_range_in_variable() ! {
	expect('r = 1..4
r', '[1, 2, 3]')!
}

fn test_range_index() ! {
	expect('r = 0..10
r[5]', '5')!
}

fn test_error_with_or_default() ! {
	expect("struct DivError { msg string }
fn divide(a int, b int) int!DivError {
    if b == 0 {
        error DivError{ msg: 'div by zero' }
    } else {
        a / b
    }
}
divide(10, 0) or 0",
		'0')!
}

fn test_error_with_or_success() ! {
	expect("struct DivError { msg string }
fn divide(a int, b int) int!DivError {
    if b == 0 {
        error DivError{ msg: 'div by zero' }
    } else {
        a / b
    }
}
divide(10, 2) or 0",
		'5')!
}

fn test_error_propagation_simple() ! {
	expect('struct E { code int }
fn might_fail(x int) int!E {
    if x < 0 { error E{ code: 1 } } else { x * 2 }
}
might_fail(5)!',
		'10')!
}

fn test_error_with_receiver() ! {
	expect('struct E { code int }
fn fail() int!E {
    error E{ code: 42 }
}
fail() or err => 99',
		'99')!
}

fn test_optional_none_with_or() ! {
	expect("struct User { name string }
fn find(id int) ?User {
    if id == 0 { none } else { User{ name: 'found' } }
}
result = find(0) or User{ name: 'default' }
result.name",
		'default')!
}

fn test_optional_some_with_or() ! {
	expect("struct User { name string }
fn find(id int) ?User {
    if id == 0 { none } else { User{ name: 'found' } }
}
result = find(1) or User{ name: 'default' }
result.name",
		'found')!
}

fn test_simple_interpolation() ! {
	expect("name = 'world'
'Hello, \$name!'", 'Hello, world!')!
}

fn test_expression_interpolation() ! {
	expect("'Result: \${1 + 2}'", 'Result: 3')!
}

fn test_multiple_interpolations() ! {
	expect("a = 1
b = 2
'\$a + \$b = \${a + b}'", '1 + 2 = 3')!
}

fn test_interpolation_with_variable() ! {
	expect("x = 42
'Value: \$x'", 'Value: 42')!
}

fn test_block_returns_last() ! {
	expect('{
    a = 1
    b = 2
    a + b
}', '3')!
}

fn test_block_as_value() ! {
	expect('result = {
    x = 10
    y = 20
    x * y
}
result', '200')!
}

fn test_spawn_returns_pid() ! {
	result := run('pid = spawn(fn() { 42 })
inspect(pid)')!
	assert result.starts_with('<pid '), 'Expected PID, got ${result}'
}

fn test_self_returns_pid() ! {
	result := run('inspect(self())')!
	assert result == '<pid 0>', 'Expected <pid 0>, got ${result}'
}

fn test_send_and_receive() ! {
	expect("pid = spawn(fn() {
    msg = receive()
    msg
})
send(pid, 'hello')
'done'",
		'done')!
}

fn test_inspect_int() ! {
	expect('inspect(42)', '42')!
}

fn test_inspect_string() ! {
	expect("inspect('hello')", 'hello')!
}

fn test_inspect_bool() ! {
	expect('inspect(true)', 'true')!
}

fn test_inspect_none() ! {
	expect('inspect(none)', 'none')!
}

fn test_inspect_array() ! {
	expect('inspect([1, 2, 3])', '[1, 2, 3]')!
}

fn test_inspect_struct() ! {
	result := run('struct Point { x int, y int }
inspect(Point{ x: 1, y: 2 })')!
	assert result == 'Point{ x: 1, y: 2 }' || result == 'Point{ y: 2, x: 1 }', 'Unexpected: ${result}'
}

fn test_negative_numbers() ! {
	expect('-3', '-3')!
	expect('-42', '-42')!
	expect('5 + -3', '2')!
	expect('x = -10
x', '-10')!
	expect('- -5', '5')!
}

fn test_deeply_nested_calls() ! {
	expect('fn id(x int) int { x }
id(id(id(id(42))))', '42')!
}

fn test_empty_array() ! {
	expect('[]', '[]')!
}

fn test_string_concatenation() ! {
	expect("'hello' + ' ' + 'world'", 'hello world')!
}
