# The new VM

This is the plan for the VM that runs a compiled Scarlet program. None of it is built yet. It lands one small PR at a time, in the order at the bottom.

Each point is marked:

- **Decided**: you chose it. Most of these are also in `docs/semantics.md`.
- **Proposed**: my recommendation. It needs your yes before code depends on it.
- **Open**: not decided.

## What the VM is given

The compiler hands over one `core_ir::Program` (`crates/scarlet_core/src/core_ir/mod.rs`):

| Field | What it is |
|---|---|
| `fns` | Every function, numbered by `FuncIdx`: declared ones, lambdas, and the wrappers the compiler writes when a constructor or built-in is used as a value. |
| `consts` | Every constant, numbered by `ConstId`: Int, Float, String and Binary. |
| `inits` | Each imported module's toplevel, in the order they must run. The prelude comes first. |
| `toplevel` | The entry file's own toplevel. |
| `main` | The function the program starts at. It's `None` for a REPL entry, which runs its toplevel and prints the value. |
| `globals` | How many module-level slots the program needs. Toplevels write them, and functions read them with `Load::Global`. |

Each function is a `LoweredFn`: its `module`, its source `name`, its `core` body, and the `pool` that the types in the body are numbered in.

A body is already in A-normal form: every operand is a local, and nested expressions have been flattened into a chain of `Let`s. The whole language comes down to these pieces:

- **Values** (`Atom`): a local, a constant, a global or capture, `nil`, a bool, a constructor, a closure, a primitive op (`PrimOp`, like `IntAdd`), a built-in call (`Intrinsic`, one of 135), and a call. A call names its target as a known function, the function itself, or a closure held in a local.
- **Control** (`CoreExpr`): `Let`, `If`, `Match`, `Tail` (return a value or tail-call), `LetJoin` (a branch whose value is used), `LetCont` and `Goto` (a shared "try the next case" point for pattern matching), and `Drop`.
- **Memory hints from Perceus**: `Drop x` at the last use of `x`, and `Ctor { reuse: Some(x) }` when a new constructor may overwrite `x`'s cell in place.

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

This matters because Perceus decides what to drop by type, and it leaves some types alone on purpose: `Int`, `Float`, `String`, and generic type variables (`ResolvedPool::is_heap`). Under this split those are still freed, just at frame exit. A big `Int` is a heap object Perceus never drops, and it's still correct.

**Open:** when a read may *move* a reference instead of copying it, for example a local's last use as a call argument. That's an optimisation, but it's also what lets reuse fire in the callee. It has to be written down against `perceus.rs` before the VM does it.

**Proposed: constants are frozen.** They live in an area that is never freed, and they're shared across threads with no counting, as in the old VM.

**Proposed: freeing a big value is a loop, and can be paused.** Dropping a million-item list must not freeze a scheduler thread. The audit measured a 3.4 s freeze on a large `==`.

**Decided: a heap per process.** Each process allocates from its own heap, and reference counting runs inside it. This is BEAM's shape, with counting where BEAM has a GC.

- **Why it works:** messages are copied when sent, so every object in a process's heap is reachable only from that process. Only the owner ever allocates or frees in it, so it needs no locks and no atomic counts.
- **A limit is just the heap's size.** A process that passes its limit stops, and that's a resource death, not a bug.
- **Death frees everything at once:** the heap's memory is returned in whole chunks, with no walk over the objects in it.

**Proposed: how.**

- The heap is plain data owned by the process: chunks of memory, carved into cells by size, with a free list per size. So it moves with the process when the process moves to another scheduler thread. The old concern, that allocators like mimalloc tie a heap to one thread, doesn't apply to a heap we own.
- A few things must live outside any one process's heap, because more than one process can hold them:
  - constants, in the frozen area;
  - big binaries, which are shared on send rather than copied, as in the old VM, and counted with atomic counts;
  - handles to OS resources, like sockets and files.
- A process keeps a list of the outside things its heap points to. When it dies, it releases each of them before returning its chunks. This is BEAM's "off-heap" list.

**Open:** how big a heap's first chunk is, and how a heap grows. A million tiny processes each with a large first chunk would waste a lot of memory.

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

Each step is one PR or a few. Each PR removes the `#[ignore]` from exactly the tests it makes pass, so `cargo test -p scarlet -- --ignored` counts what's left. It's 321 today.

1. The `scarlet_ir` crate.
2. A VM that runs `pub fn main() { println(1 + 2) }`: the value word with small ints only, `IntAdd`, calls, `Println`. `scarlet run` uses it. The `hello` golden passes.
3. Control flow and data: `If`, `Match`, `LetJoin`, `LetCont`/`Goto`, constructors, tuples and fields.
4. Closures, captures, globals, and module inits.
5. The per-process heap and reference counting, then Perceus's `Drop` and reuse. The allocation-count tests come back here.
6. Big ints.
7. Floats with the no-NaN rule.
8. Strings, binaries (with binary patterns), arrays and maps.
9. The rest of the intrinsics, one stdlib module at a time.
10. Processes: mailboxes, the scheduler, preemption, then links, monitors and supervisors.
11. IO (files, sockets, TLS, HTTP), `os`, and `wire` after its redesign.

## Open, all in one place

- How many bits a small int gets.
- How big a process heap starts, and how it grows.
- When a read moves a reference instead of copying it.
- When a program ends, and the exit code when `main` returns `Err`.
- From `docs/semantics.md`: `x % 0`, `sqrt(-1.0)`, and float literals too large to represent.
