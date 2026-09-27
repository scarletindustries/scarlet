# The wire format

What `scarlet/wire` writes. Version 3 is what the old VM wrote; **version 4 is what the new one will write**, and this document describes 4. The new VM has no wire yet — it comes back with the descriptor builder, through `scarlet_ir` (`docs/vm-design.md`).

Anything that changes a number a peer already holds bumps the version byte.

## What version 4 changes, and why

**An `Int` has no size limit now.** The format has to say how an Int past 64 bits is written.

**A closure names its code, not its run.** Version 3 stamped every closure with the 128 random bits of the run that made it, and refused any other run. So a closure could never cross a network — not unsafely, but *at all*.

Version 4 names the code instead: the module, the lambda's own id from source, a fingerprint of the lambda's shape, and a hash of the module's source. A peer holding the same code decodes it. A peer holding different code gets a clear refusal. Handles (`Pid`, a socket) stay tied to their run, because a handle from another machine means nothing.

The name is a **lookup key, never an index**. Three reasons it cannot be "function number 7":

- A function index is minted fresh each compile.
- Scarlet may specialise generic functions later (see `docs/vm-design.md` on a C backend). Then which functions exist depends on how the whole program used them, so two peers built from the same source can hold different sets.
- A C backend has no runtime function table to index at all. Wire needs a static table of `(module, lambda id, shape fingerprint) → function`, emitted at build time.

A combination the peer never built is `NoSuchCode`. That is honest: you cannot call an instantiation nobody compiled.

**Captures are checked against a descriptor.** Version 3 checked a capture's *form* — one tag byte says "I am an Int" — and nothing checked that the body reading it expected an Int. The run check was what made that safe: only your own encoder ever wrote those bytes. Take the run check away and a peer can hand you captures of the wrong kinds, and the body then does `IntAdd` on a string.

So each function carries a **capture descriptor**: the static type of each capture slot. Captures decode against it like any other value, which closes the hole and drops the tag bytes.

A capture whose static type is a type variable has nothing concrete to check against, and gets a wildcard node. That is sound: a body cannot do anything type-specific with a value of type `a`, so any well-formed value is safe there. Parametricity does the work. If Scarlet ever specialises generics, or passes type descriptors at runtime, those wildcards go away and the check becomes exact.

**What this does not change.** Decoding a closure means running your own code, with captures the sender chose. That is a trust decision, not a memory-safety one — the same one BEAM makes and answers with a shared cookie and a trusted cluster. Worth deciding on purpose rather than by accident when there is a real network stack.

## The idea

`encode` and `decode` are type-directed. At each call the compiler knows the concrete type, and it puts a descriptor of that type into the program. So `decode` never guesses from the bytes. `Ok(x)` is a value of exactly the type asked for. Bad bytes are a `DecodeError`, never a wrong-shaped value and never a crash.

Every value encodes, as on BEAM, so `encode` returns a plain `Binary`. The one refusal happens at compile time: a type variable at `decode` that nothing fixes gets "annotate the binding".

Bytes are tied to a type's shape, not its name. Two programs that declare the same shape interoperate. A change to the shape is a `SchemaMismatch`, not a silent misread.

- **Opaque types** from another module cross by their constructors. The owning module re-checks its invariants on what it's handed.
- **Handles** (`Pid`, `Subject`, `Connection`, `TlsConnection`, `net.Server`) cross as an identity: run, kind, number. In the run that made one, it decodes to that same handle, `==` to the original. From another run it's `OtherRun` — a handle from another machine names nothing here.
- **Closures** cross as the *name of their code*, so a peer holding the same code can run them. See "Closures" below.
  - A decoded `Subject` is a handle to the mailbox, never a second owner of it.
  - A `Port` is a record over a `Connection` and an `Int`, and crosses as one.
- **Uninhabited types** (a bodiless `pub type` that is none of the handles) are described, not refused. They're a node that is never written or read. It stands only where the walk never goes, like the element of an empty array.

## The fingerprint

This is a 64-bit hash of the type's shape. It travels in every value, and `SchemaMismatch` reports it. A second implementation that follows this section gets the same numbers.

The compiler describes a type as a table of nodes, laid out by a walk from the root. A child is an index into the table, which is how a recursive type stays finite. The hash reads the table in index order, so nothing recurses.

The hash is FNV-1a over 64-bit words:

```
mix(h, v) = (h XOR v) * 0x100000001b3   (mod 2^64)
h0 = mix(0xcbf29ce484222325, VERSION)   VERSION = 4
```

Mix in the node count, then the root's index, then each node in order. A name is mixed as its length, then its UTF-8 bytes one per mix. The length goes first, so `('ab', 'c')` and `('a', 'bc')` don't collide.

Each node mixes its kind tag first, then:

| Tag | Kind | Then |
|---|---|---|
| 1 | Int | — |
| 2 | Float | — |
| 3 | String | — |
| 4 | Binary | — |
| 5 | Array | element index |
| 6 | Map | key index, value index |
| 7 | Tuple | count, each index |
| 8 | Data | constructor count, then per constructor in declared order: tag, name, field count, then per field its label and type index |
| 9 | Identity | handle kind, type-argument count, each argument's index |
| 10 | Closure | parameter count, each parameter's index, return index |
| 14 | Uninhabited | — |
| 15 | Any | — (a capture whose static type is a type variable) |

11, 12 and 13 were the old capture tags, kept reserved so no number is ever reused. The next free number is 16.

**Included**, so a change to any of these is a schema change: node kinds, constructor order, constructor names, field labels, field types, a handle's kind and type arguments, and a function's parameter and return types.

**Excluded**: the type's own name and its module. Renaming a type or moving it leaves every peer talking.

### Handle kinds

One byte each. A kind keeps its number forever, and a new one takes the next free number. It is not a type id, which is made fresh each compile.

| Kind | Byte | What |
|---|---|---|
| Connection | 0 | a TCP stream (`net/socket.Connection`) |
| Listener | 1 | a listening socket (`net.Server`) |
| Port | 2 | a child process's stdio (`os/port.Port`'s stream) |
| Tls | 3 | an encrypted stream (`net/tls.TlsConnection`) |
| Pid | 4 | a process (`process.Pid`) |
| Subject | 5 | a mailbox (`process.Subject`) |

The descriptor names the static type's kind. The byte on the wire is the value's kind. So a Port's stream writes 2 where the descriptor says 0, and a decoder accepts that pair. No other pair is accepted: Tls under Connection is `Malformed`.

### Versions

1. Kind tags 1 to 8, no identity row. Written by canaries 0.0.1-canary.20260820.2050 to 0.0.1-canary.20260822.2154.
2. Adds Identity 9.
3. Adds Closure 10 and the capture format. Uninhabited 14 and typed `Bool` were added later without a bump. They only gave bytes to types that used to be refused, so no number a peer held moved.
4. An Int past 64 bits. A closure names its code rather than its run, and its captures decode against a descriptor rather than carrying their own tags. See "What version 4 changes".

Bytes from any earlier version are `NotWire` to a version 4 reader.

## The bytes

Little-endian. There are no per-value type tags: the descriptor gives the structure, so the bytes hold only what it can't.

```
value := "SW" . version u8 . fingerprint u64 . body
```

- A wrong magic, or a version this runtime can't read, is `NotWire`.
- A fingerprint that doesn't match the reader's is `SchemaMismatch(expected, found)`.

The body follows the root node's kind:

| Kind | Bytes |
|---|---|
| Int | zigzag LEB128 (64-bit in version 3) |
| Float | 8 bytes, the f64 bits |
| String | length LEB128, then UTF-8 bytes |
| Binary | bit length LEB128, then ceil(bits / 8) bytes |
| Array | count LEB128, then the elements |
| Map | count LEB128, then key/value pairs |
| Tuple | the elements (the arity comes from the descriptor) |
| Data | variant LEB128, then its fields. The variant is left out when there's only one, so a record costs only its fields and `Nil` costs nothing. |
| Identity | run (16 bytes), kind u8, id LEB128 |
| Closure | the code's name, then its captures (below) |
| Uninhabited | nothing |

`run` is 128 random bits from the OS, made once per run, written most significant byte first. A socket's id is its 32-bit id, zero-extended.

### Refusals

- A count is checked against the bytes left before anything is allocated. A huge count on short input is `Malformed(offset, what)`, never an allocation. An uninhabited element counts as one byte here.
- Input too short for the value is `Truncated`.
- A whole value followed by more bytes is `TrailingBytes(count)`. A caller that frames its own messages has lost sync.
- **An identity** is checked in this order:
  1. A kind the descriptor doesn't allow is `Malformed`.
  2. Another run is `OtherRun(expected, found)`, the reader's own first. `SchemaMismatch` can't say this, since two runs of one binary share a fingerprint.
  3. An id too big for the handle (past 2^48 for a pid or mailbox, past 2^32 for a socket) is `Malformed`.
- **A closure** is checked in this order, all of it before any capture is read, because a call reaches its code without re-checking:
  1. The module hash must match the module of that name. Otherwise `NoSuchCode(module)` — the peer has different code.
  2. `(module, lambda id, shape fingerprint)` must name a function the peer built. Otherwise `NoSuchCode`.
  3. That function's arity must match the type, and its capture count the descriptor's. Otherwise `Malformed`.
  4. Then the captures, against the capture descriptor.

## Closures

A closure names its code, then carries its captures.

```
closure := module . lambda id . shape fingerprint u64 . module hash u64 . captures
```

- **module** — the module path, as a String.
- **lambda id** — which lambda within that module, from the source, not from the function table's order.
- **shape fingerprint** — the hash of this lambda's full shape: its signature *and* its capture types. Under specialisation two instantiations differ here, so each names itself apart.
- **module hash** — over the module's *source*, not its compiled output, so two peers with different specialisation sets still agree they have the same code.

The three together are a lookup key into the peer's own table. A key it does not hold is `NoSuchCode`.

### Why the hash is per module

Lambdas are numbered in source order, so adding one near the top of a file shifts every later one. The module hash is what keeps that safe: the shift can only happen if the module changed, which changes the hash, so a peer holding the old code refuses the whole closure rather than reading the wrong number.

Per *function* hashing would be gentler — edit `f` and closures from `g` keep working — but per module is the one that fits how code is meant to be loaded. Reloading is a module at a time, so "do we have the same code" is a per-module question. BEAM answers it the same way, and keeps two versions of a module live: closures made before a reload keep running on the old code until it is purged, and the hash is what tells the two generations apart.

### The captures

Each function's row carries a **capture descriptor**: the static type of each capture slot, in order. The captures are written against it, exactly as any other value is written against a descriptor. No tag bytes, no counts beyond the descriptor's own.

A capture whose static type is a type variable gets the `Any` node (15). It is the one place a value describes itself — one tag byte, then that tag's payload, using the same numbers as the fingerprint's node kinds. A body cannot do anything type-specific with a value of type `a`, so any well-formed value is safe there.

Reserved forever, from version 3's capture tags: 11 Nil, 12 Bool, 13 Range. They are not reused.

**What this fixed.** Version 3 checked a capture's form and not its type; the body reading it could be handed an Int where it wanted a String. The run stamp was what made that safe, and version 4 drops the run stamp, so the check had to become real.
