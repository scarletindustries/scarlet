# AL Language TODO

## 1. Receiver Pattern for Struct Methods

Add method syntax: `fn (self Point) distance() Float { ... }` called as `point.distance()`.

**Files to modify:**

- `src/ast/ast.v` - Add `receiver: ?FunctionParameter` to `FunctionExpression`
- `src/typed_ast/typed_ast.v` - Same change
- `src/parser/parser.v` - In `parse_function()`, check for `fn (receiver Type)` before function name
- `src/type_checker/type_checker.v` - Register methods on types, resolve `expr.method()` calls
- `src/compiler/compiler.v` - Compile method calls by passing receiver as first argument
- `src/vm/vm.v` - No changes needed, methods are just functions with implicit first arg

**Syntax:**

```
struct Point { x Int, y Int }

fn (self Point) distance() Int {
    self.x * self.x + self.y * self.y
}

p = Point{ x: 3, y: 4 }
d = p.distance()
```

**Implementation approach:**

- Methods are stored in a separate table: `map[string]map[string]Function` (type name -> method name -> function)
- When resolving `expr.name()`, first check if it's a property access + call, then check method table
- `self` is just a regular parameter, nothing special

**Edge cases:**

- Method name conflicts with field name - method takes precedence when called with `()`
- Chaining: `point.scale(2).distance()`
- Methods on enums too?

---

## 2. Tuple Types

Add tuple types with `()` syntax: `(Int, String)`, created as `(1, 'hello')`.

**Files to modify:**

- `src/ast/ast.v` - Add `TupleExpression { elements: []Expression }` and `TupleType { types: []TypeIdentifier }`
- `src/typed_ast/typed_ast.v` - Same
- `src/type_def/type_def.v` - Add `Tuple { types: []Type }` variant
- `src/parser/parser.v` - Parse `(expr, expr, ...)` as tuple (distinguish from grouping parens)
- `src/parser/parser.v` - Parse `(Type, Type, ...)` as tuple type
- `src/type_checker/type_checker.v` - Type check tuples, handle tuple indexing `t.0`, `t.1`
- `src/compiler/compiler.v` - Compile tuples (similar to arrays but heterogeneous)
- `src/vm/vm.v` - Runtime representation (can reuse array/struct infrastructure)

**Syntax:**

```
pair = (1, 'hello')
first = pair.0
second = pair.1

fn divide(a Int, b Int) (Int, Int) {
    (a / b, a % b)
}

(quotient, remainder) = divide(10, 3)
```

**Key decisions:**

- Tuple indexing: `t.0` vs `t[0]` - prefer `.0` for clarity (it's not an array)
- Destructuring in let: `(a, b) = tuple` - this is the ONLY destructuring we allow
- Unit type `()` - useful for functions that return nothing meaningful
- Single element: `(x,)` with trailing comma to distinguish from grouping

**Edge cases:**

- `(expr)` is grouping, `(expr,)` is 1-tuple
- Empty tuple `()` is unit type
- Nested tuples: `((1, 2), 3)`

---

## 3. Import/Export Module System

Add `import` and `export` for sharing code between files.

**Current state:** `ImportDeclaration` and `ExportExpression` already exist in AST but aren't implemented.

**Files to modify:**

- `src/parser/parser.v` - Already parses import/export (verify syntax)
- `src/type_checker/type_checker.v` - Resolve imports, check exports exist
- `src/compiler/compiler.v` - Handle cross-module references
- `src/main.v` - Module loading, dependency resolution
- New file: `src/module/module.v` - Module system logic

**Syntax:**

```
// math.al
export fn add(a Int, b Int) Int { a + b }
export const pi = 314

// main.al
import { add, pi } from 'math.al'
result = add(1, 2)
```

**Implementation approach:**

1. Parse all files in dependency order
2. Build export table for each module
3. When type-checking imports, look up in export tables
4. Compile each module to bytecode
5. Link modules together (or load dynamically)

**Key decisions:**

- Path resolution: relative (`'./math.al'`) vs absolute vs module names
- Circular imports: detect and error, or support?
- Re-exports: `export { foo } from 'bar.al'`
- Default exports: probably not, keep it simple

---

## 4. Multi-File Project Support

Build system for compiling multiple files together.

**Files to modify:**

- `src/main.v` - Add `al build <dir>` command
- New file: `src/project/project.v` - Project configuration, file discovery

**Features:**

- `al build .` - Build all .al files in directory
- `al build src/` - Build all .al files in src/
- Entry point detection: `main.al` or file with `main()` function
- Dependency graph construction from imports
- Incremental compilation (cache compiled modules)

**Project structure:**

```
myproject/
  main.al          # entry point
  utils/
    math.al
    strings.al
  lib/
    http.al
```

**Configuration (optional, future):**

```
// al.json or similar
{
  "entry": "main.al",
  "out": "build/"
}
```

---

## 5. builtins.al for Global Stdlib

Create a prelude file that's automatically imported.

**Files to modify:**

- New file: `builtins.al` or embed in compiler
- `src/main.v` - Auto-import builtins before user code
- `src/type_checker/type_checker.v` - Pre-populate scope with builtins

**Contents:**

```
// builtins.al

// Collection functions
export fn map(arr []a, f fn(a) b) []b { ... }
export fn filter(arr []a, f fn(a) Bool) []a { ... }
export fn reduce(arr []a, init b, f fn(b, a) b) b { ... }
export fn find(arr []a, f fn(a) Bool) ?a { ... }
export fn len(arr []a) Int { ... }

// String functions
export fn trim(s String) String { ... }
export fn upper(s String) String { ... }
export fn lower(s String) String { ... }
export fn contains(s String, sub String) Bool { ... }
export fn starts_with(s String, prefix String) Bool { ... }
export fn ends_with(s String, suffix String) Bool { ... }
export fn replace(s String, old String, new String) String { ... }

// Math functions
export fn abs(x Int) Int { ... }
export fn min(a Int, b Int) Int { ... }
export fn max(a Int, b Int) Int { ... }
```

**Implementation options:**

1. Write in AL itself (ideal, but needs VM intrinsics for some)
2. Implement as VM builtins (like println)
3. Hybrid: some in AL, some as intrinsics

**Decision:** Start with VM builtins, migrate to AL as language matures.

---

## 6. Collection Functions

Add `map`, `filter`, `reduce`, `find`, `len`, `concat`, `reverse`, etc.

**Implementation as VM builtins:**

```v
// In vm.v, add to builtin handling
'map' {
    // Pop function, pop array
    // Apply function to each element
    // Push new array
}
```

**Signatures:**

```
fn map(arr []a, f fn(a) b) []b
fn filter(arr []a, predicate fn(a) Bool) []a
fn reduce(arr []a, initial b, f fn(b, a) b) b
fn find(arr []a, predicate fn(a) Bool) ?a
fn find_index(arr []a, predicate fn(a) Bool) ?Int
fn len(arr []a) Int
fn concat(a []t, b []t) []t
fn reverse(arr []a) []a
fn sort(arr []Int) []Int  // start with Int only
fn contains(arr []a, elem a) Bool
fn first(arr []a) ?a
fn last(arr []a) ?a
fn take(arr []a, n Int) []a
fn drop(arr []a, n Int) []a
fn zip(a []t, b []u) [](t, u)  // needs tuples
fn enumerate(arr []a) [](Int, a)  // needs tuples
```

**Files to modify:**

- `src/type_checker/builtins.v` - Add type signatures
- `src/compiler/compiler.v` - Emit builtin calls
- `src/vm/vm.v` - Implement builtin execution

---

## 7. String Functions

Add string manipulation functions.

**Functions:**

```
fn len(s String) Int
fn trim(s String) String
fn trim_left(s String) String
fn trim_right(s String) String
fn upper(s String) String
fn lower(s String) String
fn contains(s String, substring String) Bool
fn starts_with(s String, prefix String) Bool
fn ends_with(s String, suffix String) Bool
fn replace(s String, old String, new String) String
fn replace_all(s String, old String, new String) String
fn split(s String, delimiter String) []String  // already exists as str_split
fn join(arr []String, delimiter String) String
fn char_at(s String, index Int) ?String
fn substring(s String, start Int, end Int) String
fn index_of(s String, substring String) ?Int
fn repeat(s String, n Int) String
fn pad_left(s String, length Int, char String) String
fn pad_right(s String, length Int, char String) String
```

**Note:** `str_split` already exists, rename to just `split` or keep both.

---

## 8. Math Functions

Add math utilities.

**Functions:**

```
fn abs(x Int) Int
fn min(a Int, b Int) Int
fn max(a Int, b Int) Int
fn clamp(x Int, low Int, high Int) Int
fn pow(base Int, exp Int) Int
fn sqrt(x Int) Int  // integer sqrt
fn sign(x Int) Int  // -1, 0, or 1

// Future: Float support
fn abs_f(x Float) Float
fn min_f(a Float, b Float) Float
fn floor(x Float) Int
fn ceil(x Float) Int
fn round(x Float) Int
fn sin(x Float) Float
fn cos(x Float) Float
// etc.
```

**Constants:**

```
const max_int = 9223372036854775807
const min_int = -9223372036854775808
```

---

## Implementation Order

Suggested order based on dependencies and complexity:

1. **String functions** - Easy wins, no language changes needed
2. **Math functions** - Same as above
3. **Collection functions** - Slightly more complex (generics), but no syntax changes
4. **Tuple types** - New syntax, needed for some stdlib functions
5. **Receiver pattern** - New syntax, but self-contained
6. **Import/export** - Complex, touches many files
7. **Multi-file support** - Depends on import/export
8. **builtins.al** - Depends on import/export and stdlib functions

---

## Future Ideas (Not Confirmed)

These were discussed but not confirmed:

- **Guard clauses in match:** `n if n > 0 -> ...`
- **Pipeline operator:** `x |> f |> g` same as `g(f(x))`
- **Type aliases:** `type UserId = Int`
- **Traits/interfaces:** Polymorphism beyond generics
- **Float type:** Floating point numbers
- **Char type:** Single characters
- **Set/Map types:** Built-in hash collections

---

## Rejected Features

These were explicitly rejected:

- **For/while loops** - Language is expression-oriented, use recursion + stdlib
- **Postfix `!` unwrap** - Removed, use `or` instead
- **Default struct values** - Keep structs simple
- **Destructuring** - Language is nominal, not structural
- **Spread operator** - Same reasoning as destructuring
