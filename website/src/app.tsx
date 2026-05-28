import "./globals.css";

export const examples: { title: string; description: string; code: string }[] =
  [
    // === BASICS ===
    {
      title: "Variables and types",
      description:
        "Variables are declared with assignment. Types are inferred from values. Reassignment shadows the previous binding.",
      code: `// Types inferred from values
count = 42           // Int
name = 'alice'       // String
active = True        // Bool

// Reassignment shadows the previous binding
x = 10
x = x + 1  // x is now 11

// Arrays — indexing returns Option(t)
numbers = [1, 2, 3, 4, 5]
first = numbers[0] or 0`,
    },
    {
      title: "Constants",
      description:
        "Top-level constants are declared with 'const'. They cannot be reassigned.",
      code: `const pi = 314
const app_name = 'my app'
const max_retries = 3

greeting = 'Welcome to \${app_name}!'`,
    },
    {
      title: "Basic operators",
      description: "Standard arithmetic, comparison, and logical operators.",
      code: `a = 5
b = 3

// Arithmetic
sum = a + b
diff = a - b
prod = a * b
quot = a / b
rem = a % b

// Comparison
eq = a == b
neq = a != b
lt = a < b
lte = a <= b

// Logical
and_result = True && False
or_result = True || False
not_result = !True`,
    },

    // === FUNCTIONS ===
    {
      title: "Functions with type inference",
      description:
        "Function parameter and return types are inferred from usage. Named function bodies are always blocks. Add explicit types when needed.",
      code: `// Types fully inferred
fn double(x) { x * 2 }
fn add(a, b) { a + b }
fn greet(name) { 'Hello, ' + name }

// With explicit types
fn abs(n Int) Int { if n < 0 { -n } else { n } }

type DivError { message String }

fn divide(a Int, b Int) Result(Int, DivError) {
    if b == 0 { Err(DivError(message: 'divide by zero')) }
    else { Ok(a / b) }
}

// Call functions
println(double(21))      // 42
println(add(10, 5))      // 15
println(greet('world'))  // Hello, world`,
    },
    {
      title: "Generics",
      description:
        "Type variables are lowercase identifiers; concrete types start with an uppercase letter. The type checker infers concrete types at each call site, so the same function can be called with different types.",
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

// Same function, different types
n = identity(42)        // Int
s = identity('hello')   // String
head = first([1, 2, 3]) or 0
doubled = map_array([1, 2, 3], fn(x) x * 2)  // [2, 4, 6]`,
    },
    {
      title: "First-class functions and lambdas",
      description:
        "Functions are values. Anonymous functions (fn(...) body) infer their return type and don't need braces for a single-expression body.",
      code: `fn apply(x, f) { f(x) }
fn compose(f, g) { fn(x) f(g(x)) }

// Braceless lambda — body starts right after )
double = fn(n) n * 2
add_one = fn(n) n + 1

result = apply(5, double)  // 10

// Compose functions
double_then_add = compose(add_one, double)
println(double_then_add(5))  // 11

// Higher-order patterns
fn twice(x, f) { f(f(x)) }
println(twice(3, fn(n) n + 1))  // 5`,
    },
    {
      title: "Recursion",
      description:
        "Functions can call themselves. When the recursive call is the last thing a function does, AL reuses the stack frame — no stack overflow on deep recursion.",
      code: `fn factorial(n Int) Int {
    if n <= 1 { 1 }
    else { n * factorial(n - 1) }  // not tail recursive (n * ...)
}

// Tail-recursive with accumulator — constant stack space
fn factorial_iter(n Int, acc Int) Int {
    if n <= 1 { acc }
    else { factorial_iter(n - 1, n * acc) }  // tail call
}

fn sum(arr Array(Int), acc Int) Int {
    match arr {
        [] -> acc
        [head, ..rest] -> sum(rest, acc + head)  // tail call
    }
}

println(factorial(5))              // 120
println(factorial_iter(5, 1))      // 120
println(sum([1, 2, 3, 4, 5], 0))   // 15`,
    },

    // === CONTROL FLOW ===
    {
      title: "Everything is an expression",
      description:
        "If, match, and blocks all return values. Every if requires an else. The last expression in a block is its value.",
      code: `x = 5

result = if x > 0 {
    'positive'
} else {
    'non-positive'
}

// Blocks are expressions — and double as grouping
total = {
    a = 10
    b = 20
    a + b
}
nine = {1 + 2} * 3

// Use in function bodies
fn abs(n Int) Int {
    if n < 0 { -n } else { n }
}`,
    },
    {
      title: "Pattern matching",
      description:
        "Match on values, or-patterns, constructors, literal payloads, and arrays. Exhaustive checking ensures you handle all cases.",
      code: `fn describe(x Int) String {
    match x {
        0 -> 'zero'
        1 | 2 | 3 -> 'small'
        else -> 'other'
    }
}

// Match on arrays
fn sum(arr Array(Int)) Int {
    match arr {
        [] -> 0
        [head, ..tail] -> head + sum(tail)
    }
}

// Guards: pattern matches first, then condition filters
fn classify(n Int) String {
    match n {
        x if x < 0 -> 'negative'
        0 -> 'zero'
        else -> 'positive'
    }
}`,
    },
    {
      title: "Constructor pattern matching",
      description:
        "Match on type constructors and bind payload values. Match literal payloads for specific cases.",
      code: `type Reply {
    Ack(value String)
    Nack(value String)
}

fn handle(r Reply) String {
    match r {
        Ack('special') -> 'matched literal!'
        Ack(value) -> 'got: \${value}'
        Nack(e) -> 'failed: \${e}'
    }
}

println(handle(Ack('special')))  // 'matched literal!'
println(handle(Ack('hello')))    // 'got: hello'
println(handle(Nack('oops')))    // 'failed: oops'`,
    },

    // === DATA TYPES ===
    {
      title: "Types",
      description:
        "Define data with the 'type' keyword. Every field has a label. Single-constructor types may list fields directly; access with dot notation.",
      code: `type Person {
    name String
    age Int
}

type Point { x Int y Int }

// Create instances — constructors are function calls
person = Person(name: 'alice', age: 30)
origin = Point(x: 0, y: 0)

// Access fields
println(person.name)  // alice
println(person.age)   // 30

// Spread to copy with overrides
older = Person(..person, age: 31)`,
    },
    {
      title: "Generic types",
      description:
        "Types can have type parameters. Type arguments are inferred from field values.",
      code: `type Box(t) { value t }

type Pair(a, b) {
    first a
    second b
}

// Type args inferred from values
int_box = Box(value: 42)
pair = Pair(first: 'hello', second: 123)`,
    },
    {
      title: "Multiple constructors",
      description:
        "A type with multiple constructors models alternatives. Constructors are first-class — they're just functions.",
      code: `type Status {
    Active
    Inactive
    Banned(reason String)
}

type Shape {
    Circle(r Float)
    Rect(w Float h Float)
}

// Create values
status = Active
banned = Banned('spam')
c = Circle(r: 1.0)

// Constructors are functions — pass them around
fn map3(a, b, c, f) { [f(a), f(b), f(c)] }
xs = map3(1.0, 2.0, 3.0, Circle)`,
    },
    {
      title: "Generic constructors",
      description:
        "Constructor types can have type parameters for flexible data modeling.",
      code: `type Maybe(t) {
    Just(value t)
    Nothing
}

type Either(l, r) {
    Left(value l)
    Right(value r)
}

// Type inferred from usage
x = Just(42)
y = Nothing

result = Left('failed')`,
    },
    {
      title: "Type aliases",
      description:
        "An alias is a transparent second name for a type — they unify freely. Use a single-constructor type instead for a distinct newtype.",
      code: `type Pair(a, b) { first a second b }

type Id = Int
type StringPair = Pair(String, String)

// Alias: Email IS String
type Email = String

// Newtype: UserId is distinct from Int
type UserId { UserId(value Int) }`,
    },
    {
      title: "Tuples",
      description:
        "Fixed-size collections of mixed types with two or more elements. Access elements by index.",
      code: `// Create tuples (always 2+ elements; (e) is a syntax error)
pair = (1, 'hello')
triple = (True, 42, 'world')

// Access by index
first = pair.0   // 1
second = pair.1  // 'hello'

// Return from functions
fn divide(a, b) { (a / b, a % b) }

// Destructure with parentheses
(quotient, remainder) = divide(10, 3)`,
    },
    {
      title: "Arrays",
      description:
        "Ordered collections of values. Indexing returns Option(t). Build and concatenate with spread.",
      code: `numbers = [1, 2, 3, 4, 5]
names = ['alice', 'bob', 'charlie']

// Indexing returns Option — unwrap with 'or'
first = numbers[0] or 0       // 1
second = names[1] or 'anon'   // 'bob'

// Build with spread
combined = [..[1, 2], ..[3, 4]]  // [1, 2, 3, 4]
more = [0, ..numbers, 6]         // [0, 1, 2, 3, 4, 5, 6]

// Nested arrays
matrix = [[1, 2], [3, 4]]`,
    },
    {
      title: "Ranges",
      description:
        "Create ranges with the '..' operator. Useful for iteration patterns.",
      code: `// Create a range
r = 0..10

// Ranges in expressions
fn in_range(n Int, start Int, end Int) Bool {
    n >= start && n < end
}`,
    },
    {
      title: "Binaries",
      description:
        "The Binary type holds raw bits. Construct with '<<>>' — each bare segment is one byte. Match the same way; a length mismatch falls through, so an 'else' or ':binary' tail is required.",
      code: `import al/binary
import al/string

// Three bytes
header = <<1, 2, 3>>
println(binary.byte_size(header))   // 3

// Bit-sized segments — two 4-bit nibbles pack into one byte
packed = <<1:4, 2:4>>
println(string.inspect(packed))     // <<18>>

// Match on structure: a and b each bind one byte
sum = match binary.from_string('AB') {
    <<a, b>> -> a + b
    else -> 0
}
println(sum)  // 131  (65 + 66)

// ':binary' binds the variable-length remainder
fn drop_one(b Binary) Binary {
    match b {
        <<_, rest:binary>> -> rest
        else -> b
    }
}`,
    },

    // === STRINGS ===
    {
      title: "Strings and interpolation",
      description:
        "Strings use single quotes. Embed expressions with $ for variables or ${} for complex expressions.",
      code: `type Person { name String age Int }

name = 'world'
greeting = 'Hello, \$name!'
math = 'Result: \${1 + 2 * 3}'

// Multi-part interpolation
person = Person(name: 'Alice', age: 30)
bio = '\${person.name} is \${person.age} years old'

// Escape sequences
quote = 'She said \\'hello\\''`,
    },

    // === OPTION / RESULT ===
    {
      title: "Optional values",
      description:
        "Option(t) models a value that might be absent. The constructors Some and None are in the prelude. Unwrap with 'or'.",
      code: `type User { id Int name String }

fn find_user(id Int) Option(User) {
    if id == 0 { None }
    else { Some(User(id: id, name: 'found')) }
}

// Provide a default with 'or'
user = find_user(0) or User(id: 0, name: 'guest')

// Array indexing returns Option too
names = ['alice', 'bob']
who = names[5] or 'nobody'`,
    },
    {
      title: "Result and error handling",
      description:
        "Result(t, e) models success or failure. The constructors Ok and Err are in the prelude. Unwrap with 'or', optionally binding the error value.",
      code: `type DivisionError { message String }

fn divide(a Int, b Int) Result(Int, DivisionError) {
    if b == 0 {
        Err(DivisionError(message: 'divide by zero'))
    } else {
        Ok(a / b)
    }
}

// Provide default on failure
safe = divide(10, 0) or 0

// Bind the error value
result = divide(10, 0) or err -> {
    println('Failed: \${err.message}')
    0
}`,
    },

    // === MODULES ===
    {
      title: "Modules and imports",
      description:
        "One file is one module. Mark exports with 'pub'. Import by path; access via the qualifier or bring names in directly.",
      code: `// === main.al ===
import ./util
import ./util as u
import ./util.{quote, Id}
import al/net.{Socket}      // standard library

println(util.quote('hi'))   // qualified
println(u.quote('hi'))      // module alias
println(quote('hi'))        // selective import

// === util.al ===
// pub fn quote(s String) String { '"' + s + '"' }
// pub type Id = Int`,
    },

    // === MORE PATTERNS ===
    {
      title: "Tuple and range patterns",
      description:
        "Match on tuple structure and integer ranges. 'else' is the catch-all arm; '_' is for nested wildcards.",
      code: `fn classify(p (Int, String)) String {
    match p {
        (0, msg) -> 'zero: \${msg}'
        (1..10, msg) -> 'small: \${msg}'
        (_, msg) -> 'other: \${msg}'
    }
}

fn label(x Int) String {
    match x {
        0..10 -> 'digit'
        10..100 -> 'tens'
        else -> 'big'
    }
}

println(classify((5, 'hi')))
println(label(42))`,
    },
    {
      title: "Mutual recursion",
      description:
        "Top-level 'fn' declarations see one another regardless of order.",
      code: `fn is_even(n Int) Bool {
    if n == 0 { True } else { is_odd(n - 1) }
}

fn is_odd(n Int) Bool {
    if n == 0 { False } else { is_even(n - 1) }
}

println(is_even(10))
println(is_odd(7))`,
    },
    {
      title: "Field-exhaustive constructor patterns",
      description:
        "Positional patterns must match arity; labeled patterns may use '..' to ignore the rest.",
      code: `type User { name String age Int email String }

fn name_of(u User) String {
    match u {
        User(name: n, ..) -> n
    }
}

u = User(name: 'alice', age: 30, email: 'a@b.c')
println(name_of(u))`,
    },
    {
      title: "Discards and unused variables",
      description:
        "Unused let-bindings and parameters are an error. Prefix a name with '_' to mark it intentional, or write 'TypeName = expr' to assert a type and drop the value. Single-constructor types can be destructured at binding position.",
      code: `// '_' prefix marks a binding as intentionally unused
fn handle(_conn, msg) { println(msg) }

// Typed discard — checks the type, then drops the value
Nil = println('side effect')   // println returns Nil
Int = 1 + 2                    // asserts the rhs is Int

// Single-constructor types destructure irrefutably
type Point { x Int y Int }

Point(x: a, y: b) = Point(x: 3, y: 4)
println(a + b)  // 7`,
    },

    // === BUILTINS ===
    {
      title: "Built-in functions",
      description: "Core functions available without imports.",
      code: `import al/string

// Print any value
println(42)
println('hello')
println([1, 2, 3])

// Convert to string representation
s = string.inspect([1, 2, 3])  // '[1, 2, 3]'

// Strings module
parts = string.split('a,b,c', ',')  // ['a', 'b', 'c']
n = string.length('hello')          // 5
has = string.contains('hello', 'ell')  // True`,
    },
    {
      title: "Floats",
      description:
        "Float arithmetic uses the same operators as Int. The 'al/float' module has conversions and helpers.",
      code: `import al/float

// Float to Int
println(float.round(2.7))     // 3
println(float.floor(2.7))     // 2
println(float.ceil(2.1))      // 3
println(float.truncate(2.9))  // 2

// Int to Float
half = float.from_int(1) / 2.0

// Helpers
diff = 1.0 - 3.5
println(float.abs(diff))       // 2.5
println(float.max(1.2, 3.4))   // 3.4
println(float.to_string(half))`,
    },
    {
      title: "I/O operations (experimental)",
      description:
        "File and network I/O requires the --experimental-shitty-io flag.",
      code: `// Run with: al run --experimental-shitty-io file.al
import al/io
import al/net.{Socket}
import al/binary

// Text helpers wrap the Binary I/O
content = io.read_text('data.txt') or ''

match io.write_text('output.txt', 'hello') {
    Ok(Nil) -> Nil
    Err(e) -> println('write failed: \${e}')
}

// TCP networking — sockets carry Binary
fn handle(conn Socket) Nil {
    body = 'Hello from AL!'
    match net.write(conn, binary.from_string('HTTP/1.1 200 OK\\r\\n\\r\\n\${body}')) {
        Ok(Nil) -> Nil
        Err(e) -> println(e)
    }
    net.close(conn) or Nil
}

match net.listen(8080) {
    Ok(server) -> println('Listening on :8080')
    Err(e) -> println(e)
}`,
    },
  ];

function Code({ light, dark }: { light: string; dark: string }) {
  return (
    <>
      <div
        dangerouslySetInnerHTML={{ __html: light }}
        className="text-sm overflow-auto [&_pre]:p-4 [&_pre]:m-0 [&_pre]:border [&_pre]:border-neutral-200 dark:hidden"
      />
      <div
        dangerouslySetInnerHTML={{ __html: dark }}
        className="text-sm overflow-auto [&_pre]:p-4 [&_pre]:m-0 [&_pre]:border [&_pre]:border-neutral-800 [&_pre]:bg-[#0A0A0A]! hidden dark:block"
      />
    </>
  );
}

type RenderedExample = {
  title: string;
  description: string;
  light: string;
  dark: string;
};

const cliOutput = `   ▄▀█ █░░
   █▀█ █▄▄

   Usage:
     al run <file.al>      Run a program
     al repl               Start interactive REPL
     al check <file.al>    Type-check without running
     al fmt [path]         Format source files
     al --help             Show all commands

   Example:
     al run hello.al
     al repl`;

export function App({ examples }: { examples: RenderedExample[] }) {
  return (
    <div className="max-w-2xl mx-auto px-5 py-10 leading-relaxed">
      <header className="mb-16">
        <pre className="text-xs text-neutral-600 dark:text-neutral-400 mb-6 overflow-auto">
          {cliOutput}
        </pre>
        <div className="mt-6 p-4 bg-neutral-100 dark:bg-neutral-900 border border-neutral-200 dark:border-neutral-800">
          <div className="mb-2 text-sm font-bold">Install</div>
          <code className="text-xs tracking-tight">
            curl -fsSL al.alistair.sh/install.sh | bash
          </code>
        </div>
      </header>

      <section className="mb-16">
        <h2 className="text-lg font-bold mb-4">How it works</h2>
        <p className="text-sm text-neutral-600 dark:text-neutral-400 mb-3">
          AL is{" "}
          <strong className="text-black dark:text-white">
            statically typed with Hindley-Milner type inference
          </strong>
          . Every expression has a type known at compile time. The checker
          catches errors before your code runs, while inference keeps the syntax
          clean—no annotations needed for variables or function parameters.
          Types and constructors are capitalized (
          <code className="text-black dark:text-white">Int</code>,{" "}
          <code className="text-black dark:text-white">Some</code>); variables,
          functions, and field labels are lowercase.
        </p>
        <p className="text-sm text-neutral-600 dark:text-neutral-400 mb-3">
          The compiler is written in Rust, producing a single native binary with
          no dependencies. It compiles to bytecode and runs on a stack-based
          virtual machine. Includes an{" "}
          <strong className="text-black dark:text-white">
            interactive REPL
          </strong>{" "}
          for exploration and a{" "}
          <strong className="text-black dark:text-white">code formatter</strong>{" "}
          for consistent style.
        </p>
        <p className="text-sm text-neutral-600 dark:text-neutral-400">
          AL has a single{" "}
          <code className="text-black dark:text-white">type</code> keyword for
          all data — records, sums, generics, and aliases — with constructors as
          ordinary functions. <code className="text-black dark:text-white">Option</code>{" "}
          and <code className="text-black dark:text-white">Result</code> are
          regular prelude types; both are unwrapped with the same{" "}
          <code className="text-black dark:text-white">or</code> syntax.
        </p>
      </section>

      <section className="mb-16">
        <h2 className="text-lg font-bold mb-4">Quick start</h2>
        <p className="text-sm text-neutral-600 dark:text-neutral-400 mb-4">
          Create a file called{" "}
          <code className="text-black dark:text-white">hello.al</code>:
        </p>
        <pre className="text-sm p-4 bg-neutral-100 dark:bg-neutral-900 border border-neutral-200 dark:border-neutral-800 mb-4 overflow-auto">
          {`type Person {
    name String,
    age Int,
}

fn greet(p Person) String {
    'Hello, \${p.name}! You are \${p.age} years old.'
}

person = Person(name: 'Alice', age: 30)
println(greet(person))`}
        </pre>
        <p className="text-sm text-neutral-600 dark:text-neutral-400">
          Run with{" "}
          <code className="text-black dark:text-white">al run hello.al</code>
        </p>
      </section>

      <main>
        <h2 className="text-lg font-bold mb-8">Language reference</h2>
        {examples.map((example, i) => (
          <section key={i} className="mb-12">
            <h3 className="font-bold mb-2">{example.title}</h3>
            <p className="text-sm text-neutral-600 dark:text-neutral-400 mb-4">
              {example.description}
            </p>
            <Code light={example.light} dark={example.dark} />
          </section>
        ))}
      </main>

      <section className="mt-16 mb-16">
        <h2 className="text-lg font-bold mb-4">Command reference</h2>
        <div className="text-sm space-y-4">
          <div>
            <code className="text-black dark:text-white font-bold">
              al run &lt;file.al&gt;
            </code>
            <p className="text-neutral-600 dark:text-neutral-400 mt-1">
              Type-check, compile, and run a program. Add{" "}
              <code>--experimental-shitty-io</code> for file/network I/O.
            </p>
          </div>
          <div>
            <code className="text-black dark:text-white font-bold">
              al check &lt;file.al&gt;
            </code>
            <p className="text-neutral-600 dark:text-neutral-400 mt-1">
              Type-check without running. Useful for IDE integration.
            </p>
          </div>
          <div>
            <code className="text-black dark:text-white font-bold">
              al fmt [path]
            </code>
            <p className="text-neutral-600 dark:text-neutral-400 mt-1">
              Format source files. Use <code>--check</code> to verify without
              modifying, <code>--stdin</code> to read from stdin.
            </p>
          </div>
          <div>
            <code className="text-black dark:text-white font-bold">
              al repl
            </code>
            <p className="text-neutral-600 dark:text-neutral-400 mt-1">
              Start an interactive session. Definitions persist across entries.
            </p>
          </div>
          <div>
            <code className="text-black dark:text-white font-bold">al lsp</code>
            <p className="text-neutral-600 dark:text-neutral-400 mt-1">
              Start the Language Server Protocol server for IDE integration.
            </p>
          </div>
        </div>
      </section>

      <footer className="mt-16 pt-6 border-t border-neutral-200 dark:border-neutral-800">
        <p className="text-xs text-neutral-500">
          AL is open source. View the code on{" "}
          <a
            href="https://github.com/alii/al"
            className="underline hover:text-black dark:hover:text-white"
          >
            GitHub
          </a>
          .
        </p>
      </footer>
    </div>
  );
}
