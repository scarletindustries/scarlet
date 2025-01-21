# al compiler

A TypeScript implementation of the al compiler.

## Features

- Lexical analysis (Scanner)
- Parsing (AST generation)
- JavaScript code generation
- CLI interface

## Installation

```bash
bun install
```

## Usage

Build the compiler:

```bash
bun run build
```

Run the compiler:

```bash
bun run start build <entrypoint>
```

## Development

The compiler consists of three main components:

1. **Scanner** (`src/scanner/scanner.ts`): Performs lexical analysis, converting source code into tokens.
2. **Parser** (`src/parser/parser.ts`): Parses tokens into an Abstract Syntax Tree (AST).
3. **Generator** (`src/generator/js.ts`): Generates JavaScript code from the AST.

## Example

```al
const name = 'alistair'

export fn main() {
    println(name)
}
```

## License

MIT
