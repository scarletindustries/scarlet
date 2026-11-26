# Scarlet semantics

This file says what Scarlet programs mean. When the code disagrees with it, one of the two has a bug.

Each rule is marked:

- **Built**: the code does this today.
- **Decided**: agreed, but not built yet. What the code does today is written next to it.
- **Open**: not decided.

## The rule

**Decided.** Code never crashes, and failure is a value. There is no panic, assert or todo, on purpose. The only crash is a real one, like a machine losing power.

- `Option` means a value may be absent, as with `xs[i]` or `map::get`.
- `Result` means an operation can fail. The error is `Nil` when it carries no data, and the stdlib never uses `Option` for failure.
- Supervisors, links and monitors are for deaths that come from outside the code: a killed process, a process hitting a resource limit, a lost machine. They are not for bugs.

Under this rule, every way code can still crash a process is a bug. The stdlib's `Crash` type in `std/scarlet/process.sca` names two of them:

- `ForeignReceive`, a `receive` on a subject that another process created
- `Supervision`, a supervision declaration that the tree refuses

The new VM doesn't run processes yet, so neither can happen today.

A stdlib function must never make up a value to hide a failure. It returns a `Result` instead.

## Integers

**Built: Int is arbitrary precision.** `+`, `-` and `*` always give the exact answer, so `int::max_value + 1` is `9223372036854775808`. An Int that fits in 51 bits is stored inside the value word, and any larger one goes on the heap. A program cannot tell the two apart. An Int *literal* past 64 bits is still a compile error, because the compiler keeps Int constants as 64-bit numbers.

**Built: bitwise operations are any size.** `int::bitwise_and`, `or`, `xor`, `not` and the shifts treat an Int as an endless row of two's-complement bits, as Erlang does: endless 0s above a non-negative Int, endless 1s above a negative one. No bit falls off an end, so `int::bitwise_shift_left(1, 64)` is `18446744073709551616`, and a right shift divides by a power of 2, rounding down.

**Built: division.**

- `/` truncates toward zero: `-7 / 2 == -3`.
- `x / 0` is `0`, the same rule as Gleam. Division never crashes.
- `%` is the remainder, and it takes the sign of the dividend: `-7 % 2 == -1`.
- `x % 0` is `x`, so `a == b * (a / b) + a % b` holds for every `b`.
- `int::divide(a, b)` and `int::remainder(a, b)` return `Result(Int, Nil)`, with `Err(Nil)` when `b` is 0. Use them when a zero divisor is possible and has to be noticed.

**Open:** whether `x % 0` stays `x` or becomes `0`, as it is in Gleam.

## Floats

**Decided: Erlang-style floats, with no NaN and no infinity.** A Float is an ordinary 64-bit IEEE float, stored inside the value word, so float arithmetic never allocates. That matters for numeric work such as game simulation. No operation ever produces NaN or infinity:

- `x / 0.0` is `0.0`, the same as Int.
- `x % 0.0` is `x`, the same as Int, so `a == b * (a / b) + a % b` holds for Floats too.
- A result too large to represent stops at the largest float, ±1.7976931348623157e308. So order is kept: a product of large numbers still compares greater than `1.0`.
- A result with no answer at all, like `0.0 / 0.0`, is `0.0`.

Erlang crashes in these cases, and Scarlet gives a harmless value instead.

**Built**, all four (`scarlet_vm/src/float.rs`, and `Value::finite` and `Value::float`, one of which every Float goes through). `0.0 == -0.0`, as in IEEE. `float::floor`, `ceil`, `round` and `truncate` give the exact Int, however large, since an Int has no bounds.

**Built:** there is no implicit conversion between Int and Float, so `1 + 1.5` is a type error.

**Open:**

- What `sqrt(-1.0)` returns, once maths functions exist.
- A float literal too large to represent is the largest float today, by the rule above. It could become a compile error instead, as an oversized Int literal already is.

## Arrays

**Built: indexing and slicing never crash.**

- `xs[i]` is `Option(a)`: `None` when `i` is negative or past the end.
- `xs[a..b]` is `Result(Array(a), Nil)`: `Ok` of the elements from `a` up to but not including `b`, and `Err(Nil)` when that range is not inside the array, meaning `a` is negative, `b` is past the length, or `a` comes after `b`. `xs[a..a]` is `Ok([])`.

A slice is a `Result` rather than a shorter array, because a range that misses the array is a failure the caller should see, not one to hide by clamping. `binary::slice_bits` follows the same rule. The old VM crashed on an out-of-range slice instead.

**Built: a write at an index never crashes, and comes in two forms.**

- `array::set(xs, i, x)` is `Result(Array(a), Nil)`: `Ok` of the array with `x` at index `i`, and `Err(Nil)` when `i` is negative or past the end. It is the form that says when the write did not happen, by the stdlib rule that an operation that can fail returns a `Result`.
- `array::put(xs, i, x)` is `Array(a)`: the same write, and the array as it was when `i` is negative or past the end. It is for a caller that knows the index is inside, or does not mind. Elm's `Array.set` has the same rule.
- `array::swap(xs, i, j)` changes two elements over, and gives the array as it was when either index is outside it.

None of them changes an array anything else can see. `xs` after `ys = array::put(xs, 0, 9)` is what it was. What differs is the cost. When the write is the last thing to read `xs` and nothing else holds that array, the VM writes the one slot in place and copies nothing. Otherwise it copies the nodes on the way to the slot, at most one a level, and shares the rest. An index outside the array costs a length check and nothing else.

A fallback that gives `xs` back is no such read. `match array::set(xs, i, x) { Ok(ys) -> ys  Err(Nil) -> xs }`, `array::set(xs, i, x) or xs` and `array::put(xs, i, x)` are the same write, and each is in place when nothing else holds the array, because the fallback runs only when nothing was written. It is the same when the `Result` has a name that nothing but the `match` straight after it reads. A `Result` that is kept, or passed to a function such as `result::then`, is made before anything looks at it, and what reads `xs` after that keeps the array.

`[h, ..t]` takes over an array that nothing else holds, and does not copy it whole. It is so behind a guard that does not read `t`, and when the array was a part of a tuple that the match took apart. A guard that reads `t` has it cut off before it runs. Where a later arm reads the array as well, the array is still held then, and cutting it off makes a few new cells and shares the rest. `[x, ..xs]` and `[..xs, x]` add to an array in the same way.

A `match` on `xs[i]`, on a slice, on `array::set` or on `map::get`, or an `or` after one, makes no `Option` and no `Result`, unless an arm has a name for the whole of it. One that nothing reads, such as `_ = array::set(xs, i, x)`, is not made either, and the array is not written. The default of an `or` runs only when there is nothing.

## Matches

**Built: arms are tried in the order written.** The first arm whose pattern fits, and whose guard says `True`, is the one that runs. What the compiler asks of the value first is its own business, because asking cannot be seen. What can be seen keeps its order:

- A guard runs once its whole pattern has fitted, at most once for each way its pattern can fit, and before anything of a later arm runs.
- What an arm's names are for is read before its guard runs, and what nothing in the match reads after that is given up before the guard runs too. The one thing that waits for the guard is the tail of `[h, ..t]`, when the guard does not read it and one of the next sixteen arms reads the array. Taking a tail is taking over the array, and a guard that fails leaves the array to the arms behind it.
- An or-pattern that binds names tries its alternatives in the order written, and each one with the rest of the pattern and the guard. In `(x, _) | (_, x) if x > 5`, the guard may run twice. An or-pattern that binds none goes on with the first alternative that fits, and the guard runs once.
- The size of a segment in a `<<>>` pattern is an expression. It runs when the segments in front of it have fitted, and what is behind it in its arm is asked after it.

## Fields

**Built: a field is found by its name.**

- `s.x` works when every variant of `s`'s type has a field named `x` of the same type. Where `x` sits in each variant does not matter, so reordering a variant's fields never breaks a `.x`. A type with one variant meets this for all its fields.
- `C(..s, y: 1)` builds a `C` with the fields you name, and fills each field you leave out with `s.field`. So each of those must be a field `s.field` could read. On a type with several variants that means only the fields they all have, and the compiler rejects a spread that leaves out any other, since it cannot know which variant `s` will be.

When every variant holds `x` in the same slot, `s.x` is one read. When they don't, the compiler writes the `match` a person would, one arm per variant.

**Built: a record update runs in the order written, and gives the old record up as soon as it has read it.** The base and the fields you name run in the order they are written, so `C(y: f(), ..g())` calls `f` and then `g`. The fields the update keeps are read as soon as the base has run, before any field written after it. So the old record's last use is that read, or the last field written after the base that reads it, and it is given up there and not when the new record is made. In `State(..s, v: array::put(s.v, i, x))` the read of `s.v` is the last use of `s`, so `s` is given up before the write. When nothing else holds that record, the write finds the array with one holder and changes it in place, and a `map::set` on a map the record held does the same. A base with nothing to run counts as written first, so it does not matter where its spread is written. That is a name, or a field or tuple element read off one: `..s`, `..o.inner`, `..t.0`.

Where nobody can tell the difference, the compiler holds the old record until the new fields are computed and writes them into it, when nothing else holds it (`scarlet_lower/src/update.rs`). Nobody can when each field that is set holds only numbers, or a value with no fields, because then nothing that can be written in place or closed is held for longer. And nobody can when computing the new fields is a few steps however large the values are: constants, names, field reads, Float arithmetic, and constructors, tuples, arrays and choices made of those. An Int may be big, so its arithmetic is not one of them. `Held(..h, conn: None)` closes the old connection as it makes the new record.

A program can see the old record being given up in one case, which is when it held the last reference to a handle such as a listener or a connection. The handle is then closed before the later fields run. In `Held(..h, conn: redial(h.port))` the connection `h` held is closed before `redial` runs. To keep it open until the new one is made, make the new one first: `next = redial(h.port)` and then `Held(..h, conn: next)`.

It is the same wherever the update is, in a branch of an `if` or `match` too.

Some spellings still copy. A field written after the write that reads the base, as `count: s.count + 1` does, keeps the base until then, so put such a field before the write. A base that has to run and is written after the write, as in `State(v: array::put(s.v, i, x), ..f(s))`, runs after it, so the write finds `s` still held. Those two are the order you wrote.

The old compiler accepted any spread whose base had the same type, and the old VM read the fields by position, so `Circle(..square, r: 1)` could fill a Circle's `x` with the Square's `side`. And `s.x` was refused when the variants held `x` in different slots.

## Maps

**Built: a map's order depends only on what it holds.** `map::keys`, `map::values`, `map::to_list`, `map::fold` and printing list the entries of two maps that are `==` in the same order, however each was built, and in the same order on every run and every machine. The order follows the keys' hashes, so it looks shuffled, and no particular order is promised. The one exception is keys whose whole 64-bit hashes are equal, which a map keeps in the order they went in.

**Built:** any value can be a key, and a key is found by any value `==` to it: `0..3` finds `[0, 1, 2]`, and a map finds an equal map built in another order.

## Functions

A function takes at most 250 parameters and a call passes at most 250 arguments. The compiler refuses a function or a lambda with more, a function type with more, a call with more, and a constructor of more than 250 fields read as a function. Building such a constructor is not a call, and is allowed. The VM cannot yet run a function that holds more than 254 values at once, counting its parameters: a run stops when one is called (step 48 of `docs/perf-plan.md`). Building a record of more than 253 fields needs that many, and so does an update of one that has to read each field it keeps. A long array, interpolation or binary literal does not, nor does an array literal with a spread in it.

## Memory

**Decided: reference counting, with Perceus.** There is no tracing garbage collector.

- Perceus inserts drops at compile time (`scarlet_lower/src/perceus.rs`).
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
- Whether `wire::decode` has to be safe on untrusted input.
