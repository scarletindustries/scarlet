# The new VM

This is the plan for the VM that runs a compiled Scarlet program. None of it is built yet. It lands one small PR at a time, in the order at the bottom.

Each point is marked:

- **Decided**: you chose it. Most of these are also in `docs/semantics.md`.
- **Proposed**: my recommendation. It needs your yes before code depends on it.
- **Open**: not decided.

## What the VM is given

The compiler hands over one `core_ir::Program` (`crates/scarlet_ir/src/core_ir/mod.rs`):

| Field | What it is |
|---|---|
| `fns` | Every function, numbered by `FuncIdx`: declared ones, lambdas, and the wrappers the compiler writes when a constructor or built-in is used as a value. |
| `consts` | Every constant, numbered by `ConstId`: Int, Float, String and Binary. |
| `inits` | Each imported module's toplevel, in the order they must run. The prelude comes first. |
| `toplevel` | The entry file's own toplevel. |
| `main` | The function the program starts at. It's `None` for a REPL entry, which runs its toplevel and prints the value. |
| `globals` | How many module-level slots the program needs. Toplevels write them, and functions read them with `Load::Global`. |
| `types` | The names of every type the program builds or matches a constructor of, by `TypeId`: the type's name, and each variant's name and field labels. Only for showing a value to a person (`Some(1)`, `Point{ x: 1, y: 2 }`); nothing the program *does* depends on a name. |

Each function is a `LoweredFn`: its `module`, its source `name`, its `core` body, and the `pool` that the types in the body are numbered in.

A body is already in A-normal form: every operand is a local, and nested expressions have been flattened into a chain of `Let`s. The whole language comes down to these pieces:

- **Values** (`Atom`): a local, a constant, a global or capture, `nil`, a bool, a constructor, a closure, a primitive op (`PrimOp`, like `IntAdd`), a built-in call (`Intrinsic`, one of 135), and a call. A call names its target as a known function, the function itself, or a closure held in a local.
- **Control** (`CoreExpr`): `Let`, `If`, `Match`, `Tail` (return a value or tail-call), `LetJoin` (a branch whose value is used), `LetCont` and `Goto` (a shared "try the next case" point for pattern matching), and `Drop`.
- **Memory hints from Perceus**: `Drop x` at the last use of `x`, and `Ctor { reuse: Some(x) }` when a new constructor may overwrite `x`'s cell in place.
- **Bool and Nil are never constructors.** In the source, `True`, `False` and `Nil` are the prelude's constructors. A compiler pass (`core_ir/immediates.rs`) turns each one into the `bool` or `nil` value and turns a `match` on a Bool into an `If`, so the VM never needs to know which type is the prelude's `Bool`.

`scarlet dis FILE` prints all of this for any program.

## The rule the VM keeps

**Decided.** Code never crashes, and failure is a value. For the VM this means nothing Scarlet code does can take down a process, let alone the program.

The old VM broke this in these ways. The new one must not:

| The old VM | The new VM |
|---|---|
| `xs[a..b]` out of range crashed the process. | Returns a value (the stdlib signature changes with it). |
| A `receive` on another process's subject crashed. | Impossible, or a value. |
| Printing or hashing a deeply nested value overflowed the Rust stack. | Walks values with a loop, the way the old `==` already did. |
| `println` into a closed pipe crashed. | Stops quietly. |
| A huge timeout crashed the scheduler. | The timeout is clamped. |
| Tampered `wire` bytes aborted the program. | `decode` returns `Err`. |
| One process using too much memory aborted everything. | Only that process stops. That's a resource death, not a bug. |
| A bug in the VM itself aborted everything. | Stops only the process it happened in, and says where, straight away. |

**Proposed: the VM never recurses in Rust on anything the program controls.** Call frames live on a stack the VM allocates itself, and printing, hashing, equality and freeing walk values with explicit work lists. Then deep recursion or deep data just uses memory. No stack overflow is reachable from Scarlet code.

## Values

**Decided: every value is one 64-bit word, and floats are stored as themselves.** Floats never allocate. Game servers were named as a target, and they need fast floats.

**Proposed: NaN boxing, like the old VM.** A float's NaN bit patterns hold every other kind of value: small ints, `nil`, bools, and pointers to heap objects. Scarlet has no NaN (below), so every NaN pattern is free for this. The old VM had to tidy stray NaNs first; the new one never meets one.

**Decided: Int is arbitrary precision.** Small ints live in the word, and big ones are heap objects.

- **Proposed:** the fast path is a checked add on two small ints. On overflow, the result becomes a big int.
- **Proposed:** a result that fits in the word is always stored in the word. So each number has exactly one representation, and `==` and map keys can't disagree about it.
- **Proposed:** use the `num-bigint` crate, behind our own small `BigInt` module, so the choice can change in one place later.
- **Open:** how many bits a small int gets. The old VM used ±2^47.

**Decided: floats have no NaN and no infinity.** After each float op:

- `x / 0.0` is `0.0`. This needs its own check before dividing: IEEE says `1.0 / 0.0` is infinity, and clamping that would give the largest float instead of `0.0`.
- A result that is too big stops at ±1.7976931348623157e308.
- A result with no answer, like `0.0 / 0.0`, is `0.0`.

**Heap objects**: constructor cells, tuples, closures, strings, binaries, arrays, maps, big ints, and handles like process ids.

## Memory

**Decided: reference counting and Perceus.** There is no tracing GC. Values can't form cycles, so counting frees everything.

**Proposed: the VM counts references itself, and Perceus makes it earlier and cheaper.** This is the split the old VM had:

- Reading a local into an operand takes a new reference. Overwriting a slot, or leaving a frame, gives one up.
- That alone is always correct, whatever Perceus did or didn't insert.
- Perceus's `Drop x` gives the reference up at `x`'s last use instead of at frame exit. If it was the only reference, the cell is kept empty for a following `Ctor { reuse: Some(x) }` to fill in place.
- A tail call reads its arguments, then gives up every reference its frame still holds, because the callee takes the frame's place. So Perceus puts no `Drop` of an argument before a tail call. The old VM's self tail call kept its frame and needed one, which its emitter moved past the argument reads; in the new VM that drop would free the argument before the call read it.

This matters because Perceus decides what to drop by type, and it leaves some types alone on purpose: `Int`, `Float`, `String`, and generic type variables (`ResolvedPool::is_heap`). Under this split those are still freed, just at frame exit. A big `Int` is a heap object Perceus never drops, and it's still correct.

**Open:** when a read may *move* a reference instead of copying it, for example a local's last use as a call argument. That's an optimisation, but it's also what lets reuse fire in the callee. It has to be written down against `perceus.rs` before the VM does it.

**Proposed: constants are frozen.** They live in an area that is never freed, and they're shared across threads with no counting, as in the old VM.

**Proposed: freeing a big value is a loop, and can be paused.** Dropping a million-item list must not freeze a scheduler thread. The audit measured a 3.4 s freeze on a large `==`.

**Decided: a heap per process.** Each process allocates from its own heap, and reference counting runs inside it. This is BEAM's shape, with counting where BEAM has a GC.

- **Why it works:** messages are copied when sent, so every object in a process's heap is reachable only from that process. Only the owner ever allocates or frees in it, so it needs no locks and no atomic counts.
- **A limit is just the heap's size.** A process that passes its limit stops, and that's a resource death, not a bug. Its links hear about it like any other death, as with BEAM's `max_heap_size`.
- **Death frees everything at once:** the heap's memory is returned in whole chunks, with no walk over the objects in it.

**Proposed: how.**

- The heap is plain data owned by the process: chunks of memory, carved into cells by size, with a free list per size. So it moves with the process when the process moves to another scheduler thread. The old concern, that allocators like mimalloc tie a heap to one thread, doesn't apply to a heap we own.
- A few things must live outside any one process's heap, because more than one process can hold them:
  - constants, in the frozen area;
  - big binaries, which are shared on send rather than copied, as in the old VM, and counted with atomic counts;
  - handles to OS resources, like sockets and files.
- A process keeps a list of the outside things its heap points to. When it dies, it releases each of them before returning its chunks. This is BEAM's "off-heap" list.

**Proposed: how a heap grows.**

- **No heap until it's needed.** A process gets no chunk until it makes its first heap value. One that only does Int maths or waits on messages costs nothing here.
- **Small values fill chunks.** The first chunk is about 2 KB, and each new chunk is about 1.6 times the last. Growing in steps, rather than by exactly each value's size, keeps the number of chunks small: a 1 MB list of 24-byte cells fits in about 13 chunks instead of about 43,000. Allocation stays cheap, and a death returns a handful of chunks, not thousands.
- **A value bigger than the current chunk size gets a chunk of exactly its size.**
- **A chunk that empties goes back to the system.**

**Where the numbers come from: BEAM.** Checked against Erlang/OTP `master` at `bf52b4d716` (2026-09-21):

- A new process's heap starts at 233 words (`H_DEFAULT_SIZE`, `erts/emulator/beam/erl_vm.h`). A whole fresh process is 327 words, about 2.6 KB on a 64-bit machine (`system/doc/efficiency_guide/eff_guide_processes.md`).
- Heap sizes follow Fibonacci plus one word: 12, 38, 51, 90, 142, 233, 376, 610, and so on. That's about 1.6 times per step, up to 833,026 words (about 6.4 MB). After that, each step adds 20% (`erts_init_gc` in `erts/emulator/beam/erl_gc.c`).
- When a heap has no room, BEAM allocates a separate heap fragment of exactly the size needed (`erts_heap_alloc`, `erts/emulator/beam/utils.c`). A message that can't go straight into the receiver's heap goes into one too (`erts_try_alloc_message_on_heap`, `erl_message.c`).
- The Fibonacci table only sizes the main heap at the next collection, which copies everything alive into one block. At that point a heap grows to the next size that fits, and shrinks when it's using under 25% (`erl_gc.c`).

**Why we differ.** BEAM's copying GC moves values, so it can put a heap back into one block, and it rounds up then to keep copies rare. Reference counting never moves a value, so our heap stays a list of chunks. We need BEAM's first mechanism (exact-size space when short), but not its second (copying into a table-sized block).

**Open: fragmentation.** Freed cells leave gaps that a copying GC would squeeze out, and we can't. BEAM hit its own version of this: shrinking hibernated heaps in place "caused serious fragmentation problems when large amounts of processes were hibernated", so `hibernate` now copies what's alive into a new heap of exact size (`garbage_collect_hibernate` in `erl_gc.c`). We should measure the gaps once processes exist.

## Running code

**Decided: a flat list of register instructions per function, made once when the program loads.** Each Core construct becomes one instruction, or a few. A local is a register: `Let x = IntAdd(a, b)` becomes `x = add a, b`. `If` and `Match` become branches, `LetCont`/`Goto` become labels and jumps, and a `Tail` call reuses the frame.

A frame is then just "which function, which instruction, where its registers start". That keeps tail calls, the preemption check, and "no Rust recursion" simple. `dis` can print the instructions next to the Core IR, so nothing is hidden.

It is not the old bytecode: there's no stack machine, no fused super-instructions and no peephole pass.

**Decided: a JIT comes later.** It compiles the same instructions to machine code, so the interpreter and the JIT agree by construction on what each instruction means. Speed work, JIT included, waits for a benchmark to point at.

**Proposed: every call is a proper tail call when it's in tail position.** Scarlet programs rely on it (`examples/tco.scrl`).

**Proposed: each `Intrinsic` is one Rust function that returns a value and never panics.**

## Where the VM lives

**Decided: a new `scarlet_ir` crate holds the Program types, and `scarlet_vm` depends only on it.** That's the Core IR, `PrimOp`, `Const`, `Intrinsic`, `TypeId` and the type pool. The passes (`lower`, `perceus`) stay in `scarlet_core`, which depends on `scarlet_ir` too.

Everything the VM can see is then in one crate, which is the whole contract between the compiler and the VM. Moving the types is a few PRs with no behaviour change.

Either way, `CLAUDE.md`'s crate-layout paragraph is out of date. It still describes the old VM, with bytecode, a JIT, and a "language-agnostic" runtime. The new VM runs Scarlet's own Core IR, so it isn't language-agnostic any more. The paragraph gets rewritten in the first VM PR.

## Processes

**Proposed: keep the old design's shape.** That's lightweight processes, typed mailboxes (`Subject`), one scheduler thread per core, preemption by counting "reductions" (units of work), and links, monitors and supervisors for real deaths.

What the audit asks to change:

- Native work is charged per item, so a big `==`, send or free can't hold a thread.
- A dead process says why and where, straight away.
- **Open:** when a program ends. Today it's when every process has ended, which hangs if one never does. The audit's pick is "when `main` returns". Also open: the exit code when `main` returns `Err`.

The first VM PRs run one process on one thread. Processes come after the single-process VM is solid.

## Order of work

Each step is one PR or a few. Each PR removes the `#[ignore]` from exactly the tests it makes pass, so `cargo test -p scarlet -- --ignored` counts what's left: 311 after step 2, 301 after step 4, 300 after step 5, and 294 once Bool and Nil stopped being constructors. (Each count is one higher than first written: one Linux-only network test, which skips itself on a Mac, was parked late.)

1. The `scarlet_ir` crate. **Done.**
2. A VM that runs `pub fn main() { println(1 + 2) }`: the value word with small ints only, Int operations, calls, `Println`. `scarlet run` uses it. **Done.**
3. Control flow: `If` and `LetJoin`. **Done.** `Match` and `LetCont`/`Goto` come with constructors, which most matches are over.
4. The per-process heap and reference counting, so there is somewhere to put a heap value. **Done** for one process, with strings as the first heap value: the smallest one, and enough to run `examples/hello.scrl`. The limit comes with processes.
5. Big ints. Moved up: when step 2 landed, 119 of the parked tests stopped at a 64-bit constant, most of them in a stdlib module's toplevel (`int.max_value` and the like), before the test's own code ran. **Done.** Those 119 then stopped one step later, at a constructor, so constructors are next. An Int *literal* past 64 bits is still a compile error, because the compiler keeps Int constants as `i64`; that is the compiler's to fix.
6. Constructors, tuples, fields, then Perceus's `Drop` and reuse. The allocation-count tests come back here.
7. Closures that capture, and calling a function value.
8. Floats with the no-NaN rule.
9. The rest of strings, then binaries (with binary patterns), arrays and maps.
10. The rest of the intrinsics, one stdlib module at a time.
11. Processes: mailboxes, the scheduler, preemption, then links, monitors and supervisors.
12. IO (files, sockets, TLS, HTTP), `os`, and `wire` after its redesign.

## Open, all in one place

- How many bits a small int gets.
- How much memory a process heap loses to gaps between freed cells, once processes exist.
- When a read moves a reference instead of copying it.
- When a program ends, and the exit code when `main` returns `Err`.
- From `docs/semantics.md`: `x % 0`, `sqrt(-1.0)`, and float literals too large to represent.
