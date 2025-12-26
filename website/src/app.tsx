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
active = true        // Bool
nothing = none       // None

// Reassignment shadows the previous binding
x = 10
x = x + 1  // x is now 11

// Arrays
numbers = [1, 2, 3, 4, 5]
first = numbers[0]`,
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
      code: `// Arithmetic
sum = 1 + 2
diff = 5 - 3
prod = 4 * 2
quot = 10 / 3
rem = 10 % 3

// Comparison
eq = a == b
neq = a != b
lt = a < b
lte = a <= b

// Logical
and_result = true && false
or_result = true || false
not_result = !true`,
    },

    // === FUNCTIONS ===
    {
      title: "Functions with type inference",
      description:
        "Function parameter and return types are inferred from usage. Add explicit types when needed for clarity or error types.",
      code: `// Types fully inferred
fn double(x) { x * 2 }
fn add(a, b) { a + b }
fn greet(name) { 'Hello, ' + name }

// Explicit types when needed
fn divide(a Int, b Int) Int!Error {
    if b == 0 { error Error{ message: 'divide by zero' } }
    else { a / b }
}

// Call functions
println(double(21))      // 42
println(add(10, 5))      // 15
println(greet('world'))  // Hello, world`,
    },
    {
      title: "Generics",
      description:
        "Use lowercase type variables for polymorphic functions. The type checker infers concrete types at each call site.",
      code: `fn identity(x a) a { x }

fn first(arr []a) ?a {
    match arr {
        [] -> none,
        [head, ..] -> head,
    }
}

fn map_array(arr []a, f fn(a) b) []b {
    match arr {
        [] -> [],
        [head, ..rest] -> [f(head)] + map_array(rest, f),
    }
}

// Works with any type
n = identity(42)        // Int
s = identity('hello')   // String
head = first([1, 2, 3]) or 0`,
    },
    {
      title: "First-class functions",
      description:
        "Functions are values. Pass them around, store them in variables, return them from functions.",
      code: `fn apply(x, f) { f(x) }
fn compose(f, g) { fn(x) { f(g(x)) } }

double = fn(n) { n * 2 }
add_one = fn(n) { n + 1 }

result = apply(5, double)  // 10

// Compose functions
double_then_add = compose(add_one, double)
double_then_add(5)  // 11

// Higher-order patterns
fn twice(x, f) { f(f(x)) }
twice(3, fn(n) { n + 1 })  // 5`,
    },
    {
      title: "Recursion",
      description:
        "Functions can call themselves. AL optimizes tail recursion.",
      code: `fn factorial(n Int) Int {
    if n <= 1 { 1 }
    else { n * factorial(n - 1) }
}

fn fibonacci(n Int) Int {
    match n {
        0 -> 0,
        1 -> 1,
        else -> fibonacci(n - 1) + fibonacci(n - 2),
    }
}

println(factorial(5))   // 120
println(fibonacci(10))  // 55`,
    },

    // === CONTROL FLOW ===
    {
      title: "Everything is an expression",
      description:
        "No statements. If/else, match, and blocks all return values. The last expression in a block is its value.",
      code: `result = if x > 0 {
    'positive'
} else {
    'non-positive'
}

// Blocks are expressions
total = {
    a = 10
    b = 20
    a + b
}

// Use in function bodies
fn abs(n Int) Int {
    if n < 0 { -n } else { n }
}`,
    },
    {
      title: "Pattern matching",
      description:
        "Match on values, or-patterns, enums, literal payloads, and arrays. Exhaustive checking ensures you handle all cases.",
      code: `fn describe(x Int) String {
    match x {
        0 -> 'zero',
        1 | 2 | 3 -> 'small',
        else -> 'other',
    }
}

// Match on arrays
fn sum(arr []Int) Int {
    match arr {
        [] -> 0,
        [head, ..tail] -> head + sum(tail),
    }
}

// Wildcard pattern
fn ignore_second(pair) {
    match pair {
        [a, _] -> a,
        else -> none,
    }
}`,
    },
    {
      title: "Enum pattern matching",
      description:
        "Match on enum variants and bind payload values. Match literal payloads for specific cases.",
      code: `enum Result {
    Ok(String)
    Err(String)
}

fn handle(r Result) String {
    match r {
        Ok('special') -> 'matched literal!',
        Ok(value) -> 'got: \$value',
        Err(e) -> 'error: \$e',
    }
}

// Use it
handle(Ok('special'))  // 'matched literal!'
handle(Ok('hello'))    // 'got: hello'
handle(Err('oops'))    // 'error: oops'`,
    },

    // === DATA TYPES ===
    {
      title: "Structs",
      description:
        "Define data structures with named fields. Access fields with dot notation.",
      code: `struct Person {
    name String
    age Int
}

struct Point {
    x Int
    y Int
}

// Create instances
person = Person{ name: 'alice', age: 30 }
origin = Point{ x: 0, y: 0 }

// Access fields
println(person.name)  // alice
println(person.age)   // 30`,
    },
    {
      title: "Generic structs",
      description:
        "Structs can have type parameters. Type arguments are inferred from field values.",
      code: `struct Box(t) {
    value t
}

struct Pair(a, b) {
    first a
    second b
}

// Type args inferred from values
int_box = Box{ value: 42 }
pair = Pair{ first: 'hello', second: 123 }

// Or specify explicitly
Box(String){ value: 'world' }`,
    },
    {
      title: "Enums",
      description:
        "Model variants with enums. Variants can carry payloads of any type.",
      code: `enum Status {
    Active
    Inactive
    Banned(String)
}

enum Option {
    Some(Int)
    Empty
}

// Create enum values
status = Status.Active
banned = Status.Banned('spam')
some_value = Some(42)  // Short form`,
    },
    {
      title: "Generic enums",
      description:
        "Enums can have type parameters for flexible data modeling.",
      code: `enum Maybe(t) {
    Just(t)
    Nothing
}

enum Result(ok, err) {
    Ok(ok)
    Err(err)
}

// Type inferred from usage
x Maybe = Just(42)
y Maybe = Nothing

result Result = Ok('success')`,
    },
    {
      title: "Tuples",
      description:
        "Fixed-size collections of mixed types. Access elements by index.",
      code: `// Create tuples
pair = (1, 'hello')
triple = (true, 42, 'world')

// Access by index
first = pair.0   // 1
second = pair.1  // 'hello'

// In function returns
fn divide(a Int, b Int) (Int, Int) {
    (a / b, a % b)
}

quotient, remainder = divide(10, 3)`,
    },
    {
      title: "Arrays",
      description:
        "Ordered collections of values. Access by index, concatenate with +.",
      code: `numbers = [1, 2, 3, 4, 5]
names = ['alice', 'bob', 'charlie']

// Access by index
first = numbers[0]  // 1
second = names[1]   // 'bob'

// Concatenate
combined = [1, 2] + [3, 4]  // [1, 2, 3, 4]

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

    // === STRINGS ===
    {
      title: "Strings and interpolation",
      description:
        "Strings use single quotes. Embed expressions with $ for variables or ${} for complex expressions.",
      code: `name = 'world'
greeting = 'Hello, \$name!'
math = 'Result: \${1 + 2 * 3}'

// Multi-part interpolation
person = Person{ name: 'Alice', age: 30 }
bio = '\${person.name} is \${person.age} years old'

// Escape sequences
quote = 'She said \\'hello\\''`,
    },

    // === ERROR HANDLING ===
    {
      title: "Optional values",
      description:
        "Functions that might not return a value use ? in their return type. Handle missing values with 'or'.",
      code: `fn find_user(id Int) ?User {
    if id == 0 { none }
    else { User{ id: id, name: 'found' } }
}

// Provide a default with 'or'
user = find_user(0) or User{ id: 0, name: 'guest' }

// Handle with receiver
result = find_user(0) or missing -> {
    println('User not found')
    User{ id: -1, name: 'default' }
}`,
    },
    {
      title: "Error handling",
      description:
        "Functions that can fail use ! with an error type. Handle errors with 'or', optionally binding the error.",
      code: `struct DivisionError {
    message String
}

fn divide(a Int, b Int) Int!DivisionError {
    if b == 0 {
        error DivisionError{ message: 'divide by zero' }
    } else {
        a / b
    }
}

// Provide default on error
safe = divide(10, 0) or 0

// Handle error with receiver
result = divide(10, 0) or err -> {
    println('Error: \${err.message}')
    -1
}`,
    },
    // === BUILTINS ===
    {
      title: "Built-in functions",
      description: "Core functions available without imports.",
      code: `// Print any value
println(42)
println('hello')
println([1, 2, 3])

// Convert to string representation
s = inspect(Person{ name: 'alice', age: 30 })

// Split strings
parts = str_split('a,b,c', ',')  // ['a', 'b', 'c']`,
    },
    {
      title: "I/O operations (experimental)",
      description:
        "File and network I/O requires the --experimental-shitty-io flag.",
      code: `// Run with: al run --experimental-shitty-io file.al

// File operations
content = read_file('data.txt')
write_file('output.txt', 'hello world')

// TCP networking
listener = tcp_listen(8080)
client = tcp_accept(listener)
data = tcp_read(client)
tcp_write(client, 'HTTP/1.1 200 OK\\r\\n\\r\\nHello')
tcp_close(client)`,
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
            statically typed with full type inference
          </strong>
          . Every expression has a type known at compile time. The type checker
          catches errors before your code runs, while inference keeps the syntax
          clean—no type annotations needed for local variables or even function
          parameters.
        </p>
        <p className="text-sm text-neutral-600 dark:text-neutral-400 mb-3">
          The compiler is written in V, producing a single native binary with no
          dependencies. It compiles to bytecode and runs on a stack-based
          virtual machine. Includes an{" "}
          <strong className="text-black dark:text-white">
            interactive REPL
          </strong>{" "}
          for exploration and a{" "}
          <strong className="text-black dark:text-white">code formatter</strong>{" "}
          for consistent style.
        </p>
        <p className="text-sm text-neutral-600 dark:text-neutral-400">
          AL supports{" "}
          <strong className="text-black dark:text-white">generics</strong> via
          type variables, closures, tagged enums with pattern matching,
          first-class functions, and a unified error handling model where both
          optional values and errors are handled with the same{" "}
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
          {`struct Person {
    name String
    age Int
}

fn greet(p Person) String {
    'Hello, \${p.name}! You are \${p.age} years old.'
}

person = Person{ name: 'Alice', age: 30 }
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
