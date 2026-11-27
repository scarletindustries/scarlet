# Speed design: Arrays: write at an index, push in place, walking with [h, ..t], index reads, building, and the stdlib surface

This is the design for one area of the speed work, written on 23 November 2026. `docs/perf-plan.md` puts its steps in order with the other areas' steps, and settles where the areas disagree. Where this file and the plan differ, the plan is right.

# Arrays that their one holder edits in place

## The design in nine sentences

1. An array has four forms. The empty array is an immediate word. An array of 1 to 32 elements is one flat cell with a window. A longer array is a root over 32-slot nodes: a head leaf, a strict radix trie and a tail leaf. A range `a..b` is a cell of its two ends.
2. Every array operation that returns an array is handed one reference to the array it works on. The handler takes the register when the loader marked the operand `moved`, and shares it otherwise. So inside `array.rs`, "the count is 1" is the whole test for "this is mine to change".
3. Three small functions (`own_root`, `own_node`, `own_child`) are the only places a node is copied. Each hands back the node itself when its count is 1 and a copy otherwise. That one path handles unique, shared and half-shared arrays.
4. The three hottest operations (write, push, uncons) also have an in-place function that changes nothing unless every node on the way is unique. It is a second copy of the all-unique case, and a test switch forces the general path so both are checked on the same inputs.
5. A read, a slice and a write that can miss are branches in the Core IR (`CoreExpr::Find`), and an array pattern is a pattern (`CorePat::Array`). Nothing boxes an `Option` or `Result` that is matched at once, and no loader pass recognises a shape.
6. A pattern takes the tail only once its arm can no longer fail. An arm with a guard or a refutable element reads its heads without consuming, runs its tests, and then takes the tail.
7. The relaxed (RRB) machinery goes. A join pushes the shorter array onto the longer one a leaf at a time, in bounded steps the VM can pause between.
8. The work lands in eight steps. The first six need no decision about the tree and carry about 97% of the gain. The seventh swaps the form above 32 and is the only one that reverses a recorded decision.
9. Anything that makes a count higher than it should be costs a copy and never a wrong answer. The places where that still happens are named below, each with the test that pins it.

## Two things the owner has to decide

**A. Relaxed nodes (before step 7 only).** `docs/vm-design.md` says, as Decided, "an array stays the RRB tree". I recommend a strict trie above 32 instead. The reason is simplicity and a stated worst case. It is not speed: on the scoreboard the choice of form above 32 moves the geomean by under 1%.

| what the strict trie buys | strict trie | today's tree, tuned in place | today |
|---|---:|---:|---:|
| read at 5,000 | 146 | 240 | 272 |
| write in place at 5,000 | 186 | 259 to 309 | about 55,000 |
| code | one node shape, no size tables | merge, rebalance, plan, repack and size tables stay, and every in-place push and uncons must keep the tables right | |

| what it costs: a join, in instructions | strict trie, join alone | with the result later freed | today, with the result freed |
|---|---:|---:|---:|
| 100 + 5,000 | 7,204 | 14,241 | 25,623 |
| 5,000 + 100 | 9,059 | 14,247 | 29,554 |
| 1,001 + 1,003 | 40,473 | 67,785 | about 32,000 |
| 5,001 + 5,003 | 208,543 | 322,044 | 41,084 |
| 100,000 + 100,000 (lengths that are multiples of 32, so leaves are shared whole) | 2.38M | 2.65M | 51,535 |
| 100,001 + 100,000 | 4.45M | 6.91M | 57,688 |
| 99,990 + 100,000 | 4.48M | 7.02M | 52,150 |
| 1,000,001 + 1,000,003 | 40.6M | 68.8M | about 60,000 |

So a strict join costs about 44 instructions per element of the shorter side (Int elements, measured on my tuned build), and about 25 more per element when the result is later freed. It beats today's join when the shorter side is under roughly 700 to 900 elements and loses above that: 5 times at 5,000, about 80 times at 100,000 and about 700 times at a million. No program in the repository joins two long arrays. A program that does will feel it.

If the answer is no, steps 0 to 6 and 8 are unchanged, and step 7 becomes "in-place push and uncons on today's tree, with capacity in the head and tail leaves".

**B. The names of the checked and the total write.** The primitive is a write with a miss edge. The stdlib rule says a fallible operation returns `Result(_, Nil)`, so I name the checked form `array::set` and the total form `array::put`. Both cost the same and both write in place. The names are the owner's call.

## Data layouts

### The empty array

`Value::EMPTY_ARRAY = TAG_IMMEDIATE | 3`, seen as `View::EmptyArray`. `[]` allocates nothing and the `[]` test is a word compare.

### The flat cell, `Kind::ArrayFlat`, 1 to 32 elements

```
word 0    header: count | kind | size
word 1    start (bits 0-7) | len (bits 8-15)
word 2..  slots; cap = size - 2, and cap is always 4, 8, 16 or 32
```

The live elements are `slots[start .. start + len]`. Every slot outside the window holds `Nil`.

- Capacity is always the next of 4, 8, 16, 32 that fits. So an array of n elements takes `max(4, next power of two) + 2` words, and a push reallocates at most three times in a flat array's life. I chose this over exact capacity because a regrow costs 691 (4 to 8), 796 (8 to 16) and 1,025 (16 to 32) against 85 to 87 for a push in place.
- A push with no room on its side slides the window to the far end when the cell is unique and has room at the other end. Otherwise the items move to the next class, with the spare room on the side being pushed.
- A full cell of 32 that is pushed again becomes a root. Its 32 slots are copied once into a node (2,014 for that push).

### The root, `Kind::ArrayRoot`, 7 words

```
word 1  len
word 2  shift (bits 0-7) | head_len (8-15) | tail_len (16-23)
word 3  tree_off
word 4  head   Nil, or a leaf whose live slots are its LAST head_len
word 5  tree   Nil, or a node of height `shift`
word 6  tail   Nil, or a leaf whose live slots are its FIRST tail_len
```

### The node, `Kind::ArrayNode`, always 33 words

A header and exactly 32 slots. A leaf's slots are elements and a branch's are children. The tree knows which from `shift`, so one kind serves both. Every array cell is 34 words or fewer, so all of them use the dense free lists.

```
   root [ len | shift,head_len,tail_len | tree_off | head | tree | tail ]
                                                      |      |      |
   [ . . . . . h h h ]  live at the end  <-----------+      |      +--> [ t t t . . . . ]  live at the start
                                                           branch: 32 slots, Nil outside the window
                                                          /   |    \
                                                       leaf  leaf  leaf      every live leaf is full
   radix position:                           tree_off .......... tree_off + tree_len
```

`tree_len = len - head_len - tail_len`. It and `tree_off` are multiples of 32. Reading element `i`:

```
if i < head_len            -> head[32 - head_len + i]
j = i - head_len
if j >= tree_len           -> tail[j - tree_len]
p = j + tree_off; n = tree
for s = shift down to 5    -> n = n[(p >> s) & 31]
                           -> n[p & 31]
```

`tree_off` is what lets the front move without relaxed nodes. Dropping from the front hands the tree's first leaf to `head` and adds 32 to `tree_off`. Prepending a full head hangs it at `tree_off - 32`. When `tree_off` is 0 the tree grows a level with the old tree in slot 1.

**Height.** After every cut, `collapse` drops a level while the first and last live leaf sit under the same child. Then one more rule runs: when the tree holds at most 32 leaves and is still taller than one branch, those leaves are hung under one new branch (at most 32 pointer moves, one allocation). So a tree of up to 1,024 elements is always one branch of leaves, whatever it was cut from. I built this: an array of 100 elements cut from 2,000, 100,000 and 2,000,000 had shift 15, 20 and 25 without the rule and has shift 5 with it. Above 1,024 elements a tree can still be one level taller than a packed one (about 23% of root-form results in the random test) and very rarely two (about 0.1%).

A root is made when an array grows past 32 or a cut leaves more than 32. A root that shrinks below 33 by `[h, ..t]` stays a root until it is empty, so an array that hovers around 32 does not flip between forms. A slice that leaves 32 or fewer builds a flat cell.

### The range, `Kind::Range`, 3 words

Its two ends, as today. An empty range is the immediate. A unique range is walked by moving its start in place. A write to one or a push onto one builds it as an array first.

### One door from a value to an array

```rust
pub(crate) enum Seq { Empty, Flat(Cell), Root(Cell), Range(Cell) }
pub(crate) fn seq(heap: &Heap, v: Value) -> Option<Seq>      // the only way in
pub(crate) enum ArrayError { Full, Broken(BadArray) }         // Broken names the shape that failed
```

`Machine::seq` is the only caller outside `array.rs`, and it turns `None` into `Stop::broken`. Every function in `array.rs` that makes an array returns a `Value`, and three private functions are the only places an array cell is minted: `new_flat`, `new_root` and `new_range`. Each returns `EMPTY_ARRAY` for length 0. So a zero-length cell cannot be built. Equality and hashing go through `Seq` and compare lengths and elements, so they would still be right if one were. An uncons that meets a zero-length cell stops with `Broken(EmptyCell)` instead of taking a wrong branch.

## The ownership rule

**Every `array.rs` function that returns an array is handed one reference to each array it works on.** The handler makes that true in one line:

```rust
let a = if *moved { self.take(base, *src) } else { self.share(self.get(base, *src)) };
```

A register that is read again keeps its own reference, so the count the function sees is at least 2 and it copies. A moved register hands its reference over, so a count of 1 means nothing else can see a write. I put this rule into the prototype behind a switch and ran the differential test with it, with and without the fast paths. It passed, and it costs 95 to 130 instructions on paths that copy anyway (1,269 against 1,174 for a shared write at 16 elements).

```rust
/// The node `bits` names, made the caller's alone: itself when nothing else
/// holds it, and otherwise a copy that takes its place in `bits`.
fn own_node(heap, bits: &mut u64, live: Range<usize>) -> Result<Option<Cell>, ArrayError> {
    let c = cell_of(*bits)?;
    if heap.unique(c) { return Ok(Some(c)); }
    let copy = copy_node(heap, c, live)?;   // shares each live slot, carries nothing outside `live`
    *bits = Value::cell(copy).bits();
    heap.release(c);                         // the count was above 1, so this only decrements
    Ok(Some(copy))
}
```

A write on the general path:

```rust
let c = own_root(heap, c)?;
let mut n = own_node(heap, &mut r.tree, 0..32)?;
while s > 5 { n = own_child(heap, n, 1 + ((p >> s) & 31))?; s -= 5; }
let old = replace(slot(n, p & 31), x);  release(old);
```

Once a copy is made its children have a count of 2 or more, so every node below is copied too. The same code edits a unique array in place, path-copies a shared one, and copies only the shared lower nodes of an array whose top is already its own.

**The fast paths, said plainly.** The general path is slow when everything is unique, because it re-reads the root, finds each cell again and rewrites all six root words. Measured in place: 376 against 155 for a write at 1,000, 470 against 185 at 5,000, 243 against 151 per walked element, 291 against 178 for a push. So `set`, `push` and `uncons` each have an in-place function that finds each node once, checks each count, and writes only after the last check. It allocates nothing, so it cannot fail half way, and if any node is shared it returns "not mine" having changed nothing. Each public entry is one `#[inline]` call to the in-place function and, on "not mine", an `#[inline(never)]` call to the general path. That is two copies of the all-unique case for three operations. `array.rs` keeps a test-only switch that skips the in-place functions, and the differential test runs both ways.

## What each operation costs

Costs are instructions inside the array function, from the prototype on a copy of the real `heap.rs`. The hot paths were re-measured with arithmetic the VM crate's lints accept (checked and saturating forms): that added 0% on flat cells and 3 to 5% on roots, and those are the numbers here. A handler adds 46 to 58 (measured in a small interpreter). A `Drop` that finds its register empty adds 30 until the dead-drop work deletes it.

| operation | held alone | shared | today |
|---|---|---|---|
| read `xs[i]`, 16 / 1,000 / 5,000 / 100,000 | 50 / 117 / 146 / 175 | the same | 121 / 211 / 272 / 336 |
| write, same sizes | 78 / 154 / 186 / 218 | 1,269 / about 5,800 / 7,735 / about 11,100, result freed | about 55,000 as slice, push and join; 111 / 244 / 309 / 375 with an in-place set on today's tree |
| push at the back of one long array | 178 | 2,601 when another holder keeps the array at every push, flat from 1,000 to 100,000 | 947, and about 2,950 held |
| push on a flat cell | 85 to 87; a regrow is 691, 796 or 1,025 | | 853 to 897 |
| walk with `[h, ..t]`, per element, 8 / 32 / 1,000 / 5,000 | 64 / 68 / 151 / 153 | kept by the caller, so copied once at the start: 183 / 119 / 160 / 158 | 1,596 / 2,167 / 395 / 442 |
| the same walk when something else holds the array at every step, Ints, 4 / 8 / 16 / 32 / 33 / 1,000 | | 570 / 711 / 871 / 1,158 / 860 / 921 | 1,340 / 1,558 / 1,782 / 2,127 / 743 / 965 |
| the same, elements that are cells | | 766 / 1,033 / 1,446 / 2,237 / 963 / 1,025 | |
| the first uncons of a long array the caller keeps, built by appends | | 4,284 at 1,000 and 6,261 at 5,000, once per walk | 8,645 and 11,459 |
| walk a range, per element, 4 / 32 / 1,000 | 146 / 93 / 86 | 141 / 93 / 86 | |
| slice half, source kept, result freed, 8 / 32 / 1,000 / 5,000 | not measured | 1,266 / 2,918 / 7,384 / 14,211 | 2,919 / 4,339 / 15,361 / 18,966 |
| join of two flat cells into one, 4+4 / 16+16 | 616 / 912 alone; 911 / 1,702 with the result freed | | 754 / 750 with the result freed |

In words:

- **set.** Flat: one store. Root: walk down with one lookup per node. Shared: copy the flat cell, or the root and one node per level. An index the array does not have is a miss: nothing is written and nothing is taken.
- **push.** Flat: store at the window's edge. Root: store in the tail (or head) and bump two fields in the root. A full buffer is hung in the tree.
- **uncons.** Flat and unique: move the element out, leave `Nil`, bump `start`. No count changes. When the last element leaves, the cell is freed and the tail is the immediate. Root: the same on the head, and an empty head is refilled with the tree's first leaf. Shared flat: the rest is copied into a new cell. Shared root: the root is copied (7 words). When the head is empty and the tree is shared, the nodes above the first leaf are copied too, which is the 4,284 to 6,261 in the table. I looked at handing over the first leaf by sharing it and leaving the shared branch as it is. I did not take it: when the other holder later lets go, every leaf the walk has passed stays referenced from those branches until the next refill, and if the tail is then stored and never walked again they stay for good. That is a leak nobody could see.
- **slice.** `take` then `drop_front`, through the same `own_` functions, so an owned slice edits in place and a shared one copies the two boundary paths. Nothing outside the window stays referenced. I keep `moved` on the slice so the consume rule has no exception. The extra code is `prune` and `scrub`, which the in-place uncons needs anyway.
- **join.** If both fit in 32, the result is one flat cell written from the two windows. Otherwise the longer side becomes a root the join owns, and the shorter side is added to it a leaf at a time: onto the back when the right side is shorter, onto the front when the left is. A full source leaf that lands on a leaf boundary is shared whole. The root is read once and written once.
- **range.** A kept range costs one 3-word copy per walk and then moves its start in place.

## Invariants, and what upholds each

| | invariant | what upholds it |
|---|---|---|
| C | An operation that returns an array holds one reference to each array it was handed. | The handler's take-or-share line. Nothing else calls the writing functions. |
| D | When `moved` is set, no other register of the frame holds that array, unless a value still holds it or the case below applies. | `moves_on_last_use` in `code.rs` sets `moved` on `Move`, on call arguments, on constructor fields and on every array operand and item (`Arg`). Perceus drops a heap local after its last use. One gap is left and named: Perceus puts no drop inside a `LetJoin`'s body (`perceus.rs`, the `SpineRhs::Join` comment), so a record or array whose last use is inside an `if` or `match` used as a value stays referenced until the join ends. That costs a copy. A test asserts `cells_made` for both spellings and fails until Perceus drops inside joins. |
| O | A node is written only when its count is 1 and it was reached through nodes whose counts are 1. | The in-place functions test each count on the way down before the first write. The general path gets every node from `own_root`, `own_node` or `own_child`. |
| M | `moved` is set only when no later instruction reads the source register on the hit edge, and the instruction does not also read it as another operand. | `moves_on_last_use`: `!before[pc + 1].has(src)`. The miss edge takes nothing, so its readers do not count. |
| S | Every slot of a flat cell or node is `Nil` or one counted reference, live or not. | A node is only made from a buffer whose references were counted. Every store is replace-then-release-old. The heap frees with `held = 2..size` for flat, `1..size` for nodes and `4..size` for roots. |
| E | No array cell has length 0. | `new_flat`, `new_root` and `new_range` are the only minting functions and return the immediate for 0. `==` and hashing do not depend on it. |
| F | Flat: `1 <= len`, `start + len <= cap`, cap is 4, 8, 16 or 32, and slots outside the window are `Nil`. | In-place paths write `Nil` where they move out. Copies carry only live items. |
| W | Root: `head` is `Nil` exactly when `head_len` is 0, and the same for the tail and the tree. | `refill_head`, `hang_back`, `hang_front` and `collapse` set the node and its length together. |
| T | Tree: every live leaf is full, `tree_off` and `tree_len` are multiples of 32, every branch slot outside the window is `Nil`, the first and last live leaf sit under different children of the top branch, and a tree of at most 32 leaves is one branch. | Only `hang_back` and `hang_front` add leaves, and only full ones. `detach_first_leaf` and `prune` empty what a cut leaves outside. `collapse` runs after every cut. |
| L | Only a head or tail leaf can hold references past its window, at most 31, and only when another array once read that leaf with a wider window. | Tree leaves are full. Every store into a slot past the window releases what was there. |
| Q | `==`, hashing, `show`, JSON and `wire` see the element sequence and nothing of the shape. | One leaf iterator over `Seq` is the only way they read an array. |
| R | Every operand of an instruction is read before any of its destinations is written. | Each array handler is written in that order. A dead destination may share a register with an operand, so the loader test for `ArraySplit` covers it. |

Tests that move into `array.rs`: the differential test against a `Vec` model with `check` on every result and on every kept array at every step, the leak count at the end with boxed elements, a run where the heap fills after 0 to 4 allocations and every still-held array is checked afterwards, a run with arrays of `Nil` elements, a node with a sticky count, ranges as operands, and `ArraySplit` with 2 and 3 heads on every form. The first, the heap-fill and the `Nil` runs exist in the scratch crates and pass. All of them run again with the in-place functions switched off.

## Core IR (`scarlet_ir::core_ir`)

```rust
pub enum CorePat {
    Wild, Bind(CoreBind), Lit(ConstId), Ctor { variant, fields },
    /// `[a, _, ..rest]`, `[a, b]`, `[]`. A `_` element has no binder.
    Array { prefix: Vec<Option<CoreBind>>, rest: ArrayRest },
}
pub enum ArrayRest { Exact, Discard, Bind(CoreBind) }

pub enum CoreExpr {
    // as today, plus:
    /// Run `op` on `args`. When it has an answer, bind it and go on with
    /// `found`. Otherwise go on with `missing`. Nothing is boxed, and a miss
    /// consumes nothing.
    Find { op: FindOp, args: Vec<LocalId>, bind: CoreBind,
           found: Box<CoreExpr>, missing: Box<CoreExpr>, ty: RTy },
}
pub enum FindOp { ArrayIndex, ArraySlice, ArraySet, MapGet }
```

- `PrimOp::ArrayIndex`, `ArrayIndexOr`, `ArraySlice` and `ArrayElem` go. `ArrayDrop` stays, with its count a constant from the pattern. `MakeArray`, `MakeRange`, `ArrayPrepend`, `ArrayAppend`, `ArrayConcat` and `ArrayLen` stay.
- New intrinsics: `ArraySet = "array__set"`, `ArrayPut = "array__put"` and `ArrayPushCopies = "array__push_copies"`.

**Reads, slices and writes.** The lowering emits `Find` where it already knows the answer is consumed at once:

- A two-arm `Some(v) -> A | None -> B` match with no guards on `xs[i]` becomes `Find { ArrayIndex, [xs, i], bind: v, found: A, missing: B }`. `xs[i] or d` is that shape, and the default now runs only on a miss.
- The same for `Ok(v) / Err(Nil)` on a slice, on a call to `array::set` (the callee is `Callee::Builtin { intrinsic }` in the typed IR, so this is a fact and not a guess) and for `Some / None` on a call to `map::get`.
- Any other use of one of these (stored, returned, matched with nested patterns) lowers to a `LetJoin` over a `Find` whose arms build the `Option` or `Result` with ordinary constructors. So the VM stops building `Some`, `None`, `Ok` and `Err` itself.
- `array::put(xs, i, x)` lowers to `Find { ArraySet, [xs, i, x], bind: ys, found: ys, missing: xs }` under a `LetJoin`.
- The loader's `MapFind` recognition and `option_arms` are deleted, so there is one mechanism.

**Array patterns.** `TypedPat::Array` stops emitting `ArrayLen`, `IntGe`, `ArrayElem` and `ArrayDrop` chains. The lowering picks one of two forms, from facts it already has when it builds the arm:

- *The arm cannot fail after the length test* (every element pattern is a name or `_`, and there is no guard). It emits `CorePat::Array { prefix, rest }` as written. With `rest: Bind` this consumes the array.
- *The arm can still fail* (a refutable element pattern such as `[" ", ..rest]` or `[Some(x), ..t]`, or a guard). It emits the pattern with `rest: Discard` in place of the binder, which reads the heads and takes nothing. Then come the nested tests and the guard. Only then comes `let t = ArrayDrop(n)(xs)`, which takes the tail. If the guard itself reads the tail, the `ArrayDrop` comes before the guard, and it shares, because the next arm still reads the array.

When a match's arms are exactly `[]` and one cons pattern with no guard, the lowering emits the cons arm first. They cannot both match, so the order is not observable, and the walk then pays one instruction per element and the `[]` test once at the end.

**Record updates read their kept fields first.** `translate.rs`'s `construct` today reads the fields a `..base` spread keeps after it has evaluated the given arguments, so `State(..s, v: [..s.v, n])` appends while `s` still holds the array. It changes to read every kept field straight after the base is evaluated (a field read has no effect a program can see). Then the last use of `s` is the `s.v` read, Perceus drops `s` there, and the array reaches the append with one holder. This needs no new Perceus feature.

## Instructions (`code.rs`)

```rust
Move          { dst, src, moved }
Array         { dst, elements: Box<[Arg]> }            // `[]` is a Const of EMPTY_ARRAY
Range         { dst, start, end }
ArrayLen      { dst, src }
ArrayGet      { dst, src, index, miss: u32 }           // dst = src[index] with one more reference, or jump
ArraySet      { dst, src, index, value: Arg, moved, miss: u32 }
ArrayPush     { dst, src, item: Arg, end: End, moved }
ArrayUncons   { head, tail, src, moved, miss: u32 }    // exactly `[h, ..t]`
ArraySplit    { src, heads: Box<[Option<Reg>]>, rest: Rest, moved, miss: u32 }   // every other array pattern
ArrayDrop     { dst, src, n: u32, moved }              // the tail, once an arm has committed
ArraySlice    { dst, src, start, end, moved, miss: u32 }
ArrayJoin     { acc, rest, cursor, a: Arg, b: Arg, done: u32 }
ArrayJoinStep { acc, rest, cursor }
```

`ArrayElem`, `ArrayPrepend`, `ArrayAppend`, `ArrayIndex`, `ArrayIndexOr`, `ArrayConcat` and `MapFind`'s loader fusion go.

Rules the loader keeps:

- **A miss writes nothing and takes nothing.** The handler tests first and takes after.
- **`Instr::writes` is a visitor**, like `reads`: `fn writes(&self, f: impl FnMut(Reg))`. `ArraySplit` writes one register per named head plus the rest, `ArrayUncons` writes two, `ArrayJoin` writes three. Liveness, `allocate` and `regs_mut` walk every one.
- **Clash edges.** `allocate` adds an edge between each destination and everything live after the instruction, and between every pair of destinations of one instruction. Without the second, a head nobody reads could share a register with the tail.
- **Liveness per edge.** For an instruction with a miss edge: `before = (before[hit] - writes) + before[miss] + reads`. Today the written register is removed on every successor, which is only right because a destination has one definition.
- **`moved`** is `!before[pc + 1].has(src)`, and false when the instruction reads `src` as another operand too.
- **`Move` takes at a last use.** `moves_on_last_use` sets `Move { moved }` by the same liveness. Today `Move` always shares, so `ys = if c { t = fresh(n) ... t } else { .. }` leaves a dead register holding the array and the next write copies. I measured that on the real VM: 4 cells against 3 for the same function spelled two ways.
- **`[a, b, ..xs]` and `[..xs, a, b]`** load as one `ArrayPush` per item. Every push after the first finds the array unique.
- **The join is two instructions.** `ArrayJoin` takes or shares both sides. If one is empty or both fit in 32 it finishes, sets `cursor` to 0 and jumps to `done`. Otherwise `acc` is an owned root of the longer side, `rest` is the shorter side, and `cursor` is how many elements are still to move, with its sign saying which end. `ArrayJoinStep` moves at most 64 leaves and stays at its own pc until `cursor` is 0, then releases `rest`. Between steps the frame holds two ordinary arrays and a small Int, so a step is a safe place to yield, like a call.
- The guard for an in-place flat write is one masked compare on the header: kind is `ArrayFlat` and count is 1.

A `Find` whose `found` is just the bound value under a `LetJoin` costs one moved `Move` (about 45). I do not special-case it. Copy coalescing in `allocate` would delete that `Move` and every `let %8 = %7; drop %7` pair, and it needs the same instruction-deleting pass as the dead-drop work, so it belongs there.

## The stdlib surface (`array.sca`)

```
// `Ok` of the array with `x` at index `i`, or `Err(Nil)` when `i` is outside it.
@vm(array__set) pub fn set(xs Array(a), i Int, x a) Result(Array(a), Nil)
// The array with `x` at index `i`. An index outside the array changes nothing.
@vm(array__put) pub fn put(xs Array(a), i Int, x a) Array(a)

// An array of `n` copies of `x`, and `[]` when `n` is 0 or less.
pub fn repeat(x a, n Int) Array(a) { fill([], x, n) }
fn fill(acc Array(a), x a, n Int) Array(a) {
	if n <= 0 { acc } else { fill(push_copies(acc, x, n), x, n - 1024) }
}
// `xs` with up to 1,024 more copies of `x`.
@vm(array__push_copies) fn push_copies(xs Array(a), x a, n Int) Array(a)

pub fn swap(xs Array(a), i Int, j Int) Array(a) {
	match xs[i] {
		None -> xs
		Some(a) -> match xs[j] {
			None -> xs
			Some(b) -> xs |> put(i, b) |> put(j, a)
		}
	}
}
pub fn map(xs Array(a), f fn(a) b) Array(b) { map_onto(xs, [], f) }
fn map_onto(xs Array(a), acc Array(b), f fn(a) b) Array(b) {
	match xs {
		[] -> acc
		[h, ..t] -> map_onto(t, [..acc, f(h)], f)
	}
}
pub fn contains(xs Array(a), target a) Bool { contains_from(xs, target, 0) }
fn contains_from(xs Array(a), target a, i Int) Bool {
	match xs[i] {
		None -> False
		Some(x) -> if x == target { True } else { contains_from(xs, target, i + 1) }
	}
}
```

- `filter` and `reverse` get the same direct loop as `map`. `fold` and `each` keep their source and get `ArrayUncons`. These pass elements on, so moving them out of a unique array matters.
- `contains`, `find`, `any` and `all` become index loops. They only look, they often stop early, and an index loop never copies anything, whoever else holds the array. A `[h, ..t]` walk of a kept long array would pay 4,284 to 6,261 before looking at one element.
- `repeat` is a Scarlet loop around a built-in that does at most 1,024 elements, so the loop's tail call is where the VM can yield. `push_copies` adds to the element's count with a new `Heap::share_many(cell, n)` that stops at the sticky value, as `share` does. A count no heap can hold is never reached, because the heap fills first and that stops the process as any allocation does.
- `concat`'s comment changes: the join costs the shorter side.
- `map`, `fold`, `filter` and `each` stay Scarlet functions. A built-in that called a closure would put a Scarlet frame on the host stack and hide a call from the reduction count.

## Worked example: fib

fib has no array value and no array instruction, so nothing changes. Core IR before and after (`scarlet dis benches/fib/fib.sca`):

```
fn main.fib(%0:0) -> :0
  let %1:0 = c0
  let %2:1 = IntLt(%0, %1)
  if %2 :0
    ret %0
  else
    let %3:0 = c1
    let %4:0 = IntSub(%0, %3)
    let %5:0 = call self(%4)
    let %6:0 = c0
    let %7:0 = IntSub(%0, %6)
    let %8:0 = call self(%7)
    ret IntAdd(%5, %8)
```

The loaded instructions are the same before and after: `JumpUnlessCmpImm; Ret; IntImm; CallSelf; IntImm; CallSelf; Int; Ret`. It stays at 355.2 instructions per call. The only shared changes are the `View::EmptyArray` arm, which fib never reaches, and the `moved` flag on `Move`, which fib has none of.

## Worked example: sieve

Before, one turn of `strike` calls `clear`, which is two `ArraySlice`s each matched and dropped, an `ArrayAppend` and an `ArrayConcat`: 53,349 to 56,408 instructions.

After step 1 the source is `strike(array::put(flags, k - 1, False), k + step, step, size)` and `clear` is gone:

```
fn main.strike(%0:6, %1:0, %2:0, %3:0) -> :6
  let %4:1 = IntLe(%1, %3)
  if %4 :6
    let %5:0 = c1
    let %6:0 = IntSub(%1, %5)
    let %7:1 = false
    letj %8:6 = find ArraySet(%0, %6, %7) | %10 -> ret %10 | missing -> ret %0
    drop %0
    let %9:0 = IntAdd(%1, %2)
    ret call self(%8, %9, %2, %3)
  else
    ret %0

0  JumpUnlessCmp  %1 <= %3 else -> 10                50
1  IntImm         %6 = %1 - 1                        59
2  Const          %7 = false                         31
3  ArraySet       %10 = %0[%6] <- %7, moved, miss -> 6    about 245 on the trie, about 370 on today's tree
4  Move           %8 = %10, moved                    about 45
5  Jump 7                                            18
6  Move           %8 = %0, moved                     ; the miss edge, never taken here
7  Drop           %0                                 30, finds the register empty
8  Int            %9 = %1 + %2                       70
9  TailCallSelf   (%8, %9, %2, %3)                   about 250
10 Ret            %0
```

`flags` is unique here: `sieve` passes it to `strike` at its last use, the set takes it on the hit edge, and the tail call moves the result on. One turn is about 800 on the trie and about 920 on today's tree, against about 57,000. LuaJIT spends about 180.

The read in `sieve`, before and after step 3:

```
before:
  letj %9:1 =
    let %7:12 = ArrayIndex(%0, %6)
    match %7 :1
      | 7.1() -> ret false
      | 7.0(%8) -> ret %8
  ArrayIndex (allocates Some) ; JumpUnlessVariant ; Field ; Drop (frees Some)        900 to 1,163

after:
  letj %9:1 =
    find ArrayIndex(%0, %6)
      | %8 -> ret %8
      | missing -> ret false
  ArrayGet %8 = %0[%6] miss -> L ; Move %9 = %8, moved ; Jump J ; L: Const %9 = false ; J:
  about 600 on today's tree, about 265 on the trie
```

## Worked example: walking, and an arm with a guard

`array::fold` after step 4:

```
fn array.fold(%0, %1, %2)
  match %0
    | [%3, ..%4] -> drop %0
                    let %5 = call value %2(%1, %3)
                    ret call self(%4, %5, %2)
    | [] -> ret %1

0  ArrayUncons  head %3, tail %4 = %0, moved, miss -> 4
1  Drop         %0                       ; finds it empty
2  CallValue    %5 = %2(%1 moved, %3 moved)
3  TailCallSelf (%4 moved, %5 moved, %2)
4  ArraySplit   %0, no heads, exact, miss -> 6     ; the `[]` test: one word compare, once per walk
5  Ret          %1
6  Bad          NoArm
```

Seven array-related instructions per element become one. On a flat unique array the step is a move and a bump: 64 in the library against 1,596 (8 elements) and 2,167 (32).

A guarded arm today (`scarlet dis` on `[h, ..t] if h > pivot -> keep(t, pivot, acc + h)`):

```
let %6 = ArrayLen(%0) ; let %7 = c1 ; let %8 = IntGe(%6, %7)
if %8
  let %9 = ArrayElem(0)(%0) ; let %10 = c1
  let %11 = ArrayDrop(%0, %10)          ; the tail is built before the guard
  let %12 = IntGt(%9, %1)
  if %12 ... else drop %11 ; goto j1     ; and thrown away when the guard fails
```

The next arm reads `%0`, so that `ArrayDrop` can never be `moved` and copies every time. After step 4:

```
match %0
  | [%9, ..] ->                          ; reads the head, takes nothing
      let %12 = IntGt(%9, %1)
      if %12
        let %11 = ArrayDrop(1)(%0)       ; the arm has committed
        drop %0
        let %13 = IntAdd(%2, %9)
        ret call self(%11, %1, %13)
      else goto j1
  | _ -> goto j1

ArraySplit %0, heads [%9], discard, miss -> j1 ; JumpUnlessCmp ; ArrayDrop %11 = %0 drop 1, moved ; ...
```

On a unique array that is about 110 for the read and about 120 for the take, where the fused form would pay 892 to 6,261 per element.

## Worked example: permute's swap

After step 2, `moved = State(..s, v: swap(s.v, n1, i))` lowers to:

```
let %4 = Field(1)(%0)     ; count, a kept field, read first
let %5 = Field(2)(%0)     ; check, a kept field
let %6 = Field(0)(%0)     ; s.v, the argument
drop %0                   ; the State is freed here, so %6 is the array's only holder
let %7 = call swap(%6, %1, %2)
let %8 = ctor State(%7, %4, %5)
```

And `swap` with the stdlib's `put`:

```
0  ArrayGet  %3 = %0[%1] miss -> 3      4  ArrayGet  %6 = %0[%2] miss -> 7
1  Move      %5 = %3, moved             5  Move      %8 = %6, moved
2  Jump 4                               6  Jump 8
3  Const     %5 = 0                     7  Const     %8 = 0
8   ArraySet  %9  = %0[%1] <- %8, moved, miss -> ..   ; unique: one store, about 135
9   ArraySet  %10 = %9[%2] <- %5, moved, miss -> ..   ; about 135
10  Ret %10
```

One swap is about 700 instructions against 10,803 for the two recursive `set` calls plus 840 for the reads.

## Why a persistent structure above 32, and not one flat block

All the designs considered use the same flat cell for arrays of 32 or fewer, so they only differ above 32. There a flat block wins every operation on an array one holder has (about 60 against 146 for a read at 5,000) and loses by the array's length whenever something else holds the array at a write:

| per operation | strict trie | flat block |
|---|---:|---:|
| write to a shared array of Ints: 1,000 / 5,000 / 100,000 | about 5,800 / 7,735 / about 11,100 | 7,353 / 32,821 / 562,662 |
| write to a shared array of cells, 1,000 | about 8,000 | 94,295 |
| push onto an array another holder keeps: 1,000 / 10,000 | 2,601 / about 2,600 | 7,979 / 61,773 |

The record case is not an argument here: step 2 removes it for either design. What is left:

- **Arrays shared on purpose.** Values are immutable, so keeping an old version is ordinary code: queens keeps the board to backtrack, a server keeps a snapshot while it builds the next state, a list of defaults is extended per request. With a block every such write costs the array's length.
- **Arrays shared by the compiler.** A closure that captures an accumulator (`x <- result::then(..)` in `benches/json/json.sca`, which builds a 156-element array of cells that way), and a value whose last use sits inside an `if` used as a value (invariant D's gap). The programmer cannot see either. With a trie the price is a bounded path copy.
- **Cell size.** Every array cell is 34 words or fewer. No instruction copies megabytes, no array needs a chunk of its own, and no push is a length-sized reallocation that could trip a process's heap limit.

The scoreboard does not decide this: critic arithmetic I checked puts the difference at 8 to 16% of sieve and under 1% of the geomean either way. The reason is that the trie's worst case is a constant a person can state, and the block's depends on what Perceus and closure captures did.

## What I rejected, and why

- **One flat block for every size**, for the reasons above.
- **A slice-view kind for shared walks.** It would make a held walk about one small allocation per step instead of a copy of at most 31 slots. It adds a fifth form to every operation, and the cause (a second holder) is removed where it arises: by step 2 for records, by the tuple work for `match (xs, ys)`, and by drop specialisation for `match t.input`.
- **Handing a shared tree's first leaf over without copying the branches above it.** It leaks, as described under uncons.
- **Loader fusion** for reads, slices, map lookups and the `[]` and `[h, ..t]` pair. `Find` and the lowering's arm order say the same things as facts.
- **`ArraySwap`, `ArrayPushMany`, `last`, in-place `map` through take and put, an `update` built on a typed hole.** None had a measured gain as an instruction.
- **`repeat` built from one shared leaf.** It makes the first write to every leaf of a "unique" array copy.
- **An `owned` flag through every function.** The consume rule replaces it, and I tested that.
- **A total `set` with a separate `try_set` written in Scarlet.** A caller that handles `Err` by using the original array keeps it live across the call, so the checked form would always copy. The miss edge fixes that for both forms.
- **Exact capacity for literals and slices.** The first push would always reallocate.
- **A rule that repacks any tree two levels too tall.** It would put a length-sized step inside one instruction. The bounded rule (at most 32 leaves) covers arrays up to 1,024.

## What this design needs from other areas

- **Perceus drops inside joins** (`refcount-traffic`). Invariant D's gap. Until it lands, an array or record whose last use is inside an `if` or `match` used as a value is copied on its next write.
- **Scrutinees that are still held.** `match t.input { [p, ..rest] -> Task(..t, input: rest) }` and `match (xs, ys) { ([a, ..ta], [b, ..tb]) -> .. }` uncons an array a record or tuple still holds, so each step is on the "held at every step" row (570 to 2,237 flat, 860 to 1,074 root). Drop specialisation at a match and not building a tuple for a tuple scrutinee remove it. Nothing here changes when they land.
- **Closures that capture an accumulator** (`no-inlining`).
- **Cell lookup** (`heap-cell-addressing`). A node visit is about 30 instructions, of which the lookup is about 16.
- **Alloc and free** (`cell-alloc-free-path`). The copy paths are about four fifths one cell made and one freed. After this lands, fannkuch makes 3 to 4 cells per flip turn instead of 9.0 and storage stops making 5,461 root cells per round, so whoever sums the areas must take the alloc, free and addressing shares for sieve, permute, queens, fannkuch and storage from a run made after this, not from today's diagnosis.
- **Dead drops and copy coalescing** (`dead-binds-and-noop-drops`). The `Drop` after a consuming instruction is a 30-instruction no-op, and the `Move` after an `ArrayGet` in a join is about 45.
- **Decision-tree matching** (`match-linear-chains`). When it lands, `[Plus, ..r] | [Minus, ..r] | []` becomes one head read and a switch, and the read-then-commit rule becomes a case of it.


## Steps

### 1. 0. A `Move` takes its source at a last use

**Changes.** crates/scarlet_vm/src/code.rs: `Instr::Move { dst, src, moved }`; `moves_on_last_use` sets `moved` when nothing later reads `src` (it runs on one-register-per-local code, before `allocate`). crates/scarlet_vm/src/exec.rs (the `Instr::Move` arm, line 152): take when `moved`, share otherwise. A test in scarlet_vm that asserts `internal::cells_made` is the same for `ys = if c { t = fresh(n) ... t } else { fresh(0) }` and for the same function ending the branch with `fresh(n)`.

**Expected gain.** No scoreboard claim. Measured on the real VM today: the first spelling makes 4 cells per call and the second 3, because `ret %7` inside the join loads as a sharing Move and the dead register keeps the array. Each `let %8 = %7; drop %7` pair also stops paying a share (19) and a non-freeing release (about 50), about 70 per joined heap value, which belongs to the refcount area's count. This step exists so that 'the count is 1' means what the later steps need.

**Risk.** Low. A `Drop` of the source already tolerates an empty register, which is how calls move today. A `DropReuse` counts as a read, so a cell paired with a constructor is not taken.

### 2. 1. A write at an index, on today's tree: `array::set` (checked), `array::put` (total), `swap`

**Changes.** crates/scarlet_ir/src/intrinsic.rs: `ArraySet = "array__set"`, `ArrayPut = "array__put"`. crates/scarlet_vm/src/code.rs: `Instr::ArraySet { dst, src, index, value: Arg, moved, miss }` with a miss successor; loaded from `ArrayPut` as the set, a Jump, and a moved Move on the miss edge; loaded from `ArraySet` as the set with `Ok` built on the hit edge and `Err(Nil)` on the miss (step 3 moves that into the lowering). `moves_on_last_use` sets `moved` from the hit edge. crates/scarlet_vm/src/array.rs: `set` for today's tree. In place when the root and every node on the path are unique (45 lines, built by a critic in perf-scratch/arrays-attack-future/src/old_array.rs); otherwise copy the leaf and one branch per level. crates/scarlet_vm/src/exec.rs: the handler, with the take-or-share line after the bounds test. crates/scarlet_core/src/std/scarlet/array.sca: `set`, `put`, `swap`. benches/sieve (clear), permute (set), queens (set), fannkuch (set, swap01, swap12): call `array::put`. docs/semantics.md ("Arrays": what a write out of range does). A changeset. The owner picks the two names first.

**Expected gain.** sieve: the non-clear remainder is 1.047e9 (measured by a critic with clear made a no-op); 498,105 sets at 309 + 58 add 0.183e9, so 29.08e9 becomes about 1.23e9, ratio 328 to about 13.9. permute: the set recursion is 111.69M of 154.9M per iteration; 10,078 swaps become one copying set (about 1,400 on today's tree, estimated from today's 1,466 for a prepend that makes one leaf and one root) and one in-place set (about 170), 15.8M, so 59.0M is left, ratio 32.5 to about 12.4. queens: set is about 7.38M of 10.25M per run; 452 copying sets at about 1,400 are 0.63M; 3.5M left, ratio 29.6 to about 10.1. fannkuch: set 5.40G to about 0.3G and the swaps over set save about 1.7G, 6.8G of 27.74G, ratio 27.7 to about 20.9. Geomean factor exp((-3.16 - 0.97 - 1.08 - 0.28) / 23) = 0.788, so 6.17 to about 4.86. The diagnosis priced this item alone at 0.776.

**Risk.** The benchmark sources change in this step, so its before and after run different programs; that is the point of the step and it is the only one where it is true for these four. The copy path on today's tree was not built, so its 1,400 is an estimate. permute's and queens' arrays are shared at the write today (the sceptic's correction 2), so they land on that path until step 2.

### 3. 2. A record update reads its kept fields first

**Changes.** crates/scarlet_lower/src/translate.rs, `construct` (lines 667 to 702): spill each kept field's read (`spread.fills`) into a `let` straight after the base is evaluated, before the remaining given arguments, instead of reading them inside the final `Ctor`. A lowering test that `State(..s, v: [..s.v, n])` prints `Field, Field, drop %0, ArrayAppend` in `scarlet dis`. docs/core-ir-spec.md if it states the order.

**Expected gain.** Measured on the real VM with the source hand-edited to the new order: `go(State(..s, v: [..s.v, n]), n - 1)` over 300,000 steps is 1,372,223,504 instructions as written and 739,833,727 with the fields read first, which is 2,108 saved per step; taking the record apart with a match gives 734,235,174. permute: the first set of each swap goes from copying (about 1,400) to in place (about 170) for 10,078 swaps, 12.4M of 59.0M, ratio about 12.4 to about 9.8. Geomean factor exp(-0.235 / 23) = 0.990, so about 4.86 to 4.81. Not counted: richards' shared map set is the same shape (`Scheduler(..s, tasks: map::set(s.tasks, k, v))`, 10.7% of that run by the diagnosis), but whether its record is unique there was not checked.

**Risk.** Low. A field read has no effect a program can see, so evaluation order as the source wrote it is unchanged. It does nothing when the record itself is shared, or when the update sits inside an `if` or `match` used as a value, because Perceus puts no drop inside a join; that gap is named under the invariants.

### 4. 3. Reads, slices, writes and map lookups that branch: `Find` in the IR

**Changes.** crates/scarlet_ir/src/core_ir/mod.rs: `CoreExpr::Find { op, args, bind, found, missing, ty }` and `FindOp::{ArrayIndex, ArraySlice, ArraySet, MapGet}`; remove `PrimOp::ArrayIndex`, `ArrayIndexOr` and `ArraySlice`. crates/scarlet_lower/src/lower.rs: `index_or` (842 to 880) emits a bare `Find` for a two-arm `Some/None` match with no guards on `xs[i]` whatever the arms are, and the same for `Ok/Err` on a slice or on a call whose callee is `Builtin { ArraySet }`, and for `Some/None` on `Builtin { MapGet }`; any other use lowers to a `LetJoin` over a `Find` whose arms build the `Option` or `Result` with the constructors the compiler already resolves for `Program::abi`; `array::put` lowers to a `Find` whose `missing` is the array. crates/scarlet_lower/src/perceus.rs and the reuse walk: `Find` reads its args and binds one owned local in `found`. crates/scarlet_vm/src/code.rs: `Instr::ArrayGet { dst, src, index, miss }` and `Instr::ArraySlice { dst, src, start, end, moved, miss }`; `ArraySet` and `MapFind` are emitted from `Find`; delete the `MapGet`-then-`Match` recognition (1384 to 1410) and `option_arms`. crates/scarlet_vm/src/exec.rs: the handlers; the VM's own building of `Some`, `None`, `Ok` and `Err` for arrays goes. `scarlet dis` prints `find`. docs/core-ir-spec.md.

**Expected gain.** On today's tree a read that boxes costs 900 to 1,163 and one that does not costs 325 to 604 (sceptic and sieve report). queens: 1,452 reads per run save about 520 each, 0.755M of 3.5M, ratio about 10.1 to 7.9. sieve: 225,000 reads save 559 each, 0.126e9 of 1.23e9, about 13.9 to 12.5. string_keys: the `""` default of `letters[r % 6] or ""` is 10.7% of the run (sceptic) and now runs only on a miss, 2.93 to about 2.62. fannkuch: about 2.27M slices stop building and freeing a Result at about 400 each, 0.9G of 20.9G. permute and spectral_norm already use the fused read. Geomean factor exp(-0.51 / 23) = 0.978, so about 4.81 to 4.71.

**Risk.** A new CoreExpr form touches every pass that walks Core IR; the no-wildcard lint finds each place. `match xs[i] { Some(0) -> .. }` still builds the Option until decision-tree matching lands. The string_keys part overlaps the strings area's sticky literals, and only one of the two should count it. `FindOp::MapGet` changes code the maps area owns and must be agreed with it. A default with a side effect now runs only on a miss, which is what the source says, and the old fusion refused such defaults, so no program can tell.

### 5. 4. Array patterns in the IR: `ArrayUncons`, `ArraySplit`, and a tail taken only once the arm has committed

**Changes.** crates/scarlet_ir/src/core_ir/mod.rs: `CorePat::Array { prefix: Vec<Option<CoreBind>>, rest }` with `ArrayRest::{Exact, Discard, Bind}`, added to `CorePat::binds`; remove `PrimOp::ArrayElem`; `PrimOp::ArrayDrop` takes a constant count. crates/scarlet_lower/src/lower.rs: `TypedPat::Array` (1173 to 1205) emits the pattern as written when the arm cannot fail after the length test, and otherwise emits it with `Discard`, queues the nested tests and guard, and emits `ArrayDrop` after them (before the guard only when the guard reads the tail); `match_arms` (935 to 952) treats `CorePat::Array` as a refutable head and puts the cons arm first when the arms are exactly `[]` and one unguarded cons. crates/scarlet_lower/src/perceus.rs: the arm's binders are owned. crates/scarlet_vm/src/code.rs: `Instr::writes` becomes a visitor and `liveness`, `allocate` and `regs_mut` walk every destination; `allocate` adds a clash edge between every pair of one instruction's destinations; liveness is per edge for instructions with a miss; `Loader::pattern` emits `ArrayUncons` for `[h, ..t]` and `ArraySplit` for every other array pattern; `ArrayPrepend` and `ArrayAppend` become one `Instr::ArrayPush { dst, src, item: Arg, end, moved }` per item; `Instr::Array`'s elements become `Arg`s. crates/scarlet_vm/src/exec.rs: the handlers, on today's `get`, `skip` and `skip_owned`; `Machine::args`'s Vec goes for array literals. Loader tests: `[a, b, c, ..rest]` with a fifth value live across it shares no register; `[_h, ..t]` gives the head no register; a guarded arm's `ArrayDrop` is `moved` and its head read is not.

**Expected gain.** On today's tree this removes two `ArrayLen` (111 each), two compares, a `Const` and part of `ArrayElem` per walked element, about 450 of 770 to 1,050 on long arrays and of about 2,090 on short ones; the cost inside `skip` stays until steps 6 and 7. closures: the walk group is about 24% of the run and about half of it goes, 8.30 to about 7.3. strings about 4%, bounce about 4%, nbody about 3%, richards about 2%, fannkuch about 1%. Guarded and refutable cons arms stop building and dropping a tail per failed test: examples/interpreter.sca, examples/chat.sca and json/decode.sca have 7 such arms of about 50; no benchmark has one, so this is not on the scoreboard. Geomean factor exp(-0.27 / 23) = 0.988, so about 4.71 to 4.65.

**Risk.** An instruction with several destinations is new to the loader, and a missing clash edge would alias two of them; the tests above pin the cases a critic found. The lowering needs to know whether a guard reads the tail binder. A scrutinee read out of a record or tuple that is still held gets the instruction saving but not the in-place walk. The `drop` Perceus puts after a consuming pattern is a 30-instruction no-op until the dead-drop work deletes it. If decision-tree matching lands first, array patterns become a case of it.

### 6. 5. The empty array is an immediate, and no array cell can have length 0

**Changes.** crates/scarlet_vm/src/value.rs: `Value::EMPTY_ARRAY` (TAG_IMMEDIATE | 3) and `View::EmptyArray`, with an arm in every match on a View. crates/scarlet_vm/src/array.rs: `Seq::Empty`; `seq(heap, Value)` as the only door; `from_values`, `skip`, `take`, `slice`, `concat` and the range constructor return `Value` and mint cells in one private function each, which returns the immediate for 0; `ArrayError { Full, Broken(BadArray) }` mapped to `Stop::broken`. crates/scarlet_vm/src/exec.rs: the built-ins that wrap `from_values` in `Value::cell` (lines 1216, 1227, 1401, 1470, 1679, 1997, 2176) use the returned value. eq.rs and hash.rs: arrays compare and hash through `Seq`. crates/scarlet_vm/src/code.rs: a `MakeArray` of nothing loads as a `Const`. A test that passes `[]` to every built-in that takes an array, that `5..5 == []`, and that a map finds under `[]` a value stored under `5..5`.

**Expected gain.** storage: 5,461 empty arrays per round stop making and freeing a root cell, about 400 each, 2.2M of 54.5M, ratio 3.81 to about 3.66. A walk's `[]` test becomes a word compare. Geomean factor about 0.998, so about 4.65 to 4.64.

**Risk.** The crate has 66 `as_cell()` call sites that would quietly treat the immediate as 'not a cell', against 57 `.view()` sites the no-wildcard lint protects; routing every array through `seq` is what covers them, and the test above is the check. Tests that assert `cells_made` for array programs need new numbers.

### 7. 6. The flat cell for 1 to 32 elements

**Changes.** crates/scarlet_vm/src/heap.rs: `Kind::ArrayFlat` (held 2..size), `cell`, `cell_mut`, `make_filled`. crates/scarlet_vm/src/array.rs: the flat form with its window and capacity classes 4, 8, 16, 32: `get`, `set`, push at both ends with the slide, `uncons`, `drop_front`, `take`, the join of two flat cells into one, the conversions to a root at 33 and back from a cut that leaves 32 or fewer, and the in-place functions for set, push and uncons, ported from perf-scratch/arrays-final/src/pv.rs with every function consuming one reference (the `handed` switch there, made unconditional). The leaf iterator used by eq.rs, hash.rs, show.rs, binary.rs and json.rs gets a flat arm. crates/scarlet_vm/src/exec.rs: `Instr::Array` writes its Args straight into the cell. Today's tree stays as it is above 32. The differential test, the heap-fill test and the `Nil` test move into array.rs's tests, with today's functions as the model for the root arm. docs/vm-design.md: the array paragraph says arrays of 32 or fewer are one cell.

**Expected gain.** Unit costs are the library costs plus 46 to 58 of handler. fannkuch: in-place prepends fall from 874 and 877 to about 145, the offset prepend from 2,819 to about 700, slices, reads and rotate save about 2G, less one regrow per flip turn at about 860 (1.4G) that a critic showed the first estimate missed; 54 to 57% of the original is gone with steps 1 and 3, so ratio about 20.0 to about 11.4 here. storage: appends are 27.5M of 54.5M per round and become 22,419 x 145 + 5,461 x 250 + 2,458 regrows x 750 + 819 x 856 = 7.2M, ratio about 3.66 to about 2.25. permute: 20,156 reads fall from 420 to 110, 6.2M of 46.6M, about 9.8 to 8.4. queens: reads fall by a further 375 each (0.54M) and the copying set to about 1,270, 2.75M to 2.12M, about 7.9 to 6.1. strings: the 4-element walk goes from about 1,640 per element to about 150, 13.8% of what is left. nbody about 13%, richards about 6%, string_keys about 5.5% (read 290 to 110), json about 2%. Geomean factor exp(-1.89 / 23) = 0.921, so about 4.64 to 4.27.

**Risk.** Every array test runs on new code for short arrays. The glue between a flat cell and today's root (grow past 32, cut to 32 or fewer) is replaced in step 7 if the owner says yes there; it is about a hundred lines. A join of two short arrays copies their elements where today's tree shares two leaves: 911 against 754 at 4 + 4 and 1,702 against 750 at 16 + 16, with the result freed. Capacity classes cost up to twice the words of an exact cell. The fannkuch checkpoint will read about 11 to 12, not the 10 an earlier estimate gave.

### 8. 7. The form above 32: a strict trie, and a join in bounded steps (needs the owner's yes)

**Changes.** Needs decision A first, because it changes the Decided RRB point in docs/vm-design.md. crates/scarlet_vm/src/array.rs: replace today's root, leaf and branch with `ArrayRoot` and `ArrayNode`, porting pv.rs (get, set, push at both ends, uncons, take, drop_front, slice, from_values, the own functions, hang_back, hang_front, detach_first_leaf, prune, collapse with the 32-leaf re-hang, check) and the join as built in arrays-final (`join`, `own_root_of`, `extend_back`, `extend_front`, `run_ending`), with a leaf budget and a cursor on the two extend functions. Delete size tables, size_slot, tree_take, tree_drop, merge, rebalance, plan and repack. Today's array.rs moves into the test tree as the model for one differential test in this PR. crates/scarlet_vm/src/heap.rs: `ArrayLeaf` and `ArrayBranch` become `ArrayNode`. crates/scarlet_vm/src/code.rs and exec.rs: `ArrayConcat` becomes `ArrayJoin { acc, rest, cursor, a, b, done }` and `ArrayJoinStep { acc, rest, cursor }`; the instruction set is otherwise unchanged. The range arm of uncons moves a unique range's start in place. crates/scarlet_core/src/std/scarlet/array.sca: `concat`'s comment. docs/vm-design.md: the array paragraph. A changeset. If the owner says no: build in-place push and uncons on today's tree with capacity in the head and tail leaves instead, and keep its join.

**Expected gain.** sieve: sets fall from 367 to 244 (0.061e9), 225,000 pushes from about 1,457 with their loop to about 535 (0.207e9), reads from about 604 to about 265 (0.076e9); 1.10e9 to about 0.76e9, ratio about 12.5 to about 8.5. closures: appends are 14.8% at 823 and become about 235, and a walked element of a 1,000-element array falls from about 450 to about 240; 18% of what is left, about 7.3 to 6.0; its first map walks a kept range at 86 to 146 per element (measured), which is within that. bounce (100 balls): about 11%. spectral_norm (100 elements): x[j] 362 to about 205, about 8%. Geomean factor exp(-0.80 / 23) = 0.966, so about 4.27 to 4.12. An in-place-tuned version of today's tree would get within 1% of this (read 240 against 146, write 259 to 309 against 186), so this step's scoreboard gain does not depend on the answer to decision A.

**Risk.** It reverses a recorded decision for simplicity, not for speed. A join of two long arrays gets slower by 5 times at 5,000 a side, about 80 times at 100,000 and about 700 times at a million (measured, in the table under decision A). The budget and cursor on the join, and the two-instruction form, are specified and not built; the join itself is built and passes the differential test. Element costs for a join of cells were not measured (expect about 25 more per element for the share). The in-place functions were measured as calls, not inlined in the real dispatch loop.

### 9. 8. Stdlib loops, `array::repeat`, and the two benchmark sources that still spell a loop the slow way

**Changes.** crates/scarlet_core/src/std/scarlet/array.sca: `map`, `filter` and `reverse` as their own loops over `[h, ..t]` with an accumulator; `contains`, `find`, `any` and `all` as index loops; `repeat` as a loop over `push_copies`. crates/scarlet_ir/src/intrinsic.rs: `ArrayPushCopies = "array__push_copies"`. crates/scarlet_vm/src/heap.rs: `share_many(cell, n)`, which stops at the sticky count. crates/scarlet_vm/src/code.rs and exec.rs: `ArrayPushCopies` as a `Builtin` that appends at most 1,024 copies. benches/sieve: `make_flags` becomes `array::repeat(True, 5000)`. benches/fannkuch: `flip` reverses in place with `array::put`, as the Lua twin does. A changeset.

**Expected gain.** closures: the sceptic measured this rewrite of map and filter on the original at 26,385M to 23,846M; that 2.54G is about 13.4% of what is left, about 6.0 to 5.2. sieve: `make_flags` is 225,000 pushes at about 535 with their loop, 0.120e9 of 0.76e9, about 8.5 to 7.1. nbody about 4.5%. fannkuch: flip and its regrows are about 10G of the 11.4G left; with an in-place reversal a flip turn is about 1.6 swaps at about 600 plus about 400 of loop, about 2.2G for 1.59M turns, so about 3.6G to 4.5G is left, ratio about 11.4 to about 4. Geomean factor exp(-1.41 / 23) = 0.940 with the fannkuch rewrite and exp(-0.37 / 23) = 0.984 without it, so about 4.12 to about 3.9 or 4.05.

**Risk.** Low for the stdlib part. The closures gain overlaps the inliner in the no-inlining item. The fannkuch figure is the softest number in this plan: it is arithmetic on a source rewrite nobody has run. The index loops free a last-use array at the end of the search and not leaf by leaf.

## Experiments

Everything below was run or read in this session. Nothing ran inside the real VM with the new arrays, so every scoreboard figure is still arithmetic from the diagnosis's shares and these unit costs.

**New code, in perf-scratch/arrays-final (a copy of a critic's copy of the prototype, own target directory, `cargo build --release --offline`).**

1. *A tuned join, both directions* (`join`, `own_root_of`, `extend_back`, `extend_front`, `run_ending` at the end of src/pv.rs). It builds one flat cell when both sides fit in 32, and otherwise reads the root once, adds the shorter side a leaf at a time at either end, shares a whole source leaf when it lands on a boundary, and writes the root once. I swapped it into the differential test: it failed on the first run (a root that had shrunk below 33 was read as one run), I fixed that, and it then passed on 8 seeds x 2 element kinds x 20,000 steps with the leak count at 0. Per join, sources kept, as join alone / with the result freed / today with the result freed: 4+4: 616 / 911 / 754. 16+16: 912 / 1,702 / 750. 33+40: 2,665 / 4,221 / 7,918. 100+100: 7,618 / 10,975 / 11,310. 100+5,000: 7,204 / 14,241 / 25,623. 5,000+100: 9,059 / 14,247 / 29,554. 1,001+1,003: 40,473 / 67,785 / about 32,000. 5,000+5,000: 201,308 / 336,707 / 40,171. 5,001+5,003: 208,543 / 322,044 / 41,084. 100,000+100,000: 2.38M / 2.65M / 51,535. 100,001+100,000: 4.45M / 6.91M / 57,688. 99,990+100,000: 4.48M / 7.02M / 52,150. 1,000,001+1,000,003: 40.6M / 68.8M. So about 44 per element of the shorter side for the join and 25 more when the result is freed, Int elements. Before the rewrite the same binary's join gave 10.18M for 100,001+100,000 and 36.73M for 99,990+100,000, which reproduces the critic's 10.19M and 36.69M. Runs with few iterations over a large setup are noisy (1,003+100,001 gave 71,570 and 104,006 on two runs).

2. *The bounded re-hang rule* (in `collapse`): when a cut leaves at most 32 leaves under a tree taller than one branch, hang them under one new branch. The critic's `tall` probe gave shift 15 / 20 / 25 for a 100-element array cut from 2,000 / 100,000 / 2,000,000 without the rule and gives shift 5 with it. The differential test passes on 8 seeds. Root-form results one level over the packed height fell from about 31% to about 23% (3,780 of 16,225 on seed 1), and two levels over from about 1% to about 0.1%.

3. *The consume-one-reference rule* (`handed`, behind `CONSUME=1`): every public function shares a kept array and then runs as its owner. The differential test passes on 6 seeds x 2 kinds x 20,000 steps, with the fast paths on and off. It costs 1,269 against 1,174 for a shared write at 16 elements, 7,735 against 7,622 at 5,000, 2,601 against 2,474 for a held push at 1,000, and 183 against 167 per element of a kept 8-element walk.

4. *Ranges* (`range`, `range_uncons`): a walk costs 146 / 93 / 86 per element at 4 / 32 / 1,000 when owned and 141 / 93 / 86 when the caller keeps the range, so the closures benchmark's first map (a kept `0..1000`) is cheaper per element than a trie walk.

**Critics' numbers I re-ran in the same crate, all of which reproduced.** A walk where something else holds the array at every step, per step: 570 / 711 / 871 / 1,158 at 4 / 8 / 16 / 32 Ints, 860 at 33, 921 / 971 at 1,000 / 5,000; with cell elements 766 / 1,033 / 1,446 / 2,237, 963, 1,025 / 1,074; today's tree 1,340 / 1,558 / 1,782 / 2,127, 743, 965 / 1,018. Unique walk 64.5 / 68 / 151 / 153. Write in place with and without the fast path: 83 / 88 at 16, 155 / 376 at 1,000, 185 / 470 at 5,000, 215 / 563 at 100,000. A critic's 45-line in-place set on today's tree: 111 / 244 / 309 / 375. Reads 52 / 115 / 142 / 169 against today's 121 / 211 / 272 / 336. Slice of half, source kept, result freed: 1,266 / 2,918 / 7,384 / 14,211 against today's 2,919 / 4,339 / 15,361 / 18,966.

**Real VM (./target/release/scarlet).**

5. The dead-register probes in scratch/atk: n_a.sca makes 4 cells per call and n_b.sca 3 for the same function, and `scarlet dis` shows `ret %7` inside the `letj` with no drop. m_a.sca, with one parameter instead of five, makes 3.

6. The record probes in perf-scratch/attack-arrays-numbers/sca over 300,000 steps: spread_a.sca (the update as written) 1,372,223,504 instructions, spread_b.sca (matched apart) 734,235,174, spread_c.sca (fields read into locals first, no match) 739,833,727. `scarlet dis` of spread_c shows `Field(0)`, `Field(1)`, `drop %0`, then `ArrayAppend`.

7. `scarlet dis` of guard.sca shows `ArrayDrop` before the guard's `IntGt` and `drop %11; goto j1` when the guard fails, and `scarlet dis benches/fib/fib.sca` for the worked example.

**Source read to check the critics.** code.rs: `writes` returns `Option<Reg>` (734), `allocate` adds edges only between the one written register and live-out (2129 to 2135), `liveness` removes the written register on every successor (2258), `moves_on_last_use` has no `Move` (2052 to 2101) and runs before `allocate`, and the `MapGet` fusion tests a read count and `option_arms` (1384 to 1410). exec.rs 152: `Move` always shares. perceus.rs 116 to 122: no drops inside a join. lower.rs 1173 to 1205: the tail is bound before nested tests and the guard; 935 to 952: `head_refutable` knows only `Lit` and `Ctor`. translate.rs 667 to 702: kept fields are read after the given arguments. eq.rs 40 to 58: an immediate never equals a cell. heap.rs 268 to 274: `share` adds one and stops at the sticky count.

**What I did not build or measure.** The budget and cursor on the join and its two-instruction form. The read-then-commit lowering of patterns. `Find`. The copy path of a set on today's tree (step 1's 1,400 is an estimate). A join of cell elements. An owned slice. The flat-plus-flat join is direct, but a join of a short root with a flat cell still goes through a buffer. The in-place functions were measured as calls, not inlined in the real dispatch loop. Elapsed time and cache behaviour were not measured, only instructions.

## Prior art

Read in this session:

- Erlang/OTP's list append, https://raw.githubusercontent.com/erlang/otp/master/erts/emulator/beam/erl_bif_lists.c. `++` is "implemented by copying LHS and setting its tail to RHS". It does a bounded amount of work per call (`CELLS_PER_RED = 40` or `60` times `ERTS_BIF_REDS_LEFT(p)`), charges it with `BUMP_REDS`, and when input remains it saves an `ErtsAppendContext` (the two lists, an iterator, the partial result) and returns through `BIF_TRAP2` to be called again. It keeps that state in a magic binary and turns the collector off while trapped. I took the shape for the join: a linear join in a process VM is normal, and it is done as bounded steps with saved state. Here the state is two ordinary arrays and a small Int in three registers, so nothing has to be switched off between steps.

- Clojure's PersistentVector, https://raw.githubusercontent.com/clojure/clojure/master/src/jvm/clojure/lang/PersistentVector.java. A strict 32-wide trie with a tail and `tailoff()`, no relaxed nodes, no size tables and no concat method. `TransientVector.ensureEditable(Node)` returns the node when `node.edit == root.edit` and a new node with a cloned array otherwise, and `doAssoc` calls it at each level on the way down. `own_node` and `own_child` are that, with `count == 1` in place of the edit token. Its `ChunkedSeq` holds `vec`, `node`, `i` and `offset`: a walk is a view object over the vector, which is the alternative I rejected for shared walks.

Carried from the earlier rounds of this design, which read them; I did not re-read them this session:

- Lean 4's runtime header, https://raw.githubusercontent.com/leanprover/lean4/master/src/include/lean/lean.h. `lean_is_exclusive` is `rc == 1`, `lean_ensure_exclusive_array` copies otherwise, and `lean_array_uset` starts with it. `lean_array_swap` returns the array unchanged when an index is out of range. `lean_array_set` out of range panics, so Lean is not a precedent for a total write.
- Lean 4's PersistentArray, https://raw.githubusercontent.com/leanprover/lean4/master/src/Lean/Data/PersistentArray.lean. A reference-counted language shipping a strict trie (`root`, `tail`, `size`, `shift`, `tailOff`) whose `append` is a fold of `push`.
- Scala 2.13's Vector, https://github.com/scala/scala/pull/8534. "Like the old Vector, the new one does not use a 'relaxed' radix-balanced tree", and the author "did not get around to implementing an optimized concatenation where the right-hand side vector determines the alignment", which "would allow concatenation of two vectors of sizes m and n in O(min(n, m) + log(n+m)) time". That is the join rule here, and the join in arrays-final is a build of it with measured costs.
- Roc's list runtime, https://raw.githubusercontent.com/roc-lang/roc/main/src/builtins/list.zig. One flat list with `makeUnique` and seamless slices. It is the flat-block design I measured against above 32, and the source of the window in the flat cell.
- Elm's Array, https://raw.githubusercontent.com/elm/core/master/src/Array.elm. "If the index is out of range, the array is unaltered." That is `put`.
- im-rs (https://github.com/bodil/im-rs/blob/master/src/vector/mod.rs) for a small flat form in front of a tree; imbl (https://github.com/jneem/imbl/blob/main/src/nodes/rrb.rs) and the core.rrb-vector reports (https://ask.clojure.org/index.php/6864) for how hard relaxed concat is to keep correct; Immer's design notes (https://github.com/arximboldi/immer/blob/master/doc/design.rst); and Bagwell and Rompf's RRB paper (2011), which today's array.rs cites.

The repository itself: docs/perf-diagnosis.md (causes 1 to 6 and 12 to 20 and the unexplained list read in full this session, the rest by heading), docs/vm-design.md (all), docs/semantics.md ("Arrays" and "Fields"), and the source lines listed under experiments.

## What it gives up, and what the critics found

## What this design gives up, worst first

1. **A join costs the shorter side.** About 44 instructions per element, and about 25 more when the result is later freed. That is cheaper than today below roughly 700 to 900 elements on the shorter side and dearer above: 5 times at 5,000 a side, about 80 times at 100,000, about 700 times at a million (40.6M instructions). No program in the repository joins two long arrays. This is the main thing the owner is asked to accept, and it is asked for simplicity, not speed.
2. **Reads and writes on arrays over 32 cost two to three times a flat block's** (146 against about 60 for a read at 5,000). I chose that over a cost that is the array's length whenever something else holds the array.
3. **A shared write or walk still costs a cell made and a cell freed.** 1,269 on a flat cell, about 2,600 for a held push, 570 to 2,237 per step of a walk on an array a record or tuple still holds. This design bounds those. Removing them is Perceus's job (drops inside joins, drop specialisation at a match), and the first uncons of a kept long array is 4,284 to 6,261 once per walk.
4. **Three operations exist twice.** Set, push and uncons each have an in-place function beside the general path, because the general path is 2 to 2.6 times slower when everything is unique.
5. **A join of two short arrays copies their elements**, where today's tree shares two leaves: 1,702 against 750 at 16 + 16 with the result freed.
6. **Capacity classes** cost up to twice the words of an exact flat cell.
7. **A tree above 1,024 elements can be one level taller than a packed one** (about 23% of results in the random test), which is one more node visit on a read.
8. **Up to 31 dead elements can stay alive in a head or tail leaf that was once shared.** Today's `offset` has the same bound.
9. **It is not less code** until step 7 deletes the relaxed machinery, and it is eight PRs instead of four.
10. **`array::put` out of range changes nothing and says nothing.** `array::set` is the form that says.

Where the numbers are soft: no benchmark ran on the new arrays; step 1's copy path on today's tree is an estimate; the fannkuch figure in step 8 depends on a source rewrite nobody has run; the gains overlap the alloc, inlining and string areas as each step says.

## What the critics found, and what I did

- **`writes` returns one register but `ArraySplit` writes several, and two destinations could share a register** (two critics, checked in code.rs 734 and 2129 to 2135). `writes` is now a visitor, `allocate` adds an edge between every pair of one instruction's destinations, a `_` element gets no register, and liveness is per edge.
- **A dead register can hold the array, so a moved operand still copies** (reproduced: 4 cells against 3). `Move` takes at a last use, as step 0. Operands that store a reference are `Arg`s with their own `moved`. The gap that remains (no Perceus drops inside a join) is named in the invariants with a test that fails until it is closed.
- **A zero-length cell was only promised** (checked: `from_values` and seven built-ins could make one). Three private minting functions return the immediate for 0, `Seq` is the only door in, and `==` and hashing no longer depend on it.
- **The walk table left out an array held at every step** (reproduced to the instruction). The rows are in the table. Step 4 no longer counts a walk gain for scrutinees read from a field or tuple beyond the instructions it removes. I rejected a slice-view kind and say why.
- **Guarded and refutable cons arms copy per element** (checked in lower.rs 1196 and in `dis`). A pattern now reads its heads without consuming, and `ArrayDrop` takes the tail after the tests.
- **A shared uncons on an append-built array copies the tree's left side, not only the root** (true: 4,284 to 6,261). I corrected the text, moved the stdlib's searches to index loops so they never pay it, and rejected the lazy hand-over because it leaks.
- **The prototype's tests missed things, and the consume rule and front join were untested.** I built both, and they pass the differential test with the fast paths on and off. The critics' heap-fill and `Nil` tests are listed for porting, with sticky counts, ranges and multi-head splits.
- **`try_set` would always copy, and a silent `set` is against the stdlib rule.** The write has a miss edge and `FindOp::ArraySet`. The checked form is `set`, the total form is `put`, and the names go to the owner.
- **The join numbers were the lucky case and the front join was 367 per element** (reproduced: 10.2M and 36.7M). I built a leaf-at-a-time join for both ends and quote unaligned figures: 4.45M alone and 6.9M with the result freed for 100,000 a side. 'About 20 per element' is withdrawn; it is 44.
- **A long join cannot be interrupted.** It is two instructions, and the second repeats over at most 64 leaves with its state in three registers, as BEAM's `++` traps. `repeat` is a Scarlet loop over a bounded built-in.
- **The fields-first spread needs no new Perceus feature** (reproduced: 739.8M against 1,372.2M). It is step 2, counted here, and the flat-block argument no longer uses the record case.
- **Step 1 bundled a gain that needs no decision with a reversal of one, and could not be measured alone.** The order is now set on today's tree, the spread, `Find`, patterns, the empty immediate, the flat cell, and only then the tree. The fallback text that said an in-place set on today's tree loses sieve was wrong and is gone: it gets sieve to about 13.9.
- **Gains overstated.** fannkuch after the flat cell is about 11.4, not 10.8 (regrows). Storage's regrows are 2.5M, not 1.64M. Step 7's geomean factor is 0.966 and does not depend on the tree choice. Unit costs are the lint-clean ones (3 to 5% higher on roots). A walked element is 150 and a strike turn about 800 until dead drops go.
- **'No second algorithm' was not true of the advertised numbers** (reproduced: 470 against 185). The design now says each hot operation has an in-place function and a general path, with a test switch.
- **Ranges were on the scoreboard and unmeasured.** Measured: 86 to 146 per element.
- **Slices had no row, and the in-place root slice was unjustified.** The shared rows are in the table. I kept `moved` on the slice because the consume rule then has no exception and the code is the `own_` path; the owned cost is still unmeasured, and I say so.
- **The loader still recognised two shapes, and maps kept a third.** Both array special cases are gone, and `FindOp::MapGet` replaces the `MapFind` fusion.
- **Height depended on history.** The 32-leaf re-hang rule is built and fixes the critic's case. Above 1,024 elements it does not, and that is tradeoff 7.
- **Smaller points.** `array.rs` gets `ArrayError { Full, Broken }` so a broken shape is named. `Heap::share_many` saturates. `repeat` of 0 or less is `[]`. Other areas are told to re-measure their alloc and addressing shares after this lands.

## Processes and a JIT

**Processes.**

- An array is plain cells in one process's heap, found by chunk and word like every other cell. There is no global table, no thread-local and no atomic count. "In place" rests on one fact, `count == 1` read from the cell's own header, and with a heap per process only the owning process can change that count. Arrays must stay copied on send for this to hold, so they should not join the off-heap list.
- No array cell is larger than 34 words. A million-element array is about 32,000 small cells in the ordinary free lists, never one 8 MB block in a chunk of its own. A heap limit is never tripped by a doubling reallocation.
- No array instruction calls Scarlet code, so no Scarlet frame sits on the host stack. `map`, `fold`, `filter` and `each` stay Scarlet functions, so their calls to `f` are ordinary calls where the scheduler can switch.
- Long work is cut into bounded pieces in two ways. The join is `ArrayJoin` followed by `ArrayJoinStep`, which moves at most 64 leaves (about 90,000 instructions) and stays at its own pc until it is done. Between steps the frame holds two ordinary arrays and a small Int, so a step is as safe a place to yield as a call, and the scheduler should treat it as one and charge a reduction per leaf. This is how BEAM's `++` works, without its saved context or its switched-off collector. `array::repeat` is a Scarlet loop around a built-in that appends at most 1,024 elements, so its tail call is the yield point.
- What is still one unbounded instruction: a write to or a push onto a range, which builds the range's elements first (about 10 instructions per element, so about 3 ms for a million). Charging it afterwards is all this design does. An array literal is bounded by its source. A slice that drops a large part frees it through `Heap::release`'s work list, which is the loop the design doc wants to make pausable.
- One thing a pausable free changes: while a dying value's free is paused, the arrays it still holds have a count above 1, so an in-place edit of one of them becomes a copy until the free finishes. That is a cost, not an error, and whoever builds the pausable free should know it.
- Sending an array reads it with the leaf iterator and rebuilds it with `from_values` in the receiver's heap. The receiver gets a dense array it alone holds, so its first write is in place. That copy is the scheduler's loop to bound.
- A constant array will have a sticky count, which is never 1, so no instruction ever writes it. That needs no extra check.
- A process that runs out of heap half way through a path copy stops. The half-built copy is unreachable, because the handler took or shared the register before it called. A critic's test fills the heap after 0 to 4 allocations on about 20% of 27,000 operations per run and checks every still-held array afterwards; it passes on the prototype.

**JIT.**

- The JIT compiles the same instructions. `moved`, each `miss` target and each `Arg`'s flag are fixed per instruction at load, so it emits the taking or the sharing form with no flag test at run time.
- The flat forms are short straight-line code it can inline: a tag test on the word, one header load, one masked compare (kind is `ArrayFlat`, and count is 1 for a writer), a bounds test against the window word, and a load or store. Root, range and shared cases stay as calls into the same `array.rs` general paths the interpreter uses, so there is one implementation of the tree.
- `ArrayGet`, `ArraySet`, `ArrayUncons`, `ArraySplit` and `ArraySlice` have a miss edge, which is a plain branch in machine code. Because `Find` is in the IR, there is no `Option` or `Result` to build and then optimise away.
- The read-then-commit form of a guarded arm is two instructions with a compare between them, which a JIT can fuse into one guarded uncons when it sees the array is unique. The interpreter does not need to.
- The empty immediate makes the `[]` test of a walk a compare with a constant.
- `ArrayJoinStep` is a loop back-edge with its state in registers, so the JIT can compile it as a call in a loop with a reduction check, the same as any back-edge.
- Core IR carries element types, so a JIT can drop the share-on-read and release-on-write tests for Int, Float and Bool elements. The layout does not block an unboxed Float leaf later, but nobody has designed one.
- What a JIT cannot do here that it could with a flat block is turn a read of a long array into one load. A read at 5,000 elements stays four dependent loads. That is the price in tradeoff 2, paid in the JIT too.

