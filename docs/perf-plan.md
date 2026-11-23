# The speed plan

This is the build order for making the interpreter fast on one core, written on 23 November 2026. It follows from `docs/perf-diagnosis.md`. The five designs it orders are in `docs/perf-design-core.md`, `perf-design-heap.md`, `perf-design-arrays.md`, `perf-design-compiler.md` and `perf-design-text-and-maps.md`.

How it was made: for each area three designers each wrote a design from a different starting point, a judge merged them, three sceptics attacked the result, and the design was revised to answer them. One more pass put every step into the order below and found where the designs disagree.

Each step is built alone, measured with `benches/run.py`, and reverted if it does not pay. The owner took the recommended answer to every decision below on 23 November 2026.

## What to expect

The start is 6.17 on instructions and 2.83 by the clock. These figures are my own combination of the five areas' numbers, with each gain reduced where an earlier step has already taken part of it. The gains overlap, so I did not multiply the areas' own totals: doing that gives about 1.4, which puts json, cfold and mandelbrot well under 1.0 and is not believable.

After the first third (steps 1 to 16: the array write, the value word, the heap, the interpreter core with today's ownership, and Update): about 3.1, and I would be surprised outside 2.8 to 3.5. The array write (6.17 to about 4.8) is firm, because the diagnosis measured it by ablation. The heap factor (about 0.84 here, against 0.826 measured on today's benchmarks) is the best-founded number in the plan, because it ran in a copy of the real VM. The core factor (about 0.75 to 0.80) is the soft one: three benchmarks were measured on hand-assembled programs and twenty are arithmetic, and no loader pass has been built.

After the second third (steps 17 to 35: the IR's shape, matching, Probe, the rest of the arrays, strings, binaries and maps): about 2.5, in a range of 2.2 to 2.9. Without the fannkuch source rewrite add about 0.08. The trie decision moves it by under 1%. The risk here is in single rows, not the geomean: matching is the riskiest step, and the string figures lean on probes whose free path is cheaper than the real one.

After the last third (steps 36 to 50: the middle end, ownership, then the gated steps): about 2.2, in a range of 1.9 to 2.6. If the owner stops before ownership it ends near 2.4. If every gated step (42 to 49) is declined it ends near 2.3, because together they are worth about 0.97. Step 50 is a cost of 1% to 2% and belongs to the processes work.

For comparison, the diagnosis estimated 2.3 to 2.5 for all twenty causes, CPython is 2.46 and Node with no JIT is 1.95. The plan lands a little better than the diagnosis because core's probe measured calls and Float instructions cheaper than the diagnosis assumed.

By the clock expect about half of this in logs, because most of what goes is cheap instructions: about 2.0 after the first third, 1.7 after the second and 1.5 at the end, each give or take 0.2. The array write and the string copies are real memory work, so those rows gain more by the clock than the call and dispatch rows do.

Where it will still be bad at the end: sieve (about 4 to 6), permute and queens (about 4 to 5), fannkuch (about 3 with its rewrite, about 9 without), and bounce, closures and richards (about 3 to 4). Rows that read below 1.0 by this arithmetic (json, cfold, mandelbrot) should not be trusted, and the diagnosis notes that LuaJIT's own number is inflated on spectral_norm, map_churn, cfold and closures. `benches/run.py --baseline`, added in step 1, is what will show which of these estimates was wrong, one step at a time: it prints how far each benchmark moved from a saved run, in machine instructions, VM instructions and cells made.

## Decisions

Each was settled as its recommended answer.

### 1. docs/vm-design.md proposes that the VM counts references itself and that this is always right whatever Perceus did. Steps 39 to 41 end that: the compiler states every count, two checkers verify it, and the VM stops sweeping frames and testing registers on write. Do you want that?

- **A.** Yes. Build steps 39 and 40 with the VM still checking, and land step 41 only when every test and benchmark shows the VM's own counting released nothing. A compiler mistake after that is a leak or a typed stop, never undefined behaviour.
- **B.** No. Stop after step 38 and keep the net. fib stays near 165 instructions a call instead of 112, the geomean ends near 2.4 instead of 2.2, and borrowed parameters and Open from the compiler are not built.

Recommended: Option A. It is the last step for exactly this reason, it can be reverted alone, and the call, register and tail-call costs cannot be fixed soundly without it.

### 2. docs/vm-design.md records as Decided that an array stays the RRB tree. Step 23 replaces the tree above 32 elements with a strict trie. May it?

- **A.** Yes. One node shape and no size tables, a read at 5,000 elements costs 146 instead of 240, and a join costs about 44 instructions per element of the shorter side, so joining two long arrays gets slower (5 times at 5,000 a side, about 700 times at a million).
- **B.** No. Keep the RRB tree and build in-place push and uncons on it. The scoreboard ends within 1% of option A, and the merge, rebalance and size-table code stays.

Recommended: Option A, for simplicity and a worst case you can state, not for speed. No program in the repository joins two long arrays. Steps 1 to 22 do not depend on the answer.

### 3. What are the checked and the total array write called, and what does a write out of range do? Step 3 needs this before it starts.

- **A.** `array::set(xs, i, x)` returns `Result(Array(a), Nil)` and `array::put(xs, i, x)` returns the array, unchanged when `i` is outside it. Both write in place and cost the same.
- **B.** `array::set` is the total form (as Elm's is) and the checked form gets another name such as `array::try_set`.

Recommended: Option A. It follows the stdlib rule that a fallible operation returns Result, and the plain name is the one that tells you when it failed.

### 4. docs/vm-design.md records as Decided that there are no fused super-instructions. Core steps 8 and 9 add constant-operand forms, compare-and-jump forms and call forms split by whether an argument is shared, about 115 opcodes in all. May the loader choose such forms?

- **A.** Yes. Reword the line to say the loader picks a specific instruction from what it knows about the operands, as BEAM's loader does, and that there is still no peephole pass over emitted code.
- **B.** No. Keep one instruction per Core construct. mandelbrot keeps about 160 of the 508 instructions a turn that step 8 would remove, the bare loop is about 130 a step instead of 87, and fib pays 3 to 23 more a call.

Recommended: Option A. Each form is one Core construct with a fact the loader already holds, and a JIT maps each to a template more easily, not less.

### 5. The 256-register window sets limits a program can meet: 250 parameters to a function, 254 values live at once in one function, 65,520 constructors with fields, 16 GiB of heap per process, and small Ints of plus or minus 2^50. A function over 254 registers stops when it is called until step 48 is built. Is that acceptable?

- **A.** Yes. Over 250 parameters is a compile error, and a function too wide for the window stops with NotBuiltYet until step 48. The widest of the 1,352 functions in the repository needs about 133.
- **B.** No. Build the spill path (step 48) straight after step 10, before anything else depends on the window, so no valid program can stop.

Recommended: Option A, and move step 48 up the day a real program hits it. The spill path is a second storage class that only its own tests would exercise today.

### 6. The inliner (steps 37 and 38) can copy a function from one module into another. That ties the caller's code to the callee's, which works against replacing one module while a program runs. Where may it inline?

- **A.** Anywhere in the program. `same_unit` says yes to every pair of modules, and can be narrowed later.
- **B.** Only inside one module, and from the stdlib into any module. A call into another user module stays a call.

Recommended: Option B from the start. Every measured gain is either inside one module (bounce, spectral_norm, towers, map_churn) or a stdlib function inlined into user code (json's `result::then`, the folds in closures), so nothing on the scoreboard is lost and a module's code depends on no other user module.

### 7. Step 26 feeds a string's whole words to the hash, so `==` and the hash read the same words. Every map with string keys changes its order once, and golden outputs that print one change with it. Accept that?

- **A.** Yes. One hash path, and a string-key lookup falls from about 1,664 instructions to about 600.
- **B.** No. Keep today's hash values by feeding the last partial word as bytes, which keeps a second path in the hasher.

Recommended: Option A, with a changeset. The project is pre-release and the order of a map was never promised.

### 8. Text's kernels read a string's words as bytes with `bytemuck::cast_slice` and search with `memchr`. bytemuck is a new dependency with `unsafe` inside it, as memchr, smallvec and simd-json already have. The workspace itself stays free of `unsafe`. Add it?

- **A.** Yes. Add bytemuck and a compile error on a big-endian target. All four release targets are little-endian.
- **B.** No new dependency. Compare and hash by words and search with hand-written word loops, which loses memchr and memmem on split, contains and replace.

Recommended: Option A. It is one safe function over plain integers, and the alternative is slower code that is harder to read.

### 9. Several steps add public stdlib functions: `array::set`, `array::put`, `array::swap` and `array::repeat`, and `string::join` and `string::concat`. `string::replace` and `int::from_string` become built-ins with the same signatures. Add the new names?

- **A.** Yes. `string::join` is the documented linear way to build a long string, and `array::repeat` is the way to make a filled array.
- **B.** Add only the array write of step 3 now and keep the rest private until someone asks.

Recommended: Option A. Appending in place cannot be seen in the source, so the stdlib needs one spelling that is linear whatever else holds the string.

### 10. Steps 3 and 24 change benchmark sources: sieve, permute, queens and fannkuch call `array::put`, and later sieve builds its flags with `array::repeat` and fannkuch reverses in place. The scoreboard then measures different Scarlet programs. Do both?

- **A.** Yes to both. I checked benches/fannkuch/fannkuch.lua: the Lua twin already swaps in place, and the Erlang twins already use array:set, so this makes the programs match their twins.
- **B.** Only the step 3 change. fannkuch then ends near 11 times LuaJIT instead of about 4.

Recommended: Option A, with each row's old and new source measured on the same binary on the day it changes, so the source change and the VM change are never read as one number.

## Where the designs disagreed, and how each is settled

- The value word. Core moves every tag (small Int is the 13 top ones, so 0xFFF8 and up; immediates 0x7FF8, function 0x7FF9, cell 0x7FFC, nullary 0x7FFD). Heap still writes references as 0xFFFC and nullary as 0xFFFD, and builds JumpUnlessTag constants from them. Text takes 0xFFFE for a frozen literal, and heap also reserves 0xFFFE for a shared constant arena. Under core's layout 0xFFFC to 0xFFFE are all small Ints. Settle it in step 5 with one table in value.rs that every area uses: core's five tags, plus 0x7FFE for a frozen literal, plus the empty array as immediate 0x7FF8 | 3. Heap's reference becomes 0x7FFC | variant (bits 32 to 47) | index (bits 0 to 30), and its tag-test constants become 0x7FFC_0000 | k and 0x7FFD_0000 | k. Text's map position gets 50 bits of small Int instead of 47, which only helps.
- String and binary literals have two designs. Heap step 2 makes each literal once per process as a cell with a frozen count and remembers it in `consts`. Text step 1 keeps literals out of the heap in a frozen area of the loaded program, behind a value tag, and measured that at 26 instructions against 57 for a sticky cell. Take text's design for String and Binary constants, because it is measured cheaper, costs nothing per process and is what docs/vm-design.md already proposes. Heap step 2 then freezes only big Int constants, and its 10.7% of string_keys is booked to text step 1. Core's side tables for strings and binaries are not needed either: a literal is an immediate word inside `Const`. The frozen area's cells must use heap step 3's header, so text step 1 comes after it.
- The cell header against the array cells. Heap's header says the first `scan` words after the header are values, with 255 as the escape for kinds that must be asked. Arrays put a raw window word at word 1 of the flat cell and three raw words at the front of the root, which would force the escape on the cells the array benchmarks free most. Settle it when arrays step 6 is built: put values first in both cells (flat: header, slots, then the window word last, scan = capacity; root: header, head, tree, tail, then the three raw words, scan = 3). An ArrayNode is a header and 32 values, so scan = 32. Only the three map kinds keep scan = 255. Arrays' masked compare (kind is ArrayFlat and count is 1) works on the new header unchanged.
- Raw cell sizes against the string rules. Heap step 2 rounds raw cells up to size classes. Text's invariant S3 says a string cell has exactly ceil(len / 8) data words, and its in-place append relies on the only room being the zero bytes of the last word. Settle it by leaving rounding out of step 7 and landing it in step 27 with `text::write`, the one maker of strings. From then on the rule is text's later one: capacity is the cell's size, the live words are ceil(len / 8), the last live word is zero padded, and spare words may hold anything. Step 30's append then writes into spare words as well, and step 46 only adds reallocation. A string past 253 words lives in a box in heap's `outside` table, so text, binary and map code must ask the heap for `data(cell)` (the words after the header) and never for one slice from the header on. Text step 11's request to raise DENSE_SIZES is moot after step 7.
- Noticing a dead cell. Core step 6a and compiler step 0 want `release` and `share` to return a typed Broken when the count is 0. Heap step 2 makes dup and drop infallible and sets a `stale` field that is read later. Settle it as a typed result from the out-of-line branch only. Heap's inline paths already test the count range (drop: above 1; dup: above 0) and call out of line otherwise, so that out-of-line function returns Err(DeadCell) for a count of 0 and the hot path gains nothing. The stop then happens at the instruction, which is what vm-design.md asks for, and `stale` is not needed. The exact live count that step 1 adds is replaced in step 7 by heap's audit walk, run by the test harness at exit.
- Three audits. Heap has `Heap::audit(roots)`, core has a test run that recounts owners after every instruction, and compiler has a quarantine of freed cells, read checks and a poison word in debug builds. Build one: heap's audit, with core's verifier maps and the globals as its roots, run after every instruction in a test mode. The poison word is the cell tag with index 0, which heap guarantees is never a cell. Quarantine must be a switch and not every debug build, because the allocation-count tests assert that a freed cell is taken again.
- One instruction enum. Heap, arrays, compiler and text all describe new instructions in today's `Instr` style, with boxed operand lists and a `moved` bool each, and four of them separately change `Instr::writes` (arrays for ArraySplit, heap for Unpack, text for MapNext, core through `Ins::operands`). Core step 3 replaces the enum. Settle it by landing core steps 3 and 4 (steps 9 and 10) before any other area adds an instruction, apart from the small ArraySet and Move of steps 2 and 3. Every later instruction is written once as `Ins<R>` with u8 registers, the window's look, take and put, and inline operands or a `lists` entry with one bit per entry (core's copy bit, text's `show`). I checked the sizes: every proposed instruction fits 16 bytes, and a constructor form with its shape inline has room for seven fields, so heap's wish for fixed forms up to five fields is met. `Ins::operands` must carry two things core did not list: several written registers, with a clash edge between every pair (arrays' rule, which Unpack and MapNext also follow), and a write that happens only on the hit edge of an instruction with a miss target.
- Who decides ownership. Arrays and text set `moved` and `own_first` in the loader's `moves_on_last_use`. Heap puts `moved` on Con and Update and has the loader ignore reuse. Core wants Perceus to state move or copy and the loader only to encode. Compiler writes Lend, Give and Copy on every operand and deletes `moves_on_last_use`. These are one fact stated four ways. Settle it in two stages. Until step 39 the loader's liveness is the one place every such flag is set, which is text's S0 with every register owning. At steps 39 and 40 compiler's marks become the only statement, and the loader encodes them as core's `dups` bits, kill bits and `Drop`. `own_first` is then a Give on a keeping operand, so text's S0 becomes checker rule V4. Compiler's `sig.rs` is the one table of operand positions and core's `Ins::operands` is written from it. Concat's first part, ArraySet, ArrayPush, ArraySlice and ArrayJoin sources, MapSet's map and Update's base must be added to its keeping list, operand by operand.
- Three classes or two. Core's contract has scalar, int and any, with a never-a-cell bit per RTy in ResolvedPool. Compiler has `Repr { Word, Counted }` from `Program::repr`, with Int as Counted, and lets a Give sit on any looking operand. Settle it with compiler's two values in the IR and one accessor, `Program::repr`. Its data must be filled by the compiler for every type a body mentions, not only the types a program builds or matches, which is core's point. The loader reads 'this local is an Int' from the bind's type and uses it for core's `Int` verifier state and kill bits. A Give on a looking operand becomes a kill bit where the instruction has one and a `Drop` after it otherwise. Compiler's 'Ret releases what its folded Drop names' and core's 'Ret releases nothing' are settled as core's: the Drop stays its own instruction before Ret, and a multi-register Drop makes that cheap.
- Two verifiers. Compiler checks Counted Core in scarlet_ir and returns `Verified`. Core checks the emitted `Ins<Reg>` in the VM and returns `Checked`. Keep both, because each checks a different crate's work: the first proves scarlet_lower balanced every count, and the second proves the loader's translation and register allocation kept that (a register shared by two live owned values, a wrong shift, a write to register 0) and writes the call-site maps. `load` takes `Verified` and `Machine::new` takes `Checked`. They share one test: core's generated test per instruction is compiler's table test of every handler against its `Sig` row. Watch the `startup` row for the cost of two walks.
- Reuse tokens. Core keeps DropReuse, a `Token` verifier state, `fits_ctor` and an `entry:` and `loop:` label per function for tokens carried round a loop. Heap retires tokens in its step 5 and measured that on the real VM as a gain on every benchmark that uses them (cfold 7.9% fewer instructions). Compiler retires them at its step 9 and expected cfold to lose up to 4%. Settle it with heap's measurement: tokens go at step 13, before core's tail-call step. Core then needs no Token state, no `fits_ctor` and one entry label, and compiler's predicted cfold loss does not happen because the heap of step 7 pops the freed cell straight back.
- Unpack and Open are one instruction. Heap's Unpack is found by a loader pattern on a `Match` arm that starts with the scrutinee's Drop. Compiler's Open is placed by the rc pass, and compiler step 4 deletes `Match`, which the loader pattern needs. Settle it with one VM instruction and one name, `Open`. Step 13 lands it with heap's loader pattern, which is what was measured. Step 18 must carry it over when `Match` goes: the old Perceus emits `CoreExpr::Open` at a switch arm where the scrutinee is dead on entry, and the loader pattern is deleted, or list, binary_trees and cfold lose step 13's gain. Step 42 only widens where Open is placed, so most of compiler step 11's 0.992 is already booked at step 13.
- Record update has three designs on the same code. Arrays step 2 changes `construct` in translate.rs to read kept fields first. Compiler step 2 replaces those reads with `Atom::Update`. Heap step 6 adds `Update` and `Take` with reuse specialisation in Perceus and a hole check in the loader. Compiler step 13 turns an update that reads its own base into Open and a fresh constructor, and measured that turning every rebuild into Update made permute worse. Settle it like this. `Atom::Update` is one node, built once at step 16, with heap's VM half (operands first, shape compare, copy path) and compiler's lowering rule (only from a source `..base` whose variant is known). The lowering emits Update when no given argument reads the base, and keeps arrays' kept-fields-first order with a fresh constructor when one does, because then the base must die before the argument is computed. That is compiler step 13's result reached in the lowering, so step 44 has little left to do. `Take`, holes and reuse specialisation of ordinary rebuilds are a gated step (45): they are the cheapest form on the toy (157 against 311) but add a state nothing at run time can check.
- Find, Probe and Find. Arrays step 3 defines `Find` with one bind and four ops. Compiler step 5 defines `Probe` with several binds and four other ops. Text step 13 defines `Find` with several binds and nine ops, lowered a third way. Build one node at step 19: compiler's shape (several binds, a hit arm and a miss arm) with the union of the ops. The miss rule must be arrays': a miss takes nothing and writes nothing, because `array::put` returns the same array on a miss and the next pattern arm still reads it. Compiler's 'a kept operand is consumed on both paths' is dropped, and the checker applies a probe's gives on entry to the hit arm only. The box is avoided in two ways that do not overlap: the decision tree emits a bare probe when a match is directly on the call (compiler), and the simplifier's jump-with-a-known-constructor rule removes the rest after inlining (text's general rule, which is compiler step 6 rule 6). Text step 9's widened MapGet pattern is not built, because step 19 comes first and deletes `option_arms`.
- Array patterns. Arrays step 4 adds `CorePat::Array`. Compiler step 4 deletes `CorePat` and `Match`. Settle it by order: matching lands first (step 18), and array patterns are then checks in the decision tree (step 20), which the arrays design already allows for. The read-then-commit rule becomes a non-consuming head probe, the nested tests and guard, then `ArrayDrop`. The VM half of arrays step 4 (ArrayUncons, ArraySplit, ArrayPush) is unchanged.
- Variant tests. Heap step 3 gives `JumpUnlessTag` on the top 32 bits of the value. Compiler step 4 gives `Switch` and estimated its cost on a toy that reads the tag from memory. They fit together if heap goes first: Switch is a table jump on bits 32 to 47 of the value word with no load, and JumpUnlessTag stays as the one-way and two-way form. Both need `Program::types` to list every variant a program names with its field count, so that change lands once, at step 11.
- The closure a frame runs as. Core puts it in register 1 and reads captures through it. Heap says GetCapture stays its own instruction because the closure is not in a register. Compiler makes it `params[0]` and a capture an ordinary part read. After step 10 the closure is in a register, so settle it as compiler's IR (env is `params[0]`), pinned by the loader to register 1, with a capture read as a field read at index + 1 of the closure cell. Heap's closure layout (header, function word, captures) is what core's CallValue reads.
- The scratch Vec. Core step 5 deletes `passing`, heap step 4 deletes `Machine::args` but leaves `Instr::Array` using `passing`, and arrays steps 4 and 6 make array literals fill their cell directly. In this order core step 5 (step 14) lands before arrays, so `Instr::Array` keeps a small buffer of its own until step 22.
- The tail-argument scheduling pass (core step 5) is written into scarlet_lower before compiler's passes exist. It must run last before count insertion, after inlining and specialisation, and step 17 ports it from `Tail(Call)` to `TailCall`. The inliner also removes calls core made cheap, so core's and compiler's gains on closures, spectral_norm, map_churn and towers must be read from the scoreboard and not added.
- Gains booked by more than one area. Each of these counts once, in the step that lands first. Literals: heap 2 and text 1 (text). Ownership: core 6 and compiler 9 and 10 (core's table). Open: heap 5 and compiler 11 (step 13). Update: heap 6, compiler 2 and diagnosis cause 12 (step 16). An array or map held in a record being unique at its write: arrays 2, compiler 13, heap's Take and text 10 and 11 for richards (step 4 measured 0 on richards: 7.2% of it is booked at step 33 and the rest at step 44). The Some round trip on a map lookup: text 9 and arrays 3 (step 19). The `''` default in string_keys: arrays 3 and text 1. Dead field reads and failed tests: heap 3 and 5 and compiler 4. Tuples in bounce and closures in json: heap 4 and compiler 6 and 7. map and filter in closures: arrays 8 and compiler 7 and 8. The cell work inside array writes: heap 1 and 2 measured it on today's benchmarks, and step 3 deletes most of it first.

## The order

### Step 1. Instruments and a heap that notices a dead cell. `scarlet dis --instrs` prints the loaded instructions, a per-opcode run counter is added, benches/run.py prints cycles beside instructions and, with `--baseline`, how far each benchmark moved from a saved run, the live-cell count is exact, the golden harness asserts no cell is live at exit, and `release` and the unique test return a typed `Broken::DeadCell` on a freed cell.

From: compiler 0, core 6a, core 1 (scoreboard only).

**Why here.** Every later step is judged on these numbers, and every later step that touches counts relies on a doubled release being loud. It may also turn up a bug that exists today, which is better found before anything changes.

**Expected gain.** None. The scoreboard must not move, and that is the acceptance test.

**What happened.** The board moved. Every benchmark but sieve got cheaper, by 1.7% on the geomean (6.17 to 6.06), and sieve read +0.1%, which is inside its own run-to-run spread. None of the gain is the step's work. The compiler laid `Machine::call` out differently, and each VM instruction lost about 3.5 machine instructions: one turn of a bare loop went from 346 to 332. The step's real cost sits under that. It is about one instruction for each release call, two for each cell freed, and a few on each owned array or map edit, and it is why sieve, which runs the fewest VM instructions, shows no gain. The next edit to `Machine::call` can lose the 1.7% again for no better reason.

So until step 9 replaces the loop, the board's net change does not judge a step that claims under about 2%. Such a step is judged by the exact counts from `scarlet run --counters` (VM instructions, cells made and cells reused), and by probes: small loops that each run one path, measured as the slope between two sizes, with the change in a bare loop taken off. The probes are in the perf scratch directory under `step1/answer`, with the script that counts one loop turn by function under lldb and the ones that compare call sites and function sizes between two builds.

### Step 2. `Move` takes its source at a last use: `Instr::Move { dst, src, moved }`, set by `moves_on_last_use`, with the test that two spellings of the same function make the same number of cells.

From: arrays 0 (and the Move half of text 6). Needs: 1.

**Why here.** It makes 'the count is 1' mean what the array write, the string append and the map edit all need, and it is a few lines on today's loader.

**Expected gain.** No scoreboard claim. It removes a share and a release per joined heap value (about 70 instructions each).

**What happened.** The VM's own counts did not move on any benchmark. The same instructions ran and the same cells were made and reused, because no benchmark has the shape the test pins. In that test a branch that ends in a name made 4 cells and now makes 3, the same as the branch that ends in the call. On the board the geomean moved by 0.06%. The rows whose Moves carry cells got cheaper: fannkuch by 0.73%, sieve by 0.47%, json by 0.10%, richards by 0.06% and strings by 0.05%. Every other row is inside 0.05% either way, which is what the rows that run no Move at all show too.

Probes give the cost of one Move exactly, as the slope between two sizes with a bare loop beside them, which did not move. A Move that takes a cell is 74 machine instructions cheaper than one that shares it and leaves the old register for a later `Drop` to release. A Move of a cell that is read again is 3 dearer, which is the test of the flag. A Move of an Int or a Bool is 1 dearer. The handler looks at the value before the flag to get that last number: only a cell has a reference to hand over, so any other value is copied and its register left alone. The first version emptied the register whatever it held, which cost 4 on every Int and Bool, and bounce, permute and queens read 0.05% to 0.13% worse with it.

Not built here: a Move the register allocator gave one register for both ends (`r3 = Move move r3`) does nothing, and every Move that permute, queens, map_churn and string_keys run is one, 1.8 to 4.2 million times each. Step 9's loader should not emit them. The owning-register set that text's step 6 passes to `moves_on_last_use` is not built either. Every register owns its reference today, so the set would be all of them, and it belongs with the borrowing that first makes it smaller.

### Step 3. A write at an index on today's tree: the `ArraySet` instruction with a miss edge, `array::set`, `array::put` and `array::swap`, and the four array benchmarks changed to call `array::put`.

From: arrays 1. Needs: 2, the owner's answer on the two names.

**Why here.** It is the largest single gain in the whole effort, it is low risk, and it needs nothing else. It goes first so that every later measurement is taken on benchmarks that no longer spend most of their time rebuilding arrays.

**Expected gain.** Arrays' arithmetic gives a geomean factor of 0.788, so 6.17 becomes about 4.86 (sieve 328 to about 14, permute 32 to about 12, queens 30 to about 10, fannkuch 28 to about 21). The diagnosis priced it at 0.776. The copy path's cost (about 1,400) is an estimate.

**What happened.** The board moved as forecast. The geomean went from 6.05 to 4.79, a factor of 0.792 against the 0.788 expected. Three runs of the board gave the factor as 0.792 to 0.793, and the two ends moved by 0.01 with LuaJIT's own rows. sieve went from 325 times LuaJIT to 13.9 (about 14 expected), permute from 32.1 to 12.9 (12.4), queens from 29.2 to 10.5 (10.1) and fannkuch from 27.2 to 20.8 (20.9). The other 19 rows did not move. The whole gain is the four sources calling `array::put`. On the old sources the new binary runs the same VM instructions and makes and reuses the same cells as the old one on all 23 rows, and its machine instructions are within 0.13% on every row in every run. strings reads 0.06% to 0.07% worse each time, which is far under what the board can judge.

The exact counts, old source to new on the new binary: sieve ran 20,668,414 VM instructions and now runs 8,713,896, and made 9,858,420 cells and now makes 469,620. permute went from 164,199,409 to 83,146,211 and from 19,019,201 cells to 6,771,401. queens went from 253,568,006 to 92,736,008 and from 20,877,500 to 8,848,000. fannkuch went from 108,819,434 to 105,285,484 and from 20,483,142 to 15,299,877. They run 498,105, 4,031,200, 1,582,000 and 986,408 writes. Not all of those are in place. A review run that forced one write site at a time to copy found that every write in sieve and fannkuch is in place, that half of permute's writes copy (2,015,600 of them, which step 4 should end), and that every write in queens copies, because `place_queen` keeps the board to backtrack to. So queens' gain is a copying write in place of a rebuild, and queens will keep copying.

The write costs more than the arrays design booked, and sieve landing on its estimate does not show otherwise. Probes give an in-place write about 390 machine instructions on an array of 32 elements or fewer and about 620 at 5,000, against the design's 170 and 367. Most of it is cell lookups: two reads of the root, then a unique test and a node read at each level. The design counted sieve as 498,105 writes at 367 on top of a remainder of 1,047M. Measured, the writes are about 311M and the remainder about 930M, and the two errors cancel. Forecasts for sieve at steps 6, 22 and 23 should start from the measured write. A copying write costs 1,652 at 6 elements, 2,101 at 16, 6,938 at 1,000 and 7,964 at 5,000, against the estimated 1,400, which is why permute and queens sit 4% above their estimates. A write that misses costs about 190. A loop turn that misses costs 295 more than a bare turn, because the `Const`, the `Move` on the miss edge and the `Drop` run too.

One walk was built, not the design's two. The design has an in-place function beside a general copying one, because its general path measured 2 to 2.6 times slower on an array nothing else holds. `set` in array.rs walks once from the root. It uses a node as it is when the node's count is 1, and otherwise copies it and puts the copy in the node above. On an array nothing else holds, that makes the same lookups as the in-place function would.

The checked `array::set` does not yet cost what `put` costs, though decision 3 says it does. The loader builds `Ok` on the hit edge and `Err(Nil)` on the miss edge, and the two join before the program's `match`. So an `Err` arm that gives the array back, or `array::set(xs, i, x) or xs`, is reached from the hit edge as far as liveness can tell, and the write shares the array and copies the path. At 16 elements that write costs 2,874 and makes 3 cells. It costs 1,117 and makes 1 cell (the `Ok`) when the `Err` arm does not read the array, and `put` costs 468 and makes none. Filling 5,000 elements that way makes 24,984 cells. The test `a_checked_write_copies_when_its_err_arm_reads_the_array` pins that number, and step 19 should bring it to 5,000 or fewer. The loader's `MapGet` recognition is no short cut to that: it needs the scrutinee to have no other read, and Perceus drops a `Result` in both arms, so the loader would have to delete drops it did not write. Until step 19, the stdlib comment, docs/semantics.md and the changeset say where the copy happens and point at `put`.

Some things are left for the steps that own them. `set` in array.rs reads the array's length again after the handler has tested it. For an index it does not have, or a size table that hides the slot, it gives the value up and returns the array as if it had written. The two reads cannot disagree today. Step 21 should have one function find the slot and hand `set` a witness of it, with a typed fault for a broken tree, and the second read (25 to 30 instructions) goes with it. A write to a range builds the whole range as a tree first. A loop that keeps `0..5000` and writes to it pays about 316,000 instructions and 164 cells a write, and one that does not keep it builds it once. The miss code for `put` is inline, so every hit runs a `Jump` (18), and `array::swap` builds two `Some` a call. Step 19 removes both. fannkuch's `swap01` and `swap12` write a 0 into an array shorter than their indices, where the old arms left it alone, and they should call `array::swap` once it costs no more. `ArraySet` and `MapFind` now say in `Instr::writes` that they write on the hit edge only (`Write::OnHit`), and liveness reads the miss edge from that value. It is the fact step 9's `Ins::operands` carries for every instruction with a miss edge.

### Step 4. A record update reads its kept fields straight after the base is evaluated (`construct` in translate.rs), so the base dies before the new field is computed.

From: arrays 2. Needs: 3.

**Why here.** It is a small lowering change with a measured saving, and it is the rule step 16 keeps for an update whose new value reads the base.

**Expected gain.** About 0.990 (permute about 12.4 to about 9.8), measured at 2,108 instructions a step on a hand-edited source. Measure richards here too: its `map::set(s.tasks, ..)` is the same shape, and if its map becomes unique this step takes the 10% that three later steps also claim.

**What happened.** permute got about two thirds of its forecast, and richards got nothing yet. The geomean went from 4.80 to 4.77, a factor of 0.9934 against the 0.990 expected, and three more runs of the board gave 0.9935 to 0.9936. permute went from 12.89 times LuaJIT to 11.04, where 9.8 was expected, and runs 14.31% fewer machine instructions. The VM runs the same instructions as before on all 23 rows, kind for kind, because the step only changes the order of the field reads. Cells made and reused are the same on 22 rows. permute made 6,771,401 cells and now makes 3,748,201, and reuses 2,016,000 as before. Of the other rows only richards (0.22% to 0.25% worse) and nbody (0.10% worse) moved by more than 0.05% on a quiet machine. Rows that load the same instructions from both binaries show what the board cannot judge: bounce and closures read 0.03% better on two runs and level on two, and on a run that shared the machine with a build, fib and queens read 0.19% and 0.18% worse.

permute still copies at 504,000 of its writes, where it copied at 2,015,600. Each copy is two cells, and each one avoided saved 1,164 machine instructions. The ones left are not this step's to end. `swaps` and `permute` build a `State`, pass it to a call, and keep its cell so that a later `State` can take it, and a cell kept that way still holds the array while the call runs. So the array has two holders in the callee, and the first write under each `permute(_, 1)` copies it, 2,520 times a run (and 360 times with five elements, which is how many such calls there are). With reuse switched off in a test build, permute makes 400 array cells, which are its 200 literals, no write copies, and it runs at 10.38 times LuaJIT. Step 13 takes reuse tokens away, and those copies go with them. That leaves permute near 10.4 and not 9.8, because a write costs more than the arrays design counted (step 3).

richards did not move, and its map is still copied at its sets, 2,476,025 of them. The step does its part: in `put_task`, `release` and `create_task` the record is now given up before the set. Two other things stand in the way. The loader only lets a set take its map when the map's drop is the first thing after the set, and Perceus orders drops by local number, so in `put_task` the task's drop comes first and the set is loaded as the plain built-in, which copies. 2,243,600 of the sets load that way. And `hold_self` and `queue_packet` read `s.hold_count` and `s.queue_count` in an argument written after the set, so the record has to live past it. Test builds give the sizes. A loader that finds the map's drop behind other drops makes 20,586,083 cells where 23,910,908 are made now, which is about two thirds of the sets in place, and richards runs 7.2% fewer machine instructions than it did before this step. With those two reads also moved before the update in the source, it makes 18,958,933 cells and runs 10.8% fewer, and all but about 40 sets are in place. The same loader change on the old field order saves no cell at all, so this step is needed for that 10% but does not deliver it. Step 33 sets the flag from liveness and books the 7.2%. The rest needs the two reads moved, by step 44 or in the benchmark's source.

Reading early has three costs, and all are measured. The first is register width. A kept field now lives in a register while the new fields are computed, so some functions need a register more and some one fewer. A register costs about 8 machine instructions each time a frame is swept, and 21 in a frame that keeps cells for reuse. That is all of nbody's 0.10%: with every frame padded to the same size the two orders run the same count. json has one lambda a register wider and read 0.02% worse on three runs and 0.02% better on the fourth, which is under what the board can judge. The second cost is richards' 0.22% to 0.25%, and it stays when the frames are padded. When the write still copies, the record now dies before the old map does, so the old map is freed by a second pass of `release` and not inside the record's. The four runs of the board put it at 77.0M to 86.3M machine instructions over the 2,243,600 sets that load as the plain built-in, 34 to 39 an update. A count of two scheduler cycles under lldb gave 28, which is too low. That cost goes when the set is in place. The third cost is a cell that is no longer reused, in one shape no benchmark has. When the base was built in the same function, its drop leaves its cell for the new record to take, and that drop used to sit straight before the new constructor. It now sits before the fields written after the base. If one of those is an `if`, `match`, `&&` or `||` used as a value, the reuse walk in perceus.rs forgets every waiting cell at the join, so the old cell is freed and the new record is made fresh. `t = P(i, i, i)` and then `P(..t, c: if i > 5 { 1 } else { 2 })` made 10,001 cells in 10,000 turns and reused 10,000, and now makes 20,001 and reuses none. A field that is a call or plain arithmetic keeps the cell. Step 13 takes reuse away for every shape, and step 16 updates this one where it is, since no field it is given reads the base.

One thing was built that the step did not ask for. A base with nothing to run counts as written first, so `State(v: f(s.v), ..s)` lowers to the same code as `State(..s, v: f(s.v))`. That is a name, or a field or tuple element read off a name, so the nested `Outer(inner: Inner(v: f(o.inner.v), ..o.inner), ..o)` writes in place too: 5,000 such updates made 29,984 cells and now make the 10,000 records. `runs_nothing` in translate.rs decides it, with one arm for each kind of expression. A base that has to run keeps its place among the arguments.

One thing a program can see does change, which the arrays design missed when it said a field read has no effect. The read has none, but the earlier drop it allows has one when the old record held the last reference to a handle. In `Held(..h, conn: redial(h.port))` the connection `h` held was open until the new record was built, and is now closed before `redial` runs. That is the rule every other value already follows, given up at its last use, so it stays, and docs/semantics.md and the changeset say so. The changeset is `major` for that reason. The order in which the base and the given fields run does not change: the program in `a_record_update_runs_its_parts_in_the_order_written` prints the same lines on the old binary and the new one.

The tests in crates/scarlet_vm/tests/run.rs pin the counts. In `a_record_update_gives_up_the_record_before_it_runs_the_new_fields`, filling 5,000 elements of an array held in a record made 24,984 cells and now makes 5,000, the records themselves. It also pins the spellings that still copy. An argument after the write that reads the base is the order the source asked for. An update inside an `if` or `match` used as a value copies because Perceus puts no drop inside a join, which the compiler design ends at step 17, where Perceus walks into a join. And a record kept for reuse across the call copies each node of the tree once, 164 cells, which step 13 ends. `a_nested_record_update_writes_in_place_wherever_its_spread_is` pins the nested update at 10,000. `a_record_update_sets_a_map_it_holds_in_place` pins a map set at 3,000 cells for 1,000 sets where it made 5,436, and pins the `put_task` shape at 5,436 for step 33 to bring down. `a_record_update_loses_its_cell_to_a_branch_in_a_later_field` pins the third cost. Two tests with a listener as the peer pin when a connection only the old record held is closed.

The binary, the boards, the probes and the test builds are in the perf scratch directory under `step4`, and what the reviews of the step added is under `step4/answer`.

### Step 5. The final value word: small Ints of 51 bits under the 13 top ones, immediates, function, cell and nullary at 0x7FF8, 0x7FF9, 0x7FFC and 0x7FFD, with 0x7FFE reserved for a frozen literal and immediate 3 for the empty array. `Value::finite` and `Value::float` become the only ways in from an f64, with the test that every tagged word is a NaN.

From: core 1. Needs: 1.

**Why here.** It is a format every other area builds on (heap's variant in the reference, text's frozen tag, arrays' empty immediate), it is low risk, and it must be settled once before any of them is written.

**Expected gain.** 0.989: map_churn loses its big-Int excursion (22.2%, measured by the diagnosis).

### Step 6. One arena: a cell is named by the index of its header word in one `Vec<u64>`, and the heap's methods keep their signatures. Long cells go behind a two-word stub.

From: heap 1. Needs: 1.

**Why here.** It is measured in a copy of the real VM, it changes no instruction and no handler, and the array, string and map kernels of later steps are all written against it.

**Expected gain.** Measured 0.915 on today's benchmarks. After step 3 the four array rows have less cell work left, so expect about 0.93. fib, tak and mandelbrot rose 1.4% to 2.1% in the port for an unknown reason; find out why before this lands.

### Step 7. The heap's fast paths: free lists threaded through the cells, a signed count, inline dup and drop, a free loop with no side Vec, and the audit. Three changes from the heap design: only big Int constants are frozen here, raw-cell rounding waits for step 27, and a count of 0 is a typed stop from the out-of-line branch.

From: heap 2. Needs: 6.

**Why here.** It is measured, it still changes no instruction, and cheap alloc and free change what every later step is worth, so later steps should be measured after it.

**Expected gain.** Measured 0.928 on top of step 6; expect about 0.94 after step 3. Its frozen-literal gain (10.7% of string_keys) is booked to step 25.

### Step 8. One instruction per Int and Float operator in today's format, with constant-operand forms, fused compare-and-jump, an inline Float path checked by `Value::finite` on the result, and one cold `int::slow` and `float::slow`. A macro writes each handler and its test.

From: core 2. Needs: 5, the owner's answer on fused forms.

**Why here.** It is low risk and behaviour does not change. It needs the tag test from step 5, and it goes before the format rewrite so that rewrite ports handlers that are already in their final shape.

**Expected gain.** About 0.96: mandelbrot about 37%, nbody about 17%, spectral_norm about 15%, fib 4%.

### Step 9. One 16-byte `Ins<R>` enum in one code array, matched in place, with `Ins::operands` as the single list of each instruction's registers and roles (including several written registers and writes on the hit edge only), cold state behind one `&mut Rest`, and the loader split into translate, live, alloc and emit.

From: core 3. Needs: 8.

**Carried from step 2.** The emit pass leaves out a `Move` whose two ends got the same register. It does nothing, and it is most of the Moves that sieve, fannkuch, permute, queens, map_churn and string_keys run.

**Why here.** This is the instruction format. Doing it now means every instruction the other four areas add is written once, in the final enum.

**Expected gain.** About 0.955: dispatch falls from 15 to 9 on every VM instruction. Accept it when objdump shows a 9-instruction dispatch block and one indirect branch.

### Step 10. The frame window: 256 registers per frame as `&mut [Value; 256]`, the link in register 0, the closure in register 1, calls that copy their arguments, and `steps` returning `Pause::NeedStack` to an outer loop. Today's ownership stays: a write releases the old value and `Ret` sweeps.

From: core 4. Needs: 9, the owner's answer on the window's limits.

**Why here.** It is the second half of how a handler is written (look, take, put), so it also goes before other areas add handlers. Reference counting is still right by construction, so its size is not matched by its risk.

**Expected gain.** About 0.88: fib 355 to about 165 a call with steps 8 and 9, tak about 29%, measured in the probe build that keeps the sweeps.

### Step 11. The new cell header (count, scan, size, class), constructor, tuple and closure layouts without the tag word, the variant carried in the reference, the constructor table built from `Program::types`, and `JumpUnlessTag` on the top 32 bits of the value.

From: heap 3. Needs: 5, 7, 10.

**Why here.** It is the cell format. It goes after the instruction format so its instructions are written once, and before the array cells, the frozen literals and `Switch`, which all depend on it.

**Expected gain.** About 0.99 by arithmetic from a toy: deriv 3.4%, list 3.7%, cfold 2.3%, binary_trees 2.2%.

### Step 12. Constructors, tuples and closures filled straight from the registers: `Con1` to `Con3` (and up to seven fields, which fit) with the shape inline, and a list form for wider cells.

From: heap 4. Needs: 11.

**Why here.** It follows the layout it fills, and it removes the temporary Vec that the next steps would otherwise carry.

**Expected gain.** Measured 0.992 for the list form (bounce 13%, json 5.5%), and about 0.995 more for the fixed forms by toy arithmetic.

### Step 13. `Open` (heap's Unpack): one instruction that takes a dying cell apart, found by the loader on a match arm that starts with the scrutinee's drop. Reuse tokens, DropReuse, `hollow`, `fits_ctor` and `Body::carried` are deleted.

From: heap 5. Needs: 12.

**Why here.** It is measured on the real VM, including that retiring tokens is itself a gain. It goes before core's tail-call step so that step never has to carry tokens round a loop or split a function's entry in two.

**Expected gain.** Measured 0.981: cfold 10%, list 8.6%, binary_trees 8.2%, json 4.7%.

**Carried from step 4.** permute's last 504,000 copying writes are booked here. A `State` cell kept for reuse across a call still holds the array, so the first write in the callee copies. With reuse switched off in a test build permute makes no array cell but its 200 literals and runs at 10.38 times LuaJIT, against 11.04 after step 4. Two tests in crates/scarlet_vm/tests/run.rs change with the tokens. In `a_record_update_gives_up_the_record_before_it_runs_the_new_fields` the kept record's 5,164 becomes 5,000. In `a_record_update_loses_its_cell_to_a_branch_in_a_later_field` nothing is reused any more, so both loops make two cells a turn, until step 16.

### Step 14. A tail call to a known function becomes a parallel move into the parameter registers and a `Jump`, with the allocator preferring a parameter's own register and a small pass in scarlet_lower that schedules tail-call arguments. `TailCallValue` reads its arguments into locals, and `passing` goes.

From: core 5. Needs: 10, 13.

**Why here.** It finishes the interpreter core while ownership is still the VM's. With tokens gone it needs one entry label per function and no carried scan.

**Expected gain.** About 0.93 to 0.95: the bare loop 346 to about 93, mandelbrot's turn 1,364 to about 325. The allocator preference has never been built, so a weak one costs moves, not correctness.

### Step 15. `Repr { Word, Counted }` and `Program::repr`, filled by the compiler for every type a body mentions, and no drop for a value that can never be a cell.

From: compiler 1. Needs: 1.

**Why here.** It is small and safe, and it introduces the one accessor that the ownership steps and the loader's Int class later read.

**Expected gain.** About 0.996: rbtree 5%, bounce 2.4%.

### Step 16. `Atom::Update { base, variant, sets }` and the `Update1` and `UpdateN` instructions: heap's handler (operands first, shape compare, in place when the count is 1, copy otherwise) and compiler's lowering (only from a source `..base` of known variant, and only when no given argument reads the base; otherwise step 4's order stays). Land the handler and one probe first and measure it.

From: compiler 2 and the Update half of heap 6. Needs: 4, 11, 13.

**Why here.** It is the largest remaining gain for one benchmark, it needs the shapes of step 11, and it must be built once from the two designs that describe it.

**Expected gain.** Compiler says 0.966 (towers 40%). After steps 12 and 13 a record copy already costs about half, so expect about 0.975 (towers about 25%, richards and permute less).

**Carried from step 4.** Two things for the lowering rule. First, 'no given argument reads the base' has to mean more than 'no given argument names it'. In `v = s.v` followed by `State(..s, v: array::put(v, i, x))` no argument names `s`, yet step 4's order is what lets the write go in place, 5,000 cells against 24,984, because `s` is given up before the put. An `Update` that holds `s` until the new fields are computed would copy there again. The `named` case of `a_record_update_gives_up_the_record_before_it_runs_the_new_fields` pins the 5,000. Second, the shape step 4 made worse comes back here: `t = P(i, i, i)` and then `P(..t, c: if ..)` reads the base in no argument, so it becomes an `Update` and writes where `t` is. `a_record_update_loses_its_cell_to_a_branch_in_a_later_field` should then make one cell a turn in both loops. An `Update` also keeps a handle the base holds open until the new fields are computed, where step 4 closes it before them. Decide which the language means, and keep `a_record_update_closes_a_handle_only_the_old_record_held_before_the_new_field_runs` and docs/semantics.md in step with it.

### Step 17. The Core IR's control flow in its final form: one `Join` with parameters and `Jump`, `Ret` and `TailCall` in place of `Tail`, and `SetGlobal` in place of a pinned bind. The scheduling pass of step 14 is ported.

From: compiler 3. Needs: 14.

**Why here.** It is the IR's format change. Matching, Probe, array patterns, the simplifier, the inliner and count insertion are all written in it, so it goes before all of them.

**Expected gain.** None. The scoreboard should move by under 0.5%, which is the test that the reshaping changed nothing.

**Carried from step 4.** One cell count does change here, off the scoreboard. A record update inside an `if` or `match` used as a value keeps its base until the value is made, because Perceus puts no drop inside a join, and the write under it copies. Once Perceus walks into a join the base is given up inside the branch. The `joined` case of `a_record_update_gives_up_the_record_before_it_runs_the_new_fields` in crates/scarlet_vm/tests/run.rs pins 24,984 cells and should read 5,000, and the sentence on it in docs/semantics.md goes.

### Step 18. A match is a decision tree (matching.rs) that reads a field where it is used, with `Switch` as a table jump on bits 32 to 47 of the value, `SwitchInt` with a big-Int path, and literal cases keyed by `LitKey`. `Match` and `CorePat` go. The old Perceus emits `CoreExpr::Open` at an arm where the scrutinee is dead on entry, and step 13's loader pattern is deleted. It lands with the differential test over both lowerings.

From: compiler 4. Needs: 11, 13, 17.

**Why here.** It replaces the pattern code that Probe and array patterns would otherwise be written into and then rewritten. It needs the variant in the reference so that a switch reads no memory.

**Expected gain.** Compiler says 0.951. Steps 11 and 13 already took the cheaper test and the dead reads of consumed arms, so expect 0.965 to 0.975 (rbtree, deriv and list most). A wrong tree is a wrong answer, not a slow one, so this is the riskiest step before ownership.

### Step 19. One `Probe` node with several binds, a hit arm and a miss arm that takes nothing, for the union of the three designs' ops (array get, slice and set, map get, and later map next and the partial string and binary built-ins). The tree emits a bare probe for a match directly on the call, and any other use builds the Option or Result in the arms. `array::put` and `array::set` are lowered through it, and `option_arms`, `index_or` and the loader's MapGet recognition go.

From: compiler 5, arrays 3, text 13. Needs: 18.

**Why here.** Three areas need this node and each designed it differently, so it is built once, straight after the matching it is emitted from.

**Expected gain.** Arrays say 0.978 (queens, sieve, string_keys, fannkuch). With cells cheaper after step 7, expect about 0.985. It also takes richards' Some round trip (4.5% today), which text step 9 would otherwise claim.

### Step 20. Array patterns as checks in the decision tree: `ArrayUncons` for `[h, ..t]`, `ArraySplit` for every other pattern, a tail taken only once the arm has committed, the cons arm tested first, and one `ArrayPush` per item for `[a, ..xs]` and `[..xs, a]`.

From: arrays 4. Needs: 19.

**Why here.** It needs the tree and Probe, and the flat cell's in-place walk two steps later needs this instruction to exist.

**Expected gain.** About 0.99 here (closures about 12%, strings, bounce and nbody 3% to 4%). The rest of the walk's cost goes at steps 22 and 23.

### Step 21. The empty array is an immediate word, `Seq` is the only door from a value to an array, and three private functions are the only places an array cell is made.

From: arrays 5. Needs: 5, 20.

**Why here.** It is a small format point that the flat cell's code assumes, so it goes immediately before it.

**Expected gain.** About 0.998 (storage 4%).

### Step 22. The flat cell for 1 to 32 elements with a window and capacity classes 4, 8, 16 and 32, written values-first so the header's scan covers it, with in-place set, push at both ends and uncons, and every function consuming one reference. Array literals fill their cell directly.

From: arrays 6. Needs: 11, 20, 21.

**Why here.** It is the second largest array gain and needs no decision from the owner. It is written against the final heap, header and instruction format.

**Expected gain.** Arrays say 0.921 (fannkuch about 20 to about 11, storage 3.7 to about 2.3, permute, queens, strings, nbody). Its unit costs were measured on the old heap, so expect about 0.93.

### Step 23. The form above 32: a strict 32-wide trie with a head and tail leaf, and a join in bounded steps (`ArrayJoin` and `ArrayJoinStep`). If the owner says no, in-place push and uncons on today's tree instead.

From: arrays 7. Needs: 22, the owner's answer on the trie.

**Why here.** It is the only array step that reverses a recorded decision, so it comes after everything that does not need the answer.

**Expected gain.** Arrays say 0.966 either way (sieve about 12.5 to 8.5, closures, bounce, spectral_norm). Node visits are cheaper on the arena, so expect about 0.975.

### Step 24. Stdlib loops (`map`, `filter` and `reverse` as their own loops, the searches as index loops), `array::repeat` over a bounded built-in, and the sieve and fannkuch sources changed to use them.

From: arrays 8. Needs: 23.

**Why here.** It uses the instructions of steps 20 to 23, and it changes benchmark sources again, so it closes the array work in one place.

**Expected gain.** 0.94 with the fannkuch rewrite and 0.984 without. The fannkuch figure is arithmetic on a source nobody has run, and the closures part (13%) is the same work the inliner removes at steps 37 and 38.

### Step 25. One door for strings and binaries (`text::of`, `Bits`), and literals as frozen values: a cell-shaped run of words in the loaded program behind the 0x7FFE tag, loaded by `Const`.

From: text 1. Needs: 9, 11.

**Why here.** It is wide but mechanical, and every other string step goes through its door. It needs the final header (the frozen cells have one) and the final `Const`.

**Expected gain.** Text says 0.992 on today's VM (string_keys 13%). A small cell is cheap after step 7, so expect about 0.996.

### Step 26. Equality and hashing read whole words through `bytemuck::cast_slice`, and the hasher loses its tail buffer.

From: text 2. Needs: 25, the owner's answers on the hash and on bytemuck.

**Why here.** It is malloc and byte loops that no other area removes, so its gain does not shrink with what lands before it.

**Expected gain.** About 0.994 (string_keys 11.5%). String-keyed maps change their order once.

### Step 27. `text::write` as the one maker of strings, with raw cells rounded to the heap's size classes and the rule that capacity is the cell's size. `length`, `contains`, `trim`, `split` and the other readers work in place.

From: text 3 (with heap 2's raw rounding). Needs: 22, 26.

**Why here.** It needs the door of step 25 and the array builder of step 22, and it is where the string layout rule has to be stated once for both heap and text.

**Expected gain.** Text says 0.993 (strings 14%). Part of split's cost is the array build that step 22 already made cheaper, so expect about 0.995.

### Step 28. Int and Float conversions without a bignum or a copy.

From: text 4. Needs: 27.

**Why here.** It is small, safe and needs only `text::write`.

**Expected gain.** 0.9986 (strings 3.2%).

### Step 29. `Concat` reads each part once and allocates once for a whole `a + b + c` or interpolation, with the lowering flattening the chain.

From: text 5. Needs: 27.

**Why here.** It is the largest string gain, and the in-place append is a case added to it.

**Expected gain.** Text says 0.976 (string_keys 20%, strings 29%). Part of that was the cost of making and freeing a cell, which step 7 already cut, so expect about 0.98.

### Step 30. Append in place when the instruction owns the last reference to its first part (the `Sole` witness) and the bytes fit the cell, with the counter and the tests that pin the ownership rule.

From: text 6. Needs: 2, 29.

**Why here.** It is one case of `Concat`, and it relies on the loader's liveness being the single place such a flag is set, which is still true here.

**Expected gain.** About 0.999 (string_keys 2% to 3%).

### Step 31. `string::replace`, `string::join`, `string::concat` and `binary::concat` as linear two-pass built-ins, and `int::from_string` as the built-in itself.

From: text 7. Needs: 27, the owner's answer on the new stdlib names.

**Why here.** It needs `text::write`, and `join` must exist before anyone is told not to rely on the in-place append.

**Expected gain.** Text says 0.997 on today's VM, but about 5 of its 6.5 points on strings were array walks and closure calls that steps 20 to 22 already removed, so expect about 0.999.

### Step 32. Binaries read as bytes when they start on a byte: `ByteAt` as an instruction, borrowed whole bytes, u64 paths for integer reads, and `BinaryAppend` with the in-place rule. Land it as three changes.

From: text 8. Needs: 25, 30.

**Why here.** It is independent of the string steps but shares their witness and their door, so it follows them.

**Expected gain.** About 0.995 (json about 10% after the built-in call path is already cheaper). No probe was built for it, so every number is arithmetic.

### Step 33. Owned map edits come from liveness: `MapSet` and `MapDelete` take an `Arg` whose `moved` the loader sets, and the two let-then-drop recognitions go. The widened lookup pattern in text's step is not built, because step 19 already replaced it.

From: text 9. Needs: 19.

**Why here.** The new map walk takes `owned` as a parameter, so the flag has to exist first.

**Expected gain.** About 0.997: richards 7.2%, measured at step 4 on a test build whose loader finds the map's drop behind other drops (20,586,083 cells against 23,910,908). richards' 4.5% from the lookup went to step 19. Off the scoreboard, `from_list`, `merge` and `filter` stop copying a path per entry.

**Carried from step 4.** Step 4 gives the record up before the set in `put_task`, `release` and `create_task`, so the map has one holder there, but the set is still loaded as the plain built-in because the task's drop sits between it and the map's. This step is what puts those sets in place, about two thirds of richards' 2,476,025. It also ends the 0.22% to 0.25% step 4 cost richards, which is the old map being freed in a pass of its own while the set copies. `a_record_update_sets_a_map_it_holds_in_place` in crates/scarlet_vm/tests/run.rs pins the `put_task` shape at 5,436 cells and should read 3,000, and the sentence on it in docs/semantics.md goes.

### Step 34. The map walk rewritten on today's layout: an attempt to finish inside one unique node, then one path down and up with every count change from one ledger. The probe's random test, its four hashers and its audit move into the VM's tests.

From: text 10. Needs: 7, 33.

**Why here.** It replaces 900 working lines for a modest gain, so it comes after the cheaper string work and with the heap's audit in place to check it.

**Expected gain.** About 0.995 (map_churn 9%). A lookup does not get cheaper from this step.

### Step 35. The map value is its top node, which removes one cell lookup from every map operation.

From: text 11. Needs: 34.

**Why here.** It follows the walk it changes. Skip it if the gain does not show: nothing later depends on it.

**Expected gain.** About 0.9985 (map_churn 1.5%, richards up to 2%). Step 4 did not make richards' map unique at any set. Step 33 does at about two thirds of them, so read richards' share here from the board after that.

### Step 36. The simplifier inside one function (copy, known field, common subexpression, known constructor, flatten a jump, jump with a known constructor, rebuilt scrutinee, dead code), with a counter per rule in `scarlet dis --stats`.

From: compiler 6. Needs: 19.

**Why here.** It is the first pass of the middle end and the inliner runs inside its walk. It needs the IR of step 17 and the probes of step 19 to simplify.

**Expected gain.** Compiler says 0.991 (bounce 15.6%, nbody 4.3%). Step 12 already made bounce's tuples cheaper, so expect about 0.993.

### Step 37. Inlining over the call graph: a small callee, a callee with one reference, a known lambda, and a lambda made once and called once even inside a cycle, with `same_unit` as the one policy function.

From: compiler 7. Needs: 36, the owner's answer on inlining across modules.

**Why here.** It needs the simplifier, and it should land before count insertion is rewritten so that pass is tested on inlined code from its first day.

**Expected gain.** Compiler measured 0.956 by hand rewrites on today's VM (json 29%, bounce 21%, spectral_norm, towers, map_churn about 12%). These are deleted calls and closures, which cost half as much after steps 10 and 12, so expect about 0.97.

### Step 38. A self-recursive function is specialised on a known lambda, with clones keyed by the function and the lambda's shape and no clone past the second generation.

From: compiler 8. Needs: 37.

**Why here.** It is called from the inliner and completes the half of the compiler work that cannot make a count wrong.

**Expected gain.** Compiler says 0.986 (closures 20%). Step 24 already rewrote `map` and `filter`, and calls are cheaper, so expect about 0.992.

### Step 39. The compiler states every count: `Stage`, `Use` and `Pass` (Lend, Give, Copy), `sig.rs` as the one table of operand positions, rc.rs in place of perceus.rs with every non-Word local tracked, and the Core checker in scarlet_ir returning `Verified`, tested by mutation.

From: compiler 9 (first half) and core 6b. Needs: 38, the owner's yes on ownership.

**Why here.** It is the contract both core and compiler designed, and it must be written after matching and the inliner, because they decide where values die.

**Expected gain.** About 0.995 directly (drops of registers a call already emptied: cfold 3%, deriv 2.4%). cfold does not regress, because tokens went at step 13.

### Step 40. The loader encodes the marks as `dups` bits, kill bits and `Drop`, and `moves_on_last_use` goes. The VM verifier walks the emitted instructions, returns `Checked` and writes the call-site maps. The table test of every handler against its role, the role check behind look and take, and the audit run land with it. The VM still stores Nil on a move and still sweeps, and counts what the sweeps release.

From: core 6c and the VM half of compiler 9. Needs: 39.

**Why here.** This is the landing where both rules hold at once, so the old one can prove the new one has nothing left for it to do.

**Expected gain.** None by itself. Its result is a number: the sweeps and write tests release zero cells on every golden program and all 23 benchmarks, with no DeadCell stop.

### Step 41. The VM obeys: `put` is a plain store, `take` a plain read, `Ret` and tail calls sweep nothing, and the Nil stores go. The debug-build poison word and the audit mode stay.

From: core 6d and compiler 10. Needs: 40.

**Why here.** It is the highest-risk step, so it is last among the planned steps and can be reverted alone.

**Expected gain.** Core measured it as the difference of two probe builds: fib about 165 to 112 a call, tak about 193 to 104, a geomean factor of 0.90 to 0.91 on core's table. With calls already removed by inlining, expect about 0.92.

### Step 42. `Open` placed by the rc pass wherever an owned local of proved shape is only read in parts and then dies, not just at an arm's head. Decide from the scoreboard after step 41.

From: compiler 11. Needs: 41.

**Why here.** It is the first gated step, and it widens an instruction that has been running since step 13.

**Expected gain.** Compiler says 0.992, but most of that was taken at step 13. What is left is arms that drop the scrutinee after an inner test (rbtree), so expect about 0.997.

### Step 43. Borrowed parameters by Lean's inference with rules O1 to O5, a `Hold` on binds, and an owned twin for closures and for tail calls that hand over something the frame owns. Decide from the scoreboard.

From: compiler 12. Needs: 41.

**Why here.** It needs the verified contract, it adds a state to both checkers, and it is the least certain gain in the compiler work.

**Expected gain.** About 0.985 (list 16%, deriv 6%). Only 76 of 429 parameters are borrowed in the census.

### Step 44. An update that takes a field, done in rc. Most of it is already in the lowering rule of steps 4 and 16, so this step is to check that rule against rc's wider view and delete whichever is redundant.

From: compiler 13. Needs: 42.

**Why here.** It needs Open from rc, and it is measured after steps 4 and 33, which take the part of richards that needs no wider view.

**Expected gain.** richards about 3.9%, not the compiler design's 9%. Step 4 measured it: richards runs 7.2% fewer machine instructions once step 33 lands and 10.8% fewer with this step's part as well.

**Carried from step 4.** What is left for this step in richards is `hold_self` and `queue_packet`. Each reads `s.hold_count` or `s.queue_count` in an argument written after the `map::set`, so the record lives past the set and the set copies, at about a third of the 2,476,025 sets. Reading those two fields before the set is the whole change, and a hand-edited source with the test loader of step 33 makes 18,958,933 cells against 20,586,083. docs/semantics.md tells a user to write such a field before the write, and that sentence goes if rc does it for them.

### Step 45. `Take` and reuse specialisation of ordinary rebuilds, with the loader's hole check. Gated: build it only if the scoreboard after step 44 still shows rebuilds that keep most of their fields.

From: heap 6 (the Take half). Needs: 44.

**Why here.** It is the cheapest form on the toy (157 against 311), but a hole is a Nil that nothing at run time can tell from a real one, and compiler's model saw permute get worse when every rebuild became an update.

**Expected gain.** About 0.995 at most (rbtree's `ins` arms, cfold under 1%).

### Step 46. Grow a string or binary in place by reallocating to a power-of-two size, with a new `string_build` benchmark and its twins to show it.

From: text 12. Needs: 30.

**Why here.** Nothing on today's scoreboard shows it working or failing, and `string::join` already exists as the linear path, so it waits for its own benchmark.

**Expected gain.** 0.9996 on today's scoreboard. Its real effect is that a loop that builds a string by appending becomes linear.

### Step 47. `map::fold` walks a position that is a plain Int, through a `MapNext` probe with three binds.

From: text 14. Needs: 19, 34.

**Why here.** Its gain is nearly nothing and it needs Probe and the new map walk, so it is late.

**Expected gain.** 0.9998 (string_keys 0.4%). `merge` and `filter` stop building an array of tuples.

### Step 48. Functions wider than the window: a spill stack with `Spill` and `Unspill`, which the verifier treats as more registers.

From: core 7. Needs: 41.

**Why here.** No program in the repository needs it, so it is built once, after the ownership rules it must follow have stopped changing. Move it up if a real program stops for it.

**Expected gain.** None for speed. It removes a stop.

### Step 49. Four optional pieces, each on its own measurement: several results in registers, reuse tokens, borrows rooted in a local, and kept-field masks.

From: compiler 14. Needs: 43.

**Why here.** Each adds a state to a checker or changes the return path, and each has a small measured ceiling.

**Expected gain.** Several results: 0.996 (nbody 7.8%). The others are under 0.5% of any benchmark on the toy.

### Step 50. Count reductions at calls and at a `Loop` instruction, with `Pause::OutOfFuel` through the same door as `NeedStack`. This belongs to the processes work, not the speed work.

From: core 8. Needs: 41.

**Why here.** The owner has put processes on hold. The earlier steps keep it possible: a call that cannot go ahead has written nothing and runs again.

**Expected gain.** A cost: fib 112 to about 116 to 125 a call, the bare loop 87 to about 91 to 94.

