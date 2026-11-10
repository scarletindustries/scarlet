# No implicit state machines

Three audits went through the new middle looking for implicit state machines: a value whose meaning depends on how far some sequence of steps has got, where nothing in its type says which step that is. They found about 70. Nearly all of them have one of six shapes, so this plan is about removing the shapes, not patching 70 sites.

It also fixes the real bugs the audits found:

- **The value restriction is unsound.** `s = process::subject()` with a function that sends to `s` lets one subject be written at `Int` and read at `String`. A weak variable keeps its deep level, so a later generalisation quantifies it.
- **Renaming a local changes whether a program checks.** The call graph walks `x = fn .. { x(..) }` with different scope rules than the checker, so a spurious edge merges two groups.
- **The CLI and the REPL print the entry's errors with no code under them.** The diagnostic says which module it is about, and the printer looks the module up again by key, which fails for `main`.
- **The editor flickers.** A file that is both open and imported has one cache slot, which is filled alternately by two different checks.
- **Type names in a function's signature are recorded as uses with no owner**, so dead-code analysis can't see them.
- **Importing two modules under one qualifier is accepted**, and uses then point at the wrong import.

## The six shapes, and what replaces each

**1. A field that changes meaning as passes run.** For example, `Checker` holds state from five passes. `OwnValue.scheme` goes from `Mono` to `Local`, and `OwnType.def` is filled in later. `Builder.temps` starts as `RTy(0)`. `Fx` is built and then has captures added.

*Replacement:* each pass is a function from finished values to a finished value. The next pass receives that value and can't see anything earlier. No struct lives across passes.

**2. Calls that must be balanced.** For example: `enter_level`/`leave_level`, `open_block`/`close_block`, `open_frame`/`close_frame`, `loading.insert`/`remove`, lowering's `mark`/`seal`, and an or-pattern frame's push and pop.

*Replacement:* the scope is a closure, `in_level(|c| ..)`, or a value the closing call consumes, like `generalize(level, ty)`. Where the scope is lexical, it is an argument passed down the recursion instead.

**3. "Set it here, take it there".** For example: `pending_self`, `owner`, `has_error_nodes`, `BinderState.used`, `discards`, `defs.last_mut()` after `new_binder`, and lowering's `join_uses` with `retract_gotos`.

*Replacement:* the value is an argument to the call that needs it, or part of what a call returns.

**4. Reserve now, fill later.** For example: `functions: Vec<Option<_>>`, `Builder.fns`, and `TempTys`/`PreludeCtors` placeholders.

*Replacement:* ids are assigned when the finished values are put together. Something that needs an id before its body exists gets it from a table built in full beforehand.

**5. Per-operation state on a long-lived object.** For example: `Build.verified`, `Build.loading`, the `checks`/`parses` counters, `Root.graph` reset by hand, and the LSP's document state spread over five maps.

*Replacement:* a context value is made for one operation and dropped at its end. The operation returns an immutable snapshot, and readers read the snapshot.

**6. A fact looked up again later instead of carried.** For example: a diagnostic's source text found again by key, the cleanness of a module computed from whatever `diags` held at that moment, the module-to-URI rule written twice, freshness decided in three places, and silent `Nil`/`SelfClosure` fallbacks.

*Replacement:* the code that makes a fact puts it in a value, and that value is the only way to the fact. A failed lookup is an error value, never a default.

## The checker

```
check_module(input)
  = imports    = Imports::bind(input)                 // qualifier → (key, alias site); duplicates reported
  ; decls      = declare(module, &imports)            // every name, its kind, dup/reserved errors
  ; types      = define_types(&decls, &imports)       // every TypeDef, complete
  ; resolved   = resolve(module, &decls, &types, &imports)
  ; inferred   = infer(&resolved, &types)             // owns the engine for this module
  ; types      = inferred.engine.freeze()
  ; findings   = finalise(&types, &inferred.tree)
  ; interface  = interface(&types, &inferred)
  ; index      = index(&types, &resolved, &inferred.tree)
  ; CheckedModule::assemble(diagnostics..., tree)     // clean iff no error anywhere
```

**Name resolution becomes its own pass.** This is the largest change, and it is what rustc and GHC do. `resolve` walks the AST once, with the scope rules in one place, and gives:

- the resolved tree, where every name already says what it is: a local binder, a module value, a constructor or a builtin;
- the binder table: each binder's name and span, whether it is at module scope, the function it belongs to, and its doc;
- each lambda's captures and the name it calls itself by;
- the call graph's edges, taken from the same resolution, so the groups can't disagree with the names. The separate `Walker` is deleted;
- every use of every binder. "Unused" is then a set difference, not a flag;
- the name-level pattern rules: a name bound twice, or not bound in every alternative of an `|`.

With that done, inference has no scope to manage. There is no `Scope`, no `pending_self`, no `module_scope` worked out from depth, and no captures to collect. It only gives types to a tree whose names are settled.

**Inference** runs a group at a time. Each group gets an `InferCtx` holding the group's own monotypes, the declaration the uses belong to (the owner), and a level token from `engine.enter()`:

- A name in the group reads its monotype from the context.
- Any other module value reads its finished scheme.
- A value that is in neither is an error, never a silent `Mono`.
- `generalize(level, ty)` consumes the token. It also lowers every variable it leaves unquantified to the outer level. That is the soundness fix.
- A binder's scheme is written once, when its binding is checked. The resolved tree guarantees that comes before any use, so nothing is overwritten.

**The engine** gets three fixes:

- A unify that fails rolls back everything it changed, using a trail, not just the last step.
- Variables are named when the engine is frozen, in a fixed order, so hover and interface letters don't depend on which error message printed first. Error messages during inference name variables locally.
- After `freeze`, only `Types` exists, and it is read-only. That is why the later passes can't unify.

**Finalise, interface and index** each take `&Types` and return what they found. Hover is computed from the frozen tree, which removes the `hovers` list. A tree's `Error` node can only be made together with its diagnostic, so `has_error_nodes` goes.

**Exhaustiveness** becomes one stateless call: `analyse(space, arms) -> MatchReport { missing, unreachable, catch_alls }`. Its patterns and interner are private to it.

## The build

```rust
trait Sources {                       // where module text comes from
    fn read(&self, m: &ResolvedModule) -> Result<SourceFile, ReadError>;
}
struct SourceFile { key: ModuleKey, origin: Origin, text: Rc<str>, revision: Revision }

impl Build {
    fn check(&mut self, sources: &dyn Sources, entry: EntrySpec) -> Analysis;
}
```

- **Text comes from a `Sources` the caller hands in.** The disk reader keeps its own stat cache, and the editor's reader puts open buffers first. The build no longer has overlays or stats.
- **A cache entry is keyed by everything that decides its result:** `CheckSpec { key, role, unused }`. A file that is both open as a program and imported as a library has two entries, which fixes the flicker.
- **Freshness is one function,** `freshness(&cached, &spec, &revision, &deps) -> Fresh | Stale`. Its inputs are the text's revision and the interfaces the check actually read, which the checker returns.
- **Each `check` makes a `CheckRun`** holding the loading stack, the modules already verified and the counters. It is dropped at the end, and the call returns an immutable `Analysis { modules: Vec<Rc<Module>> /* imports first */, entry, counts }`.
- **Every consumer reads the `Analysis`, never the build:** `compile`, the reference graph, hover, `:type` and completion. The build keeps only what the latest analyses reach, so a deleted or no-longer-imported module disappears.
- **A diagnostic carries its `Rc<SourceFile>`,** so the printer renders the text the check saw. This fixes the missing code under the entry's errors, and nothing re-reads a file to print.

`compile` returns `enum Compiled { Rejected { diagnostics }, Checked { diagnostics, analysis }, Runnable { diagnostics, analysis, program } }`, so "no errors" and "has a program" can't disagree.

## The editor and the REPL

- **One map of documents.** It is `HashMap<Uri, Doc>`, with `enum Doc { Indexed(SourceFile), Open(SourceFile) }`. The editor's `Sources` reads from it, so there are no overlay copies to keep in step. A document's root and module key are derived from its URI, not stored.
- **Each root holds its latest `Analysis` and the graph built from it**, replaced together after a check. The graph is never reset by hand.
- **A URI's diagnostics are published only by the root that owns it.** One function maps a module to its URI, and both rename and find-references use it.
- **A workspace-folder change** works out each document's owner again and re-checks the documents that moved.
- **The REPL keeps a `Vec<AcceptedEntry>`.** The replayed source and the completion names both come from it, so a reset clears one thing. `:type` reads the hover from `Compiled::Checked`.

## Lowering

```
build_program(imports: &[&CleanModule], entry: &CleanModule, defs)
  = ids      = Ids::new(defs)                 // type ids, pool builder, prelude types
  ; prelude  = Prelude::resolve(&mut ids)?    // temps and constructors, made whole
  ; symbols  = Symbols::of(&modules)          // every module fn's FuncIdx, every global's slot
  ; bodies   = translate each fn (&symbols, &prelude, &mut ids)
  ; pool     = ids.freeze()
  ; Program { fns, pool, .. }                 // one pool, not an Rc per function
```

- **Nothing is a placeholder.** `Symbols` is complete before any body is translated, and a lookup that fails is an error, not a `Nil`. A lambda or eta wrapper's body never names its own index, so it is pushed when finished and takes the index `push` returns.
- **A function's binding ids are numbered up front** from the binder table, which says which function each binder belongs to. Translating doesn't mint them along the way, and arms don't depend on field order.
- **`lower`:**
  - A spine segment is a value that `seal` consumes, or a `sealed(|lo| ..)` closure. Raw marks go.
  - Bindings are a scoped environment passed down, so an or-alternative can't leak its locals into the next.
  - Each subtree returns the joins it jumps to, so `join_uses` and `retract_gotos` go.
  - A fall-through target is made the first time something can fail, which removes the three "is this fallible" predictors.
- **`perceus`:**
  - It is told which binds are pinned globals, so `keep_globals` stops undoing its work.
  - Its two reuse walks each compute a plan, applied once.
  - It reads each local's type from a table `lower` returns, so a local can't be left unrecorded.
- **`immediates`** rewrites by value. The Core pipeline is one function, so the passes can't run out of order.

## Order of work

Each step leaves all 7 gates green. Program output is unchanged, except where a step fixes a bug.

1. **The two urgent bugs:** the missing code under entry errors, and the value restriction. Both are small.
2. **The checker:** name resolution as its own pass, inference by group, then freeze before finalise. This is the largest step.
3. **The build:** `Sources`, `CheckSpec`, one freshness function, `CheckRun`, `Analysis`, and diagnostics carrying their source.
4. **The editor and the REPL on `Analysis`.**
5. **Lowering:** `Ids`, `Prelude`, `Symbols`, binding ids numbered up front, and one pool.
6. **`lower`, `perceus` and `immediates`.**
