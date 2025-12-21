When building, always use `v .` (make sure you are in the project root before running)

You can do a production build with `v -prod .`

Be sparse when adding comments in the code. Do not add unnecessary comments. Do add comments when explaining larger, more complicated code paths. Especially in things like the parser and compiler or vm.

When working with AST, be sure to mirror any changges in both the parser AST and the typed AST. The typed AST is in `src/compiler/typed_ast/typed_ast.v`. The parser AST is in `src/compiler/parser/ast/ast.v`.

For the VSCode extension in `extension/`, use Bun for package management and running scripts (e.g., `bun install`, `bun run compile`).

I have aliased cat to be `bat`, which when piping with STDIN will add a "STDIN" string on the first line. For this reason, use `/bin/cat` explicitly for catting when piping
