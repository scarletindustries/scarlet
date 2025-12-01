// Import
from './file.al' import a, b, c

// Comment

// Const binding
const name = 'alistair the third'

// Variable binding (immutable, can shadow)
x = 10
x = x + 1

// Struct definition
export struct Person {
    name string = 'alistair',
    age  int = 19,
}

// Struct instantiation
person = Person{
    name: 'not alistair',
    age: 18,
}

// Function (everything is an expression, last expr is return value)
fn add(a int, b int) int {
    a + b
}

// Function with no return value (returns none)
fn greet(name string) {
    println('Hello, ' + name)
}

// Anonymous function
callback = fn(x int) int {
    x * 2
}

// Function with optional return type
fn find_user(id int) ?User {
    match id == 0 {
        true => none,
        false => User{ id: id },
    }
}

// Function with error type
fn divide(a int, b int) int!DivisionError {
    match b == 0 {
        true => error DivisionError{ message: 'Cannot divide by zero' },
        false => a / b,
    }
}

// Function that might fail with no success value
fn validate(x int) !ValidationError {
    match x < 0 {
        true => error ValidationError{},
        false => none,
    }
}

// Optional AND fallible
fn fetch_user(id int) ?User!NetworkError {
    match id == 0 {
        true => none,
        false => User{ id: id },
    }
}

// Error handling with or
fn handling_errors() {
    // Provide default value
    result = divide(10, 0) or 0

    // Handle with receiver
    result = divide(10, 0) or err {
        println(err.message)
        0
    }

    // Propagate error up
    result = divide(10, 2)!
}

// Option handling
fn handling_options() {
    // Provide default
    user = find_user(0) or default_user()

    // Handle with block
    user = find_user(0) or {
        create_default_user()
    }
}

// If expression (returns a value)
fn max(a int, b int) int {
    if a > b {
        a
    } else {
        b
    }
}

// If else if chain
fn classify(n int) string {
    if n < 0 {
        'negative'
    } else if n == 0 {
        'zero'
    } else {
        'positive'
    }
}

// Match expression
fn describe(x int) string {
    match x {
        0 => 'zero',
        1 => 'one',
        _ => 'many',
    }
}

// Match with complex patterns
fn handle_result(r Result) string {
    match r {
        Result.Ok(value) => 'Got: ' + value,
        Result.Err(e) => 'Error: ' + e.message,
    }
}

// Block expression (returns last expression)
fn example() int {
    result = {
        a = 10
        b = 20
        a + b
    }
    result * 2
}

// Arrays
numbers = [1, 2, 3, 4, 5]
first = numbers[0]

// Range
range = 0..10

// Method chaining
result = get_data()
    .filter(is_valid)
    .map(transform)
    .first()

// Property access
name = person.name
age = person.age

// Assert
assert x > 0, 'x must be positive'

// Boolean literals
yes = true
no = false

// None literal
nothing = none

// String interpolation
greeting = 'Hello, $name!'
complex = 'Result: ${a + b}'

// Binary operators
sum = 1 + 2
diff = 5 - 3
prod = 4 * 2
quot = 10 / 2
rem = 10 % 3

// Comparison
eq = a == b
neq = a != b
lt = a < b
gt = a > b
lte = a <= b
gte = a >= b

// Logical operators
and_result = a && b
or_result = a || b
not_result = !a

// Export
export fn main() {
    println('Hello, world!')
}

export struct Config {
    debug bool = false,
}
