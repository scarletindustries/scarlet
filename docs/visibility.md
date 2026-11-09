# What `pub` means, and why Scarlet has no `@internal`

**A value is either `pub` or private to the module it's written in. There's nothing in between.** `export_value` and `export_type` in `crates/scarlet_core/src/bytecode/analysis.rs` hold the whole rule. A `pub` name goes into the module interface's `values` or `types`, and any other name goes into `private_names`. That way an importer gets `'X' is private in module 'Y'` rather than "no member 'X'". Only two places read `private_names`: qualified member access and qualified constructor patterns. They're the only places that enforce visibility.

A *type* has one more state. `pub opaque` publishes the type but keeps its constructors private (`ctors_public = is_public && !opaque`). That's how you make a value that code outside the module can't forge or read. It says nothing about who may call the functions next to it, and that's where `@internal` would come in.

## Why there's no `@internal`

Gleam's `@internal` gives a value a third state. The value is `pub` to the package's own modules, but it's left out of the package's public API and its generated documentation. Each half needs something Scarlet doesn't have.

**The checker has no package boundary.** A module's identity is its canonical absolute file path (`file_module_path` in `crates/scarlet_syntax/src/module_path.rs`), and a `ModuleKey` is those segments joined. An import is either a `scarlet/...` stdlib path or a relative one starting `./` or `../`. A bare `import madder/token` fails with `ResolveError::BareName`, which the compiler reports as *"Unknown module 'madder/token'. Package imports aren't supported yet, so use a relative path like `./madder/token`"*. Nothing in the compiler records which package a module came from, and no code reads the root `package.sca`. So the compiler can't be asked "is this public to my package but private to yours?". The one test of where a module came from is `is_stdlib`, and it only checks whether the first path segment is `scarlet`. That's a convention about paths, not a package graph.

**There's no generated documentation to leave anything out of.** `scarlet` has `repl`, `lsp`, `check`, `fmt`, `lint`, `upgrade`, `dis` and `run`, and no doc generator. The language reference is written by hand outside this repo. Doc comments are still used: the module interface carries them, and the LSP shows them in hover (`crates/scarlet/src/lsp/handlers.rs`) and completion (`crates/scarlet/src/lsp/wire.rs`). But that's something an editor shows, not a published API, and qualified completion already filters on `is_pub` alone.

Getting `@internal` to parse and validate would take two lines in `validate_attributes`. Attributes already come before `pub`, so the grammar wouldn't change at all. That's the trap, because the meaning would have nowhere to go. `ModuleInterface` carries one boolean per name, and the two places that enforce visibility have no data to answer "is the importer in the same package as the module it imports?". If we accepted `@internal` today, the function would stay as `pub` as before, and anyone who can name its file could import it. It would read as a guarantee the compiler isn't making, which is worse than the doc comment it would replace. So we reject it on purpose, and `internal_attribute_is_not_a_scarlet_attribute` in `crates/scarlet/tests/type_semantics.rs` checks that it stays rejected.

## What to do when one caller needs to see inside an opaque type

**Export the operation that needs the value, not the value itself.** Say a type is `pub opaque` because its contents mustn't escape, but one caller still needs those contents. What should cross the module boundary is the *result of using* them. Move that use into the module that owns the secret. The secret then never leaves, and there's no hole for anyone to find.

Take a token whose secret nobody outside should read. Don't put a `pub fn reveal(t Token) String` next to `new`, because that publishes the way out right beside the constructor. Keep `reveal` private to the token module and publish `pub fn authorization_header(t Token) String` instead. The one caller that puts the credential on the wire calls that. The public API gains a function that's safe to call and loses one that isn't, and the opaque type keeps the secret rather than a doc comment.

In general, if a value has to cross a module boundary but isn't meant for callers, then either the operation belongs on the side that owns it, or the two modules should be one. Because a value in Scarlet is either public or private, that's the trade on offer: you get encapsulation at the size of a module, and the owning module grows.

This isn't an argument that `@internal` is a bad feature. Scarlet can't express it today. It becomes a real option once modules carry a package identity the compiler can compare, and not before.
