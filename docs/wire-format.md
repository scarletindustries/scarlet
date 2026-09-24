# The wire format

What `scarlet/wire` writes, as the old VM wrote it (format version 3). The new VM has no wire yet. It comes back with the descriptor builder, through `scarlet_ir` (`docs/vm-design.md`). Two things are already decided to change:

- **An `Int` has no size limit now.** The format has to say how an Int past 64 bits is written.
- **A closure names its code, not its run.** It carries its module, its function, and a fingerprint of the module's code, the way BEAM's `NEW_FUN_EXT` carries the module's MD5. So the same code can decode it after a restart. Handles stay tied to their run.

Anything that changes a number a peer already holds bumps the version byte.

## The idea

`encode` and `decode` are type-directed. At each call the compiler knows the concrete type, and it puts a descriptor of that type into the program. So `decode` never guesses from the bytes. `Ok(x)` is a value of exactly the type asked for. Bad bytes are a `DecodeError`, never a wrong-shaped value and never a crash.

Every value encodes, as on BEAM, so `encode` returns a plain `Binary`. The one refusal happens at compile time: a type variable at `decode` that nothing fixes gets "annotate the binding".

Bytes are tied to a type's shape, not its name. Two programs that declare the same shape interoperate. A change to the shape is a `SchemaMismatch`, not a silent misread.

- **Opaque types** from another module cross by their constructors. The owning module re-checks its invariants on what it's handed.
- **Handles** (`Pid`, `Subject`, `Connection`, `TlsConnection`, `net.Server`) cross as an identity: run, kind, number. In the run that made one, it decodes to that same handle, `==` to the original. From another run it's `OtherRun`.
  - A decoded `Subject` is a handle to the mailbox, never a second owner of it.
  - A `Port` is a record over a `Connection` and an `Int`, and crosses as one.
- **Uninhabited types** (a bodiless `pub type` that is none of the handles) are described, not refused. They're a node that is never written or read. It stands only where the walk never goes, like the element of an empty array.

## The fingerprint

This is a 64-bit hash of the type's shape. It travels in every value, and `SchemaMismatch` reports it. A second implementation that follows this section gets the same numbers.

The compiler describes a type as a table of nodes, laid out by a walk from the root. A child is an index into the table, which is how a recursive type stays finite. The hash reads the table in index order, so nothing recurses.

The hash is FNV-1a over 64-bit words:

```
mix(h, v) = (h XOR v) * 0x100000001b3   (mod 2^64)
h0 = mix(0xcbf29ce484222325, VERSION)   VERSION = 3
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

The capture format holds 11, 12 and 13, so the two tables share one number space. The next free number is 15.

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

Bytes from version 1 or 2 are `NotWire` to a version 3 reader.

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
| Closure | run (16 bytes), function index LEB128, capture count LEB128, then the captures |
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
- **A closure** is checked in this order:
  1. Another run is `OtherRun`.
  2. Before any capture is read: the index must name a row of the function table, that row's arity must match the type, and its capture count must match the bytes. Otherwise it's `Malformed`. A call indexes the table without checking, so this has to be caught here.
  3. Then the captures.

## The capture format

The descriptor can't drive a closure's captures. Every lambda of one signature has the same static type, but the captures differ in number and kind. So captures describe themselves, as BEAM's terms do: one tag byte, then that tag's payload. Tags keep their numbers forever and match the fingerprint's node kinds.

| Tag | Kind | Payload |
|---|---|---|
| 1 | Int | zigzag LEB128 (small and boxed ints alike) |
| 2 | Float | 8 bytes |
| 3 | String | length LEB128, UTF-8 bytes |
| 4 | Binary | bit length LEB128, bytes |
| 5 | Array | count LEB128, captures |
| 6 | Map | count LEB128, key/value captures |
| 7 | Tuple | count LEB128, captures (no descriptor carries the arity) |
| 8 | Data | type id zigzag LEB128, variant LEB128, the type's name and constructor's name as Strings, label count and label Strings, field count and field captures |
| 9 | Handle | run, kind, id, as Identity, with any kind allowed |
| 10 | Closure | function index LEB128, capture count LEB128, captures. No run: the outer run already covers it. |
| 11 | Nil | nothing (the runtime's nil word, not the prelude's `Nil`) |
| 12 | Bool | one byte, 0 or 1. Anything else is `Malformed`. |
| 13 | Range | start and end, zigzag LEB128 (the bounds, not the elements) |

Every count is checked at one byte per element before anything is allocated. An unknown tag is `Malformed` at its offset.

**A known gap.** This format checks a capture's form, not whether it fits the body that reads it. The function table records how many words a body captures, but not their kinds. So a forged capture of the wrong kind decodes, and the body's typed instructions then trip over it. The run check is what makes this acceptable: only this run's encoder writes these bytes. Closing the gap needs a capture layout per function, or a capture-shape hash in the closure row.
