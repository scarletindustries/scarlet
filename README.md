# AL

A small, expressive programming language.

## Install

```bash
curl -fsSL al.alistair.sh/install.sh | bash
```

## Usage

```
al run <file.al>      Run a program
al build <file.al>    Print the AST
```

## Overview

AL compiles to bytecode and runs on a stack-based virtual machine. The compiler is written in V, producing a single native binary with no dependencies.

### Error messages

The parser recovers from errors to report multiple issues at once:

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

Functions that might not return a value use `?` in their return type. Handle with `or`.

```
fn find_user(id Int) ?User {
    if id == 0 { none } else { User{ id: id, name: 'found' } }
}

user = find_user(0) or User{ id: 0, name: 'guest' }
```

### Error handling

Functions that can fail use `!` with an error type. Handle with `or`, or propagate with `!`.

```
fn divide(a Int, b Int) Int!DivisionError {
    if b == 0 {
        error DivisionError{ message: 'divide by zero' }
    } else {
        a / b
    }
}

safe = divide(10, 0) or 0
result = divide(10, 2)!
```

### Pattern matching

Match on values, enums, and literal payloads.

```
enum Result {
    Ok(String),
    Err(String),
}

fn handle(r Result) String {
    match r {
        Ok('special') -> 'matched literal',
        Ok(value) -> 'got: $value',
        Err(e) -> 'error: $e',
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

## License

MIT
