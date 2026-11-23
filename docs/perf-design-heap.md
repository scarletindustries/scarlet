# Speed design: The heap and values: cell addressing, allocation and freeing, retain and release, the size of a value, tuples and closures as cells

This is the design for one area of the speed work, written on 23 November 2026. `docs/perf-plan.md` puts its steps in order with the other areas' steps, and settles where the areas disagree. Where this file and the plan differ, the plan is right.

# The heap to build

## What changes, in one paragraph

A process's heap becomes one `Vec<u64>`. A cell is named by the index of its header word, so reaching a cell costs one bounds check. One header word holds the count, how many value words follow, the cell's size and its class. A new cell comes from its size's free list, and when that list is empty it is appended to the arena. Freed cells are threaded through themselves, and a cell that dies is queued through its own count field. A reference to a constructor cell carries the variant, so a `match` test reads no memory. Constructors, tuples and closures are filled straight from the registers. A match that consumes its value is one `Unpack` instruction. Perceus's reuse tokens leave the VM. A rebuild that keeps the variant becomes `Take` (move a field out of a unique cell) and `Update` (write the changed fields in place).

## What is now measured, and what is still arithmetic

I ported the first four steps into a copy of the real VM (`perf-scratch/heap-final/vm`), built it outside the repository, and ran all 23 benchmarks. Every one printed its `expected.txt`. These are instructions retired against today's VM:

| step | geomean factor | largest effects |
|---|---:|---|
| 1. flat arena | 0.915 | richards 0.862, sieve 0.872, fannkuch 0.882, binary_trees 0.915 |
| 2. lists through the cells, signed count, inline dup and drop, free loop | 0.928 | richards 0.887, towers 0.890, bounce 0.901, binary_trees 0.909 |
| 3. cells filled from the registers | 0.992 | bounce 0.870, json 0.945, cfold 0.981 |
| 4. `Unpack`, tokens retired | 0.981 | cfold 0.899, list 0.914, binary_trees 0.918, json 0.953 |
| all four | **0.826** | 6.17 becomes 5.10. binary_trees 0.752, cfold 0.728, bounce 0.702 |

The new header, the variant in the reference, the fixed-arity instruction forms, `Take`, `Update`, stubs and frozen constants are not in that port. Their numbers come from a toy interpreter and are marked as such.

## Data layouts

### The value word (`value.rs`)

```
reference:  0xFFFC | variant (bits 32..48) | index (bits 0..31)
nullary:    0xFFFD | variant (bits 32..48) | type  (bits 0..32)     unchanged
```

`variant` is the constructor's variant for a constructor cell and 0 for every other cell. "Is this value variant k" is one compare of the top 32 bits with a constant the loader builds: `0xFFFC_0000 | k` when variant k has fields, `0xFFFD_0000 | k` when it has none.

```rust
pub(crate) struct Cell(u64);   // the whole 48-bit payload: variant << 32 | index. Index is never 0.

impl Cell  { fn index(self) -> usize;  fn variant(self) -> u16; }
impl Value {
    fn cell(c: Cell) -> Value;            // tag | payload
    fn as_cell(self) -> Option<Cell>;     // the payload back, variant and all
    fn top(self) -> u32;                  // bits 32..64, what JumpUnlessTag compares
}
```

`Value::cell(v.as_cell())` gives back the same word for every cell. Only `heap.rs` can make a `Cell`, and its constructor functions return one with the variant already in it. So none of today's 116 `Value::cell(` sites can lose a variant.

Tag `0xFFFE` stays unused. If the processes work ever wants a shared, read-only constant arena, that is its tag. `as_cell` is false for it, so dup, drop and `unique` need no change.

### The header word

```
bits  0..32  count, as an i32
bits 32..40  scan: how many value words follow the header
bits 40..48  size: the cell's words, header included, 2 to 254. 0 marks a stub.
bits 48..64  class: 0 free, 1 to 15 the built-in kinds, 16 + n constructor n
```

- **Count** has one meaning per range:
  - above 0: the number of holders. New cells start at 1 and unique is `count == 1`.
  - 0: a free cell.
  - `i32::MIN`: never counted. A frozen constant starts there, and a count that overflows lands there and stays.
  - any other negative value: a cell waiting to be freed. The low 31 bits are the next such cell.

  Dup adds when the count is above 0. Drop subtracts when it is above 1 and frees at 1. So no count at or below zero ever changes.
- **Scan.** The first `scan` words after the header are values and nothing else is read as one. `scan = 255` means "this kind still says which words are values" (`held()`), and only array roots, array branches and the three map kinds use it, until the array and map rewrites move their values first.
- **Class.** The loader interns every constructor that has fields into a table that only grows, and the class is 16 plus its row. A row holds the `TypeId`, the variant and the field count. `show`, `==`, `hash`, `json` and `http` read it. More than 65,520 such constructors is a typed load error.
- **A free cell's header** is `size << 40`.

The top 32 bits of a header are the cell's shape. The loader builds one `Shape { top: u32, variant: u16 }` per constructor, tuple arity and closure arity.

### Cells by kind

| kind | words after the header | scan |
|---|---|---|
| Ctor | the fields. The tag word is gone: `Node(l, r)` is 3 words. | field count |
| Tuple | the elements. `()` is one unused word. | element count |
| Closure | the function as a `Value::func` word, then the captures | 1 + captures |
| ArrayLeaf | the items | item count |
| BinarySlice | the owner, then start and length | 1 |
| ArrayRoot, ArrayBranch, Map, MapNode, MapCollision | as today | 255 |
| String, Binary, BigInt, Range | raw | 0 |

A raw cell's size is rounded up to one of a few sizes: exact to 8 words, then four sizes to each doubling (10, 12, 14, 16, 20, 24, 28, 32, 40 and so on to 224). Its own length word says where its bytes end. Value cells keep their exact size.

### The heap (`heap.rs`)

```rust
pub(crate) struct Heap {
    words: Vec<u64>,        // the arena. Its length is how much is carved. words[0] is never a cell.
    free: Vec<u32>,         // free[n]: the first free cell of n words, or 0. Grown when a size is first freed.
    outside: Vec<Outside>,  // what stubs name
    spare: u32,
    boxed: usize,           // words held in boxes, counted against the limit
    consts: Vec<u32>,       // the frozen cell of each cell constant, or 0
    carved: u64,            // cells appended, for tests
    reused: u64,            // times Unpack, Take or Update found one holder, for tests
    stale: u32,             // a free cell something tried to count, or 0
    released: Vec<u64>,
}

enum Outside {
    Empty { next: u32 },
    Words { scan: usize, words: Box<[u64]> },   // a cell too long for the arena
    Handle(u64),                                 // the host's number for a socket or session
}
```

- **Arena.** An index stays valid when the `Vec` grows. The arena stops at 2^31 words (16 GiB), so an index fits in 31 bits and a dying cell's link can carry the sign bit.
- **Allocation is "pop, or append".** There are no runs to carve and no tuning constants. The spare capacity of the `Vec` is the memory not yet carved, and it is never touched.
- **Stubs.** A cell over 254 words lives in a `Box<[u64]>` in `outside`. Its arena cell is two words: a header with size 0 and scan 0, and the slot. A host handle is a stub too. The count is in the arena header, so dup, drop and `unique` have one path. The arena cell is taken before the box is built, and the box is built with `try_reserve_exact`.
- **Constants.** A string, binary or big Int constant is made once per process, the first time that process reads it, as an ordinary cell with count `i32::MIN`. `consts` remembers it. So there is one arena and one cell tag, and a literal stops allocating on every use.

## The operations

**Make a cell** (two values; one and three are the same):

```rust
fn con2(&mut self, shape: Shape, x: u64, y: u64) -> Result<Cell, Full> {
    let at = self.free.get(3).copied().unwrap_or(0) as usize;
    if at == 0 { return self.append(shape, &[fresh(shape.top), x, y]); }   // out of line
    if let Some([h, p, q]) = self.words.get_mut(at..at + 3) {
        if let Some(head) = self.free.get_mut(3) { *head = *p as u32; }     // p is the link
        *h = fresh(shape.top); *p = x; *q = y;
    }
    Ok(Cell::new(at, shape.variant))
}
```

`append` checks the capacity, calls `try_reserve` when short (cold, and where the heap limit is checked), and then `extend_from_slice`. The size comes from the shape and nowhere else. `append` refuses a slice whose length is not `size(shape)`.

**dup** is 3 instructions for a non-cell and 12 for a cell. **drop** is 3 for a non-cell and 13 for a cell that stays alive. Both are inline.

```rust
fn drop(&mut self, v: Value) {
    let Some(c) = v.as_cell() else { return };
    if let Some(h) = self.words.get_mut(c.index()) {
        let n = count(*h);
        if n > 1 { *h -= 1 } else { self.last(c.index(), n) }   // out of line
    }
}
fn last(&mut self, at: usize, n: i32) {
    if n == 1 { self.free(at) } else if n == 0 { self.stale = at as u32 }
}
```

**free** is a loop with no side storage, as in Lean's `lean_del_core`:

```rust
fn free(&mut self, first: usize) {
    let (mut at, mut todo) = (first, 0u32);
    loop {
        let Some(&h) = self.words.get(at) else { return };
        if size(h) == 0 {
            todo = self.free_stub(at, todo);                 // cold
        } else {
            for i in values(h, at) {                         // at+1 .. at+1+scan, or held() when scan is 255
                let Some(&w) = self.words.get(i) else { break };
                todo = self.let_go(w, todo);
            }
            self.shelve(at, size(h));                        // header = size << 40, word 1 = old head, head = at
        }
        if todo == 0 { return }
        at = todo as usize;
        todo = self.words.get(at).map_or(0, |h| *h as u32 & 0x7FFF_FFFF);
    }
}
```

`let_go` decrements a child, or, when that was its last reference, writes `0x8000_0000 | todo` into the child's count field and makes the child the new `todo`.

**field** reads a value with no count change, and the handler dups it. It is 16 instructions. The test `i < scan(h)` refuses a freed cell and a stub in one compare, and then falls to a cold function that looks in the box or returns the typed `Broken::NoField`.

**unpack** takes a cell apart and gives up the reference:

```rust
Instr::Unpack2 { src, top, a, b } => {
    let v = self.take(base, src);                        // 1. the scrutinee leaves its register
    let (x, y) = self.heap.unpack2(v, top, wanted(a, b))?;   // 2. the heap hands the values over
    if a != NONE { self.set(base, a, x) }                // 3. each is written like any other value
    if b != NONE { self.set(base, b, y) }
}
```

`heap.unpack2` checks `header >> 32 == top`, which is the constructor, the size and the field count in one compare. Then it reads the count once.
- One holder: the values move out with no count change and the cell is shelved. A value nothing wants is dropped.
- Otherwise: each wanted value is dup'd and the cell loses a holder.

The rule that makes this safe is in two parts. The scrutinee leaves its register before any destination is written. The cell is not let go until every value has been read out. So a destination may be the scrutinee's own register, two destinations may be one register, and a destination's old value may be the cell's only other holder. The list form `UnpackN` keeps the same rule.

**take** is for a field that is about to be replaced:

```rust
fn take_field(&mut self, v: Value, i: usize) -> Option<Value> {
    // the same checks as field, then:
    if count(h) == 1 { Some(replace(slot, NIL)) }         // moved out. The slot holds Nil.
    else { let x = *slot; self.dup(x); Some(x) }
}
```

A cell with `Nil` in a slot is still a valid cell. A plain drop frees it correctly.

**update** is `R(..base, i: x)` when the base is known to be an `R`:

```rust
Instr::Update1 { dst, src, shape, index, val, moved } => {
    let x = self.operand(base, val, moved);               // 1. the new value first: moved or shared
    let b = self.take(base, src);                         // 2. then the base leaves its register
    let made = self.heap.update1(b, shape, index, x)?;    // 3. in place when count == 1, else a copy
    self.set(base, dst, made);
}
```

`heap.update1` refuses a base unless `b.top() == shape.ref_top()` and `header >> 32 == shape.top`. With one holder it stores `x` and drops the slot's old value. Otherwise the cold path takes a cell of the same size, copies the words with `copy_within`, dups every kept value, stores `x`, and drops the base.

Reading the new value first is what stops a cycle. In `Wrap(..r, inner: r)` the operand is `r` itself. It cannot be a moved operand, because the base still reads it. So it is shared, the count is 2 when the test runs, and the copy path builds a new cell that holds the old one.

**Stubs.** Every fast path above tests `i < scan`, or compares the header with a shape whose size is not 0, or tests `size != 0`. A stub has scan 0 and size 0, so none of them reads a stub's slot word as a value. The cold arms are `con_wide`, `field_wide`, `unpack_wide`, `update_wide`, `free_stub`, and `data` and `data_mut` for the raw kinds. `data` is not marked cold: a long binary is hot data.

## Instructions (`code.rs`, `exec.rs`)

| instruction | what it does |
|---|---|
| `Con1`, `Con2`, `Con3 { dst, shape, a, b, c, moved }` | Operands are read first, then the cell is taken. The shape (`top` and `variant`) is inline in the instruction. |
| `ConN { dst, shape, args }` | The list form, for wider cells and for stubs. It takes the cell, then fills it in operand order. |
| `Closure { dst, shape, func, captures }` | `ConN` with the function word first. |
| `Field { dst, src, index }` | A field or a tuple element, with a dup. `Element` becomes this. |
| `GetCapture { dst, index }` | Stays its own instruction, because the closure is in the frame and not in a register. It calls the same `heap.field(env, index + 1)`. |
| `Unpack1`, `Unpack2`, `Unpack3 { src, top, a, b, c }` and `UnpackN { src, top, dsts }` | A destination may be "no register". |
| `Take { dst, src, index }` | As above. |
| `Update1 { dst, src, shape, index, val, moved }` and `UpdateN { dst, src, shape, sets }` | As above. |
| `Drop { reg }` | The inline decrement, with the out-of-line free. |
| `JumpUnlessTag { src, top, to }` | Compares bits 32 to 64 of the value with a constant. |

`DropReuse`, `Ctor { reuse }`, `Heap::hollow`, `fits_ctor`, `ctor_in`, `Body::carried`, `reused_locals`, `Machine::args` and the carried scan in `Machine::again` are deleted.

### What the loader checks and emits

- **Field counts are facts of the program.** `Program::types` already lists each variant's fields. Its doc changes from "only for showing" to "the loader reads the count". The loader builds the constructor table from it and refuses a `VariantRef` it does not list, and a pattern or constructor whose count differs, with a typed `Unloadable`.
- **`Unpack`** is emitted for a constructor arm with at least one field whose first expression is the `Drop` of the scrutinee. A zero-field arm (`drop %0 [ctor:0]`) stays a `Drop`. A binder nothing reads becomes "no register". `dsts.len()` equals the field count because the loader made both from the same table row.
- **`Instr::writes` returns every register an instruction writes**, so liveness and `allocate` see all of `Unpack`'s destinations. The scrutinee's register counts as written.
- **Holes are checked.** After `Take r.i`, the loader tracks "slot i of r is a hole" forward through the function. Until an `Update` of `r` that sets slot i, `r` may only be the source of `Take`, `Field` of another slot, `Update`, or `Drop`. Anything else is a typed load error.
- **`reuse` on a constructor is ignored** until the compiler stops emitting it.

## Why the tokens go

In the copy of the real VM, with steps 1 to 3 in, I ran the same binary with the tokens kept and with them switched off, so that every consuming match is `Unpack` and every constructor takes a fresh cell:

| benchmark | tokens kept | free, then take | change |
|---|---:|---:|---:|
| cfold | 34.12 G | 31.42 G | 7.9% fewer |
| json | 21.55 G | 20.54 G | 4.7% fewer |
| rbtree | 31.73 G | 30.71 G | 3.2% fewer |
| permute | 25.46 G | 25.25 G | 0.8% fewer |

The cell an `Unpack` shelves is the head of its size's list, and the next constructor of that size pops it. The golden examples, the stdlib tests and `vm_exec` pass both ways.

What a token could do that free-then-take cannot is keep the fields that do not change where they are. `Take` and `Update` do that on the live cell, with no token. This is what Lean's `ExpandResetReuse` does on its fast path, and what Koka's reuse specialisation does.

## Worked examples

### binary_trees

```
fn main.item_check(%0)
  match %0
    | Empty       -> drop %0 [ctor:0]; ret 0
    | Node(%1,%2) -> drop %0 [ctor:2]; match %1 …
```

```
JumpUnlessTag r0, 0xFFFD_0000        ; Empty: a zero-field arm keeps its Drop
…
JumpUnlessTag r0, 0xFFFC_0001        ; Node
Unpack2 r0 -> r0, r1                 ; the registers the real allocator gives
JumpUnlessTag r0, 0xFFFD_0000 …
```

The copy's loader printed `main.item_check: Unpack r0 -> [0, 1]`. In binary_trees, list, cfold, deriv and rbtree, all 25 `Unpack` sites have a destination in the scrutinee's register. `bottom_up_tree` ends in `Con2 r5 = Node(r2, r4)` with both operands moved.

Measured: 27.63 G, then 25.28, 22.98, 22.64 and 20.78 after steps 1 to 4. That is 24.8% fewer, and the ratio goes from 2.54 to 1.91.

### cfold, the `append_add` arm

```
| Add(%2, %3) -> drop %0 [ctor:2]; %4 = call self(%3, %1); drop %1; drop %3; ret ctor Add(%2, %4) reuse %0
```

At step 5 (loader only): `JumpUnlessTag; Unpack2 r0 -> r2, r3; call; Con2 Add(r2, r4)`.

At step 6 (the compiler emits it): `JumpUnlessTag; Take r3 = r0.1; call self(r3 moved, r1); Update1 r0 = Add(..r0, 1: r4)`. Field 0 is never read or written.

### towers, `set_pile`

```
| c0 -> %3 = Field(1)(%0); %4 = Field(2)(%0); %5 = Field(3)(%0); %6 = Field(4)(%0); drop %0; ret ctor Towers(%2, %3, %4, %5, %6)
```

`Towers` has one constructor, so this is `Update1 r0 = Towers(..r0, 0: r2)`. The sceptic measured today's form at 1,281 instructions, and towers runs it 14.75 million times.

### rbtree, `set_black`

```
| Node(%1,%2,%3,%4,%5) -> drop %0 [ctor:5]; drop %1; %6 = ctor Black(); ret ctor Node(%6,%2,%3,%4,%5) reuse %0
```

It becomes `Const r6 = Black; Update1 r0 = Node(..r0, 0: r6)`. The store drops the old colour and four fields are never touched.

### Two programs `Update` must not get wrong

- `Circle(..s, r: 1)` where `s` may be a `Square` (`perf-scratch/attack-heap-correctness/selfupd.sca` prints `Circle(7, 1)`). The lowering keeps the read-fields-and-build form here, because the base's variant is not known. If it ever emitted `Update` by mistake, the instruction would stop with a typed error.
- `Wrap(..r, inner: r)` (legal today, and it prints 2). The operand is read first, the base is then shared, and the copy path runs. The self-test checks that dropping the result frees everything.

## What must stay true, and what keeps it true

| invariant | upheld by |
|---|---|
| A reference names the header of a cell this heap made. | Only `heap.rs` builds a `Cell`. Rust's bounds check is the backstop. |
| A count above zero is the number of words that hold the cell. | Only `dup`, `drop`, `let_go`, `unpack`, `take_field`, `update` and the operand read change a count, and each tests the range first. |
| A count at or below zero never changes. | The same tests. Free, frozen, stuck and dying cells are all in that range, and `let_go` sets the top bit on a link. |
| The first `scan` words are values and nothing else is read as one. | Every header is written from a `Shape` or by `make(class, scan, size)`, which refuses `scan > size - 1`. |
| A cell's size has one source. | `con*` and `append` take `size(shape)` words and write `scan(shape)` slots. The loader refuses an operand count that is not the shape's. |
| `free[n]` holds only free cells of n words. | `shelve` is the only writer, and it writes `size << 40` first. A cell's size never changes. |
| The arena is tiled by cells. | `append` is the only thing that lengthens it, and it writes a whole cell. |
| The variant in a reference is its cell's. | `Cell` carries it and only the heap makes one. The class in the header names the same constructor, so `audit` can compare them. |
| `Unpack` cannot lose the cell it reads, whatever registers it is given. | The scrutinee leaves its register first, and the cell is let go after the last value is read out. |
| `Update` cannot make a cycle. | Operands are read before the count is tested. The loader never marks an operand that names the base as moved. |
| `Update` in place keeps the cell's shape. | It compares the reference and the header with the instruction's shape. |
| A hole is never read. | The loader's hole check. |
| No fast path reads a stub's slot as a value. | A stub has scan 0 and size 0, and every fast path tests one of them. |
| Index 0 is never a cell. | `append` pushes word 0 before the first cell. |

### The audit

`Heap::audit(roots)` is for tests. It fails when:
- the arena is not tiled from word 1 to its end;
- a root, a cell or a box holds a reference that is not the header of a live cell, or whose variant is not the one its class names;
- a counted cell's count is not the number of references to it;
- a free list holds a cell twice, or a cell that is not a free cell of that size, or a free cell is on no list;
- `stale` is set.

A test mode runs it after every instruction over the golden programs. The earlier audit passed a heap with a cell on its list twice. This one caught a stub bug in my own probe on its first run.

## What I kept and what I dropped

Kept from the earlier design: the flat arena, the one-word header, lists threaded through the cells, the Lean free loop, the variant in the reference, direct fill, `Unpack`, stubs, and retiring the tokens.

Dropped or changed:
- **Run carving** (one sixteenth of the arena, up to 512 words). Replaced by append. A heap of 1,000 small cells and one cell of each size from 4 to 66 was 5.96 times its live data with runs and is 1.00 with append.
- **Exact sizes for raw cells.** A string that grows a word at a time left 32,385 dead words with exact sizes and leaves 2,750 with rounding.
- **`Unpack` and `Update` as worded.** Both orders were wrong, and both are fixed above.
- **"Take the cell before any count changes" as a rule.** A full heap ends the process and its counts with it. The fixed forms read operands first because that measured 10 instructions cheaper.
- **The 256-entry free table.** It is a `Vec` grown on the first free of each size.
- **Moving array and map values first in this plan.** That is about 130 sites in `array.rs` and `map.rs`, and it belongs to their rewrites. `scan = 255` covers them until then.
- **The type's rank as the class.** It is a row in a table that only grows, and it names the constructor, not just the type.
- **The `made` counter on the pop.** `carved` counts in the append path only.

## Steps

### 1. 1. Name a cell by its word in one arena. The heap's methods keep their signatures.

**Changes.** crates/scarlet_vm/src/heap.rs: replace `Chunk`, `chunks`, `spare`, `current`, `next_chunk`, `own_chunk` and `new_chunk` with `words: Vec<u64>`. `Cell` becomes one index. `onward` and `onward_mut` become `words.get(i..)`, and `copy_words` becomes `copy_within`. A new cell that no free list has is appended, after a `try_reserve`. A cell over 254 words, which gets its own chunk today, lives in a box named by a two-word stub whose header has size 0 (no cell has that size today); `words`, `words_mut`, `word` and `set_word` hide it. Today's header, kinds, tag word, `FreeLists`, `release` and `dying` all stay. Rewrite the three chunk tests against the arena. crates/scarlet_vm/src/value.rs: the cell payload is the index. docs/vm-design.md: rewrite "how a heap grows". No instruction, IR or handler changes. The measured port is perf-scratch/heap-final/heap.step1.rs (it has no stubs: every cell is in the arena).

**Expected gain.** Measured in a copy of the real VM, all 23 benchmarks, outputs equal to expected.txt. Geomean factor 0.915, so 6.17 becomes 5.64. Per benchmark, instructions against today: richards 0.862, sieve 0.872, fannkuch 0.882, string_keys 0.884, permute 0.890, towers 0.891, queens 0.892, storage 0.892, map_churn 0.894, bounce 0.895, json 0.897, cfold 0.900, list 0.900, rbtree 0.901, deriv 0.902, closures 0.905, strings 0.912, binary_trees 0.915 (27.63 G to 25.28 G), nbody 0.930, spectral_norm 0.993. The earlier estimate was 0.95, from the toy (binary_trees 9.3% to 10%, the array benchmarks 5%, strings 2% to 3%); the real VM gives more on every benchmark that touches the heap, because every array, map and string operation makes many lookups. The three benchmarks that touch no cell went up: fib 1.021, tak 1.014, mandelbrot 1.016. I did not find out why. It is the size of the layout noise the critics measured in the toy.

**Risk.** Low. Every module reaches the heap through these methods, and the golden tests cover it. Growing is a realloc: I measured a 2.4 GB arena built on macOS with peak memory equal to the arena, and did not measure Linux. The stub path adds two loads to every read of a long string or binary; json reads its 25 KB input 38,753 times a parse this way, and the port did not have stubs, so that cost is not in the number. fib, tak and mandelbrot moving by 1.4% to 2.1% should be looked at before this lands.

### 2. 2. The fast paths: lists through the cells, a signed count, inline dup and drop, a free loop with no side Vec, and frozen constants.

**Changes.** heap.rs: remove `FreeLists`, `DENSE_SIZES`, the `HashMap`, the `dying` Vec and the `live` counter. Add `free: Vec<u32>`, grown the first time a size is freed, with the link in a freed cell's word 1; an inline pop in `alloc`; `shelve`. A cell of one word (today's empty tuple or empty node) has no room for a link, so those keep a small side list until step 3 makes every cell two words. The count is read as an `i32`: `share` adds when it is above 0, `release` subtracts when it is above 1 and calls the out-of-line `free` at 1. `free` is the Lean loop: a child whose last reference goes is chained through its count field with the top bit set, so the arena stops at 2^31 words. The out-of-line branch of dup and drop sets `stale` when the count is exactly 0; `append`'s cold path, the end of a run and `audit` read it. `made` becomes `carved`, counted in the append path. `live()` becomes a test-only walk and `audit()` arrives, with a test mode that runs it after every instruction. Add `freeze(cell)` (count `i32::MIN`) and `consts`; exec.rs: `Instr::Str`, `BinaryConst` and `BigInt` make their cell once per heap and read it from `consts` after. Raw cells are rounded to the size classes. Today's kind field, `held()`, `hollow` and `fits_ctor` stay. The measured port is heap.step2.rs in the same directory (no constants, no rounding).

**Expected gain.** Measured, against step 1: geomean factor 0.928, so 5.64 becomes 5.24. richards 0.887, towers 0.890, bounce 0.901, storage 0.903, json 0.905, fannkuch 0.908, rbtree 0.908, binary_trees 0.909 (25.28 G to 22.98 G), sieve 0.914, string_keys 0.916, cfold 0.917, deriv 0.917, permute 0.922, closures 0.923, list 0.924, queens 0.927, strings 0.934, nbody 0.938, map_churn 0.956. fib 0.991, tak 0.993, mandelbrot 0.992, spectral_norm 0.990. The earlier estimate was 0.94. Not in the measurement: frozen constants. From the diagnosis, a literal costs 360 to 394 each time it is evaluated, which is 10.7% of string_keys (8.5M x 360), 1.6% of json and about 3% of strings; that gain is the strings work's too and must be counted once. Growth through the append path costs 64 instructions per new 3-word cell in the probe, against 255 for today's heap.

**Risk.** Medium. While a cell waits on the dying list its count field holds a link. The link has the top bit set, so a stray dup or drop of it does nothing, and `audit` reports a reference to it. A count now sticks at 2^31 holders. A process holds its own copy of each literal it has read, and `consts` is as long as the highest constant it has read; with thousands of literals and thousands of processes that is real memory, and the way out is a shared constant arena under tag 0xFFFE. The lazily grown `free` table cost 9 to 11 more instructions on a make-and-free pair in the small toy and 0 to 6 in the 110-arm one, which is inside the noise between builds.

### 3. 3. The new header, and the constructor, tuple and closure layouts. The variant moves into the reference.

**Changes.** heap.rs: the header becomes count:32, scan:8, size:8, class:16. `make(class, scan, size, data)` refuses `scan > size - 1`. A constructor cell is `[header, fields..]`; a tuple `[header, elements..]` with `()` two words; a closure `[header, func word, captures..]`. Array roots, array branches and the map kinds keep their layouts and get `scan = 255`, which sends `free` to today's `held()` for them. A freed header is `size << 40`. `variant(cell)` goes; `ctor_of(cell)` reads the class. value.rs: `Cell` is the 48-bit payload; `Value::top()`. code.rs: the constructor table, built from `Program::types`, which the loader now requires to list every variant the program names; `Shape`; `JumpUnlessVariant` becomes `JumpUnlessTag { src, top, to }`; `TooManyConstructors` and the arity errors. exec.rs: `Field`, `Element` and `GetCapture` check `index < scan`; `is_variant` becomes one compare on the register. show.rs, eq.rs, hash.rs, json.rs, http.rs: read the constructor from the class. scarlet_ir: the doc on `Program::types`. array.rs and map.rs do not change.

**Expected gain.** Not measured in the real VM. From the toy: a variant test on a cell is 44 with the tag word behind one lookup and 33 with the variant in the reference (35 in the 110-arm build), so about 11 per test; a `Field` goes from 61 to 59; a free loses the kind chain, about 8 per cell. With the diagnosis's counts: deriv, JumpUnlessVariant is 20.8% of the run at 68 each, so 20.8% x 11/68 = 3.4%; list, 4 tests x 11 of 963 on 80% of the run = 3.7%; cfold 14.2% x 11/68 = 2.3%; binary_trees (92 x 11 + 31 x 8) / 57,047 = 2.2%; rbtree 9% x 11/68 = 1.5%; under 1% elsewhere. Geomean factor about 0.99. Constructor cells are one word shorter (a Node 4 words to 3), which I did not measure by the clock. A masked compare, which a second cell tag would need, measured the same as the plain one (32 against 33).

**Risk.** Medium. It touches every reader of a variant, though no longer array.rs or map.rs. The tag test compares the variant and whether the value is a cell, and not the type. `Unpack` and `Update` do compare the whole shape, constructor included, so a wrong-type value stops at the first of those; a `Field` by name checks only that the slot exists. A cell over 253 values becomes a stub and takes the cold paths. The 65,520-constructor limit is new.

### 4. 4. Constructors, tuples and closures are filled straight from the registers.

**Changes.** code.rs: `Con1`, `Con2`, `Con3` with the shape inline, `ConN` and `Closure` with operand lists; the loader refuses an operand count that is not the shape's scan. heap.rs: `con1`, `con2`, `con3` (pop or append), `con_list`, `con_wide`. exec.rs: the handlers; the fixed forms read their operands (moved or shared) and then take the cell, the list form takes the cell and fills it in order. `Machine::args` is deleted and `Ctor` stops using `passing`. `Instr::Array` keeps collecting into `passing` until the arrays work replaces `from_values`. The measured port (heap.step3.rs, exec.step3.rs) is the list form on today's layout.

**Expected gain.** Measured for the list form, against step 2: geomean factor 0.992. bounce 0.870 (30.29 G to 26.36 G: its three tuples a step lose the Vec's malloc), json 0.945 (its closures), cfold 0.981, nbody 0.983, binary_trees 0.985, towers 0.985, richards 0.991. The other sixteen are between 0.996 and 1.013, which is noise. The earlier estimate of 0.975 assumed about 60 saved on every constructor; the real saving on a plain constructor is about 1.5% of binary_trees, not 3.3%. On top of that, not measured in the real VM: in the toy a fixed form with its shape inline costs 141 to 145 for a 2-field cell made and the last one freed, and a shape looked up in a table costs about 10 more per instruction. Against the list form that is about 25 to 50 per small constructor: binary_trees 31 x 40 / 57,047 = 2.2%, cfold 28M x 40 of 43.1 G = 2.6%. Geomean about 0.995 more.

**Risk.** Low. If the tuples work removes tuples that only carry several values, bounce's gain moves there, and if `then` is inlined, json's does; neither should be counted twice. Records of four or more fields use the list form, at about 28 instructions a field; if the instruction format has room, fixed forms to 4 or 5 fields are worth having for richards, nbody and towers.

### 5. 5. Unpack, and the reuse tokens retire.

**Changes.** code.rs: in `CoreExpr::Match`, a constructor arm with at least one field whose body starts with the `Drop` of the scrutinee emits `JumpUnlessTag` then `Unpack1/2/3` or `UnpackN`; a binder nothing reads is "no register". `Instr::writes` returns every register written, and liveness and `allocate` use it. `Instr::DropReuse`, `Ctor`'s `reuse`, `reused_locals` and `Body::carried` are deleted: the loader reads every `Drop` as plain and ignores `reuse`. heap.rs: `unpack1/2/3`, `unpack_list`, `unpack_wide`; delete `hollow`, `fits_ctor`, `ctor_in`. exec.rs: the handlers, with the scrutinee taken out of its register first; the carried scan in `Machine::again` goes. Stdlib: `internal.cells_reused` now counts the times an `Unpack` (and later a `Take` or `Update`) found one holder; `internal.cells_made` becomes `cells_carved`; `internal.heap_words` arrives. Tests: the five aliasing cases, a hole on each path, a wrong shape, a 300-field record. docs/vm-design.md: rewrite the reuse paragraphs of step 6. The measured port is code.step4b.rs, exec.step4.rs and heap.step4.rs (list form, today's layout, tokens switched off by an environment variable).

**Expected gain.** Measured, against step 3, same outputs: geomean factor 0.981. cfold 0.899 (34.95 G to 31.42 G), list 0.914, binary_trees 0.918 (22.64 G to 20.78 G), json 0.953, rbtree 0.964, deriv 0.965, queens 0.987, richards 0.988, permute 0.992, towers 0.994, the rest 1.000. With the tokens kept and `Unpack` only on plain drops the same binary gives cfold 34.12 G, rbtree 31.73 G, json 21.55 G and permute 25.46 G, so retiring the tokens is itself 7.9%, 3.2%, 4.7% and 0.8%. The earlier estimates were list 3% to 5% (it is 8.6%), deriv 3% to 5% (3.5%) and rbtree 3% to 5% (0.5% from `Unpack` alone, 3.6% with the tokens gone: only 2 of its sites fuse, because its nested arms drop the scrutinee after an inner test). Steps 1, 2, 4 and 5 together, measured: 0.826, so 6.17 becomes 5.10; binary_trees 0.752 (ratio 2.54 to 1.91), cfold 0.728 (1.85 to 1.35), json 0.731, bounce 0.702, richards 0.749, list 0.770, towers 0.776, rbtree 0.791, deriv 0.799. With step 3 and the fixed forms, by arithmetic: about 0.81. If in-place array writes, tuples as values and inlined `then` land first and take their cells with them (the four array benchmarks, bounce's tuples, json's closures), the measured 0.826 becomes about 0.86.

**Risk.** Low to medium. The order inside `Unpack` is the whole of its safety, and the real allocator does give it its own source register at every site I looked at (25 of 25). The copy passes golden_examples (45), stdlib (22) and vm_exec (52) with tokens on and off, and scarlet_vm's run tests (64) with tokens on. The allocation-count tests change their words: "makes 0 and reuses 1000" becomes "carves 0 and reuses 1000". Arms that drop the scrutinee after an inner test are not fused; that needs the match-compilation work.

### 6. 6. Take and Update: a rebuild that keeps its variant writes only what changed.

**Changes.** scarlet_ir: `Atom::Take { src, index }` and `Atom::Update { base, variant, sets }`. scarlet_lower: lower.rs emits `Update` for `R(..r, f: x)` when `r`'s variant is known to be `R` (a one-constructor type, or inside an arm that proved it), and keeps reading fields and building where it is not (`Circle(..s, r: 1)` over a two-variant type). perceus.rs replaces the drop-and-reuse pair with reuse specialisation: a field put back in its own slot is left alone, a field the arm consumes becomes `Take`, the constructor becomes `Update`; it stops emitting `reuse`. An operand that names the base is never a move. code.rs: `Take`, `Update1`, `UpdateN`, and the hole check. heap.rs: `take_field`, `update1`, `update_list`, `copy_with`, `update_wide`. exec.rs: the handlers, operands first, then the base out of its register. Tests: `Wrap(..r, inner: r)` leaves no cycle; a wrong-variant base is refused; a hole that would be read is refused at load.

**Expected gain.** Not measured in the real VM; it needs the compiler. In the toy, on the final heap: `Update1` of one field of a unique record costs 87 to 95 whatever the record's width, and `Take` then `Update1` 149 to 161. The same work as `UnpackN` then `ConN` on a 5-field cell costs 394, and as today's five Fields, Drop and constructor on the new heap 627 to 642; the sceptic measured today's real 5-field update at 1,281. towers: 14.75M updates at 1,281 are 49% of today's run; steps 1 to 5 leave it at 0.776, and at about 150 an update (a Field or Take, then the Update) nearly all of that half goes, so about 35% to 40% more of what is left. richards: the Scheduler copy is 9.6% and is unique, and `Take` is what lets `map::set(s.tasks, ..)` see a unique map (1,951 against 441, 10.7% of the run), so 10% to 15%. permute: State rebuilds are 11.3%. rbtree: `set_black` and the arms of `ins` change one field of five, about 240 saved per node rebuilt. cfold: 9 fewer than `Unpack2` and `Con2` on a 2-field cell, under 1%. This is cause 12's gain and part of cause 20's, about 0.96 on the geomean, counted there and not in the 0.826.

**Risk.** Medium, and mostly in the compiler. Perceus must agree that `Update` consumes its base and that `Take` gives an owned value. The hole check in the loader is what turns a compiler slip into a load error and not a wrong value. The shared path of `Update` (a copy) was tested for counts but not priced.

## Experiments

Everything is under perf-scratch/heap-final/. Nothing in the repository was edited, and no cargo or git ran inside it.

**1. Steps 1 to 4 in a copy of the real VM.** `vm/` is a copy of `crates/`, `Cargo.toml`, `Cargo.lock` and `examples/`, built with its own target directory (profiling profile, rustc 1.97.1). `VERSION` there says `0.0.0-scratch` so the build script never calls git. Snapshots of each state: `heap.step1.rs`, `heap.step2.rs`, `heap.step3.rs` with `exec.step3.rs`, and `heap.step4.rs`, `exec.step4.rs`, `code.step4b.rs`. Binaries: `bin/scarlet-base`, `-step1`, `-step2`, `-step3`, `-step4b`. `bench.sh` runs a benchmark under `/usr/bin/time -l` and compares its output with `expected.txt`. All 23 benchmarks printed the expected output at every step. Results are in `step1.results` to `step4b.results`.

Instructions retired, billions, and the factor for each step:

| benchmark | today | step 1 | step 2 | fill | Unpack | all |
|---|---:|---:|---:|---:|---:|---:|
| binary_trees | 27.63 | 0.915 | 0.909 | 0.985 | 0.918 | 0.752 |
| bounce | 37.59 | 0.895 | 0.901 | 0.870 | 1.000 | 0.702 |
| cfold | 43.13 | 0.900 | 0.917 | 0.981 | 0.899 | 0.728 |
| closures | 26.40 | 0.905 | 0.923 | 1.005 | 1.001 | 0.840 |
| deriv | 39.24 | 0.902 | 0.917 | 1.000 | 0.965 | 0.799 |
| fannkuch | 27.78 | 0.882 | 0.908 | 1.006 | 0.999 | 0.804 |
| fib | 10.65 | 1.021 | 0.991 | 1.012 | 0.999 | 1.023 |
| json | 28.08 | 0.897 | 0.905 | 0.945 | 0.953 | 0.731 |
| list | 43.67 | 0.900 | 0.924 | 1.013 | 0.914 | 0.770 |
| mandelbrot | 37.88 | 1.016 | 0.992 | 1.011 | 1.000 | 1.018 |
| map_churn | 19.74 | 0.894 | 0.956 | 1.006 | 1.000 | 0.859 |
| nbody | 22.69 | 0.930 | 0.938 | 0.983 | 1.000 | 0.857 |
| permute | 31.03 | 0.890 | 0.922 | 1.000 | 0.992 | 0.814 |
| queens | 35.97 | 0.892 | 0.927 | 1.009 | 0.987 | 0.824 |
| rbtree | 38.82 | 0.901 | 0.908 | 1.003 | 0.964 | 0.791 |
| richards | 35.10 | 0.862 | 0.887 | 0.991 | 0.988 | 0.749 |
| sieve | 29.18 | 0.872 | 0.914 | 1.000 | 0.999 | 0.796 |
| spectral_norm | 40.01 | 0.993 | 0.990 | 1.012 | 1.000 | 0.994 |
| storage | 27.32 | 0.892 | 0.903 | 0.996 | 1.000 | 0.803 |
| string_keys | 28.70 | 0.884 | 0.916 | 1.006 | 1.000 | 0.815 |
| strings | 30.66 | 0.912 | 0.934 | 0.999 | 0.998 | 0.849 |
| tak | 38.46 | 1.014 | 0.993 | 1.009 | 1.000 | 1.016 |
| towers | 38.39 | 0.891 | 0.890 | 0.985 | 0.994 | 0.776 |
| **geomean** | | 0.915 | 0.928 | 0.992 | 0.981 | **0.826** |

The baseline matches the diagnosis (fib 10.65 G, binary_trees 27.63 G).

**2. Tokens against free-then-take, same binary.** `SCARLET_NO_TOKENS=1` makes the copy's loader treat every `Drop` as plain and ignore `reuse`; `SCARLET_NO_UNPACK=1` switches the fusion off. cfold: 34.12 G with tokens and `Unpack` on plain drops, 31.42 G with tokens off, 34.63 G with both off. rbtree: 31.73, 30.71, 31.62. permute: 25.46, 25.25, 25.25. binary_trees: 20.78, 20.78, 22.66. `Unpack` alone, on against off with tokens kept: binary_trees 8.2%, list 8.9%, cfold 2.5%, deriv 2.2%, richards 1.1%, towers 0.7%, rbtree 0.5%, queens 0.2%, and nothing on storage, json, bounce, nbody and closures.

**3. The registers the real allocator gives `Unpack`.** A build that prints each `Unpack` after `allocate` shows `main.item_check: Unpack r0 -> [0, 1]`, `main.is_shorter_than: Unpack r1 -> [1, 2]`, `main.fold: Unpack r0 -> [0, 3, 4, 5, 6]`. In binary_trees, list, cfold, deriv and rbtree, 25 of 25 sites have a destination in the scrutinee's register. Site counts with tokens kept: binary_trees 1, list 4, cfold 10, deriv 8, rbtree 2.

**4. Tests on the copy** (debug builds, `tests.tokens.log`, `tests.notokens.log`, `tests.vm.log`): golden_examples 45 passed and 11 ignored, stdlib 22 passed, vm_exec 52 passed and 2 ignored, each with tokens on and with tokens off. scarlet_vm's `run` tests: 64 passed with tokens on. I did not run them with tokens off, and I did not run the rest of the workspace's tests.

**5. The critics' findings, checked.**
- I ran `attack-heap-correctness/target/release/attack`: cases A and B are BROKEN with the order as the earlier design worded it, and the old audit says `true` on both. `fill2` shows a 3-word cell shelved on the 4-word list.
- `selfupd.sca` prints `Circle(7, 1)` for a `Square` base on `target/release/scarlet`. `cyc.sca` (`Wrap(..r, inner: r)`) prints 2, and `scarlet dis` shows `ret ctor 10.1(%0, %7)`.
- I ran `attack-heap-simple`'s probes: 80,406 dead words after one growing string with runs and 32,385 with exact sizes and no runs; a heap 5.96 times its live data with runs.
- code.rs: `writes()` returns `Option<Reg>` (734) and `allocate` adds a clash only between that register and what is live after (2129).

**6. The final heap as a probe.** `probe/src/fin.rs` is the heap described in the design. `probe/src/toy.rs` runs it behind the critics' toy interpreter, which I copied with its 80 padding arms behind a `big` feature, so there are two builds: about 30 arms (`target/`) and about 110 (`target-big/`). `price3.sh` and `count.sh` take slopes of instructions retired.

`probe fin-selftest` passes in both builds. It checks, with the new audit after each:
- `Unpack` in the fixed and list forms over eight cases: a destination that is the source register, unique and shared; a destination whose old value was the only other holder; two destinations in one register; a destination holding a dead cell; a hole on the unique path and on the shared path; a cell of another shape.
- `Take` from a unique and from a shared cell, and a drop of a cell with a hole.
- `Update` in place, of a shared record, of `Wrap(..r, inner: r)` (no cycle is left) and of a wrong-variant base (refused).
- A 5-field and a 300-field record through `con_list`, `field`, `take_field`, `update1`, `unpack` and drop, each holding a cell and a handle.
- A million-cell list freed by one drop, twice, with the arena the same size the second time.
- The audit refusing a root that names a free cell, a wrong variant and an index inside a cell, and `stale` being set.

The audit caught a bug of mine on its first run: the list form of `Unpack` compared a stub's header with a stub's shape, found them equal, and read the slot word as a field.

Prices in the toy, instructions per group, small build and 110-arm build:

| group | small | 110 arms |
|---|---:|---:|
| `Con2` of two moved operands, and free the last cell | 141 | 145 |
| `Unpack2` then `Con2`, unique cell, shapes inline | 158 | 166 to 170 |
| the same with shapes looked up in a table | 176 | 184 |
| the earlier probe's forms (no holes, no moved flags, no counter) | 140 to 142 | 156 |
| `Field`, `Field`, `Drop`, `Con2` on the final heap | 306 | 315 |
| `Take` then `Update1` | 149 | 161 |
| `Update1` of an Int field, less its `Const` | 87 | 95 |
| `UnpackN` then `ConN`, 5 fields | 394 | 395 |
| five `Field`s, `Drop`, `ConN`, 5 fields | 627 | 642 |
| tag test, and the same with a mask | 33, 32 | 35, 34 |
| `Field` of an Int, of a cell | 59, 78 | 61, 80 |

A table lookup for the shape costs about 10 an instruction, the `reused` counter 3, and the hole and moved flags about 12 for the pair. Reading operands before taking the cell is 10 cheaper than the other order. A form with a 4-register array and a count costs 9 more than `Con2` and 27 more for the pair.

binary_trees in the toy, per node (the real VM is 1,841 today): today's heap 1,556 and 1,582; today's instructions on the final heap 1,185 and 1,254; the final forms 1,048 and 1,120. The 110-arm build moved by 60 a node (5%) between two builds that differed only in the free table, so I read toy figures as plus or minus 5%.

Path lengths from `objdump` (arm64): dup 3 for a non-cell and 12 for a cell; drop 3 and 13; the checked field read 16; take of a unique field about 20 with its counter.

Memory: `fin-strgrow` leaves 32,385 dead words with exact raw sizes and 2,750 with rounding, the same after 5 rounds. `fin-sizes` gives 1.00 times the live data at 0, 100 and 1,000 small cells. The first cell makes a 4-word arena. `fin-grow`: 64 instructions per appended 3-word cell (114 when the cell is zero-filled first and then written); 100 million cells take 0.60 s and 2.40 GB peak, which is the carved arena, with the spare capacity untouched.

**What these do not show.** The new header, the variant in the reference, the fixed forms, stubs, frozen constants, raw rounding, `Take` and `Update` have not run in the real VM. The port's `Unpack` and fill are list forms on today's layout. fib, tak and mandelbrot rose 1.4% to 2.4% across the steps and I did not find the cause. I did not measure Linux.

## Prior art

**Read again for this revision** (fetched as raw files, the parts quoted).

- **Lean 4, `ExpandResetReuse.lean`**, https://github.com/leanprover/lean4/blob/v4.9.0/src/Lean/Compiler/IR/ExpandResetReuse.lean. `expand` tests `isShared y` and builds two branches. `eraseProjIncFor` drops the `inc` after each `proj[i] y` and records the variable in a mask ("code took ownership of this field"). On the fast path `releaseUnreadFields` decrements only the fields the code never projected, `reuseToSet` turns `reuse x ctor ws` into `set x[i] := ws[i]`, and `removeSelfSet` deletes a `set x[i] := y` where `y := proj[i] x`. On the slow path `mkSlowPath` puts the `inc`s back and decrements `y`. `Take` is the owned projection, `Update` is `setFields` after `removeSelfSet`, and the store dropping the slot's old value is `releaseUnreadFields`. It all happens on the live object, with no token.
- **Koka, `ParcReuseSpec.hs`**, https://github.com/koka-lang/koka/blob/dev/src/Backend/C/ParcReuseSpec.hs. `ruSpecialize` specialises "if we can get the necessary ConInfo and save at least a fourth of the necessary assignments": `oldTagBenefit + matches >= 1 + ((1 + length args) div 4)`. A field matches when the expression is the pattern's own variable or a dup of it. `genConFieldsAssign` then assigns only the others. That is the rule for when the compiler should emit `Take` and `Update` and when `Unpack` and `Con`; for a 2-field cell with the same tag it always specialises, which agrees with my 149 against 158.
- **Lean 4, `object.cpp`**, https://github.com/leanprover/lean4/blob/master/src/runtime/object.cpp. `push_back` and `pop_back` keep the todo list's next pointer in the low 48 bits of the dying object's header (its count and size fields), and `dec` is "above 1: subtract; 1: push; never freed: return". My free loop is this with a 31-bit index in the count field and the top bit set.

**Carried over from the earlier round, not fetched again.**

- **Koka's runtime header**, https://github.com/koka-lang/koka/blob/dev/kklib/include/kklib.h: `scan_fsize`, tag and a signed 32-bit count in one word, a scan size of 0xFF meaning "look elsewhere", and `KK_HEADER_STATIC` with a stuck count. I took the one-word header, values first, the frozen count, and an escape value for scan (255 here means "ask the kind").
- **Lean's header**, https://github.com/leanprover/lean4/blob/master/src/include/lean/lean.h: `LEAN_MAX_SMALL_OBJECT_SIZE`, above which objects go to the general allocator. That is the same split as 254 words and a box.
- **mimalloc**, https://github.com/microsoft/mimalloc/blob/master/src/alloc.c and https://github.com/microsoft/mimalloc/blob/master/src/page.c: a free list threaded through the blocks and found by a direct index. I no longer take `mi_page_extend_free`'s runs: a mimalloc page holds one size, and one arena holding sixty sizes wasted most of a small heap. Rounding raw cells to a few sizes per doubling is mimalloc's bins, from memory.
- **Erlang/OTP**, https://github.com/erlang/otp/blob/master/erts/emulator/beam/erl_bits.h, erl_message.c and erl_gc.c: a small on-heap object that names something outside the heap, and why appended heap fragments suit a collector that compacts and not a counted heap.
- **Pony**, https://github.com/ponylang/ponyc/blob/main/src/libponyrt/mem/heap.c, and **OCaml**, https://github.com/ocaml/ocaml/blob/trunk/runtime/caml/mlvalues.h: read by the earlier designers; nothing new taken.
- **Rust's standard library**: `Vec::try_reserve` and `try_reserve_exact`, https://doc.rust-lang.org/std/vec/struct.Vec.html#method.try_reserve, so running out of memory is a value. Appending with `extend_from_slice` inside reserved capacity is what makes growth 64 instructions a cell.

**From memory, not read.** Reinking, Xie, de Moura and Leijen, "Perceus: Garbage Free Reference Counting with Reuse", for drop specialisation, which `Unpack` is. Marlow, Yakushev and Peyton Jones, "Faster laziness using dynamic pointer tagging", for the constructor tag in the reference.

## What it gives up, and what the critics found

## What the critics found, and what I did

**Critic 1 (correctness).**
- *`Unpack` double-frees when a destination is the scrutinee's register.* True. I ran their probe, and the real loader gives exactly those registers at 25 of 25 sites. The scrutinee now leaves its register first. Tested in eight cases, in both forms, and in the real VM.
- *`Update` in place is wrong when the base is another variant.* True (`Circle(..square, r: 1)`). `Update` carries the shape, compares the reference and the header with it, and the lowering emits it only where the variant is known.
- *A cell's size has two sources in `fill`.* True. The shape is the only source and the loader refuses a mismatched operand count.
- *`audit` misses a reference to a freed cell.* True. It is rewritten, and it found a bug in my own probe.
- *The variant can be lost through `as_cell` and `Value::cell`.* True. `Cell` is the whole payload, and the class names the constructor so the audit can compare.
- *Loader cases are unspecified.* Zero-field arms keep `Drop`, a destination can be "no register", and field counts come from `Program::types`, which the loader now requires.
- *Stubs are specified for two operations only.* Every operation has its cold arm, and a 300-field record goes through all of them in the self-test. The arena cell is taken before the box is built.
- *The constants plan contradicts itself.* Decided: a frozen cell per process, in its own arena. One arena, one tag.
- *Minor points.* A stale dup or drop sets `stale`. `GetCapture` stays an instruction. Classes come from a table that only grows. Every cell is two words from step 3.

**Critic 2 (numbers).**
- *The prices were for instructions the design did not specify.* True. The realistic `Unpack2` and `Con2` pair is 158 to 170, not 130. Fixed-arity forms with the shape inline are in steps 4 and 5.
- *The toy flatters the new heap; binary_trees is 24% to 30%, not a third.* I did the experiment they asked for. binary_trees is 24.8% fewer after four steps in the real VM.
- *Tokens were only compared on the best case; add `Take`.* Added. And in the real VM the tokens lose to free-then-take on every benchmark that uses them.
- *The geomean counts other areas' work.* It is now reported two ways: 0.826 measured today, about 0.86 if the array, tuple and inlining work land first.
- *`Unpack` needs holes.* Added.
- *A second cell tag would cost a masked test.* The masked test measured the same.
- *Take `made` off the pop.* Done.

Their estimate for all of this was 0.89 to 0.91. The measurement is better than that, mostly because step 1 is worth 0.915 and not 0.95.

**Critic 3 (simplicity).**
- *`Unpack` order.* Fixed, as above.
- *`Update` can write a record into itself.* True. Operands are read first.
- *`Update` does not compose; add `Take`.* Added, with a load-time check that a hole is never read.
- *Run carving wastes small heaps.* True (5.96 times). Replaced by append, which is simpler than both of their suggestions.
- *Exact sizes strand memory behind string growth.* True. Raw cells are rounded (32,385 words down to 2,750), and the table is a lazily grown `Vec`.
- *Steps 5 and 6 were wide for little.* The layout change now leaves `array.rs` and `map.rs` alone.
- *The build order rewrote handlers twice.* The layout is step 3, before fill and `Unpack`. I measured fill and `Unpack` on today's layout; what they save does not depend on the header.
- *`cells_reused` was the only sign a cell was unique.* The counter stays, with that meaning.
- *binary_trees, deriv and rbtree were overstated.* Corrected with measurements. list was understated.

## What this design gives up

- **Memory goes back less readily.** The arena only grows, and a freed cell is taken again only by a cell of the same size. Long cells do go back. A deliberate copy into a fresh heap is the way out and is not designed here.
- **Growth is a realloc of the whole arena.** It measured free on macOS at 2.4 GB. Not measured on Linux.
- **A process copies each literal it reads.** It is simple, and costly with many literals and many processes.
- **The tag test checks the variant, not the type.** `Unpack` and `Update` check the whole shape. `Field` checks only that the slot exists.
- **A hole is `Nil` in a typed slot.** Nothing at run time tells a hole from a real `Nil`. The loader's check is the only guard.
- **Limits.** 2^31 arena words a process, 65,520 constructors with fields, 254 words for an arena cell.
- **More instructions.** Twelve new ones for seven deleted.

## Where it is weakest

- **Steps 3 and 6 and the fixed forms are still arithmetic** from a toy that moves 5% between builds.
- **The controls moved.** fib, tak and mandelbrot rose 1.4% to 2.4% in the real port and I do not know why.
- **The measured port has no stubs**, so the cost of reading a long binary through a box is not in any number.
- **Arms that drop the scrutinee after an inner test are not fused**, which is why rbtree gains little from `Unpack`.
- **A drop that frees a cell with children is still an out-of-line call.**
- **`Take` and `Update` put an obligation on the compiler** that tokens did not: the base's variant must be known, and a hole must be filled or dropped on every path.

## Processes and a JIT

**A heap per process.** The heap is plain owned data: one `Vec<u64>`, a free table that is empty until something is freed, the `outside` table and a few integers. Nothing in it points into itself, so it moves between scheduler threads with its process. A process that never makes a cell has allocated nothing. Its first cell makes a 4-word arena, and the arena then doubles. When a process dies, dropping the `Vec` returns the arena in one call, dropping `outside` returns every long cell, and its `Handle` entries are the sockets to release. Nothing walks the cells. The limit is one compare in the cold path of `append`, against arena words plus `boxed`, so long strings count. Only the owner touches a heap's counts, so they stay non-atomic.

**Constants.** Each process makes its own frozen copy of a literal the first time it reads it. That keeps one arena and one cell tag. It costs memory when there are many literals and many processes. If that is measured to matter, tag `0xFFFE` is free for a cell in a read-only arena owned by the loaded code: `as_cell` is false for it, so dup, drop and `unique` do not change, and only the readers of raw data (`data`) grow a second case. A variant test would need a mask only if constants ever became constructors, and the masked test measured the same.

**Messages.** A message is a small `Heap` of its own, filled by a copy that follows `scan`. For the five kinds with `scan = 255` the copy uses `held()` until the array and map rewrites land. The receiver copies the message into its own heap and drops the message's. It must not append the message's words to its arena: an earlier measurement had a receiver grow to 44 million words with nothing live. A shared large binary later becomes an `Arc` entry in `outside`, behind the same stub.

One thing follows for globals, which are not this area: a global that holds a cell lives in the heap of whichever process ran the module's toplevel, so another process needs its own copy or a frozen one.

**Reductions and yielding.** None of these instructions calls Scarlet code or keeps state on the Rust stack. The one unbounded operation is freeing a large structure, and its whole state is the head of the dying list. So the loop can take a budget, store the head in `heap.pending`, and be charged as reductions. A cell on that list has a negative count, so a dup or drop that reaches it while the free is paused does nothing. When frees can pause, `append` should finish some of the pending list before it grows the arena. Retiring the tokens removes the `carried` registers a self tail call scans around.

**A JIT.** Each instruction is a short straight line over the arena's base and length and the free table. Compiled code holds indexes, never addresses, so growing the arena cannot invalidate a value in a machine register, and the JIT needs no stack maps for moving cells. It reloads the base and length after any call that can grow the arena, which is only the append path. It can drop the shape compare in `Unpack` and the scan check in `Field` where the typed IR has proved them, and turn a chain of `JumpUnlessTag`s into a jump table on bits 32 to 48. `Take` and `Update` are what a compiler wants for reuse: no token to keep alive across a call, only the live cell. If a JIT later finds a token in a machine register pays, the header has spare classes for one.

**The loader's new duties carry over.** The constructor table, the field counts from `Program::types`, and the hole check all run once at load, so the interpreter and a JIT read the same verified instructions.

