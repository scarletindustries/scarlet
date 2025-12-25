# AL

A small, statically-typed, expression-oriented programming language.

## Install

```bash
curl -fsSL al.alistair.sh/install.sh | bash
```

## Usage

```
al run <file.al>      Run a program
al repl               Start interactive REPL
al check <file.al>    Type-check without running
al fmt [path]         Format source files
al build <file.al>    Print the AST
```

## Overview

AL compiles to bytecode and runs on a stack-based virtual machine. The compiler is written in V, producing a single native binary with no dependencies.

**Statically typed with inference.** Every expression has a type known at compile time. The type checker catches errors before your code runs, while inference keeps the syntax clean.

**Expression-oriented.** No statements—if/else, match, and blocks all return values.

**Unified error handling.** Both optional values (`?T`) and errors (`T!E`) use the same `or` syntax.

### Error messages

The parser and type checker recover from errors to report multiple issues at once:

```
error: Unexpected ')'
  --> example.al:3:8
   |
3  |     x = )
   |         ^

error: Unexpected ']'
  --> example.al:7:12
   |
7  |     value = ]
   |             ^

Found 2 errors
```

Type errors are caught at compile time:

```
error: Type mismatch: expected Int, got String
  --> example.al:5:12
   |
5  |     return 'oops'
   |            ^^^^^^

error: Unknown variable: 'undefined_var'
  --> example.al:8:5
   |
8  |     undefined_var + 1
   |     ^^^^^^^^^^^^^

Found 2 errors
```

### Interactive REPL

Explore the language interactively. Definitions persist across entries:

```
$ al repl
al 0.0.1 REPL
Type expressions to evaluate. Use 'exit' or Ctrl+D to quit.

>>> fn square(n Int) Int { n * n }
none
>>> square(5)
25
>>> x = 10
none
>>> x + square(3)
19
```

### Code formatter

Format your code with `al fmt`:

```bash
al fmt .              # Format all .al files in current directory
al fmt src/           # Format all .al files in src/
al fmt file.al        # Format a single file
al fmt --check .      # Check formatting without modifying
al fmt --stdin        # Format from stdin
```

## Language

### Everything is an expression

No statements. If/else, match, and blocks all return values.

```
result = if x > 0 { 'positive' } else { 'negative' }

grade = match score {
    90..100 -> 'A',
    80..90 -> 'B',
    else -> 'C',
}
```

### Optional values

Functions that might not return a value use `?` in their return type. Handle missing values with `or`.

```
fn find_user(id Int) ?User {
    if id == 0 { none } else { User{ id: id, name: 'found' } }
}

// Provide a default with 'or'
user = find_user(0) or User{ id: 0, name: 'guest' }
```

### Error handling

Functions that can fail use `!` with an error type. Handle with `or`.

```
fn divide(a Int, b Int) Int!DivisionError {
    if b == 0 {
        error DivisionError{ message: 'divide by zero' }
    } else {
        a / b
    }
}

safe = divide(10, 0) or 0
result = divide(10, 2) or err -> {
    println('Error: ${err.message}')
    0
}
```

### Pattern matching

Match on values, ranges, enums, literal payloads, and arrays.

```
// Match on values with or-patterns
fn describe(x Int) String {
    match x {
        0 -> 'zero',
        1 | 2 | 3 -> 'small',
        else -> 'other',
    }
}

// Match on enum payloads
enum Result { Ok(String), Err(String) }

fn handle(r Result) String {
    match r {
        Ok('special') -> 'matched literal',
        Ok(value) -> 'got: $value',
        Err(e) -> 'error: $e',
    }
}

// Match on arrays
fn first(arr []a) ?a {
    match arr {
        [] -> none,
        [head, ..] -> head,
    }
}
```

### Structs and enums

```
struct Person {
    name String,
    age Int,
}

enum Status {
    Active,
    Inactive,
    Banned(String),
}

person = Person{ name: 'alice', age: 30 }
status = Status.Banned('spam')
```

### First-class functions

```
fn apply(x Int, f fn(Int) Int) Int {
    f(x)
}

double = fn(n Int) Int { n * 2 }
result = apply(5, double)
```

### String interpolation

```
name = 'world'
greeting = 'Hello, $name!'
math = 'Result: ${1 + 2 * 3}'
```

### Static typing with inference

Types are inferred from context—including function parameters and return types.

```
// Variable types inferred
count = 42
name = 'alice'
numbers = [1, 2, 3]

// Function parameter and return types inferred
fn double(x) { x * 2 }
fn add(a, b) { a + b }
fn greet(name) { 'Hello, ' + name }

// Explicit annotations when needed
fn divide(a Int, b Int) Int!DivisionError {
    if b == 0 { error DivisionError{ message: 'divide by zero' } }
    else { a / b }
}
```

### Generics

Use lowercase type variables for polymorphic functions.

```
fn identity(x a) a {
    x
}

fn first(arr []a) ?a {
    match arr {
        [] -> none,
        [head, ..] -> head,
    }
}

// Works with any type
x = identity(42)
y = identity('hello')
head = first([1, 2, 3]) or 0
```

### Constants

Top-level constants are declared with `const`.

```
const pi = 314
const app_name = 'my app'
const max_retries = 3
```

## Experimental features

Some features require explicit opt-in flags:

```bash
# File and network I/O
al run --experimental-shitty-io server.al

# Standard library functions
al run --experimental-std-lib program.al
```

### I/O builtins (requires `--experimental-shitty-io`)

```
content = read_file('data.txt')
write_file('output.txt', content)

listener = tcp_listen(8080)
client = tcp_accept(listener)
data = tcp_read(client)
tcp_write(client, 'HTTP/1.1 200 OK\r\n\r\nHello')
tcp_close(client)
```

### Other builtins

```
// Convert any value to its string representation
s = inspect(some_value)

// Split strings
parts = str_split('a,b,c', ',')
```

## License

MIT
