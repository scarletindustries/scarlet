# Speed design: Strings, binaries and maps: copying, literals, concatenation and building, search and replace, hashing string keys, in-place map edits, lookups that box

This is the design for one area of the speed work, written on 23 November 2026. `docs/perf-plan.md` puts its steps in order with the other areas' steps, and settles where the areas disagree. Where this file and the plan differ, the plan is right.

# Strings, binaries and maps: the design to build

## The design in one paragraph

A string stays one flat cell with today's layout, and every built-in reads its bytes where they lie. A literal is not a heap cell. It is a cell-shaped run of words in a frozen area that belongs to the loaded program, and its value word has a tag of its own, so counting passes it by and loading one costs what loading an Int constant costs. `a + b + c` is one `Concat` that reads each part once and allocates once. When the instruction owns the last reference to its first part and the new bytes fit in that cell's last word, it writes them there. Growing a cell to make room is a later step with its own benchmark. `replace`, `join` and `concat` are linear built-ins that search twice and keep nothing outside the heap. A map keeps today's CHAMP layout. What changes is the walk: an edit first tries to finish inside one unique node, and otherwise walks down once remembering its path, edits the bottom node and walks back up, with every reference change written in one table. Owned edits come from liveness. A lookup that is matched at once builds no `Some`: the loader's pattern is widened to cover the cases that exist today, and the general IR form (`Find`) lands with the array and inlining work. `map::fold` walks a position that is a plain Int.

## What I checked before writing this

| Question | Evidence | Outcome |
|---|---|---|
| Is "count is 1 and the register is dead" enough to write in place? | Only while every register holds a counted reference. That is true today: every read into a register shares (exec.rs `Move` 153, `GetGlobal` 162, `Field` 480, `MapFind` 733, non-moved arguments 2275). The diagnosis's causes 14 and 2 plan borrowed registers. | S0 below is now an invariant of its own with one upholder. |
| What does a map edit cost once references are really counted? | New probe with real counts, a copy path and collision cells: 1,168 a delete-and-set pair on a model of today's heap, 1,038 on a flat arena, 1,479 when every edit takes the general walk. The earlier 890 counted nothing and had no copy path. | map_churn's gain falls from 13.1% to 9.4%. |
| Is the gain in the walk or in the layout? | My probe keeps today's layout. Design C's uncounted kernel on today's layout gives 861 a pair, the earlier head-node layout 849 with its hashes. For a get it is 296 on today's layout against 212 on the head-node, children-first layout: one cell lookup (about 28, read in the machine code) and one population count a level (about 5 each). | Size classes and children-first are dropped. The head node is its own small step. |
| What does richards' shared set cost? | 1,745 for a set on a shared 6-entry map of cells and the release of the old map. In the same model a cell made and freed is 252 and a count up and down is 66. Two cells and eleven count pairs are most of it. | The claim of 8.1% of richards becomes 0.7% for the walk and 2.0% for the head node. |
| What does `Concat` cost as an instruction? | Critic 2's handler, rerun: 256 a Concat on string_keys' mix with literal cells. With literals as frozen values: 231. An in-place append is 221, not 150. | Every string_keys number is restated from the handler. |
| What does a literal cost? | In the handler model a sticky literal cell bound and released costs 57 and an Int constant 22. A frozen value costs 26. | Literals are frozen values, not sticky cells. |
| Does `Find` reach json? | `scarlet dis benches/json/json.sca`: `capture` calls `fn#17` (the `slice_bytes` wrapper) and passes `fn#100` (`BinaryToString` as a value). Both `Ok` cells are built in callees. | json's 1.5% is removed and `Find` leaves this area's sequence. |
| Is a second search cheap enough to drop the offsets buffer? | 16.2k against 11.0k for 50 matches in 2.3 KB (measured). That is 0.04% of the strings benchmark. | Two passes, no buffer. |
| What does a position cost? | Critic 3's packed form, rerun: 284 an entry on 55,328 entries against 117 for a stack of nodes, and 134 against 86 on 6. | Kept, quoted at 284, built last. |

## S0: a register owns what it holds

Every in-place write in this design (and `ArrayAppend`'s today) reads "this register is dead after me and the cell's count is 1" as "nothing else reaches the cell". That holds only under this rule:

**S0. A register that holds a cell holds one counted reference to it. An instruction may take a register (`own_first`, `Arg.moved`, `Move.moved`) only when S0 holds for that register.**

- Today it holds for every register, because every instruction that writes a register either makes a new cell or shares the one it read.
- The loader is the one upholder. `moves_on_last_use` (code.rs:2052) sets every such flag, and it gains one input: the set of registers that own their reference. Today that set is all of them. When borrow inference lands, the pass that marks a register borrowed must pass that set in, and a borrowed value that reaches an owning operand is shared first. That is Lean's rule: its borrow inference marks a value owned when it is "subject to reset-reuse ... so their reference count accurately reflects the real amount of references".
- A loader test marks a register borrowed and asserts that no instruction takes it. A VM test runs `match r { R(name, _) -> f(r, name + "!") }` and checks `r.name` afterwards.

The hard case the critic gave is exactly that match: with a borrowed `name`, the cell's count is 1 (held by `r`), `name` is dead after the concat, and an append would rewrite `r.name`. S0 forbids the flag, so the concat copies.

## Data layouts

Every heap cell keeps today's header: the count in 32 bits, the kind in 4 and the size in words in 28. No kind is added (all fifteen are in use).

### String

```text
word 0    header
word 1    length in bytes
word 2..  the bytes, eight to a word, first byte lowest
```

This is today's layout and today's size rule: a string cell has exactly `ceil(len / 8)` data words. `"aten"` is `[1 | String | 3][4]["aten" then four zero bytes]`. The four zero bytes are the only spare room a string has until the growth step, and they are what the in-place append writes into.

### The frozen area and the frozen value

```rust
// code.rs: part of the loaded program, never written after load
pub(crate) struct Frozen { words: Box<[u64]> }   // cells end to end: [header][len][data..]
pub(crate) struct Lit(u32);                       // a frozen cell's word offset
// Code { frozen: Frozen, .. }

// value.rs
const TAG_FROZEN: u64 = 0xFFFE_0000_0000_0000;    // payload: a Lit
// View::Frozen(Lit)

// heap.rs
// Heap<'c> { frozen: &'c Frozen, .. }            // so eq, hash and map lookups can read a literal
```

- The loader interns each `Const::String` and `Const::Binary` on the whole constant, which is the key `scarlet_ir` already uses (`ConstKey::String(s)`, `ConstKey::Binary(bytes, bit_len)`). The bit length is part of a binary's identity: `<<1:4>>` and `<<16>>` share the byte `0x10`.
- A literal loads as `Instr::Const { dst, value: Value::frozen(lit) }`. `Instr::Str` and `Instr::BinaryConst` go. Anywhere the loader accepts a constant operand, a literal now fits.
- `Value::as_cell` is `None` for a frozen value, so `share`, `release`, the frame sweep and the free walk skip it on the tag test they already make. No header is loaded.
- A frozen cell has the same words as a heap cell of its kind, so a reader that has the words does not care where they came from.
- `Heap::live()` never counts a literal, so the leak tests (`cells_left_after`) keep meaning what they mean.

Why a tag and not a sticky cell in the heap: a sticky cell still pays a release call that loads its header (measured: 57 against 22 for a constant). It has to be made again in every process's heap. And once literals move to a shared area, a `Cell` would name one of two heaps and every cell lookup of every kind would pay to choose. With the tag, only the string and binary readers choose.

### Binary

An owner keeps `[header][length in bits][bytes]`, and a slice keeps `[header][owner][bit offset][bit length]`. A slice still copies nothing. The owner word of a slice is a value, so it may be a frozen literal. `Bits` gains one question, `aligned()`: the bits start on a byte of the owner. When they do, the whole bytes are a borrowed `&[u8]`. Only a binary cut off a byte boundary goes through today's bit reader.

### Map

Today's layout, unchanged:

```text
map        [header][size][top node, or Nil]
node       [header][entry mask | child mask << 32][key value ..][child ..][spare ..]
collision  [header][hash][key value ..]
```

- A node that has to move to a bigger cell gets `ROOM = 4` spare words, as today.
- One rule replaces today's "spare words hold Nil": **a node's live words are the first `Used::len` words, read from its masks, and nothing reads past them.** `==`, the hash and `show` already read that way (map.rs:1040). The free walk joins them: `held` in heap.rs reads a `MapNode`'s length from its mask word. Spare words may then hold anything, and no edit refills them.
- A collision cell is exactly sized and sits 13 levels down, where the hash has run out. The walk knows it by depth.
- Step 11 makes the map value its top node (`[header][size][masks][entries][children]`), as the BEAM's `hashmap_head_t` does. It is separate because its gain is small and measured apart.

### A position in a map

```text
bits 0..3    depth: how many levels below the top the walk is
then 5 bits  per level: which child was taken there, counted among the node's children
above those  done: how many entries of the node it is in have been given
```

A position at depth 7 or less fits a small Int (4 + 35 + 6 = 45 bits of 47). A node at depth 8 needs only two keys that share 40 hash bits, which is likely from about 1.5 million entries, so the big-Int path is an ordinary one. The largest possible position is 97 bits (13 levels and a collision cell's count), so the VM reads it as an `i128`.

## text.rs: one door to read, one to write, one witness to write in place

```rust
/// The bytes of `words` in memory order. On a little-endian machine that is
/// "first byte lowest", the way every cell stores them.
pub(crate) fn bytes(words: &[u64]) -> &[u8] { bytemuck::cast_slice(words) }

/// A string, borrowed from the heap or the frozen area.
pub(crate) struct Text<'a> { data: &'a [u64], len: usize }
pub(crate) fn of<'a>(heap: &'a Heap, v: Value) -> Option<Text<'a>>;

/// The only way a string cell is made. It allocates `ceil(len / 8) + 2`
/// words, zeroes the last data word, sets the length, and hands `fill`
/// exactly `len` bytes to write.
fn write(heap: &mut Heap, len: usize, fill: impl FnOnce(&mut [u8])) -> Result<Cell, Full>;

/// The door for callers outside this module. A `&str` is the proof of UTF-8.
pub(crate) fn from_str(heap: &mut Heap, text: &str) -> Result<Cell, Full>;

/// A heap string cell whose count is exactly 1. Only `sole` mints one, and
/// only a `Sole` can be written in place.
pub(crate) struct Sole(Cell);
pub(crate) fn sole(heap: &Heap, v: Value) -> Option<Sole>;
```

`bytemuck::cast_slice` is a safe function, and `u64` to `u8` cannot fail on size or alignment. Every scratch crate compiles with `unsafe_code = "forbid"`. A `compile_error!` under `cfg(not(target_endian = "little"))` states the one assumption, and all four release targets are little-endian.

The heap gives three things, and text.rs, binary.rs and map.rs ask for nothing else: `Heap::cell(c) -> &[u64]` and `cell_mut` (a cell from header to end in one lookup), `Heap::pair(from, to) -> (&[u64], &mut [u64])` (two different cells, for a copy, the way `copy_words` at heap.rs:625 already splits the chunks), and `alloc` and `discard`. The kernels do not care whether the heap is chunks or one arena. These three need agreeing with the heap area before step 2.

`sole` returns `None` for a frozen value, for a cell that is not `Kind::String` (a big Int has the same shape: header, a length word, data), and for any count but 1. binary.rs has the same witness for a byte-aligned owner.

The readers:

- `==` compares the value words first (the same literal is the same word), then the lengths, then `ceil(len / 8)` words.
- The hash feeds SipHash the tag, the length, then `ceil(len / 8)` whole words. Always. A binary feeds its bit length and its bits as whole words, zero padded. So the hasher takes words only and has no tail buffer. The length goes in first, so the padding cannot make two different strings collide. This changes the hash of any string whose length is not a multiple of 8, so string-keyed maps change order once and golden outputs that print them move once.
- `string::length` is the byte length minus the continuation bytes, counted a word at a time: `len - (w & !(w << 1) & 0x8080808080808080).count_ones()` summed over the words. The mask marks bytes of the form `10xxxxxx`. Zero padding has no such byte.
- `contains`, `split` and `replace` search `bytes()` with `memchr` for a one-byte needle and `memchr::memmem` otherwise.
- `trim` looks at the first and last byte. When both are ASCII and not white space it returns the string itself. `to_graphemes` and a non-ASCII `trim` run `std::str::from_utf8` on the borrowed bytes, which is a check and not a copy.

The makers (`from_str`, split pieces, `trim`, `replace`, `join`, the Int and Float conversions, `BinaryToString`, and `Concat`'s general path) all go through `write`. A test dirties the free lists on purpose, runs every string-making built-in, and checks `==` and the hash against `from_str`.

## Instructions and IR

Names say what each does. The instruction format is another area's to pack.

```rust
Const { dst, value }                              // a literal is a frozen value
Concat { dst, parts: Box<[Part]>, own_first }     // Part { reg, show }
JumpUnlessStr { src, lit: Lit, to }               // the same word, or the same bytes
Move { dst, src, moved }                          // a dead source hands its reference over
ByteAt { dst, bin, index }                        // binary::byte_at, off the built-in path
BinaryAppend { dst, a: Arg, b }                   // the same in-place rule as Concat
MapSet { dst, map: Arg, key, value }              // Arg.moved from liveness
MapDelete { dst, map: Arg, key }
MapFind { dst, map, key, to }                     // as today
MapNext { map, at, key, value, next, to }         // step 14: three results
```

New intrinsics: `StringReplace`, `StringJoin`, `StringConcat`, `BinaryConcat`, and later `MapNext`. `int::from_string` and `binary::parse_int` become the built-ins themselves and return `Result`.

`own_first` and every `moved` come from `moves_on_last_use` by the rule `ArrayAppend` already uses, plus S0: the register owns its reference, nothing later reads it, and no other operand of the same instruction names it.

## Concat in full

The two-part form is the common one, so it is settled first, in this order, after one lookup of each part through `text::of`:

1. The right part is empty: the result is the left part, taken when `own_first` is set and shared otherwise.
2. The left part is empty: the result is the right part, shared.
3. The right part is 8 bytes or fewer and `text::sole` gives a witness for the left register (which needs `own_first`) and the bytes fit the left cell: OR the right part's one word, shifted, into the left cell's last word, add to the length, and the result is the same cell. This is the whole in-place append of step 6. It never crosses a word, because an exact cell's only room is the zero bytes of its last word.
4. Both parts are 8 bytes or fewer: allocate, store two words, and release the left register if it was taken.
5. Otherwise the general path.

The general path, for any number of parts:

1. **Gather.** Read each part once: its length and its first data word. A part with `show` set that is not a string is written into the machine's `scratch` buffer by the function `ToString` uses, and gives a range of it. Add the lengths. A total past `MAX_BYTES` is `Stop::Full`.
2. **Drop empty parts.** When one part is left and it is a string, the result is that string, shared.
3. **In place.** With a `Sole` for the first register and a total that fits its cell, write the other parts after its bytes.
4. **Copy.** Otherwise `text::write` a cell of exactly the total and copy each part in. Release the first register if it was taken.

The lowering flattens `(a + b) + c` into one `StringConcatMany` when the middle value has one reader, and `interp` stops emitting `ToString` for interpolated parts. A part's `show` flag is set when its type is not `String`.

`scratch` is a `Vec<u8>` owned by the machine, used only for shown parts. It grows with `try_reserve` and a failure is `Stop::Full`. It is cut back to 4 KiB after a larger use. It belongs to a scheduler thread, not a process, because no built-in yields part way.

### Growing in place (step 12, not before)

With a `Sole` whose cell is too small, allocate a cell whose size is the needed size rounded up to a power of two, copy, free the old cell without walking it, and write the rest. If the rounded size is past the largest cell, use the exact size. Once a cell is a power of two, any need past it at least doubles it, so a run of appends is linear and the slack is never more than twice. From then on a string's capacity is its cell's size and its spare words are dirty, so `text::store` assigns a word it starts and ORs into a word it continues. Nothing on the scoreboard shows this step working or failing, so it lands with a new benchmark that builds a long string by appending.

## Linear built-ins with nothing outside the heap

`string::replace(s, pattern, to)`, `string::join(parts, sep)`, `string::concat(parts)` and `binary::concat(parts)` each run two passes. The first counts and adds up the result's size. The second `text::write`s one cell and copies the runs. `replace` builds one `memmem::Finder` and uses it for both passes. `split` counts its pieces first, asks the array module for a builder of that many items, and then makes each piece and hands it over.

There is no buffer of match offsets. A string can be 2 GiB, so a one-byte pattern could ask for 8 GB of host memory before the heap is asked for anything, and a `Vec` that cannot grow aborts the program. The second search costs 5.2k on a 16.2k replace (measured), which is 0.04% of the strings benchmark.

## The map edit in full

`set(heap, map, key, value, owned)` and `delete(heap, map, key, owned)` are the two entry points. `owned` is the instruction's `map.moved`. With it, the caller's reference to the map passes in and the result's passes out. Without it, the map is only read and the result is a new reference. The key and the value are read, and shared when the map keeps them.

**The hash is taken once**, before anything else.

### First, try to finish inside one node

When `owned` is set, walk down from the map cell while each cell's count is 1. Three cases end there with one write and no path:

- The key is there: share the new value, store it, release the old one. Nothing above changes, not even the size.
- The slot is empty and the node's cell has room: share the key and value, shift the words after the slot up by two, store, set the mask, add one to the size.
- (Delete) the key is there and the node stays a node (it keeps a child, or more than two entries): release the key and value, close the gap, clear the mask bit, take one from the size.

Each case decides before it writes. Anything else (a shared node, a slot another key holds, a full cell, a node that would be left with one entry, a collision cell) makes no change and falls to the walk below. On map_churn 79% of edits end here.

### Otherwise, the walk

1. **Down.** From the map cell, follow the hash five bits a level and read each node once. Write each `(node, the word above that names it, unique)` into a `[Step; 14]` on the Rust stack: 64 bits of hash is at most 13 levels and a collision cell. `unique` starts as `owned && count(map) == 1`, is cleared at the first cell whose count is not 1, and is never set again.
2. **Edit** the bottom node by the table below.
3. **Up.** Carry what is left of each node to the node above, by the table below. The walk stops at the first unique node that takes the change without moving.
4. **The map cell.** A unique map cell has its size and top stored. Otherwise a new map cell is made, and if the caller gave its reference, the old map is released.

No function returns an enum through memory and no node goes through a `SmallVec`. `insert`, `insert_owned`, `remove`, `remove_owned`, `settle`, `settle_cut`, `reshape`, `Gone` and `Words` go, and so do `Heap::splice` and `Edit`, which only map.rs calls. The `smallvec` dependency stays: array.rs and exec.rs use it.

### The reference ledger

Every node edit goes through one function, `change`, which makes one of two moves:

- **In place** (the node is unique): words shift inside the cell. When it is full, every word moves to a bigger cell and the old cell is freed without a walk. No count changes either way.
- **Copy** (the node is shared): a new exact-size cell gets the words that are kept, and each value among them gains a reference. The old node is untouched.

Words taken out are the caller's to deal with, and words put in arrive owned. The rows:

| Change | Node unique | Node shared |
|---|---|---|
| Replace a value | Share the new value, store it, then release the old one. In that order. | Copy without the old value. Put the new value in, shared. |
| Put an entry in an empty slot | Share key and value. In place. | Copy. Share key and value. |
| The slot holds another key | The other entry's references move into a new sub-node, with the new key and value shared. The entry's two words go out and the sub-node's one comes in. | Share the other entry into the sub-node, with the new key and value shared. Copy without the entry, with the sub-node in. |
| Take an entry out, node stays | Release key and value. In place. | Copy without the entry. |
| Take an entry out, one entry left, below the top | Release the removed entry. The other entry's references move up. Free the node without a walk. | Share the other entry. The node is untouched. |
| Take the last entry out of the top | Release it and free the node. The map's top is `Nil`. | The map's top is `Nil`. |
| Collision cell | The same rows. An added or removed entry means a new exact cell and the old one freed without a walk. Two entries with one taken leaves a lone entry. | The same rows with copies. |

Going up, with what the node below left:

| What came up | Node unique | Node shared |
|---|---|---|
| The child's cell changed | Store the new child. If the old child was shared, release it. A unique old child was already freed when it moved. Stop here. | Copy without the old child word. The new child moves in. Carry on up. |
| A lone entry, and this node has other entries or children, or is the top | If the old child was shared, release it. The child's word goes out and the entry's two come in, in place. | Copy without the child word, with the entry in. |
| A lone entry, and this node below the top holds only that child | If the old child was shared, release it. Free this node without a walk. The lone entry carries on up. | Nothing. The lone entry carries on up. |

The last row is CHAMP's rule, and it is what keeps equal maps the same shape. The probe runs all of this with real counts, and its audit caught each of nine count changes I removed on purpose.

## Lookups that box

**Now (step 9).** The loader's `MapGet` pattern (code.rs:1384) requires the `match` to follow the lookup directly. richards' `find_task` and json's `get` have `drop %2` between them, so they build a `Some`. The pattern learns to step over `Drop`s of other locals and to emit them at the head of both arms. `MapFind` shares its hit before anything is dropped, so the order is safe.

**Later, with the array and inlining work: `Find`.**

```rust
// scarlet_ir::core_ir
CoreExpr::Find {
    probe: Probe,      // MapGet, MapNext, IntFromString, BinaryIndexOf, BinaryParseInt,
                       // BinaryToString, BinarySliceBits, ArrayIndex, ArraySlice
    args: Vec<LocalId>,
    binds: Vec<Bind>,  // what a hit binds: one local, or the parts of a tuple
    hit: Box<CoreExpr>,
    miss: Box<CoreExpr>,
    ty: RTy,
}
```

The lowering emits one form for every call of a partial built-in: hit builds `Some(v)` or `Ok(v)`, miss builds `None` or `Err(Nil)`, and both jump to a join. One general rule then removes the box: a join whose body matches on its parameter, reached only by jumps that pass a known constructor, sends each jump straight to its arm. That is the known-constructor rule over join points. It is one rule where the earlier draft had three hand-written forms, and it is the rule the inliner needs anyway to make `result::then` free. When it lands, `option_arms` and the `MapGet` pattern go, and the VM's built-ins stop building `Some`, `Ok` and `Err`.

`Find` is not in this area's sequence because it earns nothing here that step 9 does not already get. json's two `Ok` cells a capture sit behind a wrapper and a function value, so they need inlining first.

## `map::fold` over a position (step 14)

```
pub fn fold(m Map(k, v), init r, f fn(r, k, v) r) r {
	walk(m, 0, init, f)
}

fn walk(m Map(k, v), at Int, acc r, f fn(r, k, v) r) r {
	match next(m, at) {
		Some((key, value, after)) -> walk(m, after, f(acc, key, value), f)
		None -> acc
	}
}

// The entry at position `at` in the map's order, and the position after it.
@vm(map__next)
fn next(m Map(k, v), at Int) Option((k, v, Int))
```

`next` is a `Find` with three binds, so no `Option` and no tuple is built. `MapNext` is the first instruction with several results, and the loader has to learn that:

- `Instr::writes` (code.rs:734) returns one register. It changes to yield every written register, and `allocate` adds clash edges between each of them and live-out, and between each other.
- The handler reads every operand and shares every result before its first register write. The allocator may give a result the register of an operand that dies here, so writing `key` first could free the map and the value with it.

A position names no node, so it cannot go stale, it needs no new kind of cell, and it copies in a message like any Int. It costs 284 an entry on a large map against 117 for a cursor that keeps its nodes. Erlang's iterator is the same integer path, but `erts_internal:map_next` pays the walk from the top once a batch, by building a chain of `{K, V, Next}` tuples. Binding into registers builds nothing and pays the walk every entry.

`merge`, `filter` and `from_list` stay Scarlet folds. Their `set(acc, ..)` is owned once `moved` comes from liveness.

## Invariants, and what upholds each

| # | Invariant | What upholds it | What relies on it |
|---|---|---|---|
| S0 | A register that holds a cell holds one counted reference, and only such a register is taken. | `moves_on_last_use`, the one place the flags are set, from the set of owning registers. | S4, M2, and `ArrayAppend` today. |
| S1 | The bytes from a string's length to the end of that word are zero. | `text::write` zeroes the last word before `fill` runs. The in-place append only ORs bytes into zero positions. | Word compare, feeding words to SipHash, the scalar count. |
| S2 | A string's bytes are UTF-8. | `text::from_str` takes a `&str` and is the only maker outside text.rs. Inside, the kernels join whole strings or cut at a match of a whole string, and UTF-8 is self-synchronising. | Counting scalars and cutting on bytes. No safety relies on it: `to_graphemes` checks and stops with `Broken::NotUtf8`. |
| S3 | A string cell has exactly `ceil(len / 8)` data words. (After step 12: its capacity is its cell's size, and only the length says how many words are live.) | `text::write` is the only maker. | The in-place test. |
| S4 | A string or binary is written in place only through a `Sole`. | `text::sole`: a heap cell, `Kind::String`, count exactly 1. The handler asks for it only when `own_first` is set. | Every reader of strings. |
| S5 | A literal is never written and never freed. | The frozen area is a `Box<[u64]>` inside `Code`, and nothing holds a `&mut` to it after load. `sole` returns `None` for a frozen value. | Sharing literals between processes with no counts at all. |
| S6 | The hash reads the words `==` reads. | Both go through `Text` and read `len` and `ceil(len / 8)` words. | Map lookups. |
| M1 | A map's nodes depend only on its entries. | The lone-entry rows of the ledger. Room is not part of the shape. The probe builds every snapshot's entries again in another order and compares live words node by node. | `==` node by node, the order of keys. |
| M2 | A node is edited in place only when every cell from the map down to it has count 1 and the caller gave the map up. | The `unique` flag, cleared once and never set, starting from `owned`. | Every other holder's map staying as it was. |
| M3 | Nothing reads a node past the words its masks count. | `Used::len` in map.rs and `held` in heap.rs are the only sources of a node's length. | The free walk, `==`, the hash. |
| M4 | A key's hash agrees with `==`. | `hash::hash` reads a value the way eq.rs does. | Lookup. |
| M5 | A position is only a place to look. | `MapNext` bounds-checks every word it reads from it. | Nothing: a wrong position gives a wrong entry or ends the walk. |
| F1 | No built-in calls Scarlet code. | `fold`, `merge`, `filter` and `from_list` are Scarlet loops over instructions. | A call being the only place the scheduler yields. |
| X1 | A `Stop` ends the machine, and nothing reads its heap afterwards. | `scarlet_vm::run` (lib.rs:61) drops the machine on any `Stop`. | An edit that runs out of heap part way (a pull-up that must grow the node above after the node below is freed) leaving a map half changed. |

## Worked examples

### fib: nothing changes

fib has no string, binary or map, and its eight instructions are the same before and after (`JumpUnlessCmpImm`, `Ret`, `IntImm`, `CallSelf`, `IntImm`, `CallSelf`, `Int`, `Ret`). The one new value tag is tested only where a value is read as a string or a binary. Int paths test the Int tag and register writes test the cell tag, and neither changes. The count stays at 355 a call.

### string_keys: `spell`

```text
fn main.spell(%0, %1, %2) =
  ...
  letj %13 = let %9 = global0; let %11 = IntRem(%0, 6); let %12 = c5; ret ArrayIndexOr(%9, %11, %12)
  let %14 = StringConcat(%2, %13)
  ret call self(%6, %8, %14)
```

One letter, before and after:

```text
before                                              after
JumpUnlessCmpImm %1 == 0             40             the same                                40
IntImm Div, Sub, Rem                178             the same                               178
GetGlobal %9 = letters               63             the same                                63
Str %12 = ""        a new cell      185             Const %12 = ""   a frozen word    about 31
ArrayIndexOr %13 = %9[%11] or %12   290             the same, less a share            about 270
Concat %14 = [%2, %13]              840             Concat %14 = [%2!, %13]           about 250
TailCallSelf (%6, %8, %14)          631             TailCallSelf, nothing to free     about 245
                             about 2,230                                          about 1,080
```

`%2!` marks `own_first`. The six letters in the `letters` array are literals, so they are frozen values: reading one shares nothing and dropping one releases nothing. A word goes like this, with the handler's measured costs:

- The first letter meets `""`, which is empty, so the result is the letter itself. 130.
- The second letter meets a frozen value, which gives no `Sole`, so the two short parts go into one new cell. 372 with that cell's free at the end of the word (the model's free is about 90 cheaper than heap.rs's, so about 460 in the VM).
- Every later letter fits in that cell's one data word and is ORed in. 221.

Over 8.5M letters and 2M words the average Concat is 231 in the model and about 250 in the VM.

In `count`, `map::get(m, w) or 0` is a `MapFind` and the set is owned, as today. Both stop copying the key: the hash reads the key's words (127 for 8 bytes) and the compare is a length word and one data word.

### strings: the benchmark's own `join`, and `replace`

```text
fn main.fn(%0, %1) =             before                           after
  let %2 = capture0
  let %3 = StringConcat(%0, %2)  copies all of acc out and in     one Concat [%0!, %2, %1]:
  ret StringConcat(%3, %1)       and again                        one cell made, three copies in
```

Fifty rows cost 1.38 million today. With one exact copy a step they cost 43.0k in the kernel (50.5k on heap.rs's real free lists), plus the fold's own 50 steps. After the growth step they cost 23.6k (24.2k). So the byte-at-a-time copy is nearly all of today's cost, and growth is worth another half of what is left.

`string::replace(row, "=", ": ")` is a split, an array walk and a fold of two concats a piece today. After, it is one built-in: count the matches, allocate once, copy the runs.

### map_churn: `churn`

```text
%8 = MapDelete(%0, %6); drop %0      MapDelete %8 = %0!, %6
%9 = MapSet(%8, %7, %3); drop %8     MapSet %9 = %8!, %7, %3
```

Of a million pairs in the probe, 828 thousand deletes and 841 thousand sets finish inside one node. The rest take the walk: 172 thousand deletes leave a lone entry that moves up, 221 thousand sets meet another key in the slot, and 138 thousand sets move a full node to a bigger cell. The pair costs 1,168 with its two hashes, against 1,775 inside map.rs today.

### richards: `find_task`

```text
before                                  after step 9
let %2 = Field(0)(%0)                   Field %2 = %0.0
drop %0                                 Drop %0
let %3 = MapGet(%2, %1)                 MapFind %4 = %2[%1] else L
drop %2                                 Drop %2;  Ret %4
match %3                             L: Drop %2;  GetGlobal; Ret
  | None -> drop %3; ret global10
  | Some(%4) -> drop %3; ret %4
```

## What each operation should cost

"Kernel" is a probe number with its allocation and free and without dispatch. "Handler" is measured in the handler model, with register reads, tag and kind tests, take, release and the write. "In the VM" is an estimate unless it says measured. Kernels marked (M) are the earlier merged probe's on a model of today's heap, and (B) and (C) are the earlier designers'.

| Operation | Today | Kernel | Handler or in the VM |
|---|---:|---:|---:|
| `""` literal, bound and dropped | 360 | 0 | 26 measured in the handler (an Int constant is 22) |
| Concat, empty + short | about 840 | | 130 measured |
| Concat, short + short, new cell, freed | about 950 | 362 (M) | 372 measured, about 460 in the VM |
| Concat, short part in place | 840 and a free | 87 (M) | 221 measured |
| Concat, average on string_keys' mix | about 840 and a free | | 231 measured, about 250 in the VM |
| Concat 512+512 | about 11,400 | 728 (M), 906 on real free lists | about 1,000 |
| `"  " + row + " "` as one concat | about 3,800 | 562 (M) | about 710 |
| `s == t`, 8 / 512 bytes | 857 / 5,583 | 88 / 347 (M) | about 150 / 410 |
| Hash a 2 / 8 / 64 byte string | inside a 1,650 get | 116 / 127 / 232 (M) | the same |
| `string::length`, 8 / 1,024 bytes | 641 / 6,803 | 56 / 624 (M) | about 240 / 810 |
| `split("score=12345", "=")` | 4,297 | 518 (B), 635 (C) | about 900 and the array |
| Fold join of 50 rows, exact copy a step | 1.38M | 43.0k (M), 50.5k on real free lists | about 185k with the fold's own steps |
| The same after growth | 1.38M | 23.6k (M), 24.2k | about 160k |
| `replace`, 50 matches in 2.3 KB | 1.43M | 16.2k two passes, 11.0k with an offsets buffer | about 17k |
| String-key get, 50,000 keys | 1,664 | 405 (M, head-node layout), about 450 on today's | about 600 |
| String-key set in place | about 1,930 | 484 (M), about 530 on today's | about 700 |
| Int-key get, 200,000 keys, with its hash | about 370 in the VM | 296 on today's heap, 262 flat | about 370: no gain until step 11 |
| Replace a value in place, 200,000 keys | | 378, 336 flat | |
| Delete and set, 200,000 keys, with hashes | 2,123 (1,775 in map.rs) | 1,168, 1,038 flat | about 1,520 |
| The same, every edit through the general walk | | 1,479 | |
| Set on a shared 6-entry map, old map released | 1,951 | 1,745, 1,567 flat | about 1,850 (about 1,560 after step 11) |
| Set in place on a 6-entry map | 441 | 259, 222 flat | about 400 |
| A cell made and freed / a count up and down | | 252 / 66 | |
| `map::fold`, an entry of 55,328 | about 3,000 | 284 by position, 117 by a stack | about 1,150 |
| `binary::byte_at` | 294 | 68 (earlier design A) | about 100 |

## What this area claims, and what other areas must not also count

- **Cell addressing (cause 8)** claims 6% of map_churn. After this design the flat arena takes the churn pair from 1,168 to 1,038 and a get from 296 to 262, which is about 2.8% of map_churn.
- **Alloc and free (cause 4)** claims 6% each on string_keys and strings. This design removes most of those cells (about ten a word become one), so what is left for cause 4 there is small.
- **The self tail call (cause 11)** prices `spell`'s tail call at 631. 387 of that is freeing two string cells, and it goes here.
- **Built-in calls.** `ByteAt` saves 194 a call, and 65 to 90 of that is leaving the built-in call path. About 3 of json's 8 points go to whichever area makes a built-in call cheap.
- **Arrays and inlining.** About 5 of step 7's 6.5 points on strings are array walks and closure calls that the `replace` built-in stops running.
- **richards.** If the compiler moves a field out of a dying record, the map is unique at the set (441 today) and steps 10 and 11 gain nothing on that set. The `Some` round trip is 4.5% today and about 2% once a small cell costs 150 to 200.

Multiplied step by step, the area's geomean factor is about 0.936. Added per benchmark it is about 0.927. The diagnosis's two causes were 0.940 x 0.982 = 0.923, so this design claims a little less than the diagnosis hoped, and the shortfall is in maps.

## What is not in the design, and why

- **Buffers outside the word heap, string windows, or a second memory system.** Liveness and the count already say when a string is uniquely held.
- **A literal as a sticky cell in each heap.** Measured dearer (57 against 26), made again per process, and it leaves the two-heap choice for every cell reader later.
- **A cached hash or a cached scalar count in every string.** A second hash of a short key costs 116 to 127, which is 0.4% of string_keys. A cache costs a word in every string and a write on a read path that frozen literals could not take.
- **String slices.** `split` copies each piece. Binaries keep their slices for the job of cutting a few large pieces out of a large buffer.
- **Size classes and children-first map nodes.** No measured gain on an edit. Children-first saves about 5 a level on a get, read in the machine code, and I did not think that worth a second order of words to learn.
- **A cursor cell for `fold`.** There is no kind left for one, and a cursor that holds node references needs a rule about who may hold it.
- **`map::update` over a private `take`, `split_once`, `starts_with`, `ends_with`.** Stdlib surface with no cause in the diagnosis. They are easy to add on text.rs and the walk later.
- **A flat map for small maps.** An earlier probe measured it slower than a one-node trie here (151 against 131 for a get).

## Steps

### 1. 1. One door for strings and binaries, and literals frozen in the program

**Changes.** crates/scarlet_vm/src/value.rs: `TAG_FROZEN`, `Value::frozen`, `View::Frozen(Lit)`; every match on `View` gains the arm (28 sites name `View::Cell`, and the no-wildcard lint finds them all). code.rs: `Frozen`, `Lit`, `Code.frozen`; the loader interns `Const::String` and `Const::Binary` on the whole constant (text or binary, bit length, bytes), writes each as a cell-shaped run of words, and loads it with `Instr::Const`; delete `Instr::Str` and `Instr::BinaryConst`; `JumpUnlessStr` and `JumpUnlessBinary` name a `Lit`. heap.rs: `Heap` holds `&Frozen`. New text.rs with `Text` and `of(heap, v)`; binary.rs `bits` takes a `Value` and `Bits.owner` is a value. Every reader that takes a cell and asks its kind today (`is_string`, `text_of`, `utf8_of`, eq.rs, hash.rs, show.rs, json.rs, http.rs, map key compare) goes through the two doors, still copying out where it copies today. Tests: two binary literals with the same bytes and different bit lengths stay distinct; a literal used as a map key finds a heap string with the same bytes; `cells_made` expectations for literals in loops move.

**Expected gain.** Alone, today: a `""` bound and dropped goes from 360 to about 31 (26 against 22 for an Int constant, measured in the handler model). string_keys: 10.5M evaluations x 329 = 3.45G of 28.70G, 12.0%, and the six letters stop being counted on every read (about 70 a letter x 8.5M = 0.6G, 2.1%, from the handler model's 118 a letter less the literal's own 31); I take 13%. strings: about 24 literals a row x 329 = 7.9k of 255.3k, 3.1%. json: 1.6% (the json report's ablation). Geomean factor exp((ln 0.87 + ln 0.969 + ln 0.984) / 23) = 0.992. If alloc and free get cheap first (cause 4), the 360 shrinks to perhaps 150 to 200 and this step is worth about half.

**Risk.** Medium, because it is wide: a new value tag touches every match on `View`, and every string and binary reader must take both forms. The compiler finds the sites, and what each does is one line (go through the door). The tag 0xFFFE must be agreed with whoever else wants a value tag. Nothing can write a literal: the frozen area has no `&mut` after load.

### 2. 2. Equality and hashing read words

**Changes.** Cargo.toml: add `bytemuck`. heap.rs: add `cell`, `cell_mut` and `pair` (agree these three with the heap area first); delete `string_is` and `padded_word`'s readers. text.rs: `bytes`, `eq`. eq.rs: the String arm compares the value words, then lengths, then words. hash.rs: a string feeds its length and `ceil(len / 8)` whole words; a binary feeds its bit length and whole words (borrowed when aligned, assembled through the bit reader when not); `Sip::bytes` and the hasher's tail buffer go. Golden outputs that print a string-keyed map are regenerated once. A unit test checks a slice and an owner with the same bits hash the same.

**Expected gain.** Alone, today. string_keys: a string get from 1,664 to about 600 (kernel 405 on the head-node layout, about 450 on today's, plus the MapFind shell) over 2M, and an in-place string set from about 1,930 to about 700 over 1M: 2.1G + 1.2G = 3.3G of 28.70G, 11.5%. deriv: 1.5% (string `==` from 859 to about 150). json: about 0.5%. Geomean factor exp((ln 0.885 + ln 0.985 + ln 0.995) / 23) = 0.994. This does not shrink when other areas land: it is malloc and byte loops, which nothing else removes.

**Risk.** Low to medium. The hash changes for most strings, on purpose, so every string-keyed map changes its order once. That is a breaking change to observable output and wants a changeset. After it, `==` and the hash read the same words by construction.

### 3. 3. Search and measure built-ins read in place

**Changes.** text.rs: `write` (the one maker), `from_str`, `scalars`, `find`, `split`, `trim`. exec.rs: `StringLength`, `StringContains`, `StringTrim`, `StringSplit`, `StringToGraphemes`, `StringInspect`, `Print` and `ToString` read through `Text` and make through `write`; `text_of`, `utf8_of`, `strings`, `Heap::string` and `Heap::read_string` go. `split` counts its pieces, then makes each and hands it to the array module (its present push until the array area gives a builder). Add the dirty-heap test: fill the free lists with cells of all-ones, run every string-making built-in, compare `==` and the hash with `from_str`.

**Expected gain.** Alone, today, per row of strings' 255.3k: ten field splits from 4,297 to about 1,700 with the array as it is, 26k (10.2%); the row split 1.4%; the two batch splits 1.5%; length 1.0%; contains 1.3%; trim 0.3%. That sums to 15.7% and I take 14%. json about 0.5%. Geomean factor exp((ln 0.86 + ln 0.995) / 23) = 0.993. How much of split's fixed 3,619 is the array build was never separated, so if the array area lands first this step's split number is smaller and theirs is larger.

**Risk.** Low. `split` with an empty separator must still give one piece per scalar value. The split figure is the softest number in the plan.

### 4. 4. Int and Float conversions without a bignum or a copy

**Changes.** text.rs: `to_int` (an i64 path, `num_bigint` only past 18 digits) and `from_int`, `from_float` written through `write`. exec.rs: `IntFromString`, `IntToString`, `FloatToString`.

**Expected gain.** Alone, today, strings: three `int::from_string` a row from 2,838 to about 1,070 (2.1%) and five Int-to-string from 1,035 to about 450 (1.1%), 3.2%. Geomean factor exp(ln 0.968 / 23) = 0.9986.

**Risk.** Low. The printed form of every Int and Float must not change, and the existing golden tests cover it.

### 5. 5. Concat allocates once, for the whole expression

**Changes.** crates/scarlet_lower/src/lower.rs: flatten a chain of `StringConcat` whose middle value has one reader into `StringConcatMany`; `interp` passes each part as it is and stops emitting `ToString`. code.rs: `Instr::Concat { dst, parts: Box<[Part]>, own_first }` with `Part { reg, show }`; `own_first` is always false in this step. text.rs: `concat`, with the two-part order (right empty, left empty, both short, general) and the general gather-then-write path. exec.rs: the `Concat` arm and `Machine::num`'s string `+` call it; a `show` part is written into the machine's `scratch` by the function `ToString` uses; `scratch` grows with `try_reserve` (failure is `Stop::Full`) and is cut back to 4 KiB after a larger use.

**Expected gain.** Alone (after step 1), from the handler model. string_keys: today a letter's Concat is 840 and the old accumulator's free about 217, so 8.98G over 8.5M letters. After: 2M first letters at 130, 2M second letters at about 460 with the cell's free, 4.5M later letters at about 450 (362 measured, plus about 90 because the model's free is cheap): 3.2G. That removes 5.8G of 28.70G, 20%. strings: 17 small concats a row at about 1,900 become about 9 three-part concats at about 710 (562 in the kernel), 25.9k (10.1%); the row's interpolation from 12.5k to about 2k, about 3.5% and not measured; the two batch folds from 2.79M to about 0.37M a batch (43.0k to 50.5k of copying each, plus the fold's own 50 steps at about 2,700), 48.4k a row, 19.0%. That sums to 32.6% and I take 29%. Geomean factor exp((ln 0.80 + ln 0.71) / 23) = 0.976. Cause 4's cheaper alloc and free would take part of string_keys' share (a cell is still made and freed for most letters until step 6).

**Risk.** Low to medium. Flattening changes which middle strings exist, so allocation-count tests move. A `show` part must print exactly what `ToString` prints, which is why both call one function. A string of 64 words or more still meets heap.rs's hash-map free list (177 a cell, measured by critic 2); that is the heap area's to fix.

### 6. 6. Append in place when the bytes fit the cell

**Changes.** code.rs: `moves_on_last_use` takes the set of owning registers (all of them today), sets `own_first` on `Concat` and `moved` on a new `Instr::Move { dst, src, moved }`. text.rs: `Sole` and `sole`; the in-place case of the two-part order (OR one shifted word into the last data word) and of the general path. exec.rs: the `Concat` arm asks for a `Sole` only when `own_first` is set. `scarlet/internal` gains `strings_appended_in_place`. Tests: the loader never takes a register marked borrowed; `match r { R(name, _) -> f(r, name + "!") }` leaves `r.name` alone; a big Int as a `show` first part is never written; a literal first part is never written. No layout rule changes: a cell still has exactly `ceil(len / 8)` data words.

**Expected gain.** Alone (after step 5), measured in the handler model: a later letter of `spell` falls from 332 with an exact copy to 191 in place, 141 saved, or about 230 with heap.rs's dearer free. Over 4.5M letters that is 0.63G to 1.04G of 28.70G, 2.2% to 3.6%; I take 3.3%. strings: nothing, because its folds outgrow one word. Geomean factor exp(ln 0.967 / 23) = 0.9985. If alloc and free get cheap first this is nearer 2%.

**Risk.** Medium. S0 and S4 are the whole safety argument. S0 must be agreed with the calls and Perceus areas before this lands, because their borrowing is what would break it. The append cannot be seen in the source: a program that keeps the old accumulator alive copies instead, correctly, with no sign. The counter makes that testable.

### 7. 7. replace, join and concat are linear built-ins

**Changes.** crates/scarlet_ir/src/intrinsic.rs: `StringReplace`, `StringJoin`, `StringConcat`, `BinaryConcat`. string.sca: `replace` becomes `@vm(string__replace)`; add `join(parts, sep)` and `concat(parts)`. binary.sca: `concat` becomes `@vm(binary__concat)`. int.sca: `from_string` is the built-in itself and returns `Result`, so `from_string_raw` and its wrapper go; the same for `binary::parse_int`. text.rs: `join` and `replace`, two passes each with one `memmem::Finder` and no buffer. exec.rs: the arms; code.rs `built` lists the arities. Stdlib call sites that fold to join (http/client.sca, oauth2.sca) call the built-ins.

**Expected gain.** Alone (after steps 3 to 5), strings: `string::replace(row, "=", ": ")` is about 13.5k (a split, a five-element array walk at about 2.1k each, closure calls and concats) and becomes about 1,400 (1,186 in design C's kernel plus the built-in path and the second search), 12.1k a row, 4.7%; the batch replace loses its fold's 51 walk steps and 50 closure calls, (185k - 17k) / 50 = 3.4k a row, 1.3%; three `from_string` lose a wrapper call, 0.5%. Together 6.5%. Geomean factor exp(ln 0.935 / 23) = 0.997. About 5 of the 6.5 points are array walks and closure calls, so if the array and call areas land first this step is worth about 1.5%. The benchmark's own `join` is user code and is fixed by step 5, not this one.

**Risk.** Low. `replace` with an empty pattern must still put the replacement between every two scalar values. Depends on step 3 for `write`.

### 8. 8. Binaries are read as bytes

**Changes.** binary.rs: `Bits::aligned` and a borrowed `whole_bytes`; `equal`, `has_at`, `bytes_are`, `join` and `to_string` take the byte path when aligned; `read_uint` and `from_int` get a u64 path for widths up to 56 bits, with no `num_bigint`. crates/scarlet_ir: `PrimOp::BinaryByteAt`. code.rs and exec.rs: `Instr::ByteAt`, and `Instr::BinaryAppend { dst, a: Arg, b }` with `moved` from liveness and the fit-in-the-cell rule of step 6 behind a witness for a byte-aligned heap owner; `BinaryToString` checks the borrowed bytes and makes the string through `text::from_str`; `BinaryFromString` copies words. Land it as three changes: `ByteAt` and aligned reads, the u64 integer paths, the append.

**Expected gain.** Alone, today. json: 38,753 `byte_at` a parse from 294 to about 100 is 7.5M of 93.4M, 8.0% (about 3 of those points are leaving the built-in call path, which the call area would also claim); cutting a string out (`slice_bytes` then `to_string`) from about 3,800 to about 1,800 with its two `Ok` cells still built, over 2,076 strings, is 4.15M, 4.4%. Together 12.4%. Geomean factor exp(ln 0.876 / 23) = 0.994. Off the scoreboard, `<<x:8, rest:binary>>` falls from 2,000 to 2,600 a byte to a few hundred.

**Risk.** Medium. binary.rs is bit-exact code with many callers. The bit reader stays as the path for binaries cut off a byte boundary, so there are two paths chosen by one test. I rebuilt none of this in a probe, so every number here is the earlier designers' or arithmetic.

### 9. 9. Owned map edits come from liveness, and the lookup pattern steps over drops

**Changes.** code.rs: `MapSet { dst, map: Arg, key, value }` and `MapDelete { dst, map: Arg, key }` replace `MapSetOwned`, `MapDeleteOwned` and the plain built-in forms; `moves_on_last_use` sets `map.moved` when the register owns its reference, is dead after the instruction and is not also the key or the value; the two `let; drop` recognitions go, and a register Perceus parked for reuse (`self.reused`) is still left alone. The `MapGet` pattern at code.rs:1384 steps over `Drop`s of other locals between the lookup and its `match` and emits them at the head of both arms. exec.rs: the two arms call the owned or the copying function by the flag. No change to map.rs.

**Expected gain.** Alone, today. richards: the `Some` round trip in `find_task`, 139,849 x about 450 = 63M of 1,404M, 4.5% (about 2% once a small cell costs 150 to 200). json's `get` has the same shape and is too rare to count. Geomean factor exp(ln 0.955 / 23) = 0.998. Off the scoreboard, `map::from_list`, `merge`, `filter` and any fold that ends in `set(acc, ..)` stop copying a path an entry: the maps study measured 4,622 an entry in tail position against 1,067 with the let-and-drop shape. Measure that with a micro-probe of `from_list`.

**Risk.** Low. It is the rule `ArrayAppend` already uses. The widened pattern is deleted when `Find` lands, so it is about twenty lines with a known end.

### 10. 10. The map walk: one node, or one path

**Changes.** map.rs rewritten on today's layout around `get`, `set(heap, map, key, value, owned)` and `delete(heap, map, key, owned)`: the attempt inside one unique node, then the `[Step; 14]` path, the `unique` flag, `change` as the one place a node's words move, and the ledger's rows. `insert`, `insert_owned`, `remove`, `remove_owned`, `settle`, `settle_cut`, `reshape`, `Gone` and `Words` go. heap.rs: `held` reads a `MapNode`'s live length from its mask word; delete `splice` and `Edit`. `smallvec` stays (array.rs and exec.rs use it). Move the probe's test across: random owned and borrowed edits over six live snapshots against `HashMap`, four hashers (the real one, one giving 8 whole hashes, one keeping only high bits, one mixed), an audit that every header's count equals the references that reach it, the shape rule, and each snapshot's entries rebuilt in another order and compared by live words. Run it with the one-node attempt on and off. Add a test that pins key order against a map built before the change.

**Expected gain.** Alone, today, from the counted probe. map_churn: the delete and set pair falls from 1,775 inside map.rs to 1,168 with its hashes, 607 x 3M = 1.82G of 19.71G, 9.2%, and the fill 0.2%; gets gain nothing (296 in the probe against about 262 to 317 in map.rs today). richards: the shared set from 1,951 to about 1,850 (1,745 in the probe plus the instruction's shell), 99,029 x 100 = 10M of 1,404M, 0.7%. string_keys 0.4%, json 0.3%. Geomean factor exp((ln 0.906 + ln 0.993 + ln 0.996 + ln 0.997) / 23) = 0.995. If the flat arena lands first the pair is 1,038 and this step's share is about the same; the arena's own gain on map_churn's map work is then about 2.8%, not cause 8's 6%.

**Risk.** The highest of the plan: it replaces about 900 lines that work. The probe covers every row of the ledger with real counts except one (a unique map cell over a shared top node, which this code never makes), and its audit caught nine planted faults. The probe's general walk is not tuned (1,479 a pair when every edit takes it), so the 1,168 depends on the one-node attempt, at the price of a second walk on about a fifth of edits. A pull-up that must grow the node above can run out of heap after the node below is freed; X1 makes that safe and the process work must keep X1.

### 11. 11. The map value is its top node

**Changes.** map.rs: the map cell becomes `[header][size][masks][entries][children]` and inner nodes stay as they are, so each position helper takes the offset of the mask word (1 or 2); an empty map is a top node with empty masks. heap.rs: `held` for `Kind::Map` starts after the size and mask words and reads the masks; raise `DENSE_SIZES` to 72 so a full top node (67 words) keeps a plain free list. eq.rs, hash.rs and show.rs read the new top. Key order does not change.

**Expected gain.** Alone (after step 10). One cell lookup fewer on every get, set and delete, about 28 (read in the machine code of the probe's get; the two probes differ by 84 on a get, of which about 28 is this, about 28 is children-first and about 20 is inlining). map_churn: 10.8M map operations x 28 = 0.3G of 19.71G, 1.5%. richards: the shared set makes and frees one cell fewer (252 measured, and the lookup), 99,029 x about 280 = 28M of 1,404M, 2.0%. Geomean factor exp((ln 0.985 + ln 0.98) / 23) = 0.9985. If the compiler learns to move a field out of a dying record, richards' share goes to nothing.

**Risk.** Low to medium. Two node shapes meet in every routine, kept apart by one offset. I did not rebuild this layout with counts; the 28 is from machine code and the earlier uncounted probe. If the number does not show in the VM, skip the step: nothing later depends on it.

### 12. 12. Grow a string or binary in place, with a benchmark to show it

**Changes.** benches/: add `string_build` (a few hundred KB built by appending short and long parts) and its twins in the other languages. text.rs: with a `Sole` whose cell is too small, allocate the needed size rounded up to a power of two (the exact size when that is past `MAX_CELL_WORDS`), copy, free the old cell without a walk; `store` assigns a word it starts and ORs into one it continues; S3 becomes "capacity is the cell's size". binary.rs: the same for a byte-aligned owner, so `<<acc:binary, c:8>>` is linear. Tests: a 1,000-step fold of `acc + x` makes about 12 cells; a 34-word need gets a 64-word cell, not 128; a grown string equals and hashes as a fresh one.

**Expected gain.** On today's scoreboard: strings' two batch folds fall from 43.0k to 23.6k each in the kernel (50.5k to 24.2k on real free lists), about 1.0k a row, 0.4%, and six row-level fold steps lose a make and a free, 0.5%. Geomean factor about 0.9996. Its real gain has no benchmark until the new one exists: a loop that builds a string by appending is quadratic with a small constant after step 5 and linear after this.

**Risk.** Medium. It is the step that adds layout rules (spare words that hold anything, a length that the cell's size no longer gives), for a property that cannot be seen in the source. PEP 8 calls CPython's version of this fragile and points at `join`, and `string::join` exists from step 7. So this is last among the string steps, and the owner can leave it out without losing anything measured today.

### 13. 13. `Find`, with the array and inlining work

**Changes.** crates/scarlet_ir/src/core_ir/mod.rs: `CoreExpr::Find` and `Probe`. crates/scarlet_lower: every call of a partial built-in lowers to one form (hit builds `Some` or `Ok`, miss builds `None` or `Err`, both into a join); one general pass sends a jump that passes a known constructor straight to the arm of a join that matches on its parameter; perceus.rs handles `Find` as it handles `Match` (`LetJoin` is matched in 18 places across 5 files, and `Find` will be too). code.rs: one instruction with a miss target per probe; delete `option_arms` and the `MapGet` pattern. exec.rs: `MapGet`, `IntFromString`, `BinaryIndexOf`, `BinaryParseInt`, `BinaryToString` and `BinarySliceBits` stop building `Some`, `Ok` and `Err`. `scarlet dis` prints the form.

**Expected gain.** Nothing in this area beyond step 9: map_churn, string_keys and richards already meet the pattern. json's two `Ok` cells a capture (2,080 x about 680 = 1.4M, 1.5%) are behind a wrapper and a function value, so they are reachable only after inlining. strings' three `Ok` cells a row (0.5%) are reachable after step 7. The array work's `xs[i] or False` is where this form earns its place.

**Risk.** Medium to high for its size of gain: a new control construct that lowering, Perceus, the loader and `dis` must all learn. It must not land before the known-constructor pass, or the lookups that are free today would box again.

### 14. 14. `map::fold` walks a position

**Changes.** Depends on steps 10 and 13. crates/scarlet_ir/src/intrinsic.rs: `MapNext`, and `Probe::MapNext` with three binds. code.rs: `Instr::MapNext { map, at, key, value, next, to }`; `Instr::writes` yields every written register and `allocate` adds clash edges for each. map.rs: `next(heap, map, position)`, reading the position as an `i128` (bigint.rs gains a two-digit read). exec.rs: the handler reads every operand and shares every result before its first register write. map.sca: `fold`, `walk` and the private `next`. Tests: fold order equals `keys` order; a map of three million Int keys, which has a node at depth 8 and so a big-Int position; a loader test with a `MapNext` whose map dies there and whose value bind is unused.

**Expected gain.** string_keys: `map::fold` is 0.18G today (0.6%, measured by the maps study) at about 3,000 an entry, and becomes about 1,150 (284 for the position, the rest the call and tail call), so about 0.4% of 28.70G. No other benchmark folds a map. Geomean factor about 0.9998. `merge` and `filter` stop building an array of tuples.

**Risk.** Low for correctness, and the gain is nearly nothing, which is why it is last. The position costs 284 an entry against 117 for a cursor that keeps its nodes. If a map-walking benchmark ever matters, that is the number to revisit; the fallback that needs no new instruction is to walk `keys(m)` and `values(m)` by index once array reads are cheap, at two words an entry while the fold runs.

## Experiments

I built one new standalone crate, reran three of the earlier probes, and checked the critics' code claims in the repository. Nothing in the repository was changed, and nothing ran inside the real interpreter.

**The crate.** `perf-scratch/final-strmap/` (safe Rust, `unsafe_code = "forbid"`, rustc 1.97.1). `perf-scratch/final-strmap/README.txt` says how to rerun everything. Counts are instructions retired from `/usr/bin/time -l`, as the slope between 1M and 3M iterations, with the loop's own cost taken off.

**1. The map walk with real counts** (`perf-scratch/final-strmap/src/bin/map.rs`). Critic 1 was right that the earlier probe replaced every release with `black_box`, had no copy path and no collision cells. This one keeps today's layout and counts everything: keys are Ints and string cells, values are cells, and nodes are shared between snapshots.

- The self-test keeps up to six live snapshots and applies random owned and borrowed sets and deletes, shares and releases, and now and then empties a map key by key. It runs under four hashers: the real one, one that gives every key one of 8 whole hashes (collision cells 13 levels down), one that keeps only high bits (long single-child chains) and a mix. Every 257 steps it audits that every reachable cell's count equals the number of references that reach it and that every live cell is reachable, checks the shape rule and the size word, looks every model key up, and rebuilds each snapshot's entries in sorted order into a fresh map and compares live words node by node. At the end every cell is freed. It passes on both heaps, with the one-node attempt on and off (200,000 steps each).
- A traced build counts how often each row of the ledger runs. Every row ran, except "a unique map cell over a shared top node", which this code never makes.
- I removed one count change at a time in nine rows (`MUTATE=n`). The audit failed every time, within 257 to 5,140 steps.

Measured, model of today's chunked heap / flat arena:

| Operation | Chunked | Flat |
|---|---:|---:|
| Delete oldest and set newest, 200,000 Int keys, with both hashes | 1,168 | 1,038 |
| The same with every edit through the general walk (`FAST=0`) | 1,479 | |
| Get, hit, with its hash, out of line | 296 | 262 |
| Replace a value in place | 378 | 336 |
| Delete of a key that is not there | 285 | 272 |
| Set in place on a 6-entry map of cells | 259 | 222 |
| Set on a shared 6-entry map of cells, then release the old map | 1,745 | 1,567 |
| Get on the 6-entry map | 138 | 128 |
| One 14-word cell made and freed | 252 | 216 |
| One count up and down | 66 | 60 |

What this settled:
- The earlier 890 a pair was a lower bound, as the earlier text half admitted. Counted and general, it is 1,168. Of a million pairs, 828k deletes and 841k sets finish inside one node; 172k deletes pull a lone entry up, 221k sets split a slot and 138k sets move a full node.
- My first version sent every edit through the general walk and cost 1,566. Reading each node once on the way down took it to 1,440 and the one-node attempt to 1,168. So the attempt is part of the design, not a tuning detail.
- A get does not get cheaper from the walk alone: 296 against 262 to 317 inside map.rs today.
- The shared small set is two cells made and freed (about 500) and eleven count pairs (about 430) out of 1,745. The earlier "about 800" was wrong.

**2. Literals as frozen values, in the handler model** (`perf-scratch/final-strmap/src/bin/handler.rs`, critic 2's crate with `FROZEN=1` added). string_keys' `spell`, instructions a word, `TUNED=1`:

| Build | Sticky literal cells | Frozen values |
|---|---:|---:|
| With Concat (steps 5 and 6) | 3,848 | 3,345 |
| With Concat, never in place (step 5 only) | 4,233 | 3,650 |
| A moved Move in Concat's place | 2,886 | 2,473 |
| That, plus one literal bound and released | 3,127 | 2,583 |
| That, plus one Int constant | 2,979 | 2,566 |

Over 4.25 letters a word: a literal costs 57 sticky and 26 frozen, against 22 for a constant. A Concat averages 256 sticky and 231 frozen. The whole step is 118 a letter cheaper with frozen values. By word length (frozen): the first letter is 100 over a Move, the second 342, each later one 191 in place or 332 with an exact copy.

**3. Reruns that confirmed the critics.**
- Critic 2's handler before my change: 4,062 / 3,802 / 3,734 a word for the three tunings and 2,861 for the Move, so 313 / 251 / 236 a Concat, as reported.
- Critic 3's packed position (`perf-scratch/attack-simple-strmap/`): 284 direct and 379 through the struct on 55,328 entries, against 231 for the struct alone and 117 for the stack; 134 and 86 on 6 entries.
- The earlier merged probe's map modes: 928 - 38 = 890 a pair, as it reported.

**4. Replace without an offsets buffer** (`perf-scratch/final-strmap/src/bin/replace.rs`, on plain byte vectors). 50 matches of `tag=` in 2.3 KB: 11.0k with a buffer of offsets, 16.2k searching twice. 250 matches of `=`: 33.0k against 40.6k. Critic 3's "a few hundred instructions" was low by ten times, and it still does not matter: 5.2k a batch is 0.04% of the strings benchmark.

**5. Read in the code or in `scarlet dis`.**
- richards `find_task`: `let %3 = MapGet(%2, %1); drop %2; match %3`. json `get` (line 461 of the dis) has the same shape. map_churn and string_keys have `MapGet` followed directly by `match`.
- json `capture`: `call fn#17(%0, %1, %3)`, `closure fn#100()`, `call fn#31(%4, %5)`, and `fn#100` is `ret BinaryToString(%0)`. `slice_bytes` is a Scarlet wrapper (binary.sca:26).
- `Instr::writes` returns `Option<Reg>` (code.rs:734), and `allocate` and `liveness` use only that.
- map.rs already gives a moved node `ROOM = 4` spare words filled with `Nil` (lines 808 to 858), and `equal_parts` already reads `Used::len` words (line 1040). So spare room is today's design, and reading the live length from the masks is one more reader of a rule that exists.
- `SmallVec` is used six times in array.rs and once in exec.rs. `Heap::splice` and `Edit` are used only by map.rs.
- All fifteen cell kinds are in use, and value tags 0xFFFE and 0xFFFF are free (value.rs).
- `scarlet_vm::run` drops the machine on any `Stop` (lib.rs:61).

**Not measured.**
- Nothing ran in the real VM. Every "in the VM" figure is a probe number plus a stated shell, and every benchmark share multiplies such a figure by a count from the diagnosis.
- I did not rebuild the head-node layout with counts. Step 11's 28 a map operation is read from the machine code of my get and from the gap between two probes.
- The probe's general walk is not tuned, and its copy path is not either. A tuned general walk might make the one-node attempt unnecessary; I did not find out.
- I rebuilt no binary path, no `split`, `trim`, `contains` or conversion, and no `Find`. Those figures are the earlier designers'.
- The frozen-value handler has only the short two-part paths.
- The strings total leans on by-part tables that the diagnosis says overlap.

**Where this leaves the scoreboard, by that arithmetic.** string_keys 2.93 to about 1.55 to 1.75, strings 3.49 to about 1.65 to 1.85, json 1.99 to about 1.69, map_churn 2.53 to about 2.25, richards 7.30 to about 6.8. The step factors multiply to about 0.936; adding shares per benchmark gives about 0.927.

## Prior art

Files I downloaded for this revision and read the named parts of (fetched 23 November 2026):

- **Lean 4 `lean.h`**, https://github.com/leanprover/lean4/blob/master/src/include/lean/lean.h. The header comment says the count "also encodes whether the object is single threaded (> 0), multi threaded (< 0), or reference counting is not needed (== 0)", and `lean_dec_ref` tests `> 1`, then `!= 0`, after loading the count. So Lean's constants pay a header load on every release. I took the idea (a constant is never counted) and moved the test into the value word, because a Scarlet cell reference is not an address and a tag test is already made on every release.
- **Lean 4 `InferBorrow.lean`**, https://github.com/leanprover/lean4/blob/master/src/Lean/Compiler/LCNF/InferBorrow.lean. Its header gives the correctness reasons to mark a parameter owned, one of which is "values which are subject to reset-reuse are owned so their reference count accurately reflects the real amount of references". That sentence is invariant S0. I took it as the contract the refcount area must keep.
- **Lean 4 `object.cpp`**, https://github.com/leanprover/lean4/blob/master/src/runtime/object.cpp. `lean_string_append` (line 2253) copies when `!lean_is_exclusive(s1)` and otherwise calls `string_ensure_capacity` (line 2121), which returns the same object when it has room and allocates `cap + sz + extra` when it has not. This is the model for steps 6 and 12: the same discipline (counts plus uniqueness) and the same three outcomes.
- **Erlang/OTP 27.1 `erl_map.c`**, https://github.com/erlang/otp/blob/OTP-27.1/erts/emulator/beam/erl_map.c. The comment above `erts_internal_map_next_3` (lines 3590 to 3635) describes the path as an integer of 4 bits a level, says "The index can become a bignum ... as you would need a tree of depth 7 or more", and gives the iterator's return as `{K1,V1,{K2,V2,none | [Path | Map]}}`. So Erlang's position is the same idea, but it pays the walk from the top once a batch and builds tuples for the batch. I took the integer path and its big-Int case as ordinary. I did not take the batch, because binding into registers builds nothing; the price is 284 an entry.
- **Erlang/OTP 27.1 `maps.erl`**, https://github.com/erlang/otp/blob/OTP-27.1/lib/stdlib/src/maps.erl. `next/1` (lines 1064 to 1077) matches `{K, V, I}` first and `[Path | Map]` second, which is the batch being unwound.
- **Erlang/OTP 27.1 `erl_map.h`**, https://github.com/erlang/otp/blob/OTP-27.1/erts/emulator/beam/erl_map.h. `hashmap_head_t` (line 139) is a thing word, a size and the items, which is the head node of step 11. `MAP_SMALL_MAP_LIMIT` is 32 (line 94).
- **Scala 2.13 `HashMap.scala`**, https://github.com/scala/scala/blob/2.13.x/src/library/scala/collection/immutable/HashMap.scala. `getKey` is `content(TupleLength * index)` (line 652), `getNode` reads from the other end (line 661), and `migrateFromInlineToNodeInPlace` (line 1026) is the builder's in-place edit of a node it owns. I took the in-place edit of an owned node and kept today's order of words.
- **PEP 8**, https://peps.python.org/pep-0008/#programming-recommendations (source at https://github.com/python/peps/blob/main/peps/pep-0008.rst, line 1220). "do not rely on CPython's efficient implementation of in-place string concatenation ... This optimization is fragile even in CPython ... the `''.join()` form should be used instead." Critic 3 quoted it from memory and it says what they said. It is why growth is the last string step and why `string::join` lands before it.

Cited by the earlier design and not fetched again by me: CPython 3.13 `unicodeobject.c` (`unicode_modifiable`, the count-is-1 test), https://github.com/python/cpython/blob/v3.13.0/Objects/unicodeobject.c; Lua 5.4.7 `lvm.c` (`luaV_concat`: n operands, one allocation, numbers converted in the same pass, an empty operand returns the other), https://github.com/lua/lua/blob/v5.4.7/lvm.c; Erlang/OTP 27.1 `erl_bits.c` and `erl_bits.h` (doubling, and one reduction per KiB as the unit for charging byte work), https://github.com/erlang/otp/blob/OTP-27.1/erts/emulator/beam/erl_bits.c; Steindorfer and Vinju, "Optimizing Hash-Array Mapped Tries for Fast and Lean Immutable JVM Collections" (OOPSLA 2015), https://michael.steindorfer.name/publications/oopsla15.pdf, for the canonical delete; `bytemuck::cast_slice` (https://docs.rs/bytemuck) and `memchr::memmem` (https://docs.rs/memchr).

Cited from memory, not fetched: Maurer, Downen, Ariola and Peyton Jones, "Compiling without Continuations" (PLDI 2017), for a jump that passes a known constructor going straight to the arm of a join. That is the rule `Find` relies on in place of three hand-written forms.

From the repository: `docs/vm-design.md` (a heap per process, "constants are frozen ... shared across threads with no counting", big binaries shared on send, no Rust recursion on program data, a limit is the heap's size) and `docs/perf-diagnosis.md` (causes 4, 6, 8, 11, 14, 19 and 20 with their sceptics' corrections), which every "today" number comes from.

## What it gives up, and what the critics found

**What the critics found, and what I did.**

Critic 1 (correctness):
- *In-place writes assume every register owns a counted reference, and planned borrowing breaks that.* Checked: true today by every read sharing, and nothing stated it. S0 is now its own invariant, the loader's flags take the set of owning registers, and two tests pin it. Lean's borrow inference states the same rule.
- *The map walk had no reference ledger and the probe counted nothing.* Checked in the old probe. The ledger is written as two tables, and a new probe runs it with real counts, a copy path, collision cells and live snapshots. Its audit caught nine planted faults. The counted walk costs 1,168 a pair, not 890.
- *`MapNext` writes several registers and the loader models one.* Checked at code.rs:734. The instruction gets a `next` register, `writes` yields every register, and the handler shares results before its first write. It is the last step.
- *Many makers of strings, two writers.* Every maker now goes through `text::write`, which zeroes the last word, and a dirty-heap test runs them all.
- *Nothing said the cell written in place is a string.* `text::sole` is the only source of the `Sole` witness: a heap `String` cell with count 1. A big Int or a literal gives none.
- *Nil-filled spare words are kept by hand.* Taken: the free walk reads a node's live length from its masks, as `==` already does, and no edit refills.
- Minor: the leak check (a literal is no longer a heap cell, so nothing to exclude); the intern key (the whole constant, bit length included); the big-Int position (now called ordinary, tested on three million keys); the growth clamp (new rule, with the exact size as the fallback); a `Stop` part way through an edit (X1 added, and the offsets buffer is gone). I did not make every edit allocate before its first write: a delete's pull-up relies on X1.

Critic 2 (numbers):
- *Concat costs 236 to 313 as an instruction, not 180.* Reran and got the same. The two-part order they proposed is in the design, and every string_keys figure is now from the handler.
- *Step 4 counted the accumulator's free twice.* Right. In-place append is now 3.3% of string_keys, and the free is in the concat step.
- *A sticky literal costs about 80, not 31.* Reran: 57 in the model. I took their fix further: a literal is a frozen value with its own tag, measured at 26.
- *Gains overlap with other areas and with each other.* Each step now says what it is worth alone and what happens if another area lands first, and one section lists what others must not count. The area total is quoted as 0.927 to 0.936.
- *The probe's free lists were not heap.rs's for big cells.* I quote their corrected rows. I did not move size classes for strings into this design; that stays with the heap area.
- Minor: the growth rule (now "needed, rounded up to a power of two", slack at most twice) and the scalar-count wording are fixed.

Critic 3 (simplicity and the future):
- *`Find` cannot reach json's `Ok` cells.* Checked in `dis`. The 1.5% is gone.
- *`Find` is in the wrong place.* Agreed. Step 9 widens the loader's pattern over drops, which gets richards' 4.5%. `Find` is specified with one lowering form and one general rule, and lands with the array and inlining work.
- *Step 9 bundled four things.* Split. The walk is on today's layout. Size classes and children-first are dropped. The head node is its own step with its own number.
- *Step 4 bundled fitting and growing.* Split. Fitting needs no layout rule at all, because the room is the zero bytes of the last word. Growth is last and waits for a benchmark.
- *The offsets and scratch buffers sit outside the heap.* Offsets are gone; two passes cost 5.2k more on a 16.2k replace, measured. `scratch` stays for shown parts, bounded, per scheduler thread.
- *The position costs 284, not 231.* Confirmed. Kept, because a cursor needs a cell kind that does not exist and the alternative arrays cost memory for every entry. Their index walk over `keys` and `values` is named as the fallback.
- *Literals made per heap, and the frozen bit deferred.* Settled now with the value tag, as they suggested.
- *Build problems.* `smallvec` stays; `splice` and `Edit` go with the walk; the old step 2 is three steps; each step names what it depends on; the three heap functions are flagged for agreement.
- *Two hash paths.* Taken: whole words always, one path, map order changes once. I did not take BEAM's 1.2 times growth past a threshold; doubling with a clamp is one rule.

**What this design gives up.**

- **Maps gain about half what the diagnosis hoped.** 11% of map_churn against 21%, and under 3% of richards against 14%. A counted, general walk is 1,168 a pair; LuaJIT's is 136 to 182. A get does not improve until the head node, and then by a tenth.
- **Only appending short parts is cheap until growth lands**, and after it only appending. `part + acc` copies, and so does `a + b` when `a` is shared. `string::join` and `string::concat` are the guaranteed linear path, and the docs should say so.
- **The in-place append cannot be seen in the source.** A closure that captures the accumulator, or a name that keeps it alive, makes every append copy, with no sign. The counter makes it testable.
- **No string slices, no cached hash, no interning.** `split` copies each piece. A string key is hashed on every map operation, so a string get stays about twice an Int get. `string::length` stays O(n).
- **A string has two homes.** Every reader goes through `text::of`, which tests a tag. That is a few instructions on every string read, paid so that literals cost nothing to hold.
- **A value tag is spent** (0xFFFE), and every match on `View` gains an arm.
- **A breaking change to map order** for string keys, taken once.
- **A new dependency**, `bytemuck`, with `unsafe` inside it, as `memchr`, `simd-json` and `smallvec` have. The workspace has none. Little-endian is assumed and said with a compile error.
- **A UTF-8 check where safe Rust needs a `&str`**, in `to_graphemes` and a non-ASCII `trim`.
- **A fold over a large map pays to find its place**, 284 an entry.
- **About a fifth of unique map edits walk twice**, because the one-node attempt gives up and the general walk starts again.

**Where it is weakest.**

- Nothing ran in the real VM. The handler model's free is about 90 cheap, and its loop is leaner than exec.rs's.
- The map step replaces 900 lines that work, and my general walk is the untuned half of the probe.
- The strings estimate leans on arrays: `split` still builds an array the present way, and most of the `replace` built-in's gain is array walks and closure calls another area also removes.
- Step 1 is wide. It is the first step and it touches every string and binary reader.
- The binary step has no probe of mine behind it.
- A long built-in does not yield part way. A replace over 100 MB runs to its end before the reduction check.

## Processes and a JIT

**Processes.**

- **A heap per process.** Strings, binaries and map nodes are ordinary cells in the owning process's heap. Uniqueness is the plain count with no atomics. The kernels ask the heap for three things (a cell's words in one lookup, two cells at once for a copy, an allocation), so they work on chunks or on one arena.
- **Literals are already where the VM design wants them.** `docs/vm-design.md` proposes that constants are frozen, never freed and shared across threads with no counting. The frozen area is that: it belongs to the loaded program, nothing holds a `&mut` to it after load, and a frozen value is never counted because it is not a cell. Spawning a process makes no literal. A heap holds a borrowed reference to the program's frozen words (or an `Arc`, one atomic step a spawn, if code can ever be replaced while it runs). A cell reference keeps meaning "this process's heap", so cell lookups stay one path.
- **Sending a message.** A frozen value copies as one word, with no bytes copied. A string copies at its exact length, so spare room does not travel. A map copies its live words node by node. A position is an Int and copies like one.
- **Big binaries shared between processes**, when they come, are a third kind of owner behind `Bits`, counted with atomics. They never give a witness for writing in place: the rule stays "only an owner in this process's heap with a count of 1".
- **S0 is the contract with the refcount work.** Borrowed registers are compatible with this design only if a borrowed value is shared before it reaches an operand that may be taken. The loader takes the set of owning registers as an input so that this is one place, not a convention.
- **Reductions.** No built-in here calls Scarlet code (F1). `map::fold`, `merge`, `filter` and `from_list` are Scarlet loops, so every callback is a call instruction where the scheduler counts and may yield, and no Scarlet frame sits on the host stack. A built-in that loops over bytes (concat, split, replace, join, a long hash or `==`) knows how many bytes it touched and should hand that back, at one reduction per KiB as the BEAM charges. An in-place append charges for the bytes added. None yields in the middle. The two-pass built-ins hold no state outside their arguments, so one could later be made to stop after its counting pass and start again.
- **Nothing a process owns lives outside its heap.** The offsets buffer is gone. `scratch` belongs to a scheduler thread, is bounded between uses, and fails with `Stop::Full`, not an abort. When heap limits exist, a shown part should be checked against the process's remaining room before `scratch` grows, since the bytes are about to be copied into the heap anyway.
- **X1 must be kept.** A map edit that runs out of heap part way leaves a half-changed map. That is safe only because a process that stops is dead and its heap is freed whole. The process work must not add a way to read a stopped process's heap.
- **No Rust recursion on program data.** The map walk uses a fixed 14-step path on the Rust stack. Building the node for two keys that share a slot recurses at most 13 deep, bounded by the hash's bits and not by the program. Hashing and comparing nested keys keep their work lists.
- **Process death** frees the heap whole. A half-built string or a fold in progress costs nothing.

**The JIT.**

- Each instruction is either a few loads the JIT can write inline, or one call to a kernel function that takes the heap and plain values and never looks at a register. The interpreter and compiled code call the same functions, so they cannot disagree on what an instruction means.
- A literal is an immediate word. There is no count traffic to emit and no table to load from.
- `own_first` and `moved` are fields the loader computes once. The JIT reads the same flags and picks the helper with no analysis of its own.
- The fast tests are on words the JIT can read inline: the same-word test and the length compare for string equality; the tag, kind, count and room tests for an in-place append, whose write is one shifted OR; a tag test, a bounds check and a load for `ByteAt` on an aligned binary; the Int-key hash, which is three multiplies. The one-node map attempt is a loop of loads and two population counts a level, and is a candidate to inline for a known Int key.
- `MapFind` today, and `Find` later, give the JIT a call and a conditional branch with registers written on the hit side, which is how the BEAM's JIT emits `get_map_elements`. A boxed `Option` would need escape analysis to remove.
- `MapNext` is the first instruction with several results. The JIT needs the same rule as the interpreter's handler: results are computed and shared before any register is written.
- Nothing here depends on the interpreter's dispatch or frame layout, so this work can land before or after the dispatch and call work. It does depend on two agreements: the three heap functions with the heap area, and S0 with the refcount area.

