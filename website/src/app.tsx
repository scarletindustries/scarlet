import "./globals.css";

// The whole site is data: an ordered list of sections, each a sequence of
// paragraphs and code snippets. Paragraphs are plain strings where `backticks`
// render as inline code. Snippets are AL source unless marked `plain`.
//
// Every AL snippet is type checked against the real compiler by
// scripts/check-examples.ts, which runs as part of the build. Keep it that
// way: if an example stops compiling, the site should stop building.

export type Snippet = {
  id: string;
  code: string;
  // Files written next to the snippet when it is type checked, so that
  // `import ./util` style imports resolve. Keys are file names.
  files?: Record<string, string>;
  // Set to false to skip type checking. Only for snippets that are
  // deliberately wrong (e.g. showing a compile error).
  check?: boolean;
  // Plain text (shell commands, compiler output, tables). Not AL code: not
  // checked, not syntax highlighted.
  plain?: boolean;
};

export type Section = {
  // Anchor id, used in the URL fragment and the table of contents.
  id: string;
  title: string;
  // Subsections render as h3 and nest under the previous top-level section
  // in the table of contents.
  sub?: boolean;
  body: (string | Snippet)[];
};

const isSnippet = (block: string | Snippet): block is Snippet =>
  typeof block !== "string";

export const sections: Section[] = [
  // ======================================================================
  // Introduction
  // ======================================================================
  {
    id: "introduction",
    title: "Introduction",
    body: [
      "AL is a small, statically typed language for writing programs that do many things at once. It was built for servers. The compiler, runtime, and standard library ship as one native binary with no dependencies, and this page is the entire documentation.",
      "Here is a complete HTTP server:",
      {
        id: "intro-http",
        code: `import al/http

http.serve(
    '0.0.0.0',
    8080,
    fn(_req) http.text('Hello from al/http!'),
) or e -> println('serve failed: \${e}')`,
      },
      "Each connection is handled by its own process. A process is a piece of runtime state costing a few hundred bytes, cheap enough to start one per connection and forget about it. The runtime schedules processes across every CPU core. Code inside a process reads like it blocks, and when it waits on a socket, the runtime parks it and runs another one. The language has no `async` keyword and no locks.",
      "There is no null and there are no exceptions. A function that can fail returns a `Result`, a value that can be absent is an `Option`, and the type checker rejects programs that ignore either case. Both unwrap with the `or` keyword.",
      "Programs are fully type checked, but you will rarely write a type. The checker infers them, generics included.",
      "The language is small on purpose. One `type` keyword defines every kind of data. Constructors are ordinary functions. Everything is an expression. There are no classes, no inheritance, no interfaces, no macros, and no loops. Recursion covers iteration, and tail calls reuse the stack frame so they never overflow it.",
      "The rest of this page walks through the whole language from the ground up. Read it top to bottom, or jump around with the contents below. Every code block on this page passes `al check` against the current compiler, so what you read is what compiles.",
    ],
  },

  // ======================================================================
  // Installation
  // ======================================================================
  {
    id: "installation",
    title: "Installation",
    body: [
      "One command:",
      {
        id: "install-cmd",
        plain: true,
        code: `curl -fsSL al.alistair.sh/install.sh | bash`,
      },
      "This puts the `al` binary in `~/.al/bin` and adds that to your PATH. Run `al upgrade` later to get a newer build. There is nothing else to install: the standard library lives inside the binary.",
      "AL is early software. The version number starts with 0.0 and means it. Things will change.",
    ],
  },

  // ======================================================================
  // Hello world
  // ======================================================================
  {
    id: "hello-world",
    title: "Hello world",
    body: [
      "Save this as `hello.al`:",
      {
        id: "hello",
        code: `println('hello, world')`,
      },
      "Run it:",
      {
        id: "hello-run",
        plain: true,
        code: `$ al run hello.al
hello, world`,
      },
      "A program is a file of statements, run top to bottom. There is no main function and no boilerplate. `println` prints any value: numbers, arrays, your own types.",
    ],
  },

  // ======================================================================
  // Comments
  // ======================================================================
  {
    id: "comments",
    title: "Comments",
    body: [
      {
        id: "comments",
        code: `// A line comment.

/* A block comment. */

/** A doc comment. It documents the declaration that follows it. */
fn add(a, b) {
    a + b
}

println(add(1, 2))`,
      },
    ],
  },

  // ======================================================================
  // Variables
  // ======================================================================
  {
    id: "variables",
    title: "Variables",
    body: [
      "A variable is a name bound to a value with `=`. Its type comes from the value. Binding an existing name again shadows it with a new variable; nothing is ever mutated.",
      {
        id: "variables",
        code: `count = 42           // Int
name = 'alice'       // String
active = True        // Bool

// Rebinding shadows: x now names a new value, the old one is gone
x = 10
x = x + 1

println('\${name}: \${count}')  // alice: 42
println(x)                    // 11
println(active)               // True`,
      },
      "An unused variable is a compile error. AL has no warnings; everything the compiler notices is either fine or an error. If a binding is intentional but unused, prefix it with an underscore:",
      {
        id: "unused-error",
        plain: true,
        code: `error: 'x' is unused; prefix with '_' to ignore
1  | x = 42
     ^`,
      },
      {
        id: "unused-fix",
        code: `_x = 42  // fine, explicitly ignored
println('done')`,
      },
    ],
  },

  // ======================================================================
  // Constants
  // ======================================================================
  {
    id: "constants",
    title: "Constants",
    body: [
      "`const` declares a top-level constant. Constants cannot be reassigned and are visible to every function in the file.",
      {
        id: "constants",
        code: `const max_retries = 3
const app_name = 'my app'

fn banner() String {
    'Welcome to \${app_name}'
}

println(banner())
println(max_retries)`,
      },
    ],
  },

  // ======================================================================
  // Functions
  // ======================================================================
  {
    id: "functions",
    title: "Functions",
    body: [
      "`fn` defines a function. The body is a block, and the last expression in the block is the return value. There is no `return` keyword.",
      "Parameter and return types are inferred from how the function is used. You can also write them out; signatures then read as documentation.",
      {
        id: "functions",
        code: `// Types fully inferred
fn double(x) { x * 2 }
fn add(a, b) { a + b }
fn greet(name) { 'Hello, ' + name }

// The same thing with explicit types
fn abs(n Int) Int {
    if n < 0 { -n } else { n }
}

println(double(21))      // 42
println(add(10, 5))      // 15
println(greet('world'))  // Hello, world
println(abs(-3))         // 3`,
      },
    ],
  },

  // ======================================================================
  // Lambdas
  // ======================================================================
  {
    id: "lambdas",
    title: "Lambdas",
    sub: true,
    body: [
      "Anonymous functions are written `fn(args) body`. A single expression body needs no braces. Functions are values like any other, so they can be passed around and stored in bindings.",
      {
        id: "lambdas",
        code: `fn apply(x, f) { f(x) }
fn compose(f, g) { fn(x) f(g(x)) }

// Braceless lambda: the body starts right after the )
double = fn(n) n * 2
add_one = fn(n) n + 1

println(apply(5, double))  // 10

double_then_add = compose(add_one, double)
println(double_then_add(5))  // 11

fn twice(x, f) { f(f(x)) }
println(twice(3, fn(n) n + 1))  // 5`,
      },
    ],
  },

  // ======================================================================
  // Recursion instead of loops
  // ======================================================================
  {
    id: "recursion",
    title: "Recursion instead of loops",
    sub: true,
    body: [
      "AL has no `for` and no `while`. Iteration is recursion, usually with an accumulator argument.",
      "When a call is the last thing a function does, the runtime reuses the current stack frame instead of growing the stack. This holds for every call in tail position, calls to other functions included, so recursive loops run in constant stack space. Recursing a million levels deep is fine.",
      {
        id: "recursion",
        code: `// Not tail recursive: the multiply happens after the call returns
fn factorial(n Int) Int {
    if n <= 1 { 1 }
    else { n * factorial(n - 1) }
}

// Tail recursive: the call is the last thing that happens.
// Runs in constant stack space.
fn factorial_iter(n Int, acc Int) Int {
    if n <= 1 { acc }
    else { factorial_iter(n - 1, n * acc) }
}

fn sum(arr Array(Int), acc Int) Int {
    match arr {
        [] -> acc
        [head, ..rest] -> sum(rest, acc + head)
    }
}

println(factorial(5))             // 120
println(factorial_iter(5, 1))     // 120
println(sum([1, 2, 3, 4, 5], 0))  // 15`,
      },
      "Top-level functions see each other regardless of the order they are defined in, so mutual recursion needs no forward declarations.",
      {
        id: "mutual-recursion",
        code: `fn is_even(n Int) Bool {
    if n == 0 { True } else { is_odd(n - 1) }
}

fn is_odd(n Int) Bool {
    if n == 0 { False } else { is_even(n - 1) }
}

println(is_even(10))  // True
println(is_odd(7))    // True`,
      },
    ],
  },

  // ======================================================================
  // Everything is an expression
  // ======================================================================
  {
    id: "expressions",
    title: "Everything is an expression",
    body: [
      "`if`, `match`, and blocks all produce values. Every `if` has an `else`, because the expression needs a value either way. The last expression in a block is the block's value, and a block in braces doubles as grouping where other languages use parentheses.",
      {
        id: "expressions",
        code: `x = 5

result = if x > 0 { 'positive' } else { 'non-positive' }

// A block is an expression. Its value is its last expression.
total = {
    a = 10
    b = 20
    a + b
}

// Blocks group, like parentheses do elsewhere
nine = {1 + 2} * 3

println(result)  // positive
println(total)   // 30
println(nine)    // 9`,
      },
    ],
  },

  // ======================================================================
  // Strings
  // ======================================================================
  {
    id: "strings",
    title: "Strings",
    body: [
      "Strings are single quoted. `$name` interpolates a variable, `${...}` interpolates any expression. Escape a quote with a backslash.",
      {
        id: "strings",
        code: `name = 'world'
greeting = 'Hello, \$name!'
math = 'Result: \${1 + 2 * 3}'

println(greeting)  // Hello, world!
println(math)      // Result: 7

quote = 'She said \\'hello\\''
println(quote)     // She said 'hello'`,
      },
      "`al/string` has the basics:",
      {
        id: "string-functions",
        code: `import al/string

parts = string.split('a,b,c', ',')
println(parts)                          // [a, b, c]
println(string.length('hello'))         // 5
println(string.contains('hello', 'ell'))  // True
println(string.trim('  padded  '))      // padded

// string.inspect turns any value into its printed form
println(string.inspect([1, 2, 3]))      // [1, 2, 3]`,
      },
    ],
  },

  // ======================================================================
  // Numbers
  // ======================================================================
  {
    id: "numbers",
    title: "Numbers",
    body: [
      "`Int` and `Float` are distinct types. The arithmetic operators work on both, but the two never mix in one expression and never convert implicitly. Convert on purpose with `al/float` and `al/int`.",
      {
        id: "numbers",
        code: `import al/float
import al/int

a = 7
b = 2
println('\${a + b} \${a - b} \${a * b} \${a / b} \${a % b}')  // 9 5 14 3 1

// Floats use the same operators
half = float.from_int(1) / 2.0
println(half)                  // 0.5
println(float.round(2.7))      // 3
println(float.floor(2.7))      // 2

diff = 1.0 - 3.5
println(float.abs(diff))       // 2.5

println(int.max(a, b))         // 7
println(int.clamp(99, 0, 10))  // 10`,
      },
    ],
  },

  // ======================================================================
  // Arrays
  // ======================================================================
  {
    id: "arrays",
    title: "Arrays",
    body: [
      "An array is an immutable ordered collection where every element has the same type. Indexing returns an `Option`, because the index might be out of range. There is no exception to catch and no crash; you handle the `Option`.",
      {
        id: "arrays",
        code: `numbers = [1, 2, 3, 4, 5]
names = ['alice', 'bob', 'charlie']

// Indexing returns Option. Unwrap it with 'or'.
first = numbers[0] or 0
missing = names[10] or 'nobody'
println('\${first} \${missing}')  // 1 nobody

// Spread builds new arrays out of old ones
combined = [..[1, 2], ..[3, 4]]
more = [0, ..numbers, 6]
println(combined)  // [1, 2, 3, 4]
println(more)      // [0, 1, 2, 3, 4, 5, 6]`,
      },
      "`al/list` has the usual functions over arrays:",
      {
        id: "list-functions",
        code: `import al/list

numbers = [1, 2, 3, 4, 5]

doubled = list.map(numbers, fn(n) n * 2)
evens = list.filter(numbers, fn(n) n % 2 == 0)
total = list.fold(numbers, 0, fn(acc, n) acc + n)

println(doubled)  // [2, 4, 6, 8, 10]
println(evens)    // [2, 4]
println(total)    // 15

println(list.reverse(numbers))         // [5, 4, 3, 2, 1]
println(list.contains(numbers, 3))     // True
println(list.find(numbers, fn(n) n > 3))  // Some(4)`,
      },
    ],
  },

  // ======================================================================
  // Tuples
  // ======================================================================
  {
    id: "tuples",
    title: "Tuples",
    body: [
      "A tuple groups a fixed number of values of mixed types. Tuples have two or more elements. Access elements by position, or destructure the whole thing in a binding.",
      {
        id: "tuples",
        code: `pair = (1, 'hello')
println('\${pair.0} \${pair.1}')  // 1 hello

// Functions use tuples to return several values
fn divide(a, b) { (a / b, a % b) }

(quotient, remainder) = divide(10, 3)
println('\${quotient} r\${remainder}')  // 3 r1`,
      },
    ],
  },

  // ======================================================================
  // Custom types
  // ======================================================================
  {
    id: "custom-types",
    title: "Custom types",
    body: [
      "The `type` keyword defines all data. A type with named fields is a record. Construct it by calling the type name with labeled arguments, read fields with a dot, and copy one with `..spread` plus the fields you want to change.",
      {
        id: "records",
        code: `type Person {
    name String
    age Int
}

person = Person(name: 'alice', age: 30)
println(person.name)  // alice

// Copy with overrides. The original is untouched.
older = Person(..person, age: 31)
println(older.age)    // 31
println(person.age)   // 30`,
      },
    ],
  },

  // ======================================================================
  // Sum types
  // ======================================================================
  {
    id: "sum-types",
    title: "Sum types",
    sub: true,
    body: [
      "A type with several constructors is a sum type: a value is exactly one of the alternatives. Constructors can carry fields or be bare names, and a field can be the type being defined. Pattern matching is how you find out which one you have.",
      {
        id: "sum-types",
        code: `type Expr {
    Num(value Int)
    Add(left Expr right Expr)
    Mul(left Expr right Expr)
}

type Status {
    Active
    Inactive
    Banned(reason String)
}

fn evaluate(e Expr) Int {
    match e {
        Num(n) -> n
        Add(l, r) -> evaluate(l) + evaluate(r)
        Mul(l, r) -> evaluate(l) * evaluate(r)
    }
}

// (1 + 2) * 3
expr = Mul(Add(Num(1), Num(2)), Num(3))
println(evaluate(expr))  // 9
println(Banned('spam'))  // Banned(spam)`,
      },
      "Constructors are ordinary functions. Pass them anywhere a function goes.",
      {
        id: "constructors-as-functions",
        code: `import al/list

type Wrapper { Box(value Int) }

boxed = list.map([1, 2, 3], Box)
println(boxed)  // [Box(1), Box(2), Box(3)]`,
      },
    ],
  },

  // ======================================================================
  // Generics
  // ======================================================================
  {
    id: "generics",
    title: "Generics",
    sub: true,
    body: [
      "Lowercase names in type position are type variables; concrete types are capitalized. The case rule is part of the grammar, the same way it separates constructors from binders in patterns. The checker resolves type variables at each call site, so one function works across many types.",
      {
        id: "generic-functions",
        code: `fn identity(x a) a { x }

fn first(arr Array(a)) Option(a) {
    match arr {
        [] -> None
        [head, ..] -> Some(head)
    }
}

fn map_array(arr Array(a), f fn(a) b) Array(b) {
    match arr {
        [] -> []
        [head, ..rest] -> [f(head), ..map_array(rest, f)]
    }
}

// The same function, used at different types
n = identity(42)
s = identity('hello')
doubled = map_array([1, 2, 3], fn(x) x * 2)

println('\${n} \${s}')  // 42 hello
println(doubled)      // [2, 4, 6]`,
      },
      "Types take parameters the same way. The binary search tree below stores any element type: `to_array` walks a `Tree(t)`, while `insert` compares values with `<` and so takes a `Tree(Int)`.",
      {
        id: "generic-types",
        code: `import al/list

type Tree(t) {
    Leaf
    Node(left Tree(t) value t right Tree(t))
}

// In-order walk: left subtree, value, right subtree
fn to_array(tree Tree(t)) Array(t) {
    match tree {
        Leaf -> []
        Node(left, value, right) -> [..to_array(left), value, ..to_array(right)]
    }
}

// Insert keeps the tree ordered, so to_array reads back sorted
fn insert(tree Tree(Int), x Int) Tree(Int) {
    match tree {
        Leaf -> Node(Leaf, x, Leaf)
        Node(left, value, right) ->
            if x < value { Node(insert(left, x), value, right) }
            else { Node(left, value, insert(right, x)) }
    }
}

tree = list.fold([5, 3, 8, 1, 9, 2, 7], Leaf, insert)
println(to_array(tree))  // [1, 2, 3, 5, 7, 8, 9]

// The same to_array works on a Tree(String)
names = Node(Leaf, 'grace', Node(Leaf, 'linus', Leaf))
println(to_array(names))  // [grace, linus]`,
      },
    ],
  },

  // ======================================================================
  // Type aliases
  // ======================================================================
  {
    id: "type-aliases",
    title: "Type aliases",
    sub: true,
    body: [
      "`type X = Y` declares an alias: a second name for the same type, interchangeable with the original. To make a distinct type that wraps another, declare a constructor instead. The wrapper does not unify with what it wraps, so the checker keeps them apart.",
      {
        id: "type-aliases",
        code: `type Pair(a, b) { first a second b }

// Aliases: interchangeable with the right-hand side
type Id = Int
type StringPair = Pair(String, String)

// A wrapper: distinct from Int, must be constructed and unwrapped
type UserId { UserId(value Int) }

n Id = 42
u = UserId(value: 42)
println(n)        // 42
println(u.value)  // 42`,
      },
    ],
  },

  // ======================================================================
  // Pattern matching
  // ======================================================================
  {
    id: "pattern-matching",
    title: "Pattern matching",
    body: [
      "`match` compares a value against patterns, top to bottom, and runs the arm of the first pattern that fits. Patterns can be literals, several literals separated by `|`, ranges, constructors, arrays, tuples, or a binder name. `else` catches everything left over.",
      {
        id: "pattern-matching",
        code: `fn describe(x Int) String {
    match x {
        0 -> 'zero'
        1 | 2 | 3 -> 'small'
        4..100 -> 'medium'
        else -> 'big'
    }
}

println(describe(0))    // zero
println(describe(2))    // small
println(describe(50))   // medium
println(describe(999))  // big`,
      },
    ],
  },

  // ======================================================================
  // Matching constructors
  // ======================================================================
  {
    id: "matching-constructors",
    title: "Matching constructors",
    sub: true,
    body: [
      "Constructor patterns destructure a value and bind its fields. In a pattern, a capitalized name is a constructor to take apart and a lowercase name is a variable to bind. That rule also catches typos: misspell a constructor and you get an error instead of a silently always-matching binder.",
      {
        id: "matching-constructors",
        code: `type Reply {
    Ack(value String)
    Nack(value String)
}

fn handle(r Reply) String {
    match r {
        Ack('ping') -> 'pong'           // match a literal payload
        Ack(value) -> 'got: \${value}'   // bind the payload
        Nack(e) -> 'failed: \${e}'
    }
}

println(handle(Ack('ping')))   // pong
println(handle(Ack('hello')))  // got: hello
println(handle(Nack('oops')))  // failed: oops`,
      },
      "Patterns on records may name the fields they care about and ignore the rest with `..`. Single-constructor types destructure directly in a binding, no match needed.",
      {
        id: "field-patterns",
        code: `type User { name String age Int email String }

fn name_of(u User) String {
    match u {
        User(name: n, ..) -> n
    }
}

// Irrefutable destructuring in a binding
type Point { x Int y Int }
Point(x: a, y: b) = Point(x: 3, y: 4)

println(name_of(User(name: 'alice', age: 30, email: 'a@b.c')))  // alice
println(a + b)  // 7`,
      },
    ],
  },

  // ======================================================================
  // Matching arrays
  // ======================================================================
  {
    id: "matching-arrays",
    title: "Matching arrays",
    sub: true,
    body: [
      "Array patterns match on length and structure. `..rest` binds whatever remains, which makes the head/tail recursion pattern natural.",
      {
        id: "matching-arrays",
        code: `fn sum(arr Array(Int)) Int {
    match arr {
        [] -> 0
        [head, ..tail] -> head + sum(tail)
    }
}

fn describe(arr Array(Int)) String {
    match arr {
        [] -> 'empty'
        [only] -> 'one element: \${only}'
        [first, ..] -> 'starts with \${first}'
    }
}

println(sum([1, 2, 3, 4, 5]))   // 15
println(describe([]))           // empty
println(describe([7]))          // one element: 7
println(describe([7, 8, 9]))    // starts with 7`,
      },
      "Array patterns nest inside other patterns. A match on a tuple of two arrays takes both apart at once. The merge step of a merge sort is one match expression:",
      {
        id: "merge",
        code: `// Merge two sorted arrays into one
fn merge(left Array(Int), right Array(Int)) Array(Int) {
    match (left, right) {
        ([], ys) -> ys
        (xs, []) -> xs
        ([x, ..xs], [y, ..ys]) ->
            if x <= y { [x, ..merge(xs, right)] }
            else { [y, ..merge(left, ys)] }
    }
}

println(merge([1, 4, 9], [2, 3, 8]))  // [1, 2, 3, 4, 8, 9]`,
      },
    ],
  },

  // ======================================================================
  // Guards
  // ======================================================================
  {
    id: "guards",
    title: "Guards",
    sub: true,
    body: [
      "A guard is an `if` between the pattern and the arrow. The arm runs only when the pattern matches and the guard is true.",
      {
        id: "guards",
        code: `fn classify(n Int) String {
    match n {
        x if x < 0 -> 'negative'
        0 -> 'zero'
        else -> 'positive'
    }
}

println(classify(-5))  // negative
println(classify(0))   // zero
println(classify(3))   // positive`,
      },
    ],
  },

  // ======================================================================
  // Exhaustiveness
  // ======================================================================
  {
    id: "exhaustiveness",
    title: "Exhaustiveness",
    sub: true,
    body: [
      "Every match must cover every possible value. This match forgets one constructor:",
      {
        id: "exhaustive-bad",
        check: false,
        code: `type Status {
    Active
    Inactive
    Banned(reason String)
}

fn describe(s Status) String {
    match s {
        Active -> 'active'
        Inactive -> 'inactive'
    }
}`,
      },
      "and the compiler rejects it:",
      {
        id: "exhaustive-error",
        plain: true,
        code: `error: Match is not exhaustive, missing: Banned(_)
8  |     match s {
              ^`,
      },
      "This is what makes refactoring safe. Add a constructor to a type and the compiler lists every match in the program that needs a new arm.",
    ],
  },

  // ======================================================================
  // Option and Result
  // ======================================================================
  {
    id: "option-and-result",
    title: "Option and Result",
    body: [
      "AL has no null and no exceptions. Absence and failure are values:",
      "`Option(a)` is a value that might not exist: `Some(value)` or `None`. `Result(t, e)` is an operation that might fail: `Ok(value)` or `Err(error)`. Both are ordinary types from the prelude. Nothing about them is built into the compiler except the `or` keyword for unwrapping them.",
      {
        id: "option",
        code: `type User { id Int name String }

fn find_user(id Int) Option(User) {
    if id == 0 { None }
    else { Some(User(id: id, name: 'found')) }
}

// Unwrap with a fallback
user = find_user(0) or User(id: 0, name: 'guest')
println(user.name)  // guest

// Array indexing returns Option too
names = ['alice', 'bob']
println(names[5] or 'nobody')  // nobody`,
      },
      {
        id: "result",
        code: `type DivError { message String }

fn divide(a Int, b Int) Result(Int, DivError) {
    if b == 0 { Err(DivError(message: 'divide by zero')) }
    else { Ok(a / b) }
}

println(divide(10, 2))  // Ok(5)
println(divide(10, 0))  // Err(DivError(divide by zero))`,
      },
    ],
  },

  // ======================================================================
  // The or keyword
  // ======================================================================
  {
    id: "or",
    title: "The or keyword",
    sub: true,
    body: [
      "`or` unwraps both `Option` and `Result`. If the left side is `Some` or `Ok`, you get the value inside. Otherwise the right side runs. The right side can be a plain fallback value, or an arrow that binds the error so you can look at it.",
      {
        id: "or-operator",
        code: `type DivError { message String }

fn divide(a Int, b Int) Result(Int, DivError) {
    if b == 0 { Err(DivError(message: 'divide by zero')) }
    else { Ok(a / b) }
}

// Fallback value
safe = divide(10, 0) or 0
println(safe)  // 0

// Bind the error and handle it
result = divide(10, 0) or err -> {
    println('failed: \${err.message}')
    0
}
println(result)  // 0

// Chains read left to right
numbers = [1, 2, 3]
doubled_first = numbers[0] or 0
println(doubled_first * 2)  // 2`,
      },
      "Functions that only produce side effects return `Nil`, and their failures are handled the same way: `socket.close(sock) or Nil` means close, and shrug if it was already closed.",
    ],
  },

  // ======================================================================
  // Modules
  // ======================================================================
  {
    id: "modules",
    title: "Modules",
    body: [
      "One file is one module. Everything in a module is private unless marked `pub`. Imports name a path: `./relative` for your own files, `al/...` for the standard library. Use the imported module through its name, rename it with `as`, or pull specific names into scope with `.{}`.",
      "A module called `util.al`:",
      {
        id: "modules-util",
        code: `pub fn quote(s String) String {
    '"' + s + '"'
}

pub type Id { Id(value Int) }`,
      },
      "And a `main.al` next to it that uses it:",
      {
        id: "modules",
        files: {
          "util.al": `pub fn quote(s String) String {
    '"' + s + '"'
}

pub type Id { Id(value Int) }`,
        },
        code: `import ./util
import ./util as u
import ./util.{quote, Id}
import al/string

println(util.quote('hi'))   // "hi"   (qualified)
println(u.quote('hi'))      // "hi"   (aliased)
println(quote('hi'))        // "hi"   (imported directly)

id = Id(value: 7)
println(string.inspect(id))  // Id(7)`,
      },
    ],
  },

  // ======================================================================
  // Binaries
  // ======================================================================
  {
    id: "binaries",
    title: "Binaries",
    body: [
      "A `Binary` is a sequence of raw bytes. The `<<>>` syntax builds one, and the same syntax takes one apart in a match. Each segment is one byte unless you give it a size in bits. A `:binary` segment matches the rest of the bytes without copying them.",
      {
        id: "binaries",
        code: `import al/binary
import al/string

// Three bytes
header = <<1, 2, 3>>
println(binary.byte_size(header))  // 3

// Sizes are in bits: two 4-bit fields pack into one byte
packed = <<1:4, 2:4>>
println(string.inspect(packed))    // <<18>>

// Match on byte structure. a and b each bind one byte.
sum = match binary.from_string('AB') {
    <<a, b>> -> a + b
    else -> 0
}
println(sum)  // 131  (65 + 66)`,
      },
      "This is the syntax network protocols are parsed with. The standard library's HTTP parser is written on top of it. Here is a small packet format, built and then parsed back into its fields:",
      {
        id: "packet",
        code: `import al/binary

// A packet: 4-bit version, 4-bit kind, 16-bit length, then the payload
payload = binary.from_string('ping')
length = binary.byte_size(payload)
packet = <<1:4, 2:4, length:16, payload:binary>>

// The same segments take it apart
match packet {
    <<version:4, kind:4, length:16, body:binary>> -> {
        text = binary.to_string(body) or '(not utf8)'
        println('version \${version}, kind \${kind}, \${length} bytes: \${text}')
    }
    else -> println('malformed packet')
}`,
      },
      "`al/binary` also has `index_of`, `parse_int`, `slice_bytes`, `append`, and case-insensitive comparison. Slices share the underlying bytes instead of copying them.",
    ],
  },

  // ======================================================================
  // Processes
  // ======================================================================
  {
    id: "processes",
    title: "Processes",
    body: [
      "`scheduler.spawn(f)` starts a process running `f`. A process is not an OS thread. It costs a few hundred bytes and starts in about a microsecond, so spawn one per connection, per job, per whatever you have many of.",
      "The runtime starts one scheduler per CPU core and spreads processes across them. Inside a process you write plain blocking code. When it sleeps or waits on a socket, the scheduler parks that process and runs another. Long-running computations get preempted, so one busy process cannot starve the rest.",
      "Every value in AL is immutable, so processes cannot share mutable state and there is nothing to lock. The program exits when every process has finished.",
      {
        id: "processes",
        code: `import al/experiments/scheduler

const greeting = 'hello from'

fn worker(id Int, delay Int) Nil {
    scheduler.sleep(delay)
    println('\${greeting} worker \${id}')
}

println('main: spawning workers')

scheduler.spawn(fn() worker(1, 40))
scheduler.spawn(fn() worker(2, 80))
scheduler.spawn(fn() worker(3, 120))
scheduler.spawn(fn() worker(4, 160))

println('main: done spawning, waiting at exit')

// Output:
//   main: spawning workers
//   main: done spawning, waiting at exit
//   hello from worker 1
//   hello from worker 2
//   hello from worker 3
//   hello from worker 4`,
      },
      "Processes live in `al/experiments/scheduler` because the API is young and will grow. Message passing between processes is the next planned piece.",
    ],
  },

  // ======================================================================
  // Sockets
  // ======================================================================
  {
    id: "sockets",
    title: "Sockets",
    body: [
      "`al/net` listens for and opens TCP connections; `al/net/socket` reads and writes them. A read parks the calling process until bytes arrive, and a write parks it until the kernel accepts the bytes. Nothing else stops while a process waits.",
      "The shape of a server: listen, accept in a loop, spawn a process per connection.",
      {
        id: "sockets",
        code: `import al/net
import al/net/socket.{Socket}
import al/binary
import al/experiments/scheduler

// Echo every byte a client sends until it disconnects
fn echo(sock Socket) Nil {
    match socket.read(sock, 65536) {
        Ok(data) -> if binary.byte_size(data) == 0 {
            // Zero bytes means the client closed the connection
            socket.close(sock) or Nil
        } else {
            socket.write(sock, data) or Nil
            echo(sock)
        }
        Err(_) -> socket.close(sock) or Nil
    }
}

fn serve(server) {
    match net.accept(server) {
        Ok(sock) -> scheduler.spawn(fn() echo(sock))
        Err(e) -> println('accept failed: \${e}')
    }
    serve(server)
}

match net.listen('127.0.0.1', 7777) {
    Ok(server) -> {
        println('echo server on port 7777')
        serve(server)
    }
    Err(e) -> println('listen failed: \${e}')
}`,
      },
    ],
  },

  // ======================================================================
  // Files
  // ======================================================================
  {
    id: "files",
    title: "Files",
    body: [
      "`al/io` reads and writes files, as `Binary` or as text. Everything returns a `Result`.",
      {
        id: "files",
        code: `import al/io

// Write, then read back
io.write_text('greeting.txt', 'hello file') or err -> {
    println('write failed: \${err}')
    Nil
}

content = io.read_text('greeting.txt') or ''
println(content)  // hello file`,
      },
    ],
  },

  // ======================================================================
  // An HTTP server
  // ======================================================================
  {
    id: "http",
    title: "An HTTP server",
    body: [
      "`al/http` is an HTTP/1.1 server written in AL itself. You give `http.serve` a handler function from `Request` to `Response`. Each connection runs in its own process, and the library handles parsing and keep-alive.",
      {
        id: "http-routes",
        code: `import al/http
import al/http.{Request, Response}
import al/binary

const ROOT = binary.from_string('/')
const HELLO = binary.from_string('/hello')

fn handler(req Request) Response {
    if req.path == ROOT {
        http.text('home page')
    } else if req.path == HELLO {
        http.text('hello!')
    } else {
        http.not_found()
    }
}

http.serve('0.0.0.0', 8080, handler) or e -> {
    println('serve failed: \${e}')
}`,
      },
      "The request gives you `method`, `path`, `version`, `headers`, and `body`. Responses are built with `http.text`, `http.ok`, `http.not_found`, and `http.with_header`.",
      "If you want to see what AL looks like at the level below `al/http`, here is a server written straight on sockets. It answers pipelined requests and serves each connection in its own process. This is `examples/http_server.al` in the repository:",
      {
        id: "http-full",
        code: `import al/experiments/scheduler
import al/net.{Server}
import al/net/socket.{Socket}
import al/string
import al/binary
import al/list
import al.{Ok}

const body = 'Hello from AL!'
const header = 'HTTP/1.1 200 OK\\r\\nContent-Length: \${string.length(body)}\\r\\nConnection: keep-alive\\r\\n\\r\\n'
const response = binary.from_string('\${header}\${body}')

// How many complete HTTP requests this read contains. Requests end with a
// blank line; clients may pipeline several into one packet.
fn count_requests(data Binary) Int {
    match binary.to_string(data) {
        Ok(text) -> list.length(string.split(text, '\\r\\n\\r\\n')) - 1
        else -> 0
    }
}

// One response per pipelined request, sent to the kernel as a single
// vectored write; the parts are never concatenated.
fn responses(n Int, parts Array(Binary)) Array(Binary) {
    match n {
        0 -> parts
        else -> responses(n - 1, [response, ..parts])
    }
}

// Serve one connection: answer every request it sends until it closes.
fn respond(sock Socket) Nil {
    match socket.read(sock, 65536) {
        Ok(data) -> match binary.byte_size(data) {
            // Zero bytes read: the client closed the connection.
            0 -> socket.close(sock) or Nil
            else -> {
                n = count_requests(data)
                match n {
                    // No complete request yet; read more.
                    0 -> respond(sock)
                    else -> {
                        socket.write_parts(sock, responses(n, [])) or Nil
                        respond(sock)
                    }
                }
            }
        }
        else -> socket.close(sock) or Nil
    }
}

// Accept connections forever. Each connection is handled by its own process,
// and the runtime spreads those processes across every CPU core.
fn serve(server Server) {
    match net.accept(server) {
        Ok(sock) -> scheduler.spawn(fn() respond(sock))
        Err(e) -> println('accept failed: \${e}')
    }

    serve(server)
}

match net.listen('0.0.0.0', 8080) {
    Err(e) -> println('listen failed: \${e}')
    Ok(server) -> {
        println('Listening on http://localhost:8080')
        serve(server)
    }
}`,
      },
      "The standard library's own implementation, `al/http` plus its `h1` parser, body streaming, and header handling, is about a thousand lines of AL. It covers keep-alive, pipelining, Expect: 100-continue, Content-Length framing, and the request smuggling rejects from RFC 7230. Reading it is a decent way to learn the language.",
    ],
  },

  // ======================================================================
  // The standard library
  // ======================================================================
  {
    id: "stdlib",
    title: "The standard library",
    body: [
      "The standard library is small and written in AL. It is embedded in the compiler binary, so imports never touch the network or a package registry. What exists today:",
      {
        id: "stdlib-list",
        plain: true,
        code: `al/list      map, filter, fold, reverse, length, contains, find, any, all, concat
al/string    split, length, contains, trim, inspect
al/int       to_string, max, min, abs, clamp
al/float     floor, ceil, round, truncate, from_int, to_string, max, min, abs
al/bool      negate, to_string
al/option    map, then, unwrap, or_else, is_some, is_none
al/result    map, map_err, then, unwrap, is_ok, is_err
al/binary    from_string, to_string, bit_size, byte_size, slice, slice_bytes,
             append, index_of, parse_int, eq_ignore_ascii_case,
             to_ascii_lower, from_int_ascii
al/io        read_file, write_file, read_text, write_text
al/net       listen, accept, connect, local_addr, close
al/net/socket    read, read_exact, write, write_parts, close
al/net/address   to_string, ip_to_string
al/http      serve, text, ok, not_found, with_header
al/http/h1   sans-IO HTTP/1.1 parser
al/http/body al/http/headers al/http/status
al/experiments/scheduler   spawn, sleep`,
      },
      "Anything missing from this list does not exist yet. The source for all of it is in `crates/al_core/src/std/al/` in the repository, and it reads like the examples on this page.",
    ],
  },

  // ======================================================================
  // Tooling
  // ======================================================================
  {
    id: "tooling",
    title: "Tooling",
    body: [
      "Everything lives in the one `al` binary:",
      {
        id: "tooling-commands",
        plain: true,
        code: `al run <file.al>     Type check, compile, and run a program
al check <path>      Type check without running
al fmt [path]        Format source files
al repl              Interactive session
al lsp               Language server
al build <file.al>   Parse a program and print its AST
al upgrade           Swap the binary for a newer build`,
      },
      "The formatter has no configuration. `al fmt --check` verifies formatting without changing files. The REPL keeps definitions across entries. The language server does hover types, go to definition, and diagnostics; the VSCode extension in the repository wires it up.",
      "Type errors point at the code that caused them, with the source line shown:",
      {
        id: "type-error",
        plain: true,
        code: `error: Type mismatch: expected 'Int', got 'String'
5  | println(add(1, 'two'))
                    ^^^^^
Found 1 error`,
      },
    ],
  },
];

// Every AL snippet on the page, flattened. scripts/check-examples.ts type
// checks each one; build.tsx renders each one with shiki.
export const examples = sections.flatMap((section) =>
  section.body
    .filter(isSnippet)
    .filter((snippet) => !snippet.plain)
    .map((snippet) => ({ title: section.title, ...snippet }))
);

// ======================================================================
// Page
// ======================================================================

type RenderedCode = { light: string; dark: string };
export type Rendered = Record<string, RenderedCode>;

// Prose paragraph. Text between `backticks` renders as inline code.
function Paragraph({ text }: { text: string }) {
  const parts = text.split("`");
  return (
    <p className="text-sm text-neutral-600 dark:text-neutral-400 mb-4">
      {parts.map((part, i) =>
        i % 2 === 1 ? (
          <code key={i} className="text-black dark:text-white">
            {part}
          </code>
        ) : (
          part
        )
      )}
    </p>
  );
}

// Plain monospace block: shell commands, compiler output, tables.
function PlainBlock({ code }: { code: string }) {
  return (
    <pre className="text-sm p-4 mb-4 overflow-auto border border-neutral-200 dark:border-neutral-800 bg-neutral-50 dark:bg-[#0A0A0A] text-neutral-800 dark:text-neutral-300">
      {code}
    </pre>
  );
}

// Shiki-highlighted AL code, light and dark variants.
function CodeBlock({ rendered }: { rendered: RenderedCode }) {
  return (
    <div className="mb-4">
      <div
        dangerouslySetInnerHTML={{ __html: rendered.light }}
        className="text-sm overflow-auto [&_pre]:p-4 [&_pre]:m-0 [&_pre]:border [&_pre]:border-neutral-200 dark:hidden"
      />
      <div
        dangerouslySetInnerHTML={{ __html: rendered.dark }}
        className="text-sm overflow-auto [&_pre]:p-4 [&_pre]:m-0 [&_pre]:border [&_pre]:border-neutral-800 [&_pre]:bg-[#0A0A0A]! hidden dark:block"
      />
    </div>
  );
}

function SectionHeading({ section }: { section: Section }) {
  const link = (
    <a href={`#${section.id}`} className="hover:underline">
      {section.title}
    </a>
  );
  if (section.sub) {
    return <h3 className="font-bold mb-3 mt-10">{link}</h3>;
  }
  return <h2 className="text-lg font-bold mb-3 mt-14">{link}</h2>;
}

// Table of contents, built from the section list. Top-level sections are
// entries; subsections nest under them.
function Contents() {
  const entries: { section: Section; subs: Section[] }[] = [];
  for (const section of sections) {
    if (section.sub && entries.length > 0) {
      entries[entries.length - 1]!.subs.push(section);
    } else {
      entries.push({ section, subs: [] });
    }
  }
  return (
    <nav className="mb-16">
      <h2 className="text-lg font-bold mb-4">Contents</h2>
      <ul className="text-sm sm:columns-2">
        {entries.map(({ section, subs }) => (
          <li key={section.id} className="break-inside-avoid mb-1">
            <a
              href={`#${section.id}`}
              className="text-neutral-800 dark:text-neutral-200 hover:underline"
            >
              {section.title}
            </a>
            {subs.length > 0 && (
              <ul className="ml-4 mt-1 mb-1">
                {subs.map((sub) => (
                  <li key={sub.id} className="mb-1">
                    <a
                      href={`#${sub.id}`}
                      className="text-neutral-500 dark:text-neutral-400 hover:underline"
                    >
                      {sub.title}
                    </a>
                  </li>
                ))}
              </ul>
            )}
          </li>
        ))}
      </ul>
    </nav>
  );
}

const logo = `▄▀█ █░░
█▀█ █▄▄`;

export function App({ rendered }: { rendered: Rendered }) {
  return (
    <div className="max-w-2xl mx-auto px-5 py-10 leading-relaxed">
      <header className="mb-14">
        <pre className="text-sm mb-6">{logo}</pre>
        <p className="text-sm text-neutral-600 dark:text-neutral-400 mb-6">
          AL is a small, statically typed language for writing concurrent
          programs. It ships as a single binary with no dependencies. The
          whole language is documented on this page.
        </p>
        <div className="p-4 bg-neutral-100 dark:bg-neutral-900 border border-neutral-200 dark:border-neutral-800 mb-4">
          <code className="text-xs tracking-tight">
            curl -fsSL al.alistair.sh/install.sh | bash
          </code>
        </div>
        <p className="text-xs text-neutral-500">
          Source on{" "}
          <a
            href="https://github.com/alii/al"
            className="underline hover:text-black dark:hover:text-white"
          >
            GitHub
          </a>
          . MIT licensed.
        </p>
      </header>

      <Contents />

      <main>
        {sections.map((section) => (
          <section key={section.id} id={section.id}>
            <SectionHeading section={section} />
            {section.body.map((block, i) =>
              typeof block === "string" ? (
                <Paragraph key={i} text={block} />
              ) : block.plain ? (
                <PlainBlock key={block.id} code={block.code} />
              ) : (
                <CodeBlock key={block.id} rendered={rendered[block.id]!} />
              )
            )}
          </section>
        ))}
      </main>

      <footer className="mt-16 pt-6 border-t border-neutral-200 dark:border-neutral-800">
        <p className="text-xs text-neutral-500">
          AL is open source, MIT licensed. Code, issues, and the standard
          library live at{" "}
          <a
            href="https://github.com/alii/al"
            className="underline hover:text-black dark:hover:text-white"
          >
            github.com/alii/al
          </a>
          .
        </p>
      </footer>
    </div>
  );
}
