# Visibility: what `pub` means, and why there is no `@internal`

**A value is `pub` or it is private to the module it is written in, and there is nothing in between.** `export_value`/`export_type` in `crates/scarlet_core/src/bytecode/analysis.rs` are the whole of the rule: a `pub` name goes into the module interface's `values`/`types`, and anything else goes into `private_names`, so an importer gets `'X' is private in module 'Y'` rather than "no member 'X'". The two sites that read `private_names`, qualified member access and qualified constructor patterns, are the only places visibility is enforced.

A *type* has one more state: `pub opaque` publishes the type and withholds its constructors (`ctors_public = is_public && !opaque`). That is the tool for making a value unforgeable and unreadable from outside. It says nothing about who may call the functions beside it, which is where `@internal` would come in.

## Why there is no `@internal`

Gleam's `@internal` is a third state for a value: `pub` to the package's own modules, and absent from both the package's public API and its generated documentation. Each half needs something Scarlet does not have.

**There is no package boundary at check time.** A module's identity is its canonical absolute file path (`file_module_path`, `crates/scarlet_syntax/src/module_path.rs`), and `ModuleKey` is those segments joined. An import is either a `scarlet/...` stdlib path or a `./`-relative one; a bare `use "madder/token"` is `ResolveError::BareName`, reported as *"package imports are not yet supported; use a relative path like `./madder/token`"*. Nothing in the compiler records which package a module came from — the root `package.scrl` is not read by any code — so "public to my package, private to yours" is not a question it can be asked. The one provenance predicate that exists, `is_stdlib`, is a comparison against the first path segment: a convention over paths, not a package graph.

**There is no documentation artifact to be absent from.** `scarlet` has `repl`, `lsp`, `check`, `fmt`, `upgrade`, `dis` and `run`, and no doc generator; the language reference is written by hand off-repo. Doc comments are not dead — they are carried through the module interface and rendered by the LSP, in hover (`crates/scarlet/src/lsp/handlers.rs`) and completion (`crates/scarlet/src/lsp/wire.rs`) — but that is an editor surface, not a published API, and qualified completion already filters on `is_pub` alone.

Making `@internal` parse and validate is two lines in `validate_attributes`; attributes already precede `pub`, so the grammar needs no change at all. That is exactly the trap. There is no sink for the meaning: `ModuleInterface` carries one boolean per name, and the three enforcement sites have no data from which to answer "is the importer in the same package as the import". An `@internal` accepted today would leave the function as `pub` as it was, importable by anyone who can name its file, while reading as a guarantee the compiler is not making — strictly worse than the doc comment it would replace. It is rejected deliberately, and `internal_attribute_is_not_a_scarlet_attribute` in `crates/scarlet/tests/type_semantics.rs` pins that.

## What to do with a deliberate one-caller hole in an opaque type

**Export the operation that needs the value, not the value.** When a type is `pub opaque` because its contents must not escape, and one caller nevertheless needs those contents, the thing that should cross the module boundary is the *result of using* them. Move that use into the module that owns the secret; the secret then never crosses, and there is no hole to find.

Concretely, for a token whose secret is deliberately unreadable: rather than a `pub fn reveal(t Token) String` sitting beside `new`, which publishes the escape hatch alongside the constructor, keep `reveal` private to the token module and publish `pub fn authorization_header(t Token) String`. The one caller that puts the credential on the wire calls that instead. The published surface gains a function that is safe to call and loses one that is not, and the opaque type — not a doc comment — is what keeps the secret.

The general form: if a value must cross a module boundary but is not for callers, either the operation belongs on the owning side of that boundary, or the two modules are one module. Scarlet's binary value visibility makes that the trade on offer — encapsulation is bought with module granularity, and the owning module grows.

This is not an argument that `@internal` is a bad feature. It is unexpressible here, and it becomes a real option once modules carry a package identity the compiler can compare, and not before.
