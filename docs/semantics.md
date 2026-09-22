# Scarlet semantics

This file says what Scarlet programs mean. When the code disagrees with it, one of the two has a bug.

Each rule is marked:

- **Built**: the code does this today.
- **Decided**: agreed, but not built yet. What the code does today is written next to it.
- **Open**: not decided.

## The rule

**Decided.** Code never crashes, and failure is a value. There is no panic, assert or todo, on purpose. The only crash is a real one, like a machine losing power.

- `Option` means a value may be absent, as with `xs[i]` or `map.get`.
- `Result` means an operation can fail. The error is `Nil` when it carries no data, and the stdlib never uses `Option` for failure.
- Supervisors, links and monitors are for deaths that come from outside the code: a killed process, a process hitting a resource limit, a lost machine. They are not for bugs.

Under this rule, every way code can still crash a process is a bug. Today there are two:

- a `receive` on a subject that another process created (`scarlet_vm/src/vm/mailbox.rs`)
- a supervision declaration that the tree refuses (`Crash.Supervision` in `std/scarlet/process.scrl`)

A stdlib function must never make up a value to hide a failure. It returns a `Result` instead.

## Integers

**Built: Int is arbitrary precision.** `+`, `-` and `*` always give the exact answer, so `int.max_value + 1` is `9223372036854775808`. An Int that fits in 48 bits stays inside the value word, and any other moves to the heap. An Int *literal* past 64 bits is still a compile error, because the compiler keeps Int constants as 64-bit numbers.

**Built: bitwise operations are any size.** `int.bitwise_and`, `or`, `xor`, `not` and the shifts treat an Int as an endless row of two's-complement bits, as Erlang does: endless 0s above a non-negative Int, endless 1s above a negative one. No bit falls off an end, so `int.bitwise_shift_left(1, 64)` is `18446744073709551616`, and a right shift divides by a power of 2, rounding down.

**Built: division.**

- `/` truncates toward zero: `-7 / 2 == -3`.
- `x / 0` is `0`, the same rule as Gleam. Division never crashes.
- `%` is the remainder, and it takes the sign of the dividend: `-7 % 2 == -1`.
- `x % 0` is `x`, so `a == b * (a / b) + a % b` holds for every `b`.
- `int.divide(a, b)` and `int.remainder(a, b)` return `Result(Int, Nil)`, with `Err(Nil)` when `b` is 0. Use them when a zero divisor is possible and has to be noticed.

**Open:** whether `x % 0` stays `x` or becomes `0`, as it is in Gleam.

## Floats

**Decided: Erlang-style floats, with no NaN and no infinity.** A Float is an ordinary 64-bit IEEE float, stored inside the value word, so float arithmetic never allocates. That matters for numeric work such as game simulation. No operation ever produces NaN or infinity:

- `x / 0.0` is `0.0`, the same as Int.
- `x % 0.0` is `x`, the same as Int, so `a == b * (a / b) + a % b` holds for Floats too.
- A result too large to represent stops at the largest float, ±1.7976931348623157e308. So order is kept: a product of large numbers still compares greater than `1.0`.
- A result with no answer at all, like `0.0 / 0.0`, is `0.0`.

Erlang crashes in these cases, and Scarlet gives a harmless value instead.

**Built**, all four (`scarlet_vm/src/float.rs`, and `Value::float`, which every Float goes through). `0.0 == -0.0`, as in IEEE. `float.floor`, `ceil`, `round` and `truncate` give the exact Int, however large, since an Int has no bounds.

**Built:** there is no implicit conversion between Int and Float, so `1 + 1.5` is a type error.

**Open:**

- What `sqrt(-1.0)` returns, once maths functions exist.
- A float literal too large to represent is the largest float today, by the rule above. It could become a compile error instead, as an oversized Int literal already is.

## Arrays

**Built: indexing and slicing never crash.**

- `xs[i]` is `Option(a)`: `None` when `i` is negative or past the end.
- `xs[a..b]` is `Result(Array(a), Nil)`: `Ok` of the elements from `a` up to but not including `b`, and `Err(Nil)` when that range is not inside the array, meaning `a` is negative, `b` is past the length, or `a` comes after `b`. `xs[a..a]` is `Ok([])`.

A slice is a `Result` rather than a shorter array, because a range that misses the array is a failure the caller should see, not one to hide by clamping. `binary.slice_bits` follows the same rule. The old VM crashed on an out-of-range slice instead.

## Fields

**Built: reading a field never needs to know the variant.**

- `s.x` works when every variant of `s`'s type has a field `x`, at the same position and of the same type. A type with one variant meets this for all its fields.
- `C(..s, y: 1)` builds a `C` with the fields you name, and fills each field you leave out with `s.field`. So each of those must be a field `s.field` could read. On a type with several variants that means only the fields they all share, and the compiler rejects a spread that leaves out any other, since it cannot know which variant `s` will be.

The old compiler accepted any spread whose base had the same type, and the old VM read the fields by position, so `Circle(..square, r: 1)` could fill a Circle's `x` with the Square's `side`.

## Memory

**Decided: reference counting, with Perceus.** There is no tracing garbage collector.

- Perceus inserts drops and in-place reuse at compile time (`scarlet_core/src/core_ir/perceus.rs`).
- Values are immutable and closures copy what they capture, so values cannot form cycles. That lets reference counting free everything with no cycle collector.

A per-process copying GC was added in `a6bc2aa` (2026-06-02) and replaced by reference counting in `7cf43d7` (2026-06-19), because it was hard to reason about.

**Built:**

- Every process allocates from one shared allocator.
- Constants live in an immortal frozen area that is shared across threads.

**Open:** how to stop one process with runaway memory from ending the whole program. The likely answer is to count memory per process and stop only that process at a limit, since that is a resource death, not a bug.

## Still open

These came out of a design audit and have no answer yet:

- Ordering: `<` works only on Int and Float, and there is no sort.
- Whether `==` should be allowed on functions.
- How values print, for example whether strings inside containers get quoted.
- When a program ends, and what the exit code is when `main` returns `Err`.
- Whether `wire.decode` has to be safe on untrusted input.
