import "./globals.css";

export const examples: { title: string; description: string; code: string }[] =
  [
    {
      title: "Everything is an expression",
      description:
        "No statements. If/else, match, and blocks all return values. The last expression in a block is its value.",
      code: `result = if x > 0 {
    'positive'
} else {
    'non-positive'
}

grade = match score {
    90..100 -> 'A',
    80..90 -> 'B',
    else -> 'C',
}`,
    },
    {
      title: "Optional values with ?",
      description:
        "Functions that might not return a value use ? in their return type. Use 'or' to provide defaults.",
      code: `fn find_user(id Int) ?User {
    if id == 0 {
        none
    } else {
        User{ id: id, name: 'found' }
    }
}

user = find_user(0) or User{ id: 0, name: 'guest' }`,
    },
    {
      title: "Error handling with !",
      description:
        "Functions that can fail use ! with an error type. Handle errors with 'or', or propagate with !.",
      code: `fn divide(a Int, b Int) Int!DivisionError {
    if b == 0 {
        error DivisionError{ message: 'divide by zero' }
    } else {
        a / b
    }
}

safe = divide(10, 0) or 0
safe_with_err = divide(10, 0) or err -> -1
result = divide(10, 2)!  // propagate error`,
    },
    {
      title: "Pattern matching",
      description:
        "Match on values, enums, and even literal payloads. Exhaustive checking ensures you handle all cases.",
      code: `enum Result {
    Ok(String),
    Err(String),
}

fn handle(r Result) String {
    match r {
        Ok('special') -> 'matched literal!',
        Ok(value) -> 'got: $value',
        Err(e) -> 'error: $e',
    }
}`,
    },
    {
      title: "Structs and enums",
      description:
        "Define data with structs. Model variants with enums that can carry payloads.",
      code: `struct Person {
    name String,
    age Int,
}

enum Status {
    Active,
    Inactive,
    Banned(String),
}

person = Person{ name: 'alice', age: 30 }
status = Status.Banned('spam')`,
    },
    {
      title: "String interpolation",
      description:
        "Embed expressions directly in strings with $ for simple variables or ${} for expressions.",
      code: `name = 'world'
greeting = 'Hello, $name!'
math = 'Result: ${1 + 2 * 3}'`,
    },
    {
      title: "First-class functions",
      description:
        "Functions are values. Pass them around, store them, return them.",
      code: `fn apply(x Int, f fn(Int) Int) Int {
    f(x)
}

double = fn(n Int) Int { n * 2 }
result = apply(5, double)  // 10`,
    },
    {
      title: "Assertions",
      description:
        "Assert conditions that must be true. If they fail, the function returns an error.",
      code: `fn process(x Int) Int!Error {
    assert x > 0, Error{ message: 'must be positive' }
    x * 2
}

a = process(5)!   // 10
b = process(-1) or err -> 0  // 0`,
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
     al --help             Show all commands

   Examples:
     al run hello.al
     al run examples/fibonacci.al

   Learn more: https://al.alistair.sh`;

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
          AL compiles to bytecode and runs on a stack-based virtual machine. The
          compiler is written in V, producing a single native binary with no
          dependencies.
        </p>
        <p className="text-sm text-neutral-600 dark:text-neutral-400">
          The VM supports closures, tagged enums with pattern matching,
          first-class functions, and a unified error handling model where both
          optional values and errors are handled with the same{" "}
          <code className="text-black dark:text-white">or</code> syntax.
        </p>
      </section>

      <main>
        <h2 className="text-lg font-bold mb-8">Language features</h2>
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

      <footer className="mt-16 pt-6 border-t border-neutral-200 dark:border-neutral-800">
        <p className="text-xs text-neutral-500">
          AL is a hobby project. Expect bugs.
        </p>
      </footer>
    </div>
  );
}
