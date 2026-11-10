# The compiler's middle, rebuilt

This is the plan for rebuilding everything between the parser and Core IR. It replaces the type checker in `scarlet_core/src/bytecode/`, the whole of `scarlet_elaborate`, the `Compiler` struct and `session.rs`. The parser, the AST, the formatter, `scarlet_ir`, the VM and the Core IR passes (`perceus`, `immediates`) stay as they are.

We are not building the new middle next to the old one. The old code is deleted first, and the build stays broken until the new middle passes every test.

## Why

Today one `Compiler` holds the state of the whole program, and every step reads and writes it.

- There is one inference engine for the whole program. A module's interface holds types that only mean something inside that engine, so an interface can't be passed around on its own. The editor and the REPL have to rewind that engine to a saved point, and that is what `Watermark`, `reset_to` and the three rewind test files exist for.
- The checker works out a fact, throws it away, and the elaborator works it out again by calling back into the checker. There are about a dozen of these: call arguments put in order twice, the field slot found twice, the `or` shape decided twice, number literals read twice, the call graph built twice, and a function's type re-read by name.
- Two of those callbacks write to the engine during elaboration.
- Facts are read from mutable state that may have moved on. One example: "this module has no errors" is read from the diagnostics of the whole compile, and before the module's last check has run. Another: whether a call goes to a known function depends on the order bodies were walked in.
- Each function body is turned into its own one-body program with its own type pool. The REPL recompiles the whole stdlib for every line. The editor checks every open file as if it were `main`.

All of these come from one cause. There is no point where a module is finished and its result is a plain value.

## The shape

Each step takes values and gives back values. Nothing reaches into another step's state.

```
source ─parse→ ast::BlockExpression
        ─check(ast, imports' interfaces)→ CheckedModule { interface, clean, diagnostics, index }
        ─build_program(every clean module)→ core_ir::Program
```

### Identity

Each thing gets one canonical identity, and every map is keyed on it.

- **A module** is its `ModuleKey`, as today.
- **A nominal type** is `TypeName { module: ModuleKey, name }`. The numeric `TypeId` the VM uses is handed out when a program is built, once per program. That removes the 256-id range reservations, `id_bases` and the overflow recovery. It also means editing one module can't shift another module's type ids, because no ids exist before a program is built.
- **A module-level value** is `ValueName { module, name }`.
- **A local binder** is a `BinderId`, numbered within its function.
- **A constructor** is `(TypeName, variant index)`.

### Types

There are two forms in the middle, and the IR's form after it.

1. **`Type`** is an owned, immutable tree with no inference variables, only named generic parameters. Interfaces, the finished typed tree, hover, error messages and exhaustiveness all use it. It replaces both `type_def::Type` and the interface's use of arena `Scheme`s.
2. **The inference engine** is union-find over its own arena, with levels, constraints (`Addable`, `Numeric`) and generalisation, much like today's `InferEngine`. The difference is that **each module check makes its own engine and drops it at the end.** An imported value's `Type` is instantiated into the engine when it is used. Nothing from one module's engine ever reaches another module.
3. **`RTy`** in `scarlet_ir` stays the VM-facing form. Lowering turns `Type` into it.

A type's definition (its parameters, variants, fields and labels, or an alias's target) is part of the interface that defines it, written in `Type`. `TypeRegistry` goes away.

### Checking a module

```rust
fn check_module(
    module: &ast::Module,
    key: &ModuleKey,
    imports: &Imports,      // each import's qualifier → its Interface (prelude included)
    options: &CheckOptions, // script or module, unused-binding policy
) -> CheckedModule
```

It is a function. It has no `&mut` to anything outside itself.

Inside, it runs these passes over one module:

1. **Declarations.** Collect the type, constructor, function and const names. Report duplicates and the use of reserved names.
2. **Types.** Resolve type annotations to `Type`, including aliases (in dependency order, with a cycle error), constructors and fields.
3. **Order.** Find the order functions and consts are inferred in: each after what it uses, and a group that uses each other together. The edges come from a walk over the source that tracks which names a local hides, as Gleam's call graph does. It only decides order; the checker resolves every name itself.
4. **Inference.** Infer each group in dependency order, then generalise it. This pass builds the typed tree with live inference types.
5. **Finalise.** Once the whole module is inferred, run every check that needs final types, as walks over the finished tree: exhaustiveness, unreachable arms, `_` over `Nil`, an `or` that drops an error, unused values and unused bindings. Then the engine is frozen, so every type in the tree reads back the same finished `Type` from then on.

Diagnostics are made with the module's key already on them. No one adds it afterwards.

### The typed tree

`TypedModule` is what the checker hands on. Everything the old elaborator worked out again is now written on the node:

- every expression's type, a `Ty` into the module's frozen `Types`, and its span
- every name already resolved: a local `BinderId`, a `ValueName`, a constructor, or a builtin `Intrinsic`
- call and constructor arguments in parameter order, with their source order kept only where the evaluation order needs it
- a field access's slot, or its per-variant slots
- the shape of an `or` (`Option` or `Result`)
- number literals parsed into their constant value
- a lambda's captures, and the name it calls itself by
- each function declaration's generalised type
- the order consts run in

Which `+` an operator is, `IntAdd` or `FloatAdd`, is read off the final operand type when the tree is lowered. That is the one place it is decided.

The same tree feeds the editor. Hover is the narrowest node under the cursor. The reference graph is a walk over the tree's names and binders. The checker no longer records references or hover facts as a side effect.

### The interface

```rust
struct Interface {
    key: ModuleKey,
    types: IndexMap<Name, TypeDef>,     // parameters, variants, fields, opacity
    values: IndexMap<Name, ValueDef>,   // generalised Type, labels, what kind of value
    private_names: BTreeSet<Name>,
    doc: Option<String>,
}
```

It is a plain value with no arena indexes, and it can be compared. A module whose interface didn't change doesn't make the modules that import it re-check.

A public value whose type still holds a variable nothing generalised, like a `const` holding a `Subject(a)`, is an error. Its type would mean something different in every module that used it, so it has to be written out.

### Cleanness is a value

`CheckedModule.clean` is `Option<CleanModule>`, and the checker fills it only when that module has no errors and holds nothing the parser couldn't read. Lowering only accepts `CleanModule`s. `into_runnable` stays the only way to get a `Program`.

### Lowering

```rust
fn build_program(modules: &[&CleanModule], defs: &dyn TypeDefs, main: Option<&ValueName>) -> Result<Program, LinkError>
```

Lowering runs once per program, over every clean module in dependency order. It hands out the program-wide ids: `FuncIdx`, global slots, `TypeId`s and the constant pool. For each module it:

- turns each function, lambda and constructor or builtin used as a value into a `TypedFn` of typed IR
- runs arguments into `let`s in the order they were written, when that isn't their parameters' order
- reads a field each variant holds in its own place with a `match`
- turns an `or` into its two-arm `match`, and a record-update spread into field reads
- makes the module's top level: its consts, then its statements

Then every `TypedFn` goes through `lower`, `immediates` and `perceus`, and the `Program` is assembled: `types`, `inits`, `main`, and the `Abi`, found by `TypeName` in the stdlib's interfaces. The checks on `main` (it must be `pub` and take no parameters) happen in the driver, on the entry, for every caller.

### The prelude

The prelude is an ordinary module, `scarlet`. Every check is given its interface as an implicit import. `PreludeBindings` is built from that interface by `TypeName`, so checking the prelude itself never runs with half-empty bindings.

## The build and the session

`Build` replaces both `compile_with` and `IncrementalSession`:

```rust
struct Build {
    sources: Sources,                          // files, embedded stdlib, editor overlays
    checked: HashMap<ModuleKey, CachedCheck>,  // CheckedModule + the hashes it was made from
}
```

A cached check is reused when its source hash and its imports' interface hashes are the same as before. Re-checking a module means calling `check_module` again and storing the result. There is no rewinding, no watermarks and no `seed`.

- **`scarlet check`, `run` and `dis`** make a `Build`, check the entry and what it imports, then lower and link if everything is clean.
- **The LSP** keeps one `Build` for each workspace root. A changed file re-checks itself, and its dependents re-check only if its interface changed. **Each file is checked as its own module, under its real key, not as `main`.** That removes `module_path_for_source`, the `StableDefId` side index and the `uri_for` short-circuit. It also fixes an existing bug where an imported module's errors are shown in the entry file.
- **The REPL** keeps one `Build` too, so the stdlib is checked once per session. It replays the definitions it has accepted as one script ahead of each new line, which is also what `:save` writes out.

## Crates

| Crate | After |
|---|---|
| `scarlet_syntax` | Unchanged |
| `scarlet_ir` | Unchanged, apart from what linking needs |
| `scarlet_types` | `Type`, `Interface`, the inference engine, exhaustiveness |
| `scarlet_check` (new) | `check_module` → `CheckedModule`, the typed tree and the source index |
| `scarlet_lower` | `build_program`, plus `lower`, `perceus` and `immediates` as before |
| `scarlet_elaborate` | Deleted |
| `scarlet_core` | Module resolution, `Build`, the compile driver, the reference graph built from each module's source index |
| `scarlet` | CLI, REPL and LSP, on top of `Build` |

## Tests

The black-box tests stay as they are, and they decide when we're done:

- `golden_examples`, `type_errors`, `type_semantics`, `modules`, `stdlib`, `repl` and `diagnostic_provenance`
- the vm and wire tests
- `lsp_handlers`
- `scarlet_vm/tests/run.rs`

Some tests check the old design's insides, not how Scarlet behaves:

- **Deleted:** `arena_rewind`, `import_rollback`, `type_body_rewind` and `deferred_elaboration`.
- **Rewritten against `Build`:** `incremental` (it checks what gets re-checked, not exact compile counts), `references`, `stdlib_session`, and the checker's unit tests.
- **Regenerated:** the `core_ir` golden files. I'll read the diff, not just accept it.

## Order of work

1. Delete `scarlet_elaborate`, `bytecode/compiler/`, `analysis.rs`, `session.rs` and the parts of `module/` that only exist for rewinding.
2. `Type`, `Interface` and the per-module inference engine in `scarlet_types`.
3. `scarlet_check`, up to a finished typed tree, measured by `type_errors` and `type_semantics`.
4. `lower_module` and `link`, measured by `golden_examples` and the VM tests.
5. `Build`, then the CLI.
6. The reference graph from typed trees, the LSP and the REPL.
7. All seven CI gates green, then the `dis` diff against today's master, every difference explained.

Commits happen on master as each step lands. I won't push until step 7 is green, so `origin/master` keeps working the whole time.

## Bugs this fixes or will show up

- Int literals above `i64` are refused (`const_number`), even though `Int` is a bignum.
- The LSP never checks `main`'s rules and never elaborates the open file's toplevel.
- An imported module's errors are shown in the entry file.
- An alias can be read before its target is known, and then it doesn't equal its target.
- Exhaustiveness runs before types are final.
- A module's bodies can be lowered before its last check reports an error.
