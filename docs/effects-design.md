# Effects

This is the plan for tracking side effects in Scarlet's types, and for letting a caller choose how an effect is carried out. None of it is built yet.

Each point is marked:

- **Decided**: you chose it.
- **Proposed**: my recommendation. It needs your yes before code depends on it.
- **Open**: not decided.

## What this is for

1. **You can tell from a function's type whether it touches the world, and how.** Reading the clock, the network, a file.
2. **You write code once, and the caller picks what its effects do.** The real clock in production and a fixed one in a test. There are no mocks, and you don't pass a `clock` argument down through ten functions.
3. **It costs nothing when you don't use it,** and almost nothing when you do.
4. **As little new syntax as possible.**

Here is where the plan ends up, from a user's side:

```scrl
import scarlet/effect
import scarlet/time

fn is_expired(issued time.Instant) Bool {
	time.since_ms(time.monotonic(), issued) > 3600000
}

// A test, with the clock stopped. Nothing above changes for it.
pub fn main() {
	start = time.monotonic()
	stopped = time.Clock(monotonic: fn() start, epoch_ms: fn() 0)
	println('${effect.handle(stopped, fn() is_expired(start))}')
}
```

Hovering over `is_expired` shows `fn(Instant) Bool uses Clock`. You never write `uses Clock` yourself. The compiler works it out, as it already does for types.

## What Scarlet has today

- **The stdlib is the only way to touch the world.** A function marked `@vm(key)` has no body, and the VM runs it. Only the stdlib may use `@vm`, and `crates/scarlet_core/src/bytecode/analysis.rs:986` rejects it anywhere else. So the compiler already knows every door to the world. There are 136 intrinsics, and 63 of them touch the world (the table under decision 2).
- **Handlers are already written by hand.** `oauth2.Store` (`std/scarlet/oauth2.scrl:43`) is a record of six closures, "caller-provided persistence", taken apart by hand in `register`, `parse_authorize`, `approve` and `token`. Nothing in the repo tests `oauth2`. A fake `Store` needs somewhere to keep what's put in it, and only a process can hold state today.
- **`const` runs any code before `main`.** It could print or spawn. There are 187 consts in 103 `.scrl` files, and a scan finds none that calls `println`, `io`, `process`, `time`, `net`, `os`, `port` or `crypto.random_bytes`.
- **The `Subject` value restriction has holes.** `generalize_restricted` (`scarlet_types/src/types/infer.rs:1231`) is meant to stop a live `Subject` from being used at two message types. It catches a subject inside a user type or a tuple. On master (52866a9), `scarlet check` still accepts both of these, and each sends an `Int` and reads it back as a `String`:

  ```scrl
  s = process.subject()
  get = fn() s
  process.send(get(), 1)
  string.trim(process.receive(get()))
  ```

  ```scrl
  const S = process.subject()
  fn get() { S }
  // main: process.send(get(), 1), then string.trim(process.receive(get()))
  ```
- **Function types have no room for effects.** `TypeNode::Fun { params, ret }` (`infer.rs:212`) is matched in 20 places, 16 of them in `infer.rs`.

## What other languages do

| Language | Tracked in types | Caller can swap | What you write |
|---|---|---|---|
| Koka | Yes, inferred | Yes, handlers | Effects in signatures. An unwritten effect means `total`, which is pure. |
| Effekt | Yes, as capabilities | Yes | Almost nothing. A function passed as a block can't escape, so signatures need no effect variables. |
| Unison | Yes ("abilities"), inferred | Yes | `a -> b` in a signature means "some abilities, worked out from use". `a ->{} b` is pure. |
| Flix | Yes, inferred | Yes (recent) | Effects in signatures. An unannotated function is pure. |
| Roc | Pure or not, nothing finer | No | `=>` for an effectful function type, and `!` at the end of its name. |
| OCaml 5 | No | Yes | Nothing, but an effect nobody handles is a crash at run time. |

What to take from them:

- **Inference makes tracking free to write.** Every typed one infers effects inside a function body.
- **Swapping needs a way to say "run this with a different Clock".** Five of the six have one. Only Roc doesn't.
- **An effect that only answers a question is cheap.** Examples: the clock, randomness, a log, a store. Koka calls these `fun` operations. The handler is just called, and the program carries on. Koka's evidence passing (Xie et al., ICFP 2020 and 2021) makes that an ordinary call.
- **An effect that pauses the program is expensive.** Generators and async need to save "the rest of the program" and resume it later. Koka calls these `ctl` operations.
- **The syntax cost lands in two places:** functions passed to other functions, and functions stored in data. Unison's rule covers the first. The second needs a choice (decision 4).
- **OCaml's model is ruled out.** "Code never crashes" (`docs/semantics.md`) means an effect nobody handles has to be a compile error.

## The decisions

### 1. Track and swap, but don't pause

**Proposed.** There are three levels:

| Level | What you get | Cost |
|---|---|---|
| 1. Tracking | Types say which effects a function uses. | Type checker only |
| 2. Swapping | A caller replaces an effect's handler, for effects that answer and carry on. | A small run-time table per process |
| 3. Pausing | Handlers that save the rest of the program and resume it later. | Saving the stack, in the VM and later in the JIT |

Build 1, then 2. Don't plan 3:

- Processes already cover concurrency.
- Failure is a `Result`, so there is no exception effect to handle.
- A generator can be a closure that returns the next step.

Nothing in 1 or 2 stops 3 being added later.

### 2. One base effect per kind of door

**Proposed.** Each world-touching intrinsic belongs to one base effect:

| Effect | Intrinsics | Count |
|---|---|---|
| `Console` | `println` | 1 |
| `Files` | `io.read_file`, `io.write_file` | 2 |
| `Net` | `net.*`, `socket.*`, `tls.*` | 20 |
| `Env` | `os.argv`, `os.env` | 2 |
| `Ports` | `port.*` (running other programs) | 6 |
| `Clock` | `time.monotonic`, `time.epoch_ms` | 2 |
| `Random` | `crypto.random_bytes` | 1 |
| `Process` | `process.*` | 26 |
| `Internal` | `internal.*` (runtime introspection for tests) | 3 |

The other 73 intrinsics are pure. They're strings, ints, floats, binaries, arrays, maps, JSON, HTTP parsing, hashes and signature checks, `address.parse`, and `wire`. Anything built only from pure parts is pure.

Why not one `IO` effect? Then you couldn't fake the clock without faking the network too. Why not one effect per function? Types would list a dozen names.

`Intrinsic` (`scarlet_ir/src/intrinsic.rs`) gets a method that names each intrinsic's effect. It's written next to the key, so a new intrinsic can't be added without choosing one.

**These are not effects:**
- **Running forever.** A pure function may still loop. Koka tracks this as `div`. Nothing here would use it, and every recursive function would carry it.
- **Failing.** That's a `Result`, which is already a value.
- **Allocating, or running out of memory.** Running out is a resource death (`docs/semantics.md`), not something code handles.

### 3. Every function type carries a row of effects, always inferred

**Proposed.**

- **What a row is.** A function type gets an effect row: a list of effect names, and maybe a variable standing for "whatever else".
  - `fn(Int) String uses Clock, Net` uses exactly those two.
  - `fn(a) b uses e` uses whatever `e` turns out to be.
  - A row with nothing in it is pure.
- **How rows unify.** Like Koka (Leijen, "Koka: Programming with Row-polymorphic Effect Types", 2014). Rows unify the way extensible records do. This fits the HM inferencer we have: same levels, same generalization, and one new kind of variable.
- **Functions passed to functions need nothing written.**
  - `array.map` infers as `fn(Array(a), fn(a) b uses e) Array(b) uses e`. `map` itself is pure, so it uses whatever its callback uses.
  - `array.map(xs, int.to_string)` is pure. `array.map(names, fn(n) println(n))` uses `Console`.
- **An annotation keeps meaning what it means today.** A function type written in a signature, like `f fn(a) b`, gets a fresh row variable. That's Unison's rule. So every existing signature in the stdlib and the tests keeps its meaning, and none needs editing.
- **A function's effects are part of its public type.**
  - They're worked out from the body. So if you add a clock read to a `pub fn`, its type changes, and a caller that must stay pure (a `const`, say) stops compiling.
  - This is how Rust's `Send` leaks through `impl Trait`.
  - Hover and `scarlet check` show the row, so the change is visible.

**Open: how a row is printed.** It is never written in source, so the printed form mustn't look like syntax you can type. This doc uses `uses Clock, Net`.

### 4. Functions stored in data carry one hidden row per type

Signatures don't reach this case:

```scrl
pub type Store {
	get_client fn(String) Option(String)
	put_code fn(String, Grant) Nil
}
```

There are 26 function fields like this across 10 `.scrl` files. They include `http`'s `handler fn(Request) Response` and `json/decode`'s `step`. There are two ways to handle them:

- **(a) A hidden row on the type.** `Store` gets an invisible effect parameter, and all of its function fields share it. It's inferred like any type parameter, from what the fields are given when the value is built. A `Store` built from pure closures is a pure `Store`. Koka makes you write that parameter. We'd infer it.
- **(b) A stored function may do anything.** Calling one adds "everything" to the row. It's simple, but then every HTTP server's type says it touches everything, and tracking stops being useful exactly where programs are big.

**Proposed: (a).** The catch is that a type holding a type with a hidden row gets a hidden row too. So the parameter spreads through type declarations, and error messages have to show it when it matters.

### 5. Declaring an effect

**Open.** Two shapes:

**(A) An attribute, with no new syntax.** An operation is a function marked `@effect(E)`, shaped like the `@vm` ones the stdlib has today:

```scrl
// std/scarlet/time.scrl
@effect(Clock)
pub fn monotonic() Instant {
	Instant(monotonic_ms())
}

@effect(Clock)
@vm(time__epoch_ms)
pub fn epoch_ms() Int

// oauth2.scrl, or any user module
@effect(Store)
pub fn get_client(id String) Option(String)

@effect(Store)
pub fn put_code(code String, grant Grant) Nil
```

- **What an operation is.** A `fn` marked `@effect(E)` is an operation of effect `E`. The operations of `E` are the ones in the same module with the same `E`. Unlike `@vm`, any module may declare one.
- **The handler type.** The compiler also makes a type `E` in that module, with one field per operation, of the same type. So `time.Clock(monotonic: ..., epoch_ms: ...)` or `oauth2.Store(get_client: ..., put_code: ...)` builds a handler.
- **Calling an operation looks like calling any function:** `time.monotonic()`, `get_client(id)`. No call site anywhere changes.
- **The default.** An operation's body, or its `@vm` intrinsic, is what runs when nobody has swapped `E`. An operation with neither must be handled. That's how `Store` differs from `Clock`.
- **No way around a fake.** A world-touching intrinsic may only be called from an operation's default. Otherwise a fake clock could be bypassed. `time.deadline_in_ms` calls `monotonic_ms()` directly today, so under this rule it becomes `add_ms(monotonic(), ms)`.

**(B) Keywords,** as in Koka and Effekt:

```scrl
effect Store {
	fn get_client(id String) Option(String)
	fn put_code(code String, grant Grant) Nil
}
```

That's easier to spot when reading, and closer to other languages. It costs a keyword, and the parser, formatter, tree-sitter grammar and highlighters all have to learn it.

**I'd pick (A).**
- Attributes already exist, and the stdlib already writes built-ins this way.
- Handlers are ordinary values, so a function can build one, like `fake_clock(start)`.
- Not one call site changes.

### 6. Swapping: `effect.handle`

**Proposed.**

```scrl
effect.handle(stopped_clock, fn() is_expired(t))
```

- **What it is.** `handle` is a built-in in a new `scarlet/effect` module. Its typing rule is its own, the way `@vm` functions have their types from the stdlib. If the body uses `E` and more, the whole call uses the "more", plus whatever the handler's own closures use.
- **What happens.** While the body runs, each operation of `E` calls the handler's field instead of the default.
- **Which handler answers.** The one in force when the operation runs. The types keep this honest: a closure that escapes a `handle` still has `E` in its type, so whoever calls it later must handle `E` too.
- **The handler's own code runs with the handlers from outside the `handle`.** That's Koka's rule. So a logging handler that logs isn't caught by itself.
- **`main` gets the defaults** for every effect it doesn't handle, and every base effect has one. If an operation with no default reaches `main` unhandled, that's a compile error, pointing at the call that brought it in.

### 7. Handlers that remember

**Proposed, with the shape Open.**

Some fakes must remember things:
- a `Store` you put a code in and take it back out of
- a log you read at the end
- a clock that ticks forward on every read

Scarlet values don't change, so that state must be passed along:

```scrl
(result, log) = effect.handle_with([], collect_lines, fn() run_job())
```

- **How it's typed.** Each field takes the state first and returns `(answer, new_state)`. `handle_with` returns the body's result and the final state.
- **One way to think of it:** it's a fold over the operations the body performs, in the order it performs them. It's the same idea as `array.fold`, so there's nothing new to learn.
- **Open: what the stateful handler type is called.** It's generated next to `E`, and it takes the state type as a parameter.

### 8. How it runs, and what it costs

**Proposed.**

- **Pure code: nothing changes.** It gets no hidden argument and no check.
- **Each process has a handler table.** It's a small immutable map from an effect to the handler in force for it. It's keyed by the effect's `TypeId`, which is its identity, not its name.
- **`handle` adds an entry** (the handler, plus the table it was added to), and leaving `handle` puts the old table back.
- **An operation looks up its effect.**
  - No handler: the operation's default runs. For a base effect that's the intrinsic, the same as today, plus one load and one branch.
  - A handler: switch to the table saved in its entry, call the field, then switch back.
- **In short:**
  - Pure code pays nothing.
  - Code using the real clock pays one branch per read.
  - Code under a fake pays one closure call, the same as passing the fake as an argument yourself.

Koka instead passes the table to every effectful function as a hidden argument ("evidence passing"). That avoids the lookup, and it lets the compiler inline a handler it can see. The meaning is the same, so it can replace the table later, when the JIT exists. The table comes first because it's the one that's easy to reason about.

### 9. What knowing "pure" gives us right away

**Proposed.**

- **`const` must be pure.**
  - Importing a module can then never print, spawn or read the clock.
  - A const could be worked out when the program is built.
  - The scan above finds no const in the repo that breaks this. It still needs a `major` changeset, since user code may have one.
- **The `Subject` holes close.**
  - Generalize a binding only when its right side is pure. That's the effect-based answer ML had before the value restriction (Talpin and Jouvelot, 1992).
  - `process.subject()` uses `Process`, so `s = process.subject()` keeps one message type. It holds with or without a wrapper: `b = Box(process.subject())` and `s = make()` stay one type too.
  - "Not generalized" has to mean the variables drop to the binding's level. Then `get = fn() s` can't generalize them either. Today a weak variable keeps the deeper level it was made at (`infer.rs:1262`), and that's how the first hole gets through.
  - `make = fn() process.subject()` is still generalized, and that's safe: each call makes a new subject.
  - The second hole goes with pure `const`: `const S = process.subject()` no longer compiles.
  - This replaces `generalize_restricted` and its list of restricted constructors.
- **Hover says what a function touches.**

### 10. Processes and wire

**Open.** Processes are on hold, so this only sets a safe starting point.

- **Proposed until then.**
  - `spawn(f)` takes an `f` that uses only base effects.
  - The child runs with the VM's real handlers.
  - So a test that fakes the clock doesn't reach a worker it spawns, and the types don't say so. That gap is the thing to settle once processes are designed.
- **Wire.** A closure on the wire names its code. Its type now has a row, so the shape fingerprint must include the row.

## Order of work

1. **Rows in function types.**
   - `TypeNode::Fun` gets a row, and unification, generalization and display learn rows.
   - Each `Intrinsic` names its effect, and hover and `scarlet check` show rows.
   - Nothing is rejected, and no program changes meaning.
   - Report how much of the stdlib comes out pure.
2. **Hidden rows on types with function fields** (decision 4).
3. **Put purity to use:** pure `const` and effect-based generalization. This is a `major` changeset, and it closes the `Subject` holes.
4. **Swapping:** `@effect`, the generated handler type, `effect.handle`, and the per-process handler table in `scarlet_vm`. Base effects become swappable, and `oauth2.Store` can become an effect with a test.
5. **Handlers that remember:** `effect.handle_with`.
6. **Later, with the JIT:** evidence passing, and inlining a handler the compiler can see.

## Open, all in one place

- **The declaration syntax:** attribute (A) or keyword (B). Decision 5; I'd pick (A).
- **How a row is printed** in hover and errors.
- **What the stateful handler type is called.**
- **Whether `internal.*` gets its own effect**, or goes in with `Process`.
- **Whether a spawned process gets its parent's handlers.** This waits on processes.
- **How rows go into wire's shape fingerprint.**

## Sources

- Leijen. *Koka: Programming with Row-polymorphic Effect Types.* MSFP 2014.
- Xie, Brachthäuser, Hillerström, Schuster, Leijen. *Effect Handlers, Evidently.* ICFP 2020.
- Xie, Leijen. *Generalized Evidence Passing for Effect Handlers.* ICFP 2021.
- Brachthäuser, Schuster, Ostermann. *Effects as Capabilities: Effect Handlers and Lightweight Effect Polymorphism.* OOPSLA 2020.
- Brachthäuser, Schuster, Lee, Boruch-Gruszecki. *Effects, Capabilities, and Boxes.* OOPSLA 2022.
- Lindley, McBride, McLaughlin. *Do Be Do Be Do* (Frank, the basis of Unison's abilities). POPL 2017.
- Madsen, van de Pol. *Polymorphic Types and Effects with Boolean Unification* (Flix). OOPSLA 2020.
- Talpin, Jouvelot. *Polymorphic Type, Region and Effect Inference.* JFP 1992.
- Koka book: https://koka-lang.github.io/koka/doc/book.html
- Unison, reading type signatures: https://www.unison-lang.org/docs/fundamentals/values-and-functions/reading-type-signatures/
- Roc, functional and effectful: https://www.roc-lang.org/functional
