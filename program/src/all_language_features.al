struct User {
    id Int,
    name String,
}

struct DivisionError {
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
    Ok(String),
    Err(String),
}

enum Option {
    Some(Int),
    None,
}

const app_name = 'my app'

x = 10
x = x + 1

person = Person{
    name: 'alistair',
    age: 18,
}

fn add(a Int, b Int) Int {
    a + b
}

fn greet(name String) {
    name
}

callback = fn(x Int) Int {
    x * 2
}

fn find_user(id Int) ?User {
    if id == 0 {
        none
    } else {
        User{ id: id, name: 'found' }
    }
}

fn divide(a Int, b Int) Int!DivisionError {
    if b == 0 {
        error DivisionError{ message: 'Cannot divide by zero' }
    } else {
        a / b
    }
}

fn validate(x Int) !ValidationError {
    if x < 0 {
        error ValidationError{ code: 1 }
    } else {
        none
    }
}

fn max(a Int, b Int) Int {
    if a > b {
        a
    } else {
        b
    }
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

fn describe(x Int) String {
    match x {
        0 => 'zero',
        1 => 'one',
        else => 'many',
    }
}

fn handle_result(r Result) String {
    match r {
        Ok(value) => 'Got: $value',
        Err(e) => 'Error: $e',
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

greeting = 'Hello, $app_name!'
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

pid = spawn(fn() {
    msg = receive()
    msg
})
send(pid, 'hello from spawn')
my_pid = self()

add_result = add(5, 3)
max_result = max(10, 20)
classify_result = classify(5)
describe_result = describe(1)
example_result = example()
enum_result = handle_result(Ok('success'))

error_result = divide(10, 0) or 0
error_with_receiver = divide(10, 0) or err => 0
success_result = divide(10, 2)!

option_result = find_user(0) or User{ id: 0, name: 'default' }

x = enum G {
    Test
    BottledIt
}
println('x is:')
println(x)
println('what')

results = [add_result, max_result, classify_result, describe_result, example_result, enum_result, error_result, success_result, option_result]
results
