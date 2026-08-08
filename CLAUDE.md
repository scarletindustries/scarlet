Build with `cargo build`. Run tests with `cargo test`. Production build with `cargo build --release`.

Before committing, `cargo fmt` and `cargo clippy --all-targets` should both be clean — CI enforces this.

Be sparse when adding comments in the code. Do not add unnecessary comments. Do add comments when explaining larger, more complicated code paths. Especially in things like the parser and compiler or vm.

Crate layout: `crates/al_vm` is the language-agnostic runtime (bytecode ISA, NaN-boxed values, heap, frozen area, interpreter/schedulers/JIT under `al_vm::vm`) and must never depend on `al_core` — Cargo enforces this, keep it that way. `crates/al_core` is the language front end (parser, types, IRs, compiler); it depends on `al_vm` and re-exports the runtime types at their historical paths (`al_core::bytecode::value`, `al_core::heap`, ...). `crates/al` is the driver (CLI, REPL, LSP); its `al::vm` module wires the generated stdlib template table (`STDLIB_TEMPLATES`) into `al_vm::vm` — the VM constructs stdlib values (Ok/Err, NetError, HTTP types) only through that injected table.

The AST is defined in `crates/al_core/src/ast/mod.rs`. When changing AST shape, also update `parser/mod.rs` (construction), `formatter/mod.rs` (rendering — a field the formatter drops silently rewrites the user's program), `bytecode/compiler/` (typecheck), and `typed_ir/elaborate*.rs` (which lowers it).

The HM type inferencer lives in `crates/al_core/src/types/infer.rs`. Type definitions are in `type_def/mod.rs`. Exhaustiveness checking is in `types/exhaustiveness.rs`.

For the VSCode extension in `extension/`, use Bun for package management and running scripts (e.g., `bun install`, `bun run compile`).

I have aliased cat to be `bat`, which when piping with STDIN will add a "STDIN" string on the first line. For this reason, use `/bin/cat` explicitly for catting when piping

Stdlib convention: fallible operations return `Result(_, Nil)`, not `Option`, even when the error carries no data — Result signals "operation failed" and chains via `result.then`; Option is for "value may be absent" (e.g. `index_of`, map lookup).

When deciding between n+1 implementations of a feature or fix, prefer the one that is more idiomatic and correct. Working on a programming language is something that has a lot of prior art. Generally consider what is more idiomatic and correct over what is more clever, "fun", or "efficient" unless efficiency is the main concern of the code path. As a rule of thumb, "effort" to achieve an implementation is not a worry here. Do not worry about things "getting complicated" or "big" - if you find yourself adding TODO comments, consider removing them and continuing with the full, correct implementation.

Never add Claude or Anthropic branding to commit messages, issue bodies, PR titles, PR descriptions, etc.

A map key must be the canonical identity of the thing it names — never a span (no file identity), a written import path (not the resolved file), a pointer's bits (not the value), or a position from another file's coordinate space. Five bugs shared this shape: `HashMap<Span, Ty>` let the REPL retype an earlier entry, `path_key` of the written import merged two different modules, and constant-pool dedup keyed on `to_bits()` (a pointer for boxed values) duplicated every big-int constant. When adding a map, name what the key is the identity *of*; if two distinct things can collide on it, it is the wrong key.
