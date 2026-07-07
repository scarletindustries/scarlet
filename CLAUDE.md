Build with `cargo build`. Run tests with `cargo test`. Production build with `cargo build --release`.

Before committing, `cargo fmt` and `cargo clippy --all-targets` should both be clean — CI enforces this.

Be sparse when adding comments in the code. Do not add unnecessary comments. Do add comments when explaining larger, more complicated code paths. Especially in things like the parser and compiler or vm.

The AST is defined in `src/ast/mod.rs`. When changing AST shape, also update `src/parser/mod.rs` (construction), `src/printer/mod.rs` (display), and `src/bytecode/compiler.rs` (typecheck + codegen).

The HM type inferencer lives in `src/types/infer.rs`. Type definitions are in `src/type_def/mod.rs`. Exhaustiveness checking is in `src/types/exhaustiveness.rs`.

For the VSCode extension in `extension/`, use Bun for package management and running scripts (e.g., `bun install`, `bun run compile`).

I have aliased cat to be `bat`, which when piping with STDIN will add a "STDIN" string on the first line. For this reason, use `/bin/cat` explicitly for catting when piping

Stdlib convention: fallible operations return `Result(_, Nil)`, not `Option`, even when the error carries no data — Result signals "operation failed" and chains via `result.then`; Option is for "value may be absent" (e.g. `index_of`, map lookup).

When deciding between n+1 implementations of a feature or fix, prefer the one that is more idiomatic and correct. Working on a programming language is something that has a lot of prior art. Generally consider what is more idiomatic and correct over what is more clever, "fun", or "efficient" unless efficiency is the main concern of the code path. As a rule of thumb, "effort" to achieve an implementation is not a worry here. Do not worry about things "getting complicated" or "big" - if you find yourself adding TODO comments, consider removing them and continuing with the full, correct implementation.

Never add Claude or Anthropic branding to commit messages, issue bodies, PR titles, PR descriptions, etc.
