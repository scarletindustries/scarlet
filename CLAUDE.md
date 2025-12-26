When building, always use `v .` (make sure you are in the project root before running)

You can do a production build with `v -prod .`

Be sparse when adding comments in the code. Do not add unnecessary comments. Do add comments when explaining larger, more complicated code paths. Especially in things like the parser and compiler or vm.

When working with AST, be sure to mirror any changges in both the parser AST and the typed AST. The typed AST is in `src/compiler/typed_ast/typed_ast.v`. The parser AST is in `src/compiler/parser/ast/ast.v`.

For the VSCode extension in `extension/`, use Bun for package management and running scripts (e.g., `bun install`, `bun run compile`).

I have aliased cat to be `bat`, which when piping with STDIN will add a "STDIN" string on the first line. For this reason, use `/bin/cat` explicitly for catting when piping

When deciding between n+1 implementations of a feature or fix, prefer the one that is more idiomatic and correct. Working on a programming language is something that has a lot of prior art. Generally consider what is more idiomatic and correct over what is more clever, "fun", or "efficient" unless efficiency is the main concern of the code path. As a rule of thumb, "effort" to achieve an implementation is not a worry here. Do not worry about things "getting complicated" or "big" - if you find yourself adding TODO comments, consider removing them and continuing with the full, correct implementation.

Never add Claude or Anthropic branding to commit messages, issue bodies, PR titles, PR descriptions, etc.
