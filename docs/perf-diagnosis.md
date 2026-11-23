# Where the VM loses time

This is the diagnosis from 22 November 2026, at commit `6c5e49af`. It says where Scarlet spends instructions on the benchmarks in `benches/`, measured against LuaJIT with its JIT off. Nothing here is a decision. It is the evidence the speed work starts from.

How it was made: one agent profiled each of the 23 benchmarks, and nine more each studied one part of the VM. One agent merged the 32 reports into the ranked list below. Then a sceptic re-measured each of the top 12 causes from scratch and tried to refute it. All 12 held, and each sceptic's corrections are given under the cause.

Numbers are machine instructions, counted with `/usr/bin/time -l` on an Apple M-series machine. "Measured" numbers came from a probe or a trace. "Inferred" ones came from reading the code.

## The scoreboard at the start

Instructions relative to LuaJIT with its JIT off, startup taken off. Lower is better.

| benchmark | scarlet | python | node, no JIT | BEAM, JIT |
|---|---:|---:|---:|---:|
| binary_trees | 2.54 | 1.50 | 1.83 | 0.10 |
| bounce | 8.34 | 2.01 | 2.12 | 0.28 |
| cfold | 1.85 | 1.90 | 1.46 | 0.17 |
| closures | 8.30 | 4.23 | 1.73 | 0.27 |
| deriv | 2.93 | 2.69 | 2.24 | 0.29 |
| fannkuch | 27.74 | 3.61 | 3.56 | 1.47 |
| fib | 2.91 | 3.75 | 2.79 | 0.23 |
| json | 1.99 | 1.48 | 1.36 | 0.22 |
| list | 7.03 | 1.89 | 1.67 | 0.21 |
| mandelbrot | 3.80 | 4.27 | 2.54 | 0.81 |
| map_churn | 2.53 | 3.26 | 1.69 | 1.84 |
| nbody | 8.01 | 4.46 | 3.31 | 0.90 |
| permute | 32.46 | 4.45 | 3.54 | 0.97 |
| queens | 29.61 | 2.70 | 2.75 | 0.57 |
| rbtree | 2.33 | 1.08 | 1.29 | 0.07 |
| richards | 7.30 | 1.57 | 1.57 | 0.31 |
| sieve | 328.29 | 4.99 | 3.89 | 9.58 |
| spectral_norm | 2.89 | 3.06 | 1.54 | 0.20 |
| storage | 3.81 | 1.35 | 0.64 | 0.12 |
| string_keys | 2.93 | 1.80 | 1.30 | 1.14 |
| strings | 3.49 | 0.72 | 0.74 | 1.24 |
| tak | 3.39 | 3.82 | 3.05 | 0.22 |
| towers | 8.86 | 2.97 | 2.98 | 0.69 |
| **geomean** | 6.17 | 2.46 | 1.95 | 0.43 |

By the clock the geomean is 2.83, because Scarlet's instructions are cheap ones. The instruction count is the measure that repeats, so it is the one to steer by.

## What the fixes could add up to

Each item's gain is the measured share it touches today times the fraction a realistic fix removes (stated per item), turned into a geomean factor as exp(mean over the 23 benchmarks of ln(1 - share removed)). Alone: no-array-set 0.776 (6.17 to 4.79), call-return-frames 0.895, array-push-not-in-place 0.922, cell-alloc-free-path 0.932, dispatch-and-instr-format 0.934, strings-copy-out 0.940, no-inlining 0.943, heap-cell-addressing 0.945, register-access-checks 0.950, float-ops 0.953, self-tail-call 0.954, reuse-and-record-update 0.960, array-uncons-walk 0.961, refcount-traffic 0.966, dead-binds-and-noop-drops 0.972, int-op 0.972, match-chains 0.973, tuples 0.973, array-index 0.978, map-ops 0.982. Items ranked 3 to 11 are within about 0.03 of each other, which is inside the error of these estimates, so their order is soft. The shares overlap (dispatch and register access sit inside the call numbers, heap addressing inside alloc and free, prepend inside the recursive set), so the factors cannot simply be multiplied. Two bounds: multiplying them as if independent, and multiplying with every share cut by a quarter for overlap. Top 5: 6.17 x 0.776 x 0.895 x 0.922 x 0.932 x 0.934 = 3.44, or 4.26 with the discount. Top 10: 2.61 to 3.49, so about 3.0. All 20: 1.90 to 2.77, so about 2.3 to 2.5, which is level with CPython's 2.46; the undiscounted product puts json and cfold below 1.0, which shows it double counts. After the top 10 the worst rows would still be sieve and fannkuch (about 11 each), permute and queens (about 5.5), and bounce, list, towers and richards (4.4 to 4.8), which is where the remaining compiler items (reuse, tuples, refcount traffic, match) and the array walk land. The first item needs the four array benchmarks' sources changed to call the new set. These are estimates from the reports' numbers; nothing here was run.

## The causes, biggest first

### 1. There is no write-at-index for arrays, so one element write is slices plus concat, or an O(i) recursive rebuild

`no-array-set`, kind: stdlib-or-builtin.

**Today.** The stdlib and the IR have no array set. Programs spell a write as `xs[0..i]`, `xs[i+1..n]`, an append and an RRB concat (sieve, fannkuch `set` and `rotate`), or as a recursive `[h, ..t]` walk that rebuilds with `[x, ..t]` (permute, queens). Each form path-copies the tree, frees the old path, and runs the general slice, take, skip, merge and rebalance code, which copies each 32-slot node through 264-byte SmallVecs four or five times and rebuilds size tables by visiting every child. The array is uniquely held the whole time, so none of the copying is needed.

**Where.** crates/scarlet_core/src/std/scarlet/array.sca (no set), crates/scarlet_ir (no ArraySet intrinsic or instruction), crates/scarlet_vm/src/array.rs slice, take, skip, concat, merge, rebalance, branch; benches/sieve/sieve.sca clear, benches/permute/permute.sca set, benches/queens/queens.sca set, benches/fannkuch/fannkuch.sca set, rotate, swap01, swap12

**Benchmarks hit.** sieve, permute, queens, fannkuch

**Estimated gain.** sieve: 96.4% of instructions are inside `clear` today, and an in-place set at 200 to 400 instructions would remove about 96% of the run (ratio 328 to roughly 10). permute: `set` is 72.1%, about 68% removed. queens: `set` is 73.4%, about 69% removed (7.52M to about 0.4M per run, inferred by the queens report). fannkuch: set 19.5% + swaps 7.1% + rotate 3.1%, about 27% removed directly; flip (66.8%) only goes if it is rewritten over set. Alone this is a factor of about 0.78 on the geomean (6.17 to about 4.8), the largest single item, and sieve alone is 0.87 of it.

**Evidence.** sieve report: duplicating the body of clear adds 28.097e9 over 498,105 calls, 56,408 per clear, 96.4% of 29.14e9; concat 34,210, the two slices 9,832 and 10,590, append 1,776. arrays study: one write on a 5,000-element array single-stepped at 55,228 (concat 28,064, two slices 14,503, frees 9,256), against about 180 per write all-in for LuaJIT; the existing owned map set on 5,000 keys costs 632, which is the yardstick for an owned array set. permute report: v_full 154.90M per iteration against v_idx 43.21M, so set is 111.69M (72.1%), 3,603 per step. queens report: probes weighted by the index histogram sum to 7,520,684 of 10,247,700 per run (73.4%). fannkuch report: set doubled in place adds 5.404G (19.5%), 20,733 per set and 13.4 cells.

**Agreement between reports.** Four benchmark reports (sieve, permute, queens, fannkuch) and the arrays study found it independently and agree on the cause. Per-write cost in sieve is given as 56,408 (sieve report, ablation), 55,228 (arrays, single-step), 51,765 (arrays, probe difference) and 58,435 (arrays, whole run divided by writes); these differ by what is included. permute per swap is 11,083 (permute report, set only) against 15,383 (arrays study, whole run divided by swaps).

**Fix direction.** Add `array.set` as an intrinsic and an ArraySet instruction with the same `moved` flag ArrayAppend has. When the root and the nodes on the path are unique, write the slot in place; otherwise copy one leaf and one branch per level. Keep arrays of 32 or fewer as one flat cell so set is one store. Then use it in the four benchmarks (the Erlang twins already use array:set and setelement). Rewriting fannkuch's flip over set is a separate source change.

**The sceptic** (held). All numbers are instructions retired from /usr/bin/time -l on target/release/scarlet. Scratch files are in scratch/fat/noset.

MEASURED, whole benchmarks (startup 38.8M):
- sieve 29.116e9, permute 31.014e9, queens 35.928e9, fannkuch 27.743e9.

MEASURED, ablation by doing each write twice (the second write is idempotent, and the outputs were unchanged):
- sieve: +26.574e9 over 498,105 clears, which is 53,349 per clear and 91.4% of the run. A second check from the other side: the same program with clear made a no-op costs 1.99e9 in total while doing 3 times as many strike steps, so the real non-clear remainder is about 1.2e9 (this split is an estimate). That puts clear at about 27.9e9, 96%, about 56,000 each. The claim says 56,408 and 96.4%.
- permute: +21.775e9, which is 70.3% of the run. I counted 4,031,200 set calls (20,156 per iteration), so 5,402 per set and 10,803 per swap. The claim says 72.1% and 11,083 per swap.
- queens: +25.354e9, which is 70.6%, or 7.244M per run. The claim says 73.4% and 7.52M.
- fannkuch: +5.443e9, which is 19.6%. With about 261k sets (my count from the algorithm) that is about 20.9k per set. The claim says 5.404e9, 19.5% and 20,733.

MEASURED, micro-probes (N writes in a tail loop, minus the same loop with a no-op):
- Slice+append+concat write on a 5,000-element array costs 25.6k to 52.5k depending on the index (at=1: 45.7k, 1000: 30.7k, 2500: 50.5k, 4000: 52.5k, 4998: 25.6k).
- The same write on small arrays: 9.7k flat for sizes 6 to 32, 20.7k at size 40, 22.0k at size 100, 45.2k at size 1,100. My size-9 probe gives 9.7k but fannkuch in place gives about 20.9k. I did not find out why.
- Recursive `[h, ..t]` set costs about 2,830 for index 0 plus about 2,050 for each further step (size 6 at=5: 12,960; size 16 at=15: 33,715). Keeping the old array alive changes this by under 5%.
- Yardstick for what an in-place set could cost: `xs[at] or 0` on an Int array (ArrayIndexOr) costs 325 at size 6 and 480 at size 5,000. The bare loop step is 410.

MEASURED, sampling profile (3 s each, top of stack):
- sieve: memmove 137, Heap::release 129, array::branch 118, array::node 91, own_all 70, rebalance 52, Machine::call only 40. This matches the cause named for the slice/concat form.
- permute: Machine::call 234, Heap::release 144, memmove 76, at_end 34, element 33, alloc 28, push_owned 27, enter_above 24, leaf 24. There is no merge, rebalance, branch or tree_take here.

READ, not measured:
- array.sca has no set, and scarlet_ir's prim.rs has ArrayAppend but no ArraySet.
- The geomean arithmetic checks out. With my residuals (sieve to about 16, permute x0.35, queens x0.35, fannkuch x0.73) the factor is 0.78 to 0.79, and sieve alone is 0.877.

**Corrections.** The cost is real and the sizes are right to within 10%. Four details of the cause and the gain are off.

1. The recursive form in permute and queens does not run the slice, take, merge or rebalance code. The profile shows none of it. That cost is one interpreted non-tail call per element plus ArrayLen, ArrayElem, ArrayDrop and ArrayPrepend, each allocating and freeing a root and a small leaf, at about 2,050 to 2,830 instructions per step. Roughly a third of the samples are in the interpreter loop and frame setup, and the rest are in release, alloc and leaf copies. A set intrinsic still removes it, because the walk goes away.

2. "The array is uniquely held the whole time" is true for sieve only. This is from reading the source and the dis output, not from a probe. In queens, `place_queen` keeps `b` for backtracking across `take(b, r, c)`, so every board array is shared when it is written and set must copy. In permute, `swaps` reads `Field(0)` of the State, calls swap, and only drops the State afterwards, so the State still holds `v` during the write. In fannkuch the same holds for `s` if it is reached through `Next`. For these three, the fix lands on the copy path (one leaf of 16 or fewer slots plus a root) unless Perceus also learns to move a field out of a dying constructor. That is still a few hundred instructions against 5,000 to 20,000, so the gain stands, but "write the slot in place" will not be what runs.

3. Sieve after the fix is nearer 15 to 16 times LuaJIT than 10. The non-clear remainder is about 1.2e9 (estimated from the no-op-clear run), and 498k sets at about 500 each add 0.25e9. That is 1.45e9 of 29.08e9, about 5% left, not 3 to 4%. An in-place set at 200 to 400 is plausible for small arrays, but an existing indexed read already costs 325 to 480. Part of the sieve remainder is that `flags[i] or False` on a Bool array compiles to ArrayIndex plus an Option match at 900 to 1,060 per read, where the Int form uses ArrayIndexOr at 325 to 480. That is a separate item.

4. My double-write ablation gives slightly lower shares than the claim: permute 70.3% against 72.1%, queens 70.6% against 73.4%, sieve 91 to 96% against 96.4%. The geomean factor of about 0.78 survives.

### 2. A non-tail call and its return cost about 220 to 300 instructions, and a closure call about 380

`call-return-frames`, kind: interpreter. Depends on: register-access-checks.

**Today.** Call and CallSelf run the out-of-line enter_above (prologue and epilogue about 20, a `leave(new_base)` that never releases anything, `open`, then about 27 per argument for a checked take or share and a checked set that tests the old value). The caller's 40-byte Frame is pushed onto a separate Vec with mem::replace (21 to 23). Ret takes the result, then `leave` walks every register of the frame storing Nil and testing for a cell (9 per register), releases env, pops the Frame and does a checked set in the caller. Calling a closure adds the out-of-line `callee` (kind read, function lookup, run-time arity check, a share of the closure as env) and a release on return, and each capture read is its own 70 to 90 instruction GetCapture.

**Where.** crates/scarlet_vm/src/exec.rs Instr::Call, CallSelf, CallValue, TailCallValue, Ret arms (lines 315-360, 382, 773-786), enter_above (2322), leave (2347), open (2362), callee (2292), GetCapture (401), struct Frame (38)

**Benchmarks hit.** fib, tak, binary_trees, cfold, deriv, towers, rbtree, closures, richards, spectral_norm, map_churn, storage, strings, permute, queens, list, json, bounce, string_keys, nbody, fannkuch

**Estimated gain.** Assuming a call and return pair falls from about 230 to about 80 (LuaJIT is 50 to 85): fib about 37% (call machinery is 191 of 355), tak about 35%, closures about 18% (with the closure surcharge and capture reads), towers 16%, cfold 15%, binary_trees 14%, deriv 12%, rbtree 12%, spectral_norm 9%, permute 8%, list 7%, json, map_churn, queens, richards and strings about 6% each, storage 4.5%, others 2% or less. Geomean factor about 0.90 alone.

**Evidence.** fib report (lldb trace of all of fib(10)): CallSelf 124 + Ret 97 of 355; clearing and opening frames 59, Frame push and pop 36, one argument 36, result move 27, enter_above out of line 26. tak report: CallSelf 179.3 (129 in enter_above), Ret 124, together 60% of 379.3. calls study: Call 1 arg 135, 3 args 191, Ret 88 + 9 per register, call and return of `id(x)` 223; CallValue on a closure 234, Ret from a closure frame 149, GetCapture 70, so `fn(x) x + k` is 523 against 223 by name. binary_trees: CallSelf + Ret 25.0%. cfold: 26%. deriv: 21%. towers: 27.2% (7 calls per move). closures: CallValue 15.3% + Ret 9.1% + TailCallValue 7.7% = 32%, GetCapture 8.8%. LuaJIT measured by the spectral_norm report: 85 for a call plus return; by the closures study: 71.

**Agreement between reports.** Every per-benchmark report and the interpreter-loop, calls, value and closures studies found it. The 223 for a one-argument call and return is reproduced by at least nine reports. Reports that quote 330 to 370 (json, sieve, strings, permute, bounce) include an add or a heap argument. The per-register sweep cost on Ret is 9 (fib, tak, rbtree, calls) or 7.4 to 7.6 (nbody), but the towers report measured 0 per extra local; see unexplained. LuaJIT's call and return is quoted as 30 to 40 (several reports, inferred), about 50 to 55 (loop and calls studies, counted from its source) and 71 to 85 (measured).

**Fix direction.** Have the loader record which registers can hold a cell at each Ret (the typed IR and the liveness pass already know) and release only those, so most frames return with no loop. Drop the `leave(new_base)` on entry. Write arguments into the callee's window with one bounds check for the frame, with parameters as registers 0..n, or evaluate arguments into the top of the caller's frame so nothing is copied. Keep the return record small (pc and base) and next to the register stack. For closures, borrow the closure from the caller's register for a non-tail call, drop the arity check the type checker already proved, and pass captures as hidden parameters or let instructions read them as operands.

**The sceptic** (held). All numbers are instructions retired from /usr/bin/time -l on ./target/release/scarlet, 10M iterations, startup (38,693,499) subtracted. Probes are in scratch/fat/callret.

MEASURED, per loop step:
- Bare loop `go(n-1, acc+n)`: 346.2.
- Same loop with `acc + id(n)`: 569.2. So a call and return of a one-argument `id` costs 223.0. Calling `id(id(n))` adds another 223.1.
- Two arguments: 606.1. Three arguments: 643.2. So each extra argument costs 37.0 (call plus return together). The range for one to three arguments is 223, 260, 297, which matches the claimed "220 to 300".
- Function value or capture-free closure `fn(x) { x }` called through a register: 274.1 per call and return (656.2 against a 382.2 three-parameter bare loop). That is 51 more than a call by name. A closure with no captures is a constant, not a cell, so this is the cheap case.
- Closure `fn(x) { x + k }` with one capture: 523.1 per call (905.3 - 382.2). The same body by name, `x + 3`, costs 333.1. The claim's "523 against 223" figure reproduces exactly, though the fair by-name comparison is 333, so the closure surcharge is about 190.
- A second capture and add (`x + k + j`) adds 140.0. A second constant and add by name adds 59.0. So a capture read costs about 81 more than a constant load, which puts GetCapture at roughly 80 to 110. The claim says 70 to 90.
- Per-register sweep on Ret: a callee with 9 products live at once against a chained callee with the same 9 multiplies and 8 adds costs 54.1 more per call. The register counts are inferred (about 10 against about 3 or 4 after the loader's register allocator), which gives about 8 to 9 per extra live register. My first attempt, with locals in a dead branch, showed 1.0 per register, because the loader's allocator (code.rs `allocate`) merges registers that are never live together. That is very likely why the towers report saw 0 per extra local.
- fib(35): 10,644,937,484 instructions, which is 355.2 per call over 29,860,703 calls.
- Floor for a simple instruction: three extra `+ 0` in the loop add 177.2, so a Const plus an IntAdd is 59, about 30 each.

READ FROM THE MACHINE CODE (objdump of enter_above in target/profiling/scarlet, counted by hand on the path for one shared Int argument): prologue 13, the `leave(new_base)` empty-range test about 8, `open` about 8, loop setup about 10, 28 per argument, epilogue about 8. That is about 75 in enter_above for one argument. The 28 per argument matches the claimed 27. Added to the roughly 9 per register on Ret, it matches my measured 37 per argument.

INFERRED, not measured: the split of the 223 between Call (about 135) and Ret (about 88), and the Frame push cost of 21 to 23. I did not trace these.

**Corrections.** The cost exists, is the size claimed, and has the cause named. Four parts of the claim are off.

1. The target of 80 for a call and return is too low, so the gain is overstated. I measured a Const or an IntAdd in this VM at about 30 instructions each. A call and return is two dispatches, so 80 means 40 each, which is barely above loading a constant. By my own count (inferred, not measured), a safe-Rust version of the suggested design still needs dispatch and operand decode twice, a function-table bounds check, one window bounds check, a Vec push and pop with capacity and length checks, a tag test per shared argument, an env release test, and a checked set of the result in the caller. That comes to about 105 to 120 for one argument. So the saving is about 105 to 120 per call rather than 143 to 150. For fib that is about 30% rather than 37%, and the other per-benchmark gains scale down by about the same quarter. Reaching 80 needs the dispatch and register-access work the claim lists as a dependency, so that part of the gain is shared with that claim and should not be counted twice.

2. Dropping `leave(new_base)` on entry saves only about 8 instructions. In the machine code it is a two-compare test of an empty range. It never releases anything, as claimed, but it is not a meaningful cost.

3. Skipping the Nil sweep on Ret moves a little cost rather than removing all of it. Today `set` relies on every register above `top` being Nil. If Ret stops clearing, registers that can hold a cell must still be released and cleared, or a later `set` would release a stale cell twice. Frames with only Int registers do return with no loop, so the saving of about 9 per register is real for them. The old-value test in `set` stays in the callee either way.

4. The "9 per register" applies to registers after the loader's allocator has merged them, not to locals in the source or in `scarlet dis` output. Locals that are never live together share a register and cost nothing extra. That explains the towers report's 0 per extra local, so it does not need to stay under "unexplained".

Two smaller points. The estimate text says call machinery is "191 of 355" for fib while the evidence says 124 + 97 = 221, and my probe gives 223 for a one-argument call and return, so 221 is the consistent figure. And "523 against 223 by name" compares a closure that adds with a function that does not. The like-for-like figures are 523 against 333.

### 3. Appending or prepending one element allocates and copies a leaf even when the array is unique, and a prepend after `[h, ..t]` always takes the slow path

`array-push-not-in-place`, kind: stdlib-or-builtin.

**Today.** Leaves are sized exactly, so an owned push allocates a leaf one word longer, copies the old leaf into a zeroed 32-word stack buffer, copies that into the new cell and frees the old leaf. Around it the instruction calls seq, tree (a share that is released again), items (a SmallVec for one item) and push_all, none inlined. A front push needs offset 0, and `t` from `[h, ..t]` has offset 1 or more (or no head leaf at all for arrays of 32 or fewer), so `[x, ..t]` falls to the copying push: a settled leaf, a second leaf, a new root, two frees and about seven 264-byte SmallVec moves. The first append to `[]` also takes the copying path.

**Where.** crates/scarlet_vm/src/array.rs push_owned (the `fits` test near line 574, leaf swap 585-613), push (532-560), own_head, head_items, own_all, at_end, leaf; crates/scarlet_vm/src/exec.rs Instr::ArrayAppend (620), ArrayPrepend (604), Machine::seq, tree, items, push_all

**Benchmarks hit.** storage, fannkuch, permute, queens, closures, bounce, json, nbody, richards, sieve, strings

**Estimated gain.** Assuming an owned push falls from about 800 to about 100 and the offset prepend becomes a store: storage about 35% (appends are 45.2% today), fannkuch about 30% (prepends are 16.2% + 15.9% + 5.0%), permute about 30% (slow prepends are an estimated 37.6%), queens about 27% (fast path 17.5%, fallback 14.6%), closures 12% (ArrayAppend 14.8%), bounce 9% (append 11.3%), json and nbody about 5%, richards 4%, sieve 3%, strings 2%. Geomean factor about 0.92 alone. In permute and queens this overlaps no-array-set: with a set primitive those prepends are no longer run.

**Evidence.** storage report: 104 traced in-place appends average 790 (leaf swap about 218, bookkeeping about 358), 22,419 per round = 32%; the first append to an empty array is 1,284, 12.8%. arrays study: owned append 823 in the handler, 417 inside one in-leaf push_owned (alloc 66, free_cell 63, memmove 56); prepend 477 to 488 in place against 2,289 to 3,393 on the slow path; ArrayPrepend is 43% of permute, 40% of queens and 36% of fannkuch in 120,000-instruction windows. fannkuch report: `[q1, ..back]` 2,819 and 2.84 cells each (16.2%), the 'in place' prepend 874 and exactly 1.000 cell each (15.9%). permute report: slow prepend 2,889, fast 521. queens report: fast prepend 707, fallback about 3,700 with 12 live items. bounce: append 850. closures: 770 to 860, and 5,200 to 6,000 once per 32 when a leaf moves into the tree.

**Agreement between reports.** Eight benchmark reports and the arrays study found it. The owned append is quoted as 790 (storage), 823 (arrays), 850 (bounce), 874 (fannkuch prepend) and 707 (queens prepend); the spread tracks leaf length and what is counted. The ArrayPrepend share differs between reports: permute 37.6% estimated from probes against 43% in the arrays window and 50% of time samples; queens 32% against 40%; fannkuch 37% against 36%.

**Fix direction.** Give unique head and tail leaves capacity (32 slots, or doubling) with the live length in the root, so an owned push is a uniqueness check, one store and a length bump. Let a front push on a unique root with offset above zero write the slot before the offset and decrement the offset. Treat a unique root with no tail as fitting. Skip the share and release pair when `moved` is set and skip the SmallVec for one item. On the copying path, build the new node cell to cell with one copy instead of through by-value SmallVecs.

**The sceptic** (held). All numbers are instructions retired from /usr/bin/time -l on my own probes in scratch/fat/apush, with the bare loop step (268) subtracted.

MEASURED, owned push in place. `fill([..arr, n], n - 1)` from `[]` to K items, 100,000 rounds, slope between K values: 853 per append for leaf lengths 2 to 9, 863 for 9 to 17, 897 for 17 to 31. `[n, ..arr]` gives the same totals to within 0.2%, so the in-place prepend costs the same. The claim says 790 to 874. The cost grows about 1.5 instructions per item already in the leaf, which fits a copy of the whole leaf. The first append to `[]` is 1,505 (claim 1,284; mine includes freeing a 1-item array instead of an empty one). The 33rd append, where a full leaf goes into the tree, is 2,011, and the 65th is 2,952. Average over 200 appends is 963.

MEASURED, slow prepend. `x = [n, ..base]` with the result dropped each step, against the same loop without the prepend (so the number includes freeing the result). Onto an array with no head leaf (built by append, 32 or fewer items): 1,466, flat for K = 2, 12, 13, 31. Onto a shared head leaf at offset 0: 2,086 (K=2), 2,680 (12), 2,745 (13), 3,839 (31), about 60 per item. Onto `t` from `[_h, ..t]` of a prepend-built array (offset 1): 2,696 with 1 live item, 3,740 with 11, 3,862 with 12, 5,580 with 30, about 100 per live item. The claim's 2,289 to 3,393 and "about 3,700 with 12 live items" match. The destructure-then-prepend pair `[h, ..t] -> go([h, ..t], n - 1)` costs 2,841 per step on an append-built array (any K from 2 to 32) and 3,498 / 4,186 / 4,543 / 6,383 on a prepend-built one at K = 2 / 8 / 12 / 31.

MEASURED, cause. I read push_owned, push, own_head, skip, skip_owned and the ArrayAppend/ArrayPrepend handlers. The `fits` test does require offset 0 for a front push and an existing leaf of 1 to 31 items, and the in-place path does copy the old leaf into a zeroed `[0u64; 32]`, call heap.make and discard the old leaf. Sampling the profiling binary on the append loop puts push_owned's samples on array.rs 594 (memmove), 599 (make and a second memmove), 603 (discard and free_cell) and 604 (data_mut). On the offset-1 prepend loop, 1,624 of 2,198 samples are under exec.rs:617 (push_all), and inside it own_head (head_items collect, own_all, leaf), each with its own memmove, then release. seq, tree, items, push_all and the release at exec.rs 615/631 all show up as separate frames, so they are not inlined.

MEASURED, benchmark shares (time samples, not instructions). The ArrayPrepend handler lines (exec.rs 615 and 617) are 44.6% of permute samples, 32.6% of queens and 40.4% of fannkuch. The claim says 37.6 to 50%, 32 to 40% and 36 to 37%. Storage runs 54.5 million instructions per round. With the claim's own counts (22,419 in-place appends and 5,461 first appends per round) and my prices (about 860 and 1,505) appends come to about 27.5 million, or 50%, against the claimed 45.2%.

INFERRED, not measured. From sample shares in the append loop, roughly a third of the 877 (about 290) is the shell around push_owned: seq, tree's share, items' SmallVec, and the take-and-release pair at line 631, which alone is 216 of about 1,590 handler samples. About 500 is the leaf swap. I did not count these by instruction.

**Corrections.** The cost and the cause are as claimed. My numbers sit within about 1.1x of the claim's for the owned push and the slow prepend, and about 1.2x for the first append. Three things in the claim need tightening.

1. The slow prepend is not one price. Onto an array with no head leaf it is a flat 1,466 (one 1-item leaf, one root, no item copies). Onto a head with an offset it is about 2,600 plus 100 per live item, because own_head copies the live items three times (head_items, own_all, leaf) and push then copies them again. The "2,289 to 3,393" range is the second case at small sizes, and at 30 live items it is 5,580.

2. "About 100 after the fix" needs both halves of the fix. Giving leaves capacity removes only the leaf swap, which I estimate at about 500 of the 877. The shell (seq, tree's share, items, push_all, release) is an estimated 290 and stays unless it is cut too. With only capacity leaves, expect about 350 to 400 per push, not 100.

3. Part of the cost moves instead of disappearing. This is from reading the code, not measured. Leaf length is read from the cell size today (slot_count, head_len, View::len, node_len), so a leaf with spare capacity needs its live length kept in the root or the leaf, and every reader has to use it. Freeing a leaf walks its words, so spare slots must be Nil-filled or bounded by a stored length. With fixed 32-slot leaves, storage's arrays of 1 to 10 items each pay to allocate, fill and free a 32-word cell. With doubling, a 4-item array still makes 3 leaves instead of 4 and a 10-item array 5 instead of 10. So storage's 35% is optimistic, because its appends go to very short arrays that are built once. A front push into the slot before the offset must also first release the item left behind there. The first append to `[]` (1,505, about 15% of storage on my prices) still needs a leaf allocated whatever the layout.

The permute and queens gains overlap with a set primitive, as the claim already says. The claim's 5,200 to 6,000 for a leaf moving into the tree is higher than my 2,011 (33rd append) and 2,952 (65th) on small Int arrays. I did not test larger trees or closure elements, so I cannot confirm that figure.

### 4. Making and freeing a small heap cell costs 450 to 700 instructions, and tuples, closures and array literals add a system malloc

`cell-alloc-free-path`, kind: interpreter. Depends on: heap-cell-addressing.

**Today.** Heap::alloc is an out-of-line 66 instructions (checked size maths, a pop from a Vec<Vec<Cell>> free list, a chunk lookup to write the header, two saturating counters). Instr::Ctor moves fields into the `passing` Vec and then into the cell through a memcpy call. Freeing goes through Heap::release: take the `dying` Vec, push and pop the cell, re-read the header, find held words by a kind comparison chain, look each field up again, then free_cell (63: handle check, own-chunk check, zero the header, Vec push). A release that frees nothing is still an out-of-line call that saves 14 registers. Tuple, Closure and Array collect operands with Machine::args, which returns a fresh Vec, so each pays malloc and free on top.

**Where.** crates/scarlet_vm/src/heap.rs alloc (196), FreeLists::pop/push (137, 145), release (281-312), free_cell (354-369), write_ctor (497), held (775); crates/scarlet_vm/src/exec.rs Instr::Ctor (266-304), Instr::Tuple (483), Instr::Closure (392), Instr::Array (501), Machine::args (2255, confirmed in source to collect into a new Vec for all three)

**Benchmarks hit.** richards, binary_trees, bounce, cfold, towers, storage, rbtree, nbody, fannkuch, deriv, json, string_keys, strings, sieve, permute, map_churn, queens

**Estimated gain.** Assuming alloc falls to about 10 to 20, a leaf free to about 30, a Ctor to about 60 and the args Vec goes: richards about 17% (cell traffic is 27% to 31%), binary_trees 15% (Ctor 13.8% plus freeing Drops about 16%), bounce 15%, cfold 13% (Ctor 14.7% + frees 7.1%), towers 12%, storage 10%, rbtree 10%, nbody 9%, fannkuch 8%, deriv and json 7%, string_keys and strings 6%, sieve, permute and map_churn 5%. Geomean factor about 0.93 alone. Part of this is the same instructions as heap-cell-addressing.

**Evidence.** binary_trees report: Ctor 254 every time (handler with Vec and memcpy 94, alloc 38, cell lookup 33); Drops that free cost 253 (leaf) and 333 (interior). value study: Heap::alloc 66, fresh 2-field Ctor 252, freeing Drop 256 (157 in release, 63 in free_cell), a release that frees nothing 51, Field that shares 96 against 77. richards report: 956,437 cells per iteration, 0 reused, about 446 per Some, 498 per 2-field record, 795 per 8-field record; release + alloc + free_cell are 31.7% of time samples. bounce report: 3-tuple make about 500 (629 under lldb, 363 of it libmalloc), free 280. closures study: Closure 418, of which about 196 is system malloc and free. json report: tuple about 200 more than a constructor of the same size; Machine::args 81 samples with malloc under it. nbody: 765 per 7-field Body made and freed, 708 per 2-tuple.

**Agreement between reports.** About sixteen reports found it. alloc is 66 in most traces but 82 in cfold and about 70 in deriv. A two-field Ctor is 254 (binary_trees, closures, towers) or 252 (value) but 270 in cfold; a one-field Ctor is 225 (storage), 233 (deriv) or 241 (cfold). The Vec's malloc and free cost per tuple is given as about 196 to 220 (closures study, bounce, json, nbody, in normal runs), about 320 (code-quality study, inferred) and 382 of 601 (value study, traced under lldb, where two reports say malloc runs a slower path).

**Fix direction.** Inline a fast alloc: an intrusive free list threaded through freed cells with one head per size class, or a bump pointer. Write fields straight from registers into the cell slice, with plain stores for small counts. Split release into an inlined decrement and a cold free; give the free a path for a cell that holds no cells, with no `dying` Vec. Make Tuple, Closure and Array use the `passing` buffer or write directly into the cell. Raise DENSE_SIZES past 66 so full array branches skip the HashMap free list (sieve report).

**The sceptic** (held). Measured with /usr/bin/time -l on target/release/scarlet, 5M-step tail loops, startup (40.4M) subtracted, two runs each agreeing to 4 digits. Probes are in scratch/fat/cellalloc.

Instructions per step:
- bare loop 345; bare loop carrying a third cell argument 381
- shared Field read 77 (458 - 381); tuple Element read 75
- closure call 446

Make, read one field, and free, over the bare loop (the make+free figure takes the 77 or 75 read off):
- 1-field ctor: 416, so about 339 for make+free
- 2-field ctor: 470, so about 393
- 4-field ctor: 581, so about 504
- 8-field ctor: 786, so about 709
- Some(n) plus match: 484
- 2-tuple: 789, so about 714
- 3-tuple: 841, so about 766
- closure with 1 capture: 1131 including the call, so about 685 for make+free
- 2-element array literal: 1589 including ArrayLen, IntGe and ArrayElem

So the cost exists and the sizes match. Small ctors come out at 340 to 500, a little under the claimed 450 floor. Tuples and closures are about 700. The 8-field record is 709 against the claimed 795. The 2-tuple is 714 against nbody's 708.

A tuple costs 319 more than a ctor of the same size (1134 against 815 per step). That is larger than the claimed 196 to 220, but the 319 also includes the out-of-line Machine::args and the collect, not only malloc and free.

Sampling profile by pid on the profiling binary, 300M-step versions. These are time shares, not instruction counts.
- ctor2: Heap::release 11.5%, free_cell 5.5%, alloc 4.8%, memmove plus stub 6.2%. That is 28% of an 815-instruction step outside Machine::call.
- tuple2: libmalloc (xzm_malloc_tc, xzm_free_tc, malloc_zone_malloc, free, stubs) 14.5%, Machine::args 5.2%, release 19.6%, alloc 8.1%, free_cell 7.4%.
- clos1: libmalloc about 12%.
So the system malloc under Tuple and Closure is real.

I read the code. Machine::args collects into a fresh Vec for Tuple, Closure and Array. Ctor goes through the `passing` Vec. release takes the `dying` Vec. free_cell and FreeLists are as described. The disassembled Heap::alloc fast path is roughly 60 to 70 instructions by reading. I did not trace it exactly.

Not measured: the per-benchmark gain percentages, the 51-instruction release that frees nothing, and the exact 66 and 63 path lengths.

**Corrections.** The claim holds, with three corrections.

1. Perceus reuse is not cheaper than a fresh alloc and free here. A loop that matches B(a, c) and rebuilds B(a+n, c) in place (DropReuse, then ctor reuse) costs 984 per step, which is 639 over the bare loop. Taking off two field binds (about 150 to 190) and one extra cell argument (36) leaves about 410 for hollow plus ctor-in-place. A fresh 2-field make+free is about 393. So a large part of the cost is the Ctor handler, hollow and release machinery and the per-word chunk lookups, not Heap::alloc and free_cell. A faster allocator alone will not help code that already reuses. This subtraction is inferred from separate probes, not isolated directly.

2. For a 2-field ctor, the out-of-line heap functions (alloc, release, free_cell, memmove) are about 28% of step time, roughly 230 of the 393 if time tracks instructions. The remaining 160 or so is inline in Machine::call. So the claimed targets (Ctor about 60, free about 30) need the handler and cell addressing fixed too. That is the stated heap-cell-addressing dependency, and the two gains should not be added together.

3. The title understates array literals and slightly overstates small ctors. A 2-element array literal costs about 1,300 to 1,500 to make and free, and array::from_values does much more than one malloc. One- and two-field ctors are 340 to 390, not 450 or more. Removing the args Vec should recover up to about 320 per tuple, more than the claimed 200.

### 5. Fetch and dispatch cost 14 to 15 machine instructions per VM instruction, and an instruction is 48 bytes

`dispatch-and-instr-format`, kind: interpreter.

**Today.** Each turn of the loop reloads the body's instruction pointer and length, bounds-checks pc, multiplies pc by 48 with a madd, does a saturating bump, loads the tag byte, and resolves a 16-bit offset jump table with five instructions before the branch. All handlers return to one shared dispatch site, so there is one indirect branch for the whole interpreter. Frame fields (base, env, ret_to) live in stack slots of a 1.8 KB frame and are reloaded in every handler. Register numbers are 32-bit and argument lists sit behind a Box, which is what makes the enum 48 bytes.

**Where.** crates/scarlet_vm/src/exec.rs Machine::call lines 124-134; crates/scarlet_vm/src/code.rs enum Instr (68), Reg (42), Arg (52)

**Benchmarks hit.** fib, deriv, cfold, list, mandelbrot, binary_trees, rbtree, spectral_norm, tak, bounce, towers, closures, json, map_churn, nbody, queens, richards, permute, storage, string_keys, fannkuch, strings

**Estimated gain.** Dispatch share today: fib 19.0%, deriv 18.7%, list 17.5% of the hot step, cfold about 17%, mandelbrot about 16.5%, binary_trees 15.6%, spectral_norm 14.0%, tak 13.8%, bounce 11.0%, towers 11.0%, closures 10.0%, permute 7.5%, storage 7.5%, richards about 7%, string_keys about 6%, queens 5.5%, fannkuch about 5%. Going from 15 to about 6 removes 60% of that: about 11% on fib and deriv, 8% to 10% on cfold, list, mandelbrot, binary_trees, spectral_norm and tak, 4% to 7% elsewhere, 0 on sieve. Geomean factor about 0.93 alone. Removing whole instructions (other items) cuts dispatch further.

**Evidence.** interpreter-loop study: the same 15 addresses (0x1002a963c to 0x1002a9674) after each of the four instructions of the bare loop, 60 of 346; `mov w9, #0x30; madd` at 0x1002a9650. fib report: 800 dispatches for 177 calls, every one 15. binary_trees: every one of 12 instruction kinds shows exactly 14.0, 8,918 of 57,047. tak: 11,025 of 79,759 traced instructions. deriv: 11,103 of 60,000 and 27,281 of 150,000. LuaJIT's ins_NEXT is 6 instructions, replicated per handler (loop study, read from vm_arm64.dasc).

**Agreement between reports.** About twenty reports found it. The count is 14 in nine reports, 15 in six, 15.4 to 15.5 in two, 15 to 17 in cfold and 'about 12' in storage; the difference is whether the four instructions at the loop tail and the branch back are counted. The mandelbrot share was counted from disassembly, not measured. The rbtree report calls dispatch 'fine at about 14', which is a judgement, not a different number.

**Fix direction.** Shrink Instr to 8 or 16 bytes (8-bit or 16-bit registers, wide operands and argument lists in a side table) so indexing is a shift. Have the loader prove every body ends in a terminator and every jump target is in range, and iterate a slice held in locals so pc needs no per-step check. Keep base and the frame slice in locals. This format change is the natural place to add per-operator opcodes, immediate operands and fused forms.

**The sceptic** (held). Measured, by single-stepping the profiling binary under lldb and counting program counters inside the dispatch block 0x1002a963c to 0x1002a9674 (15 instructions, one `br x9`, the only dispatch site reached in every trace):
- Bare loop `go(n - 1, acc + n)`: 20,000 traced instructions, 231 dispatches, 3,461 instructions in the block. That is 14.98 per dispatch and 17.3% of the step. The step is 4 VM instructions and 346.3 machine instructions (86.58 x 4), which agrees with /usr/bin/time: (3,500,470,001 - 39,444,155) / 10M = 346.1.
- fib(32): 15,000 traced, 190 dispatches, 2,859 in the block. 15.05 per dispatch, 19.1% (claim says 19.0%).
- deriv: 15,000 traced, 171 dispatches, 2,561 in the block. 14.98 per dispatch, 17.1% (claim says 18.7%).
- queens: 15,000 traced, 103 dispatches, 1,541 in the block. 14.96 per dispatch, 10.3% in my window (claim says 5.5%). My window starts after only 20,000 dispatches, so it may be setup code and I would not trust it over the claim.
- Every dispatch in the loop, fib and queens traces was entered by a taken branch from a handler, and each handler I read starts with `mov x24, x28` to undo the block's `mov x28, x24`. So the real per-instruction overhead is 16 to 17, not 15.
- Micro-probe: one extra reg-reg IntAdd in a loop costs 70.0 machine instructions (4,371,507,336 - 3,670,717,266 over 10M, and the same again for a second and third add). So dispatch is 15 of 70 for the cheapest arithmetic instruction, and the other 55 is the handler.

Read from disassembly: the block is `mov x28,x24; ldr len; cmp; b.hs; ldr ptr; mov w9,#48; madd; add pc,#1; ldrb tag; adrp; add; adr; ldrh; add; br`. The frame is `sub sp, #0x6a0` plus 0x70 of saves, which is 1,808 bytes. Handlers reload base from [sp,#0xd0]. All of this matches the claim, except that the pc bump is a plain `add x26, x26, #1`. The compiler already removed the saturating add.

Fix direction, measured on a toy: I built a safe-Rust interpreter with rustc (not cargo) at scratch/fat/dispatch-refute/mini/mini.rs. It has 8-byte instructions, u8 registers and the code slice held in locals. Its dispatch is 9 instructions (`cmp; b.hs; add; add; ldrb; adr; ldrb; add; br`) plus a branch back from most handlers. Its whole 4-instruction loop costs 77.0 machine instructions per turn.

Not checked: why the enum is 48 bytes. I could not run size_of without cargo. From reading the fields, `Bad { why: Broken }` (which holds a String plus a u32, or three u64) and Ctor could set the size as easily as Box and u32 registers do.

**Corrections.** The cost and its cause are as claimed: 15 per VM instruction at one shared site, and 17% to 19% of the hot step on the loop, fib and deriv. The estimated gain is too high. The target of 6 is LuaJIT's hand-written threaded dispatch. A safe-Rust `match` loop still needs the pc bounds check (2 instructions) and one shared jump-table sequence, because a load-time proof is invisible to the compiler without `unsafe`. My toy with the proposed format got 9, plus a branch back, and it had the table base hoisted, which the real 100-arm function may not get. So expect 15 to about 9 or 10. That removes 35% to 40% of dispatch, not 60%: about 7% on fib and deriv instead of 11%, and by inference a geomean factor of about 0.95 to 0.96 instead of 0.93. Two smaller points. The count should be 16 to 17 if the branch back and the `mov x24, x28` are included. There is no saturating bump in the machine code. That the 48 bytes come from u32 registers and Box is a guess, since Broken or Ctor may set the size.

### 6. String and binary operations copy operands out of the heap into a fresh Vec byte by byte, literals are allocated on every use, and join and replace are quadratic

`strings-copy-out`, kind: stdlib-or-builtin.

**Today.** read_string pushes bytes one at a time into a new Vec (system malloc), most built-ins then re-validate UTF-8, and Heap::string packs the result back a word at a time through a memcpy call. Concat starts from Vec::new() so it reallocs as it grows. Hashing a string map key and comparing two keys each make their own Vec, and the hash is never cached. Instr::Str allocates a new cell every time a literal is evaluated. `string::replace` and any join are a Scarlet fold of `acc + sep + part`, so they copy the accumulator per piece. split makes a Rust String and a heap string per piece. Binary reads go through a bit-offset reader per byte; `binary::byte_at` costs 212 to 293 through the generic built-in path.

**Where.** crates/scarlet_vm/src/heap.rs read_string (410), string (384), padded_word (805); crates/scarlet_vm/src/exec.rs Instr::Concat (252-265), Instr::Str (136), Machine::num, text_of, utf8_of, Intrinsic::StringSplit (937), StringLength, BinaryByteAt (829), BinaryToString (803); crates/scarlet_vm/src/hash.rs cell (String arm), crates/scarlet_vm/src/eq.rs cells (76-81), crates/scarlet_vm/src/binary.rs byte, bytes, join; crates/scarlet_core/src/std/scarlet/string.sca replace, binary.sca slice_bytes, concat

**Benchmarks hit.** strings, string_keys, json, deriv

**Estimated gain.** strings: about 52% (quadratic join and replace 20.5%, small concats 12.5%, split 21%, literals 3%, length/contains/trim 4%, int parse 4.6%, each cut by two thirds or more). string_keys: about 40% (Concat 24.8%, the unused `''` literal about 10% with its free, string key hash and compare inside map get and set 17.8%, of which malloc alone is 14.5% of the run). json: about 15% (byte_at 11%, slice-to-string 8.5%, `''` literal 1.6%). deriv: 1.5% (string == allocates). Geomean factor about 0.94 alone.

**Evidence.** strings report (ablation): batch join 1.451M and batch replace 1.498M instructions per batch, about 2.62M of pure copying, 20.5% of the benchmark; split of an 11-byte field 4,200; a small concat about 1,650 fixed plus 9 per byte. strings-and-binaries study: replace is 27% and join about 20% of the strings benchmark by deleting statements; 256 matches in 2 KB cost 7.19M; binary append 313 per byte; `s == t` 859 for 8 bytes; a literal about 320 each time. string_keys report (lldb trace): Concat 927 to 981 traced, about 840 in a normal run, with malloc 216 and free 144; `Str` 185 plus 170 to free; string map get 1,650 to 1,900 against 362 for an Int key. maps study: one 2-byte string get stepped at 1,692, of which malloc and free are about 784. json report: byte_at 212 to 293, 38,753 times per parse; ablating capture removes 11.3M of 93.4M.

**Agreement between reports.** Four benchmark reports (strings, string_keys, json, deriv) and the strings and maps studies found it. The fixed cost of a small concat is given as about 840 to 936 for 1+1 bytes (string_keys), about 1,390 for 8+8 bytes (strings study) and about 1,650 (strings report, which accounts for only 900 to 1,000 of it). string_keys word building is 74% of the run in its own report and 80.5% in the maps study; map work is 18% against 19.5%.

**Fix direction.** Let built-ins read a string cell's bytes in place (bytes stored as a byte region, or a checked cast of the word slice), so length, ==, contains, trim, hash and split allocate nothing. Concat should sum lengths, allocate once and copy words cell to cell. Make each literal one sticky cell created at load. Add linear `string::join`, `replace` and `binary::concat` built-ins, and lower a chain of `+` to one StringConcatMany. Hash from the cell's words, compare word by word, and consider caching the hash. Give byte_at and byte-aligned binaries a direct path.

**The sceptic** (held). I could not refute it: my own probes reproduce the claimed costs to within about 10%.

All numbers are instructions retired per operation from `/usr/bin/time -l` on `./target/release/scarlet`, 1M iterations, with a 4-argument bare loop (444 per step) subtracted. Startup was 39.1M.

**Per-operation costs (measured)**
| Operation | Mine | Claim |
|---|---|---|
| `''` literal bound and dropped | 360 | about 320 to 355 |
| 8-byte literal bound and dropped | 394 | |
| `s == t`, 8 bytes | 857 | 859 |
| `s == t`, 512 bytes | 5,583 (about 9.3 per byte) | |
| Concat 1+1 bytes | about 950 | 840 to 936 |
| Concat 8+8 bytes | about 1,440 | 1,390 |
| Concat 512+512 bytes | about 11,400 (about 11 per byte) | 9 per byte |
| `string::split('score=12345', '=')` | 4,297 | 4,200 |
| `string::length`, 1 / 8 / 64 / 512 / 1024 bytes | 549 / 641 / 974 / 3,836 / 6,803 | |
| `map::get` hit, 8-byte string key | 1,664 | 1,650 to 1,900 |
| `map::get` hit, Int key | 285 | 362 |
| `binary::byte_at` (whole binary / slice) | 294 / 300 | 212 to 293 |
| `string::trim` plus length | 1,725 | |
| `int::from_string`, 8 digits | 2,838 | |
| `binary::to_string` plus length, 8 bytes | 2,485 | |

- **Built-in floor:** `array::length` costs 185 and `binary::byte_size` 161 through the same built-in path, so that is the floor for any built-in that stays on it.
- **Concat sizes:** the concat figures are `string::length(s + t)` minus a `string::length` of the result, and the subtracted length for the 1+1 and 8+8 cases is interpolated from the length row.

**Join and replace are quadratic (measured)**
| Rows of 45 bytes | 25 | 50 | 100 | 200 |
|---|---|---|---|---|
| Fold join | 0.47M | 1.38M | 4.77M | 17.8M |
| `string::replace(text, 'tag=', '#')` | 0.52M | 1.43M | 4.72M | 17.2M |

- Cost grows 2.9x to 3.7x per doubling. The claim's 1.451M and 1.498M per 50-row batch match my 1.38M and 1.43M.
- The split inside replace is only 78K at 50 rows, so the fold is the cost.
- A fit gives about 350 to 415 times k squared, which is about 15 to 18 instructions per accumulator byte copied.

**Benchmark totals (measured)**
- strings is 30.71G, string_keys 28.72G, json 28.10G (93.5M per parse over 300 rounds) and deriv 39.24G.

**Shares (my per-operation numbers times counts read from the source, so partly inferred)**
- **strings:** batch join plus replace is (1.37M + 1.42M) x 2,400 = 6.7G, or 21.8%. Splits are about 20% and small concats about 12%.
- **string_keys:** 8.5M concats at about 950 is about 28%. The eager `''` default in `letters[r % 6] or ''` is 8.5M x 360, or 10.7%; the `dis` output shows it evaluated before `ArrayIndexOr` on every step. String-key excess over Int keys is 3M x about 1,380, or 14%.
- **json:** 38,753 `byte_at` calls x 294 is 11.4M, or 12% of a parse.

**Sampling profile (time, not instructions; leaf frames only)**
- **string_keys:** 309 of 760 samples (41%) are in `read_string`, `Heap::string`, `string_is`, malloc, free, `RawVec` growth and memmove. `read_string` alone is 151 (20%) and the malloc family 73 (9.6%).
- **strings:** 410 of 767 samples (53%) are in the same set. Of those, 81 (10.6%) are memmove and its stub called from `Heap::string` at `heap.rs:388`, which is the per-word `padded_word` pack reached from Concat at `exec.rs:263`.

**Code read**
- `read_string` extends a Vec through `flat_map(to_le_bytes).take(len)`.
- Concat starts from `Vec::new()`.
- `eq.rs` copies one side into a Vec before comparing.
- `utf8_of` is `text_of` followed by `String::from_utf8`.
- `StringSplit` builds a `Vec<String>` and then a heap string per piece.
- `Instr::Str` calls `heap.string` on every execution.

**Not measured**
- I did not measure the claim that each part can be "cut by two thirds or more", or the breakdown of split's fixed cost.
- I did not probe hash caching, binary append at 313 per byte, or deriv's 1.5%.

Scratch files are in `scratch/fat/strcopy`.

**Corrections.** The costs and the named cause stand, but four details of the gain estimate need adjusting.

- **`byte_at` saving is overstated.** The 11% of json is the gross cost of `byte_at`, not what a direct path saves. `binary::byte_size` costs 161 and `array::length` 185 through the same built-in dispatch, against 294 for `byte_at`. A direct path that stays a built-in call therefore saves about 130 per call, roughly 5% of json. More needs a dedicated instruction, which is a call-path fix rather than a copy-out fix.
- **`string::length` has the same floor.** It costs 641 for 8 bytes, of which about 185 is built-in dispatch that reading in place does not remove.
- **Split is mostly fixed cost, not per-piece cost.** A split that finds no delimiter and returns one piece costs 3,619. Each extra piece adds only about 680 to 850. The claim's wording about a Rust String and heap string per piece explains the smaller part. I did not decompose the fixed 3,600, so whether in-place reads remove two thirds of it is unverified.
- **The strings join is the benchmark's own code.** `fn join` is written in `benches/strings/strings.sca`, so a new `string::join` built-in only helps if the benchmark is rewritten to call it. Half of the 20.5% depends on that, or on making Concat append in place when the accumulator is unique. `string::replace` is stdlib and is fixable directly.

The quadratic copy also costs more per byte than the claim's 9: I measured about 15 to 18 per accumulator byte in the fold, against about 11 for a single large concat.

### 7. Nothing is inlined: one-line functions, `result::then` continuations and lambdas passed to map and filter are all real calls and closures

`no-inlining-and-stdlib-layers`, kind: compiler.

**Today.** scarlet_lower has no inliner and no specialisation of a higher-order function on a known lambda. `x <- result::then(f(..))` builds a closure for the rest of the function, calls then, and tail-calls the closure by value, which also keeps accumulators captured so they are never unique. The stdlib builds map and filter on fold with a wrapper lambda, so each element pays two closure calls and a capture read. Small leaves such as `next`, `a`, `find_task`, `get_pile`, `skip_white_space` and `int::abs` pay a full frame each time.

**Where.** crates/scarlet_lower/src/lower.rs and translate.rs (no inlining pass; lambda at translate.rs 556); crates/scarlet_core/src/std/scarlet/array.sca map (6), filter (10), fold (19); crates/scarlet_core/src/std/scarlet/result.sca then

**Benchmarks hit.** json, closures, spectral_norm, map_churn, towers, nbody, richards, bounce, strings, permute, list

**Estimated gain.** json about 33% (the `then` style is 30.8% measured, plus part of skip_white_space's 8%). closures about 20% (fold wrapper 9.6% + lambdas through CallValue 10.4%, both measured by rewrite). spectral_norm about 14% (the call of `a` is 13.9%). map_churn about 12% (inlining `next` by hand saves 275 per call over 8.6M calls). towers about 12% (4 of 7 calls per move are get_pile and set_pile; inferred). nbody about 10% (map plumbing is a 12% remainder). richards about 6%, bounce and strings about 5%, permute and list about 2%. Geomean factor about 0.94 alone.

**Evidence.** json report (ablation on the real benchmark): orig 28,061M, with every `then` chain rewritten as match 20,481M, with capture's too 19,425M; 28.8M of 93.4M per parse. code-quality study: closures_direct.sca 23.84G against 26.39G, closures_spec.sca 21.10G; a do-nothing call costs 223, `inc(x)` 333 against 59 inline; inlining `bounce` into `step` with the pair it returns takes bounce from 31.8G to 23.75G. closures report: map and filter cost about 600 extra per element (derived). closures study: array::map 3,140 per element against 2,624 for a direct recursive map and 2,260 with the operation inline. map_churn report: m_next_small_inline 451 against m_next_small 726.

**Agreement between reports.** Seven benchmark reports and the code-quality and closures studies found it. The stdlib-layer share of the closures benchmark is 9.6% measured (code-quality), about 10% derived (closures report) and about 8% estimated (closures study). The towers and richards figures are inferred from call counts, not measured.

**Fix direction.** Add a Core IR inliner for small non-recursive known callees, and specialise a higher-order function on a literal lambda so `then`, map, filter and fold with a lambda become first-order code with a join point. Until then, write array::map and array::filter as their own loops in the stdlib, and have built-ins such as int::from_string return Result directly so the wrapper goes.

**The sceptic** (held). All figures are instructions retired from /usr/bin/time -l, with startup (39.87M) subtracted where I give a per-step number. Every rewritten benchmark printed the same output as its expected.txt. Scratch files are in scratch/fat/inl.

MEASURED, whole benchmarks rewritten by hand:
- json: the original is 28,089M. With every `x <- result::then(..)` turned into a match by script it is 21,059M, which is 25.0% less. With capture's pipe chain as a match too it is 19,998M, which is 28.8% less. The claim said 27% and 30.8%, on its way to "about 33%".
- closures: the original is 26,385M. With map and filter as their own loops and the lambdas still called by value it is 23,846M (9.6% less). With the literal lambdas made first-order loops it is 21,087M (20.1% less). Both match the claim.
- spectral_norm: 40,013M falls to 34,856M with `a` inlined, 12.9% less against the claimed 13.9%.
- map_churn: 19,744M falls to 17,448M with `next` inlined, 11.6% less against the claimed 12%.
- towers: 38,388M falls to 33,725M with get_pile and set_pile inlined into push_disk and move_top_disk, 12.2% less. The claim's 12% was inferred, and it is now measured.

MEASURED, micro-probes per step, with a bare loop as the base:
- `x + 1` written inline costs 59, and `inc(x)` costs 333, so the call adds 274. A do-nothing `id(x)` costs 223. These are the claim's numbers exactly.
- A lambda with no capture called by value costs 361 more than the inline form, and one with a capture costs 453 more.
- One `x <- result::then(step(n))` costs 1,581 more than the same code as a match (3,649 against 2,068 per step).
- Building and freeing a one-capture closure each step costs 573 more than calling a ready-made one (1,869 against 1,295).
- With an array accumulator appended after the `then`, the gap grows to 3,632 per step (6,287 against 2,655). The captured accumulator therefore costs about 2,050 more per step.

READ: `scarlet dis` shows `closure fn#N(captures)` followed by `ret call fn#3(result, closure)` for each `then`. A grep for "inline" in crates/scarlet_lower/src finds nothing. array.sca builds map and filter on fold with a wrapper lambda, as the claim says.

NOT CHECKED: the nbody, richards, bounce, strings, permute and list figures. The geomean factor over the five benchmarks I measured alone is 0.959. It is about 0.947 if the claim's figures for the unchecked six are right.

**Corrections.** The cost exists, at about the size claimed, and the hand rewrites show the instructions go away instead of moving. Three corrections:

1. json is a little smaller than claimed. I measure 25.0% for the `then` rewrite and 28.8% with capture, not about 33%.

2. The `then` cost is mostly not call overhead. Of the 1,581 per use, about 570 is building and freeing the continuation closure (measured separately). The call of `then` and the call by value account for roughly 330 and 360 by my call probes. The remainder is probably the Ok cell that can no longer be reused, which is a guess. The accumulator part is real and large: about 2,050 more per step when an array is captured and then appended. I read the cause from the IR (the closure holds the array, so the append sees a shared value and copies) and did not profile it.

3. The gain overlaps with other costs. A do-nothing call is 223 instructions and a closure build is about 570, so any work that makes frames or allocation cheaper will shrink what inlining saves. The 0.94 geomean should not be multiplied with those fixes as if they were independent.

The claim's own text is also inconsistent on bounce: it cites 31.8G to 23.75G (25%) from inlining `bounce` into `step`, but estimates about 5%. I did not measure bounce.

### 8. Every heap word is reached through a chunk table with three bounds checks, and handlers repeat the lookup per word

`heap-cell-addressing`, kind: interpreter.

**Today.** A cell is a 20-bit chunk index and a 28-bit word index. Heap::onward (confirmed in source) indexes `chunks`, matches the Option<Chunk>, and slices `words` from the word index; then the header size bounds the field index. That is 10 to 17 instructions and three or four dependent loads where a pointer design does one load. kind, variant, field, share, release, word and set_word each start from scratch, so a Field does the lookup three or four times and a variant test twice.

**Where.** crates/scarlet_vm/src/heap.rs onward (712), onward_mut (719), words (727), word (739), set_word (743), held (685), kind, kind_and_data; crates/scarlet_vm/src/value.rs as_cell (135), view (139)

**Benchmarks hit.** list, deriv, towers, binary_trees, bounce, rbtree, cfold, richards, map_churn, nbody, json, fannkuch, permute, queens, sieve, closures, storage, string_keys, strings, spectral_norm

**Estimated gain.** Share today, measured: list about 22% of the hot step (about 18% of the run), deriv about 17% of the d phase, towers 15.5%, binary_trees 14.2%, bounce 14.1%, storage 6.3%; estimated: cfold 10% to 14%. One lookup per instruction on a flat arena removes about two thirds: list 12%, deriv and towers 10%, binary_trees and bounce 9%, rbtree 8%, cfold and richards 7%, map_churn and nbody 6%, array benchmarks about 5%. Geomean factor about 0.945 alone. It also unlocks most of the alloc and free fast paths.

**Evidence.** binary_trees report (trace by inlined frame): cell lookup 5,148 (9.0%) + word bounds 2,982 (5.2%) of 57,047; Field spends 27.7 of 86 there. bounce: 1,024 of 7,264 per ball-step. list: 120 finding the cell + 90 indexing it of 963. towers: 1,210 of 7,803 per move, three separate lookups in one 96-instruction Field. deriv: heap.rs:713 6.5%, :740 4.2%, :714 3.8% of d-phase instructions. value study: Field of an Int field is exactly 77, about 30 of them heap-side checks; Lean reads a field with one load. maps study: each trie level costs 55 for get and 105 to 115 for an in-place edit, mostly re-finding the cell.

**Agreement between reports.** Six reports measured it by classifying traced instructions (binary_trees, bounce, list, towers, deriv, storage) and about eight more inferred it from disassembly or handler listings (cfold, rbtree, richards, queens, permute, json, value, arrays, maps). Per-lookup cost is quoted as 9 to 10 (bounce), 10 to 15 (list), 11 to 13 (value), 12 to 14 (deriv), about 15 (cfold) and 10 to 17 (binary_trees). The cfold and rbtree shares are estimates, not measurements.

**Fix direction.** Staying in safe Rust: resolve a cell to its word slice once per instruction and take kind, size, tag, fields and count from that slice. Then move small cells into one flat Vec<u64> arena with a 32-bit word index so a lookup is one bounds check, and drop the Option discriminant (an empty chunk for a freed slot). Test 'is a cell' on the raw bits and compare kind bits against a constant.

**The sceptic** (held). Method. I single-stepped the profiling binary under lldb (40,000 consecutive instructions per benchmark, one window each, started 0.4 to 0.5 s into the run), resolved every PC with `atos -i`, and counted instructions whose innermost heap.rs frame is in lines 712 to 747 (onward, onward_mut, words, words_mut, word, set_word). I also counted physical lookups by counting executions of the Option<Chunk> discriminant load (`ldrb w, [x, #0x18]` attributed to heap.rs:713 or :720). The profiling binary retires the same count as release (4.632G vs 4.631G on my probe). Scratch files are in scratch/fat/hca (pctrace.py, classify.py, phys.py, *.trace).

Measured, micro-probe (/usr/bin/time -l, 10M steps): bare loop 382 per step; one Field of an Int field adds 77.0; Field plus IntAdd adds 147. The trace of that Field shows 74 instructions dispatch to dispatch, and I read the machine code: one lookup is 16 instructions and 6 loads (chunks.len, chunks.ptr, the Option discriminant byte, words.len, words.ptr, header), then 7 more for the header-size bound on the field index. So 23 of 77 are addressing (the claim says about 30).

Measured, share of traced instructions in heap.rs 712 to 747: list 16.3%, towers 15.3%, binary_trees 15.9%, bounce 17.9%, deriv 17.9%, storage 12.3%, queens 16.5%, rbtree 17.2%. Claimed: list 22%/18%, towers 15.5%, binary_trees 14.2%, bounce 14.1%, deriv 17%, storage 6.3%. All agree within 1.5x except storage, where my one window is twice the claim (storage has phases, so a single window is weak evidence there).

Measured, cost per physical lookup including its word bounds check: 14.6 to 15.5 instructions in every benchmark (16.0 in the Field probe). This is inside the claimed 10 to 17.

Measured, physical lookups per heap-touching VM instruction: list 1.19, deriv 1.44, rbtree 1.60, binary_trees 1.73, towers 2.46, queens 3.16, bounce 3.56, storage 4.76. Per VM instruction overall it is 0.9 to 1.7.

Limits: one 40k-instruction window per benchmark, so each share carries sampling error from where the window landed. I did not measure time, only instructions. I did not test any fix.

**Corrections.** The cost exists and is the size claimed, but one part of the named cause is wrong and the gain is somewhat overstated.

1. "A Field does the lookup three or four times and a variant test twice" is true of the source, not of the machine code. LLVM already merges the kind check and the field read into one lookup: the Field handler at 0x1002aa064 to 0x1002aa0d8 loads the chunk once and reuses the header pointer and remaining length for the field. In list, 301 of 368 heap-touching VM instructions do exactly one physical lookup. The extra lookups I saw there are for other cells: share of the field value just read (heap.rs:269, 78 times), release (283, 83 times). Those cannot be merged by "resolve once per instruction". Real repetition on the same cell exists, but in the alloc, fill and free paths and in array and map operations (callers heap.rs:209, 499, 500, 294, 303, 557), which is why towers, bounce, queens and storage show 2.5 to 4.8 lookups per touching instruction, with single instructions doing 9 to 13.

2. So for list, deriv, rbtree and binary_trees the gain has to come almost entirely from making one lookup cheaper, not from doing fewer. Inferred, not measured: a flat Vec<u64> arena still needs mask, length load, compare, branch, pointer load, header load (about 6), plus a field-index bound against the header size (3 to 7 instructions, which stays in safe Rust). That takes a lookup from about 15 to about 7 to 9, so it removes about half of the addressing cost, not two thirds. My estimate for list is about 8% rather than 12%, and a geomean factor nearer 0.955 than 0.945. Where lookups repeat on one cell (towers, bounce, storage, queens) two thirds is plausible.

3. The cost does not just move: the per-lookup saving is real because the chunk-table bounds check, the Option discriminant test and the slice-from-word check all disappear. But big cells that keep their own chunk would still need the old path or a tag test to choose, which adds a branch the claim does not count (a guess, no number).

4. Minor: Field of an Int field has 23 addressing instructions by my count of the machine code, not about 30, and one lookup costs 6 loads rather than three or four.

### 9. Each register read costs 7 to 9 instructions and each write 13, with a release test on every write

`register-access-checks`, kind: interpreter.

**Today.** `at(base, r)` does a saturating add, then get, set and take bounds-check against regs.len reloaded from the Machine, reload the regs pointer and reload `base` from a stack slot. A write also loads the old value and tests whether it was a cell to release, even when the destination's type is Int, Float or Bool. The pattern recurs inside enter_above, call_args and again for every argument.

**Where.** crates/scarlet_vm/src/exec.rs Machine::get (2411), set (2417), take (2426), at (2660)

**Benchmarks hit.** tak, fib, binary_trees, towers, list, spectral_norm, bounce, deriv, mandelbrot, cfold, rbtree, closures, map_churn, nbody, json, richards, storage

**Estimated gain.** Share today, measured: binary_trees 12.5%, fib 12.7% (arithmetic accesses only), towers 11.4%, list 10% of the step, spectral_norm about 10%, bounce 8.9%, about 10% in deriv, 17% of the bare loop; tak about 25% (inferred by hand count). A per-frame slice plus skipping the release test on cell-free destinations removes about 60%: tak about 12%, fib 8%, binary_trees 7.5%, towers 7%, 4% to 6% on most others. Geomean factor about 0.95 alone. Much of this share is also counted inside the call and tail-call items.

**Evidence.** interpreter-loop study: 4 reads cost 33 and 2 writes 26 of 346; a write is 7 to index and check plus 6 to store and test the old value. fib report: reads 25.5 and writes 19.5 per call, listing shows `adds; csinv; cmp; b.hs; ldr`. binary_trees: 7,150 of 57,047. towers: 890 of 7,803 per move. bounce: 644 of 7,264. LuaJIT reads a slot with one `ldr` off a pinned BASE.

**Agreement between reports.** Eleven reports found it (interpreter-loop, calls, value, fib, tak, binary_trees, bounce, list, towers, spectral_norm, deriv) and agree on 7 to 9 per read and 12 to 14 per write; nbody quotes about 6 and 11. The tak share (about a quarter) is a hand count the tak report marks as inferred.

**Fix direction.** Take the frame as one slice (`&mut regs[base..base+n]`) when a function is entered and have the loader verify every Reg is below body.regs, so an access is one compare against a length already in a machine register, with no saturating add. Let the loader mark destinations that can never hold a cell, from the typed Core IR, so those writes are plain stores.

**The sceptic** (held). Scratch files are in scratch/fat/regchk (add*.sca, imm*.sca, arg*.sca, call.s).

MEASURED with /usr/bin/time -l, startup 38.9M subtracted:
- Bare loop `go(n - 1, acc + n)` costs 346 instructions per step (10M steps, 3,500,031,398 total).
- One extra reg-reg `IntAdd` (2 reads, 1 write) costs 70.0 instructions. The marginal cost is the same across chains of 2, 4, 8, 16 and 32 adds (1011.8M, 1292.6M, 1852.7M, 2973.0M and 5214.7M at 2M steps).
- One extra `x + 3` (1 read, 1 write) costs 59.0 instructions (imm4 1205.2M, imm20 3093.8M).
- One extra argument passed unchanged through the self tail call costs 36.3 instructions (arg1 803.6M, arg5 1093.8M).
- Values must stay below 2^47. My first probe overflowed into bignums and cost about 450 per add, so any probe summing past 1.4e14 is measuring bigint code.

MEASURED by reading the machine code of Machine::call in target/profiling/scarlet (objdump, offsets from the symbol at 0x1002a958c). The reg-reg Int add path is 71 instructions, against 70.0 from the probe:
- Dispatch is 15 instructions (+176 to +232).
- Read 1 is 8 instructions (+4844 to +4872): `ldr w10 operand; ldp regs ptr,len from [x21,#0xf0]; ldr base from [sp,#0xd0]; adds; csinv; cmp; b.hs; ldr value`.
- Read 2 is 7 instructions (+4876 to +4896, then +9056). It reloads base from the stack again but reuses ptr and len.
- Tag checks and the op sub-dispatch are 19 instructions; the range check and retag are 8.
- The write is 14 instructions (+14852 to +14880, then +17104 to +17124): `ldr len; ldr dst operand; ldr base; adds; csinv; cmp; b.hs; b; ldr regs ptr; ldr old; str new; asr #48; cmn #4; b.ne`. That is 8 to index and check, and 6 to store and test the old value for a cell.
- So a read is 7 to 8 and a write is 14. Together they are 29 of the 70 (41%) in a pure arithmetic instruction.
- The `adds; csinv` pattern appears 100 times in Machine::call.
- The 11-instruction gap between `IntAdd` (70) and the immediate form (59) is one 7-instruction read plus a 4-instruction second tag check, which agrees.

INFERRED, not measured:
- The bare loop does about 6 register reads and 2 register writes per step, which is roughly 73 of 346 (21%). The claim says 17%.
- I did not re-measure the per-benchmark shares (fib 12.7%, binary_trees 12.5% and the rest).
- A realistic saving in the Int add is 11 to 15 of the 29 access instructions, so 38% to 52%, not 60%. Reads would drop from 7.5 to 4 if the slice ptr and len stay in machine registers, or to 6 if they are spilled the way `base` is today. Writes would drop from 14 to between 4 and 6.

**Corrections.** The cost and its cause are as claimed. The second half of the fix direction is wrong as stated, and the 60% removal figure is optimistic.

1. The release test on write cannot be dropped by the destination's type.
- The loader's `allocate` (crates/scarlet_vm/src/code.rs, about line 2115) colours registers by liveness, so one machine register holds values of different types over a frame.
- Its doc comment says the scheme depends on the test: "the value a register held is released when the next one is written over it". An Int destination can be overwriting a dead cell, and skipping the test would leak it.
- Int is not cell-free either. Anything past 2^47 is a bigint cell (crates/scarlet_vm/src/value.rs, SMALL_INT_MAX).
- The fact that would allow a plain store is "this register is empty or holds a non-cell at this write". That has to come from liveness after allocation, or from the allocator not sharing cell registers with scalar ones, or from explicit Drops. That is new loader work, and it moves a release to wherever the old cell really dies. Typed Core IR alone does not give it.

2. The per-frame slice only pays off if its ptr and len stay in machine registers.
- `base` already lives in a stack slot ([sp,#0xd0]) and is reloaded on every access, because Machine::call is short of registers. A slice ptr and len may be spilled the same way.
- In safe Rust a `&mut regs[base..end]` borrow cannot be held across `&mut self` calls such as int_op, call_args, enter_above or again. It has to be re-sliced after each one, with a bounds check each time, so part of the cost moves there.
- The saturating add (`adds; csinv`, 2 instructions per access) does go away if the loader verifies every Reg is below body.regs.

3. Expect about 40% to 50% of the access cost to go, not 60% (inferred from the instruction listing). The claim itself notes that the per-benchmark shares overlap with the call and tail-call items; I did not re-measure them.

### 10. A Float instruction costs 92 to 97 instructions through three out-of-line calls, and floats have no immediate or fused compare forms

`float-ops-out-of-line`, kind: interpreter. Depends on: dispatch-and-instr-format.

**Today.** Instr::Float calls `Machine::float` (marked #[inline(never)]) once per operand, each returning Result<f64, Stop> through memory, then the out-of-line `float::op`, which picks the operator again and runs Value::float's NaN, +inf and -inf clamp as three compare-and-branch pairs. A float compare builds a Bool that a separate JumpIfFalse decodes, because take_comparison only fuses Int. Float constants are a separate Const each time. float::from_int goes through the generic Machine::builtin at 137.

**Where.** crates/scarlet_vm/src/exec.rs Instr::Float (201-204), Machine::float (2479), Instr::JumpIfFalse (704), Intrinsic::FloatFromInt (1496); crates/scarlet_vm/src/float.rs op; crates/scarlet_vm/src/value.rs Value::float (83); crates/scarlet_vm/src/code.rs Loader::immediate (1264), take_comparison (1307)

**Benchmarks hit.** mandelbrot, nbody, spectral_norm

**Estimated gain.** mandelbrot about 49% (eight float ops are 51.8%, compare plus branch 9.9%, two constants 4.4%; a 25-instruction float op, a fused compare and immediates remove most). nbody about 24% (Float is 29%, plus about 6% for the sqrt loop's constant and compare). spectral_norm about 15% (three float ops 14.0%, FloatFromInt 6.9%, constants 3.1%). Geomean factor about 0.95 alone, from three benchmarks.

**Evidence.** mandelbrot report: probes at 1, 2 and 5 adds give 92.0 per add and 93 per multiply; disassembly shows `bl Machine::float` twice and `bl float::op`; Float Gt + JumpIfFalse is 140 against 40 for the fused Int form; Const 31. nbody report: 93 per op, 716 per step, 66.6k of 226.4k; float::op and Machine::float are 19% of time samples. spectral_norm report (trace): FloatDiv 95, FloatMul 93, FloatAdd 92, FloatFromInt 137. value study: 44 to fetch the operands, 22 in float::op including 11 of clamp, 11 to store, 15 dispatch. LuaJIT measured at 18 to 21 per float add (nbody, mandelbrot, spectral_norm reports).

**Agreement between reports.** Three benchmark reports and the value and code-quality studies agree on 92 to 97 per op. The mandelbrot report counts eight float ops at 51.8% and 1,362 per iteration; the code-quality study counts nine Float instructions at about 60% and 1,384 per iteration. A float constant load is 31 (mandelbrot) or 39 (code-quality), and the unfused compare costs 100 extra (mandelbrot) or about 90 (code-quality).

**Fix direction.** Give each operator its own instruction and inline the operand read as one compare with a cold error path; the checker already types these as Float. Replace the three-way clamp with one exponent-all-ones test on the result. Add FloatImm, a fused float compare-and-jump (also through `&&` and `||` joins), and a FloatFromInt instruction.

**The sceptic** (held). Measured with my own probes: 10M-step tail-recursive loops, /usr/bin/time -l, startup 38.84M subtracted, bare loop 340.3 per step. Files are in scratch/fat/floatref.

Per Float instruction (operands in registers):
- FloatAdd costs 92.0. Probes with 1, 2 and 5 adds give 432.29, 524.33 and 800.46 per step, so the cost is linear.
- FloatSub costs 92.0, FloatMul 93.0 (two muls 186.1), FloatDiv 95.0.
- FloatNeg costs 65.0.

Other parts of the claim:
- A Float constant costs 39.0 as the first one and 35.0 on average over two (claim says 31 or 39).
- Float Gt plus JumpIfFalse costs 140.0 when the branch is not taken and 144.0 when taken. My fused Int compare-and-jump probe costs 53.0 (claim says 40). The unfused penalty is therefore about 87 to 91.
- float::from_int costs 137.0, exactly as claimed.
- A mandelbrot `escape` iteration, isolated on a point that never escapes, costs 1363.8 (claim says 1,362). The eight arithmetic float ops are 4x92 + 4x93 = 740, which is 54% of that.

Read from the profiling binary's disassembly:
- The Instr::Float arm (0x1002a9f0c to 0x1002a9f9c in Machine::call) has `bl Machine::float` twice and `bl float::op` once.
- Machine::float's happy path is 11 instructions. It returns through memory (`stp x9, x8, [x0]`), and the caller reloads both words and tests the tag.
- float::op's Add path is 19 instructions: 6 re-pick the operator, 1 is the fadd, and about 11 are the clamp (a NaN fcmp and branch, then two materialise-infinity, fcmp, branch groups).
- The caller's own glue is about 45 instructions: argument setup, result reload and check, the dst bounds check, the store, and a check of the old value's tag.
- That is about 75 in the arm. The remaining 17 or so I attribute to dispatch by subtraction.

Source reading confirms take_comparison matches only Instr::Int and Instr::IntImm, and Loader::immediate accepts only Const::Int.

Not measured by me: the LuaJIT figure of 18 to 21, the nbody and spectral_norm shares, and whether FloatFromInt goes through Machine::builtin (I only saw the arm at exec.rs:1496).

**Corrections.** The cost and the cause are as stated. The estimated gain is too high, because it assumes a 25-instruction float op.

An Int add is already fully inline, with no calls, and it measures 70.0 per op in the same loop (iadd minus ibare). FloatNeg, with one operand and the clamp inlined, measures 65. So removing the three calls does not get near 25 on its own.

My estimate, inferred from the disassembly and not measured: the calls, the Result-through-memory round trip, the operator re-pick and two thirds of the clamp account for about 35 to 45 of the 92. The rest stays: about 17 of dispatch, three bounds-checked register accesses with saturating base+reg adds, and the old-value tag check on store. An inlined Float op in today's instruction format would land at roughly 45 to 55.

Reaching 25 needs the dispatch-and-instr-format dependency and cheaper register access, which are separate work. Float alone would then give mandelbrot roughly 35% (8 ops x about 42 saved, plus about 90 for a fused compare, plus about 70 for two constants, out of 1,364), not 49%.

One smaller point: the fused Int compare measures 53 in my probe, not 40, so the unfused penalty is about 87 to 91, not 100.

### 11. A self tail call copies its arguments out to a side Vec, clears the whole frame, and copies them back: 177 instructions for two arguments

`self-tail-call-buffer-and-sweep`, kind: interpreter. Depends on: register-access-checks.

**Today.** TailCallSelf calls the out-of-line call_args, which takes the `passing` Vec out of the Machine and pushes each argument with a capacity check and a moved-flag test (about 19 each). `again` then stores Nil into every register of the frame with a cell test on each (8 each) and copies each parameter back with a bounds check (9 each); `passed` returns the Vec. Arguments that do not change are moved all the same. A tail call to another function goes through `enter` and costs about 58 more.

**Where.** crates/scarlet_vm/src/exec.rs Instr::TailCallSelf (373-378), TailCall (363), call_args (2268), again (2375), passed (2284), enter (2337)

**Benchmarks hit.** mandelbrot, spectral_norm, tak, list, storage, nbody, map_churn, string_keys, closures, json, towers, richards, fannkuch, strings, bounce, queens, rbtree

**Estimated gain.** Share today: mandelbrot 21.8% (313 with six arguments), spectral_norm 14.7%, tak 14.6%, list 14.6%, storage 10.7%, map_churn 10.5% for the loop skeleton, nbody about 13% for sqrt's loop control, closures 6.1%, 47% of the bare loop. Parallel moves with no buffer and no sweep remove about three quarters: mandelbrot 16%, spectral_norm 11%, tak and list 10%, storage 8%, nbody 6%, map_churn and string_keys about 5%, 2% to 4% on eight others. Geomean factor about 0.95 alone.

**Evidence.** interpreter-loop study (trace, period exactly 346): TailCallSelf 162 + 15 dispatch; call_args 75, clear 8 per register, parameter copy 9 each; a third argument adds 36. tak report: every TailCallSelf exactly 221, 94 inside call_args. mandelbrot report: skeleton with 1, 2, 3 and 6 arguments is 268, 304 and 412 per step, 36 per argument. spectral_norm: 293 (call_args 132, again 129). list: 177 (call_args 69, again 61). nbody: a sqrt_from turn is 627, of which loop control 255, against about 114 per turn in LuaJIT. string_keys: 631 in `spell`, 387 of it freeing two string cells.

**Agreement between reports.** Fourteen reports found it. The two-argument cost is 177 (list, calls, loop study) or 185 (storage, closures study), depending on frame size. The per-argument cost is 36 in five reports but 28 in map_churn. The code-quality study adds that unchanged arguments still pay the 36.

**Fix direction.** Have the loader compile a self tail call to a parallel move into the parameter registers, ordered so nothing is clobbered and with one scratch for cycles, skipping arguments already in place, then a jump to pc 0. Release only the registers liveness says can hold a cell at that point. Do the same for a tail call to a known function.

**The sceptic** (held). The cost exists, at the size claimed and for the cause named. The benchmark shares and the size of the gain I did not measure.

MEASURED with /usr/bin/time -l (slope between N = 10M and 30M, release binary), in instructions per loop step:
- `go(n-1)`: 232.0
- `go(n-1, a)` with `a` unchanged: 268.1
- Three arguments, two unchanged: 304.1
- Four arguments: 340.1
- Six arguments: 412.1
- So each extra argument costs exactly 36, even when it does not change.
- `go(n-1, acc+1)`: 327.1, and 363.1 with a third unchanged argument.
- Mutual tail call `ping(n-1)` / `pong(n-1)`: 284.1, against 232.0 for the self call. A tail call to another function costs 52 more (the claim says about 58).

MEASURED by single-stepping the profiling binary in lldb over one exact loop period, with each pc resolved by atos:
- `go(n-1, acc+n)` has period 346, as the claim says. TailCallSelf is 162 plus 15 dispatch, which is 177. Inside it, call_args is 75, `again` (exec.rs:375) is 73, `passed` is 7, and 7 are glue.
- `go(n-1, acc+1)` has period 327. The frame is one register smaller, so `again` is 65 and TailCallSelf is 154 + 15 = 169. That is 52% of the step.
- One argument: call_args 56, `again` 48, total 118 + 15 = 133 of 232 (57%).
- Three arguments: call_args 94, `again` 82.
- Six arguments: call_args 151, `again` 133, total 298 + 15 = 313 of 412. This matches the claim's "313 with six arguments".
- Per argument, call_args adds 19 and `again` adds 17, which gives the 36. The 17 fits 8 to clear and 9 to copy back. One extra register that is not a parameter added exactly 8.
- Mutual tail call: 35 inline, call_args 56, and the out-of-line `enter` 75, which is 166 against 118 for the self call.

READ in exec.rs: call_args does take `self.passing`, push each value and get called out of line. `again` stores Nil into every register from base to end with a cell test on each, then copies the parameters back. Nothing skips an argument that is already in place.

NOT MEASURED: the per-benchmark shares (mandelbrot 21.8%, tak 14.6% and the rest) and the tak figure of 221.

INFERRED about the fix: a parallel move with no buffer and no sweep would leave about 15 for dispatch plus roughly 12 to 20 per changed argument, and nothing for unchanged ones. That is about 45 to 55 against 169 for two arguments, and about 75 against 313 for mandelbrot's six, so "about three quarters" is plausible. I did not test it. Two things could move cost instead of removing it:
- Each move still pays the checked get and set (bounds check plus the cell test on the old value) unless the register-access work lands first.
- Dropping the sweep is only safe if Perceus or liveness already releases every dead cell before the tail call. Otherwise the releases come back as explicit drops in loops that hold cells.

One detail for whoever acts on this: in these probes the loader had already fused const + compare + branch and const + add, so the two-argument step is only 4 VM instructions. The tail call is the largest single one.

### 12. Record updates copy every field into a new cell, and Perceus reuse rarely fires and saves little when it does

`reuse-and-record-update`, kind: compiler. Depends on: refcount-traffic.

**Today.** `R(..r, a: x)` reads every kept field (sharing each cell), frees the old record (un-sharing them) and builds a new cell. A drop only becomes a reuse token when the local's shape was proved by a constructor or pattern in the same arm, so a parameter of a one-constructor record type never is one; nested patterns lose the shape and every `letc` continuation starts with no tokens. When reuse does fire, DropReuse hollows the cell field by field, fits_ctor re-checks it and the constructor rewrites every field.

**Where.** crates/scarlet_lower/src/perceus.rs Perceus::shape, drop_match, record_pat, wrap_drops, ReuseWalk::pair (LetCont and Goto arms); crates/scarlet_lower/src/lower.rs record update lowering; crates/scarlet_vm/src/heap.rs hollow (323), fits_ctor (344), ctor_in; crates/scarlet_vm/src/exec.rs Instr::DropReuse (306), Machine::again (carried scan)

**Benchmarks hit.** towers, richards, permute, bounce, nbody, rbtree, cfold

**Estimated gain.** towers about 38% (three record copies per move are 46.5%; an in-place field write is about 150 against about 1,170). richards about 18% (Task copy 16.8%, Scheduler copy 9.6%, Packet copy 3.1%; the Task is shared with the map, so only part can be in place and the rest needs a block-copy update). permute about 9% (State rebuilds 11.3%). bounce about 8% (Ball 16.5%, never unique today). nbody about 5% (25 Bodies per step, 8.4%). rbtree about 3% (23 missed reuses per insert at 185). cfold about 1.3%. Geomean factor about 0.96 alone.

**Evidence.** towers report (trace): one update is 4 Field reads about 346 + a freeing Drop 405 to 605 + Ctor 341; probe r1.sca with reuse costs 1,794 per step against 1,742 without, so reuse saves nothing today. richards report: 956,437 cells made and 0 reused per iteration; Task copy 1,604 shared and 1,720 unique, 143,209 per iteration; Scheduler copy 968. refcounting study: rbtree makes 9,932,644 cells and reuses 599,974 for 300,000 inserts; 190 drop sites, 5 with a sized shape; a missed reuse costs 185. code-quality study: changing one field of a unique 7-field record costs 1,297. value study: Ctor over a reused cell 206 against 252 fresh, plus 114 for the DropReuse.

**Agreement between reports.** Seven benchmark reports and the refcounting, value and code-quality studies found it. Reports disagree on whether reuse pays today: the refcounting study prices a missed reuse at 185, the value study shows reuse costing 206 + 114 against 252 for a fresh Ctor (the free is extra), and towers measured reuse 3% worse. The cfold counts also disagree: the cfold report has 13.63M fresh and 14.34M reused constructors, the refcounting study 11.6M made and 16.3M reused.

**Fix direction.** Take a single-constructor record's shape from its type so a dropped parameter can be a token, record the proven shape on the scrutinee so nested arms keep it, and pass tokens into a continuation when every goto holds the same one. Add reuse specialisation so an update of a unique record writes only the changed fields, and a block-copy record update for the shared case. Make DropReuse a uniqueness test that leaves fields in place.

**The sceptic** (held). All numbers are instructions retired from /usr/bin/time -l, with startup (38,855,653) subtracted, over 5M-step tail-recursive probes. The probes are in scratch/fat/reuse.

MEASURED, cost of one record update `R(..r, f: x)` on a unique record that arrives as a parameter:
- The bare loop carrying the record costs 268 per step.
- A 5-field record shaped like Towers (3 pointer fields, 2 Ints) costs 1,549 per step, so 1,281 per update. The claim says about 1,170.
- A 5-field all-Int record costs 1,283 per step, so 1,015 per update.
- A 2-field Int record costs 600 per update.
- A 7-field record (4 pointers, 3 Ints) costs 1,647 per update. The claim says 1,297 for 7 fields, so mine is 1.27 times higher.
- A shared record (a second reference is kept) costs 1,256 per update, including the drop of the result.
- Going through a helper like set_pile adds 203 for the call.

MEASURED, where the cost sits:
- A Field read costs about 77 (4 reads plus 3 adds against 3 adds is 831 against 522). The claim's 346 for 4 reads compares with my 309.
- A fresh 5-field Ctor plus the freeing Drop, with no reads, costs 664.
- Each extra kept Int field costs about 138 (from the 2-field and 5-field probes). A pointer field costs about 89 more, for the share and un-share.
- So a 5-field Int update splits into about 550 of per-field copying and about 465 fixed (free a cell, allocate a cell, dispatch).

MEASURED, the IR and the code:
- `scarlet dis` shows 4 Field reads, then `drop %0` with no shape, then a `ctor` with no `reuse`, for a parameter of a one-constructor record type. This holds in my probes and in towers set_pile, push_disk and move_top_disk.
- perceus.rs record_pat gives nested field binders the shape None. ReuseWalk::pair starts each LetCont continuation with an empty token stack and returns at Goto.
- heap.rs hollow clears and releases each field one at a time, fits_ctor re-reads the header, and write_ctor rewrites every field.

MEASURED, does reuse pay:
- Using internal::cells_made and cells_reused I confirmed reuse really fired in the probes below (5,000,000 reused, 2 to 5 made).
- Pattern form `match t { I(a,b,c,_,e) -> go(n-1, I(a,b,c,n,e)) }` with reuse costs 1,356 per step against 1,283 for the Field form without reuse. With pointer fields it is 1,658 against 1,549. So the form that gets reuse today is 6 to 7% slower overall.
- Like for like (Field reads in both, two chained updates where the second gets `reuse %8`): the reused update costs 904 against 1,015 fresh. Reuse saves about 111, or 11%.

MEASURED, benchmark counts (cells made / reused):
- towers 19,675,801 / 0
- richards 23,910,909 / 0, which is 956,436 per iteration
- bounce 26,297,285 / 0
- nbody 8,900,072 / 0
- permute 19,019,202 / 2,016,000
- rbtree 9,932,645 / 599,974
- cfold 11,640,381 / 16,327,519. This matches the refcounting study, not the cfold report.

Towers share:
- Towers runs 38.35G instructions.
- It makes 600 x (3 x 8,191 + 14) = 14.75M Towers copies (counted from the source, and consistent with the 19.68M cells made).
- At my 1,281 each that is 18.9G, or 49% of the run. The claim says 46.5%.

INFERRED, not measured:
- An in-place field write does not exist yet, so its cost is a guess. If it costs 150 to 250, towers saves 15.2G to 16.7G, or about 40 to 43%. That is consistent with the claimed 38%.
- The Towers value is unique at every update. I read this from the IR and the moves_on_last_use rule: every call passes it at its last use.
- I did not re-measure the per-benchmark shares for richards, permute, bounce, nbody, rbtree or cfold, only their cell counts.

**Corrections.** The cost exists, at about the claimed size, and for the mechanism named. Four points need tightening.

1. "Copies every field" is only about half of it. For a 5-field Int record, about 550 of the 1,015 is per-field work (77 per Field read plus about 60 per field for the constructor). About 465 is the fixed cost of freeing one cell and allocating another. An in-place update removes both halves, so the gain stands. But the fixed half overlaps with any separate fix that makes alloc, free and Drop cheaper. The gains should not be added.

2. Giving a parameter drop a shape, with no other change, wins almost nothing. Like for like, reuse saves about 111 per update (11%). The pattern-match form that gets reuse today is slower overall than the `..r` form without it, by 73 to 109. Nearly all of the estimated gain depends on the new reuse specialisation (write only the changed field) and on DropReuse becoming a plain uniqueness test. The claimed 150 for an in-place write is an estimate. Nothing in the VM does that today, so it cannot be measured.

3. The reports that disagree on reuse are both partly right. It is 6 to 7% worse when comparing the pattern form against the Field form, as towers saw. It is about 11% better when comparing the same form with and without reuse. Mine is lower than the refcounting study's 185 per missed reuse.

4. Bounce does not belong under record update. Its source builds `Ball(x:, y:, x_vel:, y_vel:)` fresh from a ball taken out of an array pattern. The ball is shared with the array, so a shape-from-type token would not make it unique. That gain needs ownership out of array destructuring, which is a different fix. Balls are also only 5.0M of bounce's 26.3M cells. The tuples returned from `bounce` are the larger part.

The 7-field figure is also low: I measure 1,647 for 4 pointers and 3 Ints against the claimed 1,297.

### 13. Walking an array with `[h, ..t]` costs 770 to 1,050 instructions per element, and over 2,000 on arrays of 32 or fewer

`array-uncons-walk`, kind: stdlib-or-builtin. Depends on: array-push-not-in-place.

**Today.** A `[] / [h, ..t]` match lowers to ArrayLen and compare with 0, a jump, ArrayLen again and compare with 1 (which cannot fail), ArrayElem(0), a Const 1 and ArrayDrop. Each array instruction calls the non-inlined Machine::seq (about 30) and re-reads the six root words through checked lookups. Arrays of up to 32 elements keep everything in the tail leaf with no head, and the cheap offset path only exists for a head leaf, so each step copies the remaining leaf and makes a new root. Every 32nd step on a tree rebuilds branches and size tables for 5,900 to 7,300. The IR also emits ArrayDrop followed by a drop, so the owned path is not always tried.

**Where.** crates/scarlet_lower/src/lower.rs array pattern lowering; crates/scarlet_vm/src/exec.rs Instr::ArrayLen (562), ArrayElem (567), ArrayDrop (574), Machine::seq (2141), element (2150); crates/scarlet_vm/src/array.rs skip (752-806), skip_owned (812), tree_drop, branch, from_values; crates/scarlet_core/src/std/scarlet/array.sca fold (19)

**Benchmarks hit.** closures, strings, permute, queens, nbody, richards, bounce, fannkuch

**Estimated gain.** closures about 17% (ArrayDrop 9.5% + ArrayLen 5.9% + ArrayElem 5.3% and their compares, about 24%). strings about 15% (18.5% today, 2,090 per element on 4-element arrays). permute about 18% and queens about 10% (inside `set`, so they overlap no-array-set). nbody about 9% (walk and rebuild is 18.5%). richards about 8%, bounce about 7% (ArrayElem + ArrayDrop + two ArrayLen are 8.7%). Geomean factor about 0.96 alone.

**Evidence.** closures report: ArrayDrop averages 357 (199 in a leaf, 5,900 to 7,300 once per 32), ArrayLen 111 twice per element, ArrayElem 199. arrays study: 1,045 per element inside a leaf of a 5,000-element array against 2,456 on an 8-element array (ArrayDrop 1,356 per step); a leaf-edge skip stepped at 7,112. strings report: 4-element walk 2,090 per element, 32-element tail-only 3,650, 33-element 830. code-quality study: writing `[h, ..t]` first saves 165 per element (1,237 against 1,071). permute: the two-test match is 577, a drop on a shared array 644, on a tail-only array 1,665. nbody: tail-held walk 2,400 to 2,640 per element against 1,158 head-held.

**Agreement between reports.** Eight benchmark reports and the arrays, closures and code-quality studies found the doubled length test and the tail-only copy. ArrayLen is quoted as 85 (arrays study), 102 (queens), 107 to 112 (closures, bounce) and 170 to 190 (richards, nbody, whose probes include the compare).

**Fix direction.** Lower a `[] / [h, ..t]` match to one length read and one branch, or to a single ArrayUncons instruction that tests, reads the head and advances. Make the offset a property of the whole array so dropping from a tail-only array is an offset bump, in place when unique. Put small literals in the head or use one flat cell. Give fold, map, filter and each a built-in or index-based loop that walks leaf by leaf.

### 14. Reads of owned values pay a dup and a drop: there is no borrow inference and no drop specialisation

`refcount-traffic`, kind: compiler.

**Today.** The IR has no dup instruction and every parameter is owned. The VM shares a cell whenever a Field reads it or a non-moved argument passes it, and Perceus emits a Drop to give the reference back. A match on a unique cell shares each child, then the drop of the parent walks the fields again and un-shares them. A release that is not the last reference is an out-of-line call of about 47 to 51 instructions; a share is 19 inline.

**Where.** crates/scarlet_lower/src/perceus.rs perceus() (params owned on entry), drop_match; crates/scarlet_vm/src/exec.rs Instr::Field (share at 480), Instr::Drop (305), Instr::DropReuse (306), enter_above (2326-2331); crates/scarlet_vm/src/heap.rs share (268), release (281), hollow (323)

**Benchmarks hit.** list, cfold, rbtree, towers, deriv, binary_trees, richards, closures, nbody

**Estimated gain.** list about 15% (a share at 19 and a live Drop at 83 per cell walked, 204 of 963 per step). cfold about 15% (two Fields at 96 and a DropReuse at 218 are 410 of the 1,021-instruction hot arm; Field + DropReuse are 26.7% of the run). rbtree about 9% (dup and release pairs 8%, dups undone inside a free about 3%). towers about 9% (count logic is 13.8%). deriv about 8% (the `count` walk's traffic is about 8.6%). binary_trees about 7%. richards and closures about 5%. Geomean factor about 0.97 alone.

**Evidence.** refcounting study: one dup plus one non-last release costs 72 on top of 35 for the Drop instruction; list runs 45,105 dups and 52,973 Drops per benchmark() call (about 23% with no-op drops); rbtree runs 209.6 dups and 444 Drops per insert. list report (trace): Drop of a live cell 83, Field with share 96 against 77. cfold report: append_add's arm is 96 + 96 + 218 = 410 of 1,021, and a probe of that arm measures 1,022. towers: Heap::release is 16.85% of all traced instructions. value study: non-freeing release 51, of which 21 are register saves. calls study: about 75 per shared heap argument per call.

**Agreement between reports.** Seven benchmark reports and the refcounting, value and calls studies found it. Several shares are marked inferred by their authors: binary_trees 3.1% (report) against about 11% avoidable (refcounting study), deriv's 8.6%, and cfold's '410 to about 40' on the unique path. The refcounting study's total for rbtree (27%) includes Drop dispatch and missed reuse, which other items here count separately.

**Fix direction.** Infer borrowed parameters as Lean does (borrowed unless stored, returned, reused or passed to an owned position) and treat pattern-bound fields as borrowed while the scrutinee is alive. Emit drop specialisation at a match that consumes its scrutinee: if unique, move the fields out and free or park the shell with no count changes; otherwise share the fields and decrement. Inline the count-above-one path of release.

### 15. The compiler emits Field reads for fields nothing uses and Drops that can never free

`dead-field-binds-and-noop-drops`, kind: compiler.

**Today.** A constructor pattern reads every field into a register, including `_` and `..` fields, and then drops them; each read of a cell field also shares it. Drops are emitted by type through `is_heap`, which (confirmed in source) answers true for every non-primitive constructor type, so Bool, Nil and field-less enums get Drops. A Drop is also kept after a call that already moved the value out. Each such Drop is a full dispatched instruction of 30 to 38. The language also rejects `_` arms and some flat patterns over sum types, so programs cannot write around it.

**Where.** crates/scarlet_vm/src/code.rs Loader::pattern (CorePat::Ctor, about line 1543), moves_on_last_use (2052); crates/scarlet_ir/src/rty.rs ResolvedPool::is_heap (254); crates/scarlet_lower/src/perceus.rs wrap_drops, owns_cell

**Benchmarks hit.** list, binary_trees, rbtree, deriv, cfold, storage, bounce, towers, closures, richards, json

**Estimated gain.** list about 14% (two dead Field reads per step are 12.7%, no-op drops about 1.3%). binary_trees about 11.6% (dead `Node(..)` reads and their drops 7.4%, no-op drops 4.2%). rbtree about 10% (264 no-op drops per insert are 7.4%, plus dead binds). deriv about 9% (dead reads and drops in or-pattern chains). cfold about 7% (no-op drops 3.4%, dead binders and their drops about 3.9%). storage 2.5%, bounce 2.4%, towers 1.5%. Geomean factor about 0.97 alone.

**Evidence.** binary_trees report (trace): 79 of 124 Drops per iteration do nothing at 30 each; `scarlet dis` shows `| 10.1(%3, %4) -> drop %3; drop %4` for `Node(..)`. list report: probe walk_e minus walk1 is exactly 77 per dead field. cfold report: 48.7M Drops at exactly 30 = 1.46B. refcounting study: rbtree runs 443.7 Drops per insert, 30% of all VM instructions executed, 187.1 on never-cell values and 77.2 on emptied registers. deriv report: 43% of Fields and 53% of Drops in the d phase are dead. bounce: 6 no-work drops per ball-step. storage: two empty Drops per empty_slots step.

**Agreement between reports.** Nine benchmark reports and the refcounting and code-quality studies found it. The no-op Drop is 30 in six traced reports, 35 in the refcounting study (probe), 38 in rbtree (probe) and about 40 in the code-quality study (inferred, not probed).

**Fix direction.** Do not bind fields a pattern never names; lower such an arm to a tag test. Make is_heap false for Bool, Nil and types whose constructors have no fields. Have the loader delete a Drop whose register is moved out on every path into it, and a Field whose only reader is a Drop.

### 16. A small-Int instruction costs 59 to 72 with a second jump table inside, and Ints past 48 bits allocate

`int-op-inner-dispatch-and-48-bit-ints`, kind: interpreter. Depends on: dispatch-and-instr-format.

**Today.** Int, IntImm, JumpUnlessCmp and JumpUnlessCmpImm carry the operator as a field, so each does a second jump-table branch in bigint::small_op (6 instructions). Around it sit a tag check and sign-extension per operand, a checked op, a separate 48-bit range check and a re-tag. Anything above 2^47 becomes a heap big Int: each further op goes through the out-of-line int_op with i128 arithmetic, allocates a cell and frees the old one. Constants too wide for an immediate, a left-hand constant, and an op in tail position are not fused.

**Where.** crates/scarlet_vm/src/exec.rs Instr::Int (175), IntImm (189), JumpUnlessCmp (747), JumpUnlessCmpImm (761), Machine::int_op (2532); crates/scarlet_vm/src/bigint.rs small_op (132), wide_op; crates/scarlet_vm/src/value.rs SMALL_INT_MAX = (1<<47)-1 (line 42), small_int (131), Value::int (75)

**Benchmarks hit.** map_churn, fib, tak, spectral_norm, mandelbrot, binary_trees, cfold, string_keys

**Estimated gain.** map_churn about 26%: 22.2% is `x * 69069` leaving the 48-bit range (measured two ways), plus part of the 14.5% plain arithmetic. fib about 10% (second branch 4.2%, tag checks, range check and re-box 10.3%). spectral_norm about 7% (eight Int ops are 26.0%, of which the inner table and tag work are about a quarter). tak about 6%. mandelbrot and binary_trees 2% to 3%. Geomean factor about 0.97 alone; most of an Int op's cost is dispatch and register access, counted in other items.

**Evidence.** map_churn report: a modulus that keeps the product small drops the run from 19,747M to 15,372M; traced mul 63 to 299, add 59 to 484, rem 73 to 381 on the overflowing path. maps study: `next` is 909 per call against 400 without overflow. value study: adding 1 to a one-word big Int is 482. interpreter-loop study: inner jump table 18 of 346 (6 per op), tag checks and re-tag 32. fib report: second indirect branch 15 per call, decode and boxing 36.5. Probes in six reports agree on Int 70, IntImm 59, JumpUnlessCmpImm 40, JumpUnlessCmp 50 to 53. LuaJIT's ADDVV is about 18.

**Agreement between reports.** The per-op costs agree across about twelve reports to within 1 or 2 instructions. The 48-bit overflow was found by map_churn, the maps study and the value study; map_churn puts `next` at about 42% of the run and the maps study at about 40%. No report checked whether any other benchmark crosses 2^47.

**Fix direction.** Give each operator its own opcode. Do add and subtract on the tagged words so the CPU overflow flag is the range check. For wide values, either keep typed Int registers as raw i64 and box on escape, or reuse a unique one-word big Int in place. Extend immediate fusion to tail position, left-hand constants and wider constants.

### 17. A match is a chain of single-variant tests with backtracking, not a switch on the tag

`match-linear-chains`, kind: compiler. Depends on: dead-field-binds-and-noop-drops.

**Today.** Each arm is tried in order with its own JumpUnlessVariant (68 to 69 on a cell, 40 on a nullary value), each of which re-reads the register and looks the cell up twice. The last arm of an exhaustive match is still tested. Or-patterns and nested patterns compile to letc/goto chains: each alternative tests the variant, unpacks all fields, and on a miss drops them and jumps on, where the same node is tested and unpacked again.

**Where.** crates/scarlet_lower/src/lower.rs (match lowering to letc/goto chains); crates/scarlet_vm/src/code.rs CoreExpr::Match (about 1510), Loader::pattern (1543); crates/scarlet_vm/src/exec.rs Instr::JumpUnlessVariant (430), is_variant (2453)

**Benchmarks hit.** deriv, rbtree, list, cfold, binary_trees, richards, towers, json

**Estimated gain.** deriv about 15% (failed tests alone are about 17% of the run, chain jumps 3.8%). rbtree about 14% (a source rewrite that matches each node once saves 21.4%, part of which is the dead reads counted in the item above). list about 11% (two redundant last-arm tests per step). cfold about 7% (41.9M tests beyond one per match, 2.9B, plus jumps). binary_trees, richards, towers and json about 3%. Geomean factor about 0.97 alone.

**Evidence.** rbtree report: 38.83G falls to 30.50G when only the source is restructured (is_red 5.1%, ins 10.0%, balance 6.4%); is_red on a black node runs 5 variant tests, 10 Fields and 9 Drops. deriv report: reordering count's arms saved 471M against a predicted 493M; 69 per extra failed test; JumpUnlessVariant handlers are 20.8% of the run. cfold: 89.5M tests at 69 = 14.2%, 9 tests in an arm whose source has 5 matches. list: 4 tests per step, 274 of 963; a failed test measured at 69 by reordering arms. code-quality study: 40.5 per failed arm on its probe.

**Agreement between reports.** Five benchmark reports (deriv, rbtree, list, cfold, binary_trees) and the refcounting and code-quality studies found it. The cost of a failed test is 68 to 69 on a cell in four reports, 40 on a nullary value, 40.5 in the code-quality probe, and about 100 to 107 per variant tried in richards and queens (which include more than the test). The code-quality study rates it under 2% on its seven benchmarks, none of which is match-heavy.

**Fix direction.** Compile matches to a decision tree (Maranget style) that tests each position once and shares unpacked fields across arms. Emit one Switch instruction that reads the tag once and jumps through a table, with an or-pattern as several entries to one target. Skip the test on the last arm of an exhaustive match and answer nullary-versus-cell from the value's tag bits.

### 18. Tuples and small records used only to carry several values are allocated, read back and freed

`tuples-as-heap-cells`, kind: compiler. Depends on: no-inlining-and-stdlib-layers.

**Today.** `(a, b, c) = if .. { (..) }` and a function that returns a pair its caller destructures at once both lower to MakeTuple, TupleField reads and a drop. Core IR joins and returns carry one value, so there is nowhere else to put the parts. The same shape appears as `Ok((value, index))` results in json and as the four-field Built record in storage.

**Where.** crates/scarlet_lower/src/lower.rs (tuple patterns to MakeTuple + TupleField), crates/scarlet_ir core_ir LetJoin (one local); crates/scarlet_vm/src/exec.rs Instr::Tuple (483), Instr::Element

**Benchmarks hit.** bounce, nbody, storage, json, richards

**Estimated gain.** bounce about 35% (three tuples per ball-step are 39.7% by trace, 35% to 38% by the code-quality counterfactuals). nbody about 7% (15 tuples per step, 7.8% by ablation). storage about 6% (the Built record is about 9.8%; inferred). json about 4% (Ok, tuple and Json cells are a modelled 11%). richards 1.8%. Geomean factor about 0.97 alone.

**Evidence.** bounce report (lldb trace): Tuple 629/629/671 under lldb (about 500 outside), Element x8 = 620, freeing Drops 280/280/296, 2,976 of 7,493 per ball-step. code-quality study: rewriting the two triples as scalar ifs takes bounce from 37.7G to 31.8G, and also inlining `bounce` (removing the returned pair) to 23.75G; a pair built and dropped costs 705. nbody report: v_plumb 60.66k against v_plumb_notuple 42.92k per step, 1,183 per tuple. storage report: Ctor 312 + four Fields 327 + Drop 253 to 341 per node.

**Agreement between reports.** Five benchmark reports and the code-quality study found it. bounce's share is 39.7% in its own report (trace, with a libmalloc correction for lldb) and 35% to 38% in the code-quality study (source rewrites). A 2-tuple made and dropped is 704 to 708 in three reports and about 750 in json.

**Fix direction.** Give LetJoin and function results several destination registers. A branch ending in MakeTuple whose only readers are TupleFields writes its parts to those registers, and a function whose every return is a tuple that every caller destructures returns in registers (worker and wrapper). The Vec in Instr::Tuple is covered under cell-alloc-free-path.

### 19. An array index read costs 240 to 400 instructions, and `xs[i] or False` or a slice allocates an Option or Result cell

`array-index-read-and-option-boxing`, kind: stdlib-or-builtin. Depends on: heap-cell-addressing.

**Today.** ArrayIndexOr calls four non-inlined helpers (index, seq, position, element); array::get reads all six root words through checked slices, builds a view of head, tree and tail in turn with two chunk lookups each, and scans a size table per level even on dense trees. `index_or` in the lowering only fuses when the default is a Const or Var (confirmed at lower.rs:866), so `or False`, `or None` and `or []` build a Some cell, test it, read it and free it. Slices always return a Result cell.

**Where.** crates/scarlet_vm/src/exec.rs Instr::ArrayIndexOr (690), ArrayIndex, ArraySlice (644), Machine::index (2184), seq (2141), position (2215), element (2150); crates/scarlet_vm/src/array.rs get (482), root (195), view (243), size_slot (423); crates/scarlet_lower/src/lower.rs index_or (842-872)

**Benchmarks hit.** spectral_norm, fannkuch, queens, string_keys, permute, sieve

**Estimated gain.** spectral_norm about 14% (x[j] is 362 of 1,999, 18.1%). fannkuch about 11% (reads about 8%, and the fixed Result cost of 2.27M slices about 10%, estimated). queens about 10% (ArrayIndex 5.7%, the Some round trip 6.8%). string_keys about 8% (ArrayIndexOr 8.6% + GetGlobal 1.9%). permute about 4% (reads 5.5%). sieve under 1%. Geomean factor about 0.98 alone.

**Evidence.** spectral_norm report (trace): 362 per read, element 210, index 48, seq 31, position 17. arrays study: handler 238 on 8 elements and 392 on 5,000; `xs[3] or False` on a Bool array 939 against 357 for `xs[3] or 0`. sieve report: unfused read 1,163 against 604 fused. queens report: about 1,025 per `xs[i] or False`, about 480 of it the Option round trip. string_keys: 290 identical on every step. fannkuch: 308 per read; a slice that does no array work still costs about 1,255. LuaJIT's TGETV measured at 19 (spectral_norm report).

**Agreement between reports.** Six benchmark reports and the arrays study found the read cost, quoted from 238 to 404 depending on depth and array size. Three reports found the missing fusion but explain it differently: sieve and the arrays study point at the Const-or-Var test in index_or, which the source confirms; queens says the `or` sits inside a block joined with `&&`. The boxing surcharge is 559 (sieve), 582 (arrays) or about 480 (queens).

**Fix direction.** Inline one fast path in the handler: small-Int index, ArrayRoot, compare with length, then tail, head or radix descent with no size-table scan on dense nodes. Store arrays of 32 or fewer as one flat cell. Let index_or accept field-less constructors and `[]` as defaults, add the same fusion for `slice or default`, and never build an Option that is matched at once.

### 20. In-place map edits cost about 900 instructions each, shared maps are copied because fields are shared out, and lookups box a Some

`map-ops`, kind: stdlib-or-builtin. Depends on: heap-cell-addressing, refcount-traffic.

**Today.** On a unique map each trie level re-finds its cell three or four times, recurses through non-inlined functions returning enums through memory, and reshapes with memmove and sometimes a splice. A get is 55 per level plus about 145 of dispatch. `Scheduler(..s, tasks: map::set(s.tasks, k, v))` reads the field (a share) before the set, so the map has two holders and its path is copied. A `map::set` in tail position is not recognised as owned. `map::get` matched later allocates a Some. `map::fold` builds a list of tuples and walks it with array::fold.

**Where.** crates/scarlet_vm/src/map.rs insert_owned, remove_owned, settle_cut, reshape, get_by, copy; crates/scarlet_vm/src/code.rs MapSetOwned/MapDeleteOwned recognition (about 1352-1435), MapFind; crates/scarlet_vm/src/exec.rs Intrinsic::MapGet, MapSet; crates/scarlet_core/src/std/scarlet/map.sca fold, from_list, merge, filter

**Benchmarks hit.** map_churn, richards, string_keys, json

**Estimated gain.** map_churn about 21% (delete and set are 33.7% and get 9.1%; halving the per-level cost). richards about 14% (shared map set is 13.8% and would be 441 instead of 1,951 if unique, a 10.7% gap; find_task is 17.0%, of which the Some round trip is about 450 of 1,711). string_keys about 3% beyond the string-key cost counted under strings. json about 1%. Geomean factor about 0.98 alone.

**Evidence.** map_churn report: churn step 4,448 against 2,325 with the map ops removed; traced sets average 986 and deletes 1,032. maps study: remove_owned mean 870 and insert_owned mean 905 over 40 stepped calls each; get is 42 + 55 per level; tail-position set 4,622 per entry against 1,067 with let-and-drop; map::fold about 3,000 per entry. richards report: map_set_shared 1,951 against map_set_unique 441, 99,029 sets per iteration; find_task 1,711, 139,849 calls. LuaJIT's delete and reinsert pair is 136 to 182 (maps study).

**Agreement between reports.** map_churn, richards, string_keys and the maps study agree. The delete-and-set pair is 2,123 by ablation in map_churn against 2,018 in its trace and about 2,100 in the maps study. An in-place set is 441 (6 entries), 632 (5,000) and about 900 (200,000), which is consistent with depth.

**Fix direction.** Look each node up once per level and keep the slice; make the descent a loop that remembers the path. Give nodes capacity classes so an insert rarely moves. Decide owned update from the last use, not from one `let; drop` shape. Lower `match map::get(..)` and `map::get(..) or d` to MapFind everywhere. Fold over the trie directly.

## Not explained yet

- Frame sweep on return: fib, tak, rbtree and the calls study measure 9 instructions per frame register in Ret's `leave`, and nbody measures 7.4 to 7.6, but the towers report measured 0 per extra local (684.5 to 685.0 with 4 to 34 locals) and concluded frame size is not a cause. The source shows `leave` walks every register from base to top (exec.rs 2347) and that the loader has a register allocator (code.rs `allocate`, 2115), so the towers probe, whose extra locals sit in an arm that is never taken, probably did not grow the frame. Nobody checked the register count of those probes.
- Const load cost: 31 in the traced reports (bounce, binary_trees, mandelbrot, nbody, spectral_norm, storage), about 30 by disassembly (loop study), 39 to 53 by probe (code-quality study), and about 80 in the json report.
- No-op Drop cost: 30 (six traced reports), 35 (refcounting study), 38 (rbtree), about 40 (code-quality study, inferred).
- Dispatch is counted as about 12 (storage), 14 (nine reports), 15 (six reports) and 15 to 17 (cfold). The spread looks like a counting boundary, but no report reconciles it.
- LuaJIT's call plus return is given as 30 to 40 (bounce, cfold, deriv, map_churn, inferred), about 50 to 55 (loop and calls studies, counted from vm_arm64.dasc) and 71 to 85 (closures study and spectral_norm, measured). The measured figures should be preferred.
- The LuaJIT yardstick is inflated on some rows, so low ratios flatter Scarlet: spectral_norm (about 320 of LuaJIT's 692 per step are dual-number slow paths; the report puts the like-for-like ratio near 4.6, by arithmetic, not a run), map_churn (LuaJIT spends about 330 per generator iteration), cfold and the closures counter phase (LuaJIT's table and closure allocation cost 600 to 700 each). towers did not take LuaJIT's startup off its 904 per move.
- Cost of the temporary Vec in Tuple, Closure and Array: about 196 to 220 per construction in normal runs (closures study by subtraction, bounce, json, nbody), about 320 (code-quality study, inferred) and 382 of 601 (value study, traced under lldb). bounce and string_keys both report that malloc and free run 120 to 143 instructions longer per pair under lldb; the 236 per pair in a normal run is itself inferred from a gap.
- Heap::alloc is 66 in most traces but 82 in cfold and about 70 in deriv; a two-field Ctor is 252 to 254 in four reports but 270 in cfold.
- Whether Perceus reuse pays today: the refcounting study prices a missed reuse at 185, the value study shows 206 + 114 for the reuse path against 252 for a fresh Ctor, and towers measured reuse 3% worse than no reuse. cfold's counts also differ: 13.63M fresh and 14.34M reused (cfold report) against 11.6M made and 16.3M reused (refcounting study).
- Self tail call cost per argument: 36 in five reports, 28 in map_churn.
- mandelbrot: 1,362 per inner iteration with eight float ops at 51.8% (mandelbrot report) against 1,384 with nine Float instructions at about 60% (code-quality study). The mandelbrot report also has 12 instructions per iteration it could not place.
- strings: the fixed cost of a small concat is about 1,650 and only 900 to 1,000 of it is accounted for by reading the code; the fixed part of split (about 2,800) is only partly itemised; the by-cause table sums to about 101%. The strings study gets about 1,390 for the same 8-byte concat and string_keys 840 to 936 for a 1+1 byte one.
- json: about 7.5M of 93.4M instructions per parse (8%) are unexplained; the report guesses tuple destructuring, drops and freeing the previous tree.
- deriv: the d-phase windows give 1,685 and 1,807 per call against 1,855 for the whole phase, so about 3.7% of the benchmark is unaccounted for.
- rbtree: the model overshoots by 10% to 16% on all four variants; the report suspects its call and constructor terms (it guessed 10-register frames).
- bounce: about 180 instructions per ball-step (2.4%) are attributed to leaf-boundary paths that were never traced; fannkuch has 8.8% in residuals that were not probed piece by piece.
- Time and instructions disagree in places no report explains: memmove is 29% to 35% of map_churn's time samples but about 1.6% of its instructions; in storage the release at exec.rs:631 is 9.4% of time for about 12 instructions; in fannkuch one address in Heap::release holds half of that function's samples; string_keys MapFind is 27% of time but 11% of instructions.
- Why `xs[i] or False` is not fused: sieve and the arrays study blame the Const-or-Var test in index_or (the source agrees), while queens says the `or` is inside a block joined with `&&`. Both could be true for queens; nobody tested the second.
- ArrayPrepend's share of permute is 37.6% by probe times count, 43% in the arrays study's window and 50% of time samples; queens is 32% against 40%.
- ArrayLen is 85 (arrays study), 102 (queens) and 107 to 112 (closures, bounce) for what should be the same handler.
- string_keys: word building is 74% of the run in its own report and 80.5% in the maps study.
- No report checked whether any benchmark other than map_churn crosses the 48-bit small-Int limit.

## Each benchmark in a few sentences

**benches/binary_trees/binary_trees.sca: where the instructions go.** The benchmark touches 14,985,902 tree nodes and retires 27.59 billion instructions after startup, which is 1,841 per node; LuaJIT with the JIT off spends about 725 per node on the same work. Scarlet runs only about 20.5 VM instructions per node, so the instruction count is not the problem: each VM instruction costs 90 machine instructions on average. I single-stepped one whole loop iteration (build and check a depth-4 tree, 57,047 machine instructions, 637 VM instructions) in lldb and attributed every instruction to its inlined source line; that trace gives 1,840 per node, matching the benchmark to about 1%. The cost is spread across fixed per-instruction overheads rather than one hot spot: dispatch (15.6%), register bounds checks (12.5%), finding a heap cell through chunk and word indices with checks (14.2%), frame setup and teardown (12.1%), reference-count release and free (11.1%), and value tag decoding (6.2%), with the rest in handler bodies (23.1%). Breakdown: Measured, from the exact trace of one iteration (31 nodes, 57,047 instructions, 1,840 per node). Two views of the same 100%.

By VM instruction kind (count per iteration x average machine instructions): Drop 124 x 101 = 22.0%; Field 92 x 86 = 13.9%; Ctor 31 x 254 = 13.8%; CallSelf 60 x 124 = 13.0%; JumpUnlessVariant 108 x 64 = 12.2%; Ret 62 x 106 = 11.5%; Int 31 x 70 = 3.8%; Const 63 x 31 = 3.4%; IntImm 31 x 59 = 3.2%; JumpUnlessCmpImm 32 x 40 = 2.2%; Call and TailCallSelf of the outer loop 0.9%.

By mechanism (each machine instruction counted once, by its innermost matching inlined frame): handler bodies and other 23.1%; dispatch 15.6%; register get/set/take 12.5%; heap cell lookup (chunk, Option, range checks) 9.0%; heap word get/set bounds 5.2%; Heap::release logic 7.2%; value decode 6.2%; enter_above 4.7%; Machine::leave 4.6%; callers Vec<Frame> push/pop 2.5%; Heap::alloc 2.1%; Heap::free_cell 2.1%; free list push/pop 1.8%; small int op 1.4%; write_ctor 1.1%; Heap::share 0.5%; Machine::open 0.3%.

Unexplained: about 1%. The trace predicts 1,840 per node at depth 4, where the outer loop adds about 22 per node; the real benchmark, with deeper trees and almost no loop overhead per node, measures 1,841. I did not trace deep trees, so growth of the heap chunks, the register stack and the frame stack is not separately counted.

**benches/bounce/bounce.sca: where Scarlet's 7,493 instructions per ball-step go (.** The benchmark retires 37.54 billion instructions, which is 7,493 per ball-step over 5.005 million ball-steps; LuaJIT with the JIT off retires 4.52 billion, about 900 per step. I single-stepped the profiling binary with lldb through two whole ball-steps and got an exact count per VM instruction: one step is 57 VM instructions and 7,264 machine instructions under the debugger, which is about 6,835 outside it because libmalloc runs about 143 instructions longer per tuple under lldb. About 40% of the step goes on three heap tuples that only carry several values out of an `if` or a function (make, three or two element reads, free), about 20% on the four array instructions (the append alone is 850), about 16% on the Ball record (allocate, six field reads, free), and the rest on calls, on instructions that do no work (drops of Bools and Ints, constants, moves, jumps) and on plain arithmetic. Cut the other way, no single operation is the problem: every VM instruction pays 14 instructions of dispatch, about 11 per register operand and about 9 to 10 per heap word touched, so even an Int add costs 70 and a field read 77. Breakdown: Per ball-step, as a share of the measured 7,493 instructions. The first six rows are exact counts from the lldb trace of one non-bouncing step (6,835 after the libmalloc correction); the last four are probe-based or inferred and are marked.
- Tuples used as multiple values: 2,976 (39.7%). 3 Tuple makes at about 500 each, 3 frees at 280/280/296, 8 Element reads at 75 to 95.
- Array instructions: 1,502 (20.0%). ArrayAppend 850, ArrayElem 229, ArrayLen 2 x 112, ArrayDrop 199.
- Ball record, in the step: 940 (12.5%). Ctor 312, 6 Field reads at 77, 2 live drops at 83.
- Ball record, deferred free of the old ball when the old array dies: about 300 (4.0%). Inferred from the ctoronly probe (611 = Ctor 312 + free 299), not seen in the traced window.
- Calls: 506 (6.8%). TailCallSelf 229, Call 135, Ret 142.
- Instructions that do no work: 531 (7.1%). 6 drops of Bools/Ints/emptied registers at 30, 3 Const at 31, 5 Jump at 18, 2 Move at 42, 2 JumpIfFalse at 42.
- Int arithmetic and compares: 380 (5.1%). 2 Int adds at 70, 6 compare-and-jump at 40.
- Extra work on a bouncing step (int::abs call, 0 - x): about 120 (1.6%). Measured 449 more in the traced bouncing step, times the 26.6% of steps that bounce.
- make_balls and the final fold: about 60 (0.8%). Inferred, not measured.
- Unexplained: about 180 (2.4%). My guess is the once-per-32-elements slow paths in array skip and push (moving a leaf into the tree, freeing leaves and roots); I did not trace one.
The same traced step by mechanism (7,264 with debugger malloc): handler bodies 18.0%, libmalloc 14.9% (about 9.5% outside the debugger), heap word access through the chunk table 14.1%, dispatch 11.0%, register get/set/take 8.9%, refcount logic 7.0%, heap free lists 6.8%, argument buffers 6.8%, array.rs logic 5.1%, frame sweeps 3.3%, memmove 3.1%.

**benches/cfold/cfold.sca: where 43.1 billion instructions go.** cfold runs 43.10 billion instructions after startup (LuaJIT with the JIT off: 23.35 billion) for 42.9 million calls, so about 1,000 per call against Lua's 540. Scarlet runs only about 10 VM instructions per call, fewer than Lua's bytecodes, but each one costs about 100 machine instructions: I single-stepped one whole round under lldb and got an exact cost for every VM instruction, and a model built from those costs and exact call counts gives 43.23 billion, within 0.3% of the measurement. Allocation is not where Scarlet loses: Lua spends about 600 instructions on each table including GC, while a fresh Scarlet cell is about 250 to make and 250 to free, and a reused one 208. The loss is spread over call and return (26%), matching (variant tests, field reads and the unique-cell drop, about 41%), and constructors (15%), with 15 to 17 instructions of dispatch inside every one of them. Breakdown: By VM instruction kind (model total 43.23B against 43.10B measured; adds to 100.3%):
- Field reads: 16.3% (13.4% live, 2.9% for binders nothing reads)
- Constructors: 14.7% (fresh 7.8%, reuse in place 6.9%)
- Calls: 14.3% (CallSelf 12.7%, TailCall 1.6%)
- JumpUnlessVariant: 14.2%
- Return, without frees: 11.7%
- DropReuse: 10.4% (hollowing a unique cell 9.8%, misses 0.6%)
- Freeing cells: 7.1% (Drop that frees 4.5%, Ret freeing parked cells 2.6%)
- Other Drops: 5.7% (already-empty register 3.4%, plain decrement 2.3%)
- Int arithmetic, compares, constants: 4.9%
- Jump: 1.0%
Dispatch and loop-back (15 to 17 per VM instruction, 434.6M VM instructions) is about 17% and sits inside every line above.

By function (same model): append_add 38.5% (its recursive arm alone is 33.9%, 1,021 instructions x 14.34M), cfold 25.1% (the arm that folds into the left Val is 17.6%, 3,519 each), eval 13.8%, mk_expr 9.9%, reassoc 9.1%, shape 3.7%.

Cross-check by phase, measured by adding one phase at a time to the benchmark (Scarlet / LuaJIT, billions): mk_expr plus freeing the tree 5.05 / 3.67; eval(e) 4.20 / 1.24; reassoc 21.11 / 13.10; cfold 10.43 / 4.67; eval(f) 0.99 / 0.28; shape 1.32 / 0.36. Frees land in different phases in the two orders, so these differ from the model's per-function numbers by a few tenths of a billion.

Unexplained: 0.13B (0.3%). The per-instruction costs come from one depth-4 round with i=1; costs that depend on the heap state (a fresh cell was 241 to 270, a free 229 to 333) were taken at their medians.

**benches/closures/closures.sca: where the 26.35 billion instructions go.** The run costs 26.35G instructions net of startup against LuaJIT's 3.17G, and it executes about 188 million VM instructions at an average of 140 machine instructions each. About a third goes on calling closures (CallValue, TailCallValue and Ret), about 36% on building and walking arrays one element at a time (append 15%, the `[h, ..t]` walk 21%), about 14% on copying captures into registers and dropping them again, and the rest on the fixed cost of every small instruction (14 to dispatch, 30 to 70 in total for a Const, compare or add). By phase, the two maps are 60% (first map 20.5%, pipeline map 39.4%), filter 18.8%, the two folds 17.9% and the counter 3.3%; every array phase is 9 to 14 times LuaJIT, while the counter phase is only 1.29 times. All shares come from single-stepping the profiling binary in lldb and scaling each phase's trace to its measured instruction count, and the traced per-element costs agree with independent micro-probes to within 1 to 3%. Breakdown: Measured, by VM instruction (a partition of the 26.3G; each phase was single-stepped for 60k to 200k machine instructions and scaled to its measured total): CallValue 15.3%, ArrayAppend 14.8%, ArrayDrop 9.5%, Ret 9.1%, GetCapture 8.8%, TailCallValue 7.7%, TailCallSelf 6.1%, ArrayLen 5.9%, Int 5.7%, Drop 5.6%, ArrayElem 5.3%, JumpUnlessCmpImm 2.2%, Const 1.3%, IntImm 0.7%, Closure 0.6%, Jump 0.5%, TailCall 0.3%, Ctor 0.3%, JumpIfFalse 0.3%, Field 0.1%. That sums to 100%.

Grouped by cause: closure call and return 32%; array append 15%; array walk with `[h, ..t]` (ArrayDrop, 2x ArrayLen, ArrayElem, its two compares, Jump and Const) about 24%; capture reads plus their drops about 14%; self tail call 6%; plain arithmetic and constants about 8%; closure and constructor allocation about 1%.

The same instructions by machine function: inlined instruction arms in Machine::call 40.2%, the dispatch at the loop head 10.0%, Heap::release 9.0%, Machine::callee 7.2%, enter_above 5.4%, seq 4.2%, call_args 3.8%, push_owned 3.0%, element 2.4%, memmove 2.3%, enter 1.9%, Heap::alloc 1.7%, free_cell 1.6%, array::branch 1.4%, system malloc/free 0.5%, others under 1% each.

By phase (measured by adding one phase at a time to the benchmark; the sum is the full 26.35G): first map 5.41G (20.5%, 3606 per element), filter 4.95G (18.8%, 3300 per element), direct fold 2.51G (9.5%, 1955 per element), building the pipeline 0.04G (0.1%), map through the pipeline 10.39G (39.4%, 8079 per element), second fold 2.19G (8.3%, 1705 per element), counter 0.86G (3.3%, 2864 per step). LuaJIT on the same phases: 0.39G, 0.41G, 0.27G, 0.02G, 1.19G, 0.23G, 0.67G.

Unexplained: nothing material. The phase sum matches the whole run, and traced per-element costs (3550, 3223, 1911, 8100, 1640, 2969) are within 1 to 3% of the probe figures. The one soft spot is the 1-in-32 slow path of append and drop, which each trace window caught only once, so their amortised share (about 170 to 200 instructions per element each) is good to roughly +/-20%.

**benches/deriv/deriv.sca: where Scarlet's 39.2 billion instructions go.** deriv runs 39.20 billion instructions net of startup, against 13.23 billion for LuaJIT with the JIT off. 41% of that (16.03 billion, measured by removing the call) is the `count` walk that prints each tree's size, and 59% is the differentiation itself. In both halves each VM instruction costs about 84 machine instructions and a call needs many of them: 8.6 per `count` call and about 20 per call in d/add/mul. The biggest single consumer is matching: variants are tested one at a time at 52 instructions a test plus 15.5 of dispatch, and each or-pattern such as `Var(..) | Add(..) | Mul(..) | Pow(..) | Ln(..)` (which the language forces, `_` is refused) is lowered to a chain that tests each variant, reads its fields with a refcount increment, drops them again and jumps. After matching come the call and return pair (about 245 to 340 instructions), the 77-instruction field read, reference counting, and the bounds-checked chunk-and-word lookup behind every heap word. Breakdown: Measured split by phase: `count` 40.9% (16.03G of 39.20G, from running with the count call removed), d/add/mul/powr/ln 59.1% (23.18G).

By VM instruction kind, whole benchmark. These come from exact single-step traces of three windows (60k instructions inside count, two of 150k inside d at different tree levels, which agree with each other to within 1 point), weighted 0.409/0.591. Handler cost excludes dispatch, which is its own row.
- Dispatch (fetch, bounds check, jump table): 18.7%
- JumpUnlessVariant (variant tests): 20.8%
- Field (read a field, share it, store it): 16.0%
- Call, CallSelf, TailCall, TailCallSelf incl. enter_above: 12.8%
- Ret incl. the register-release loop: 8.2%
- Drop and DropReuse incl. Heap::release and free_cell: 13.1%
- Ctor incl. Heap::alloc: 6.5%
- Equal (string ==): 1.5%
- Everything else (Jump, Const, Int, IntNeg, compare-and-jump): 2.5%
Total 100.1% of what the windows explain.

Unexplained: the d-phase windows give 1,685 and 1,807 instructions per call, and the whole d phase averages 1,855 (23.18G over 12.49M calls counted in the Lua twin). So about 6% of the d phase, 3.7% of the benchmark, is not accounted for by extrapolating the windows. My guess (not measured) is the bulk free of the two final trees in main and a call mix at the deepest level that differs slightly from the windows.

Cross-cutting views that overlap the rows above (do not add them): failed variant tests about 17% of the benchmark; or-pattern dead field reads, drops and jumps about 10.7%; locating a heap word about 17% of the d phase; reference counting inside count about 8.6%.

**benches/fannkuch/fannkuch.sca (n = 9): where 27.70 billion instructions go.** Fannkuch runs 27.70 billion instructions after startup, against 1.00 billion for LuaJIT with the JIT off, which is about 76,400 per permutation against 2,750. About 82% of it is the array runtime: the program has no "write one slot" operation, so every write is spelled as slices, pushes and a concat on a persistent RRB tree, and the run makes 20.48 million heap cells (56 per permutation, about 1,350 instructions per cell made). Two thirds of the total is inside flip, where each reversed element costs about 1,580 instructions (an 874-instruction "in place" prepend that still allocates a new leaf, a 308-instruction read, and 400 of loop), and a fifth is the 260,648 calls to set at 20,700 instructions each. The remaining 18% is ordinary interpreter cost (calls, tail calls, Int arithmetic at about 70 instructions per add), and that part alone is five times LuaJIT's whole run. Breakdown: Total 27.70G after subtracting 0.039G startup. Measured by running each piece twice in place and taking the difference (the pieces sum to 91%; the last two rows are residuals, so the table adds to 100% by construction).

Inside flip, 18.5G = 66.8% (measured two ways: flip called twice gives +18.54G, flip removed gives -18.49G):
- 16.2% (4.48G): `[q1, ..back]`, a prepend onto a slice whose head has an offset, 2,819 instr x 1,588,945
- 15.9% (4.39G): `[q[i] or 0, ..acc]` in reversed, the "in place" prepend, 874 instr x 5,028,529
- 9.6% (2.65G): reversed's loop itself (one call plus 5.03M self tail calls, IntEq, IntAdd), about 400 per turn
- 9.3% (2.58G): `q[q1 + 1..n] or []`, 1,626 instr x 1,588,945
- 5.6% (1.55G): `q[i]` read in reversed, 308 instr x 5,028,529
- 5.0% (1.39G): `[0, ..reversed(...)]`, 877 instr x 1,588,945
- 5.3% (1.46G): residual of flip: the `q[q1]` read, compare, add, self tail call and freeing the old q, about 760 per flip turn (not probed separately)

Outside flip, 9.2G = 33.2%:
- 19.5% (5.40G): set, 20,733 instr x 260,648. Inside it: concat 9,933, the two slices 4,440 and 4,680, append 1,487
- 3.7% (1.02G): swap12, 5,602 x 181,440
- 3.4% (0.94G): swap01, 5,186 x 181,440
- 3.1% (0.86G): rotate, 10,907 x 79,209 (concat 4,685, slices 2,413 and 1,703, append 844, head/tail match about 1,260)
- 3.5% (0.98G): residual: run's loop, the call into flip, advance's loop, the Next constructor and match, about 1,570 per run/advance turn (not probed separately)

Cut the other way: array runtime operations about 82% (22.6G), interpreter control and arithmetic about 18% (5.1G). Cells made: 20.48M in total, of which flip makes 14.3M (9.0 per flip turn), set 3.49M (13.4 each), swaps 1.69M, rotate 0.73M.

**benches/fib/fib.sca (fib(35), 29,860,703 calls).** fib(35) costs 10.60 billion instructions after startup, which is 355 per fib call; LuaJIT with the JIT off spends 122 per call on the same machine (both measured). I single-stepped the profiling binary through all of fib(10) under lldb, so the split below is counted per machine instruction and adds up to 355 exactly. A fib call runs 4.5 VM instructions (compare-and-jump 40, subtract-immediate 59, half an add at 70, CallSelf 124, Ret 97), so each VM instruction costs about 79 machine instructions. About 62% is the call and return machinery (frame record push and pop, the runtime scan that clears every register, an out-of-line enter_above, moving one argument and one result), 19% is the 15-instruction dispatch, and only about 5% is the arithmetic itself. Breakdown: Measured from the lldb trace, in machine instructions per fib call (total 355, 100%).
By VM instruction: CallSelf 124 (35%), Ret 97 (27%), IntImm sub 59 (17%), JumpUnlessCmpImm 40 (11%), Int add 70 x 0.5 = 35 (10%).
By cause, cutting across those:
- Dispatch, 15 x 4.5 = 67.5 (19.0%)
- Clearing and opening frames: leave on Ret 38, leave on call 11, open 6, frame_end 4 = 59 (16.6%)
- Pushing and popping the 40-byte Frame record in the callers Vec = 36 (10.1%)
- Passing the one argument (loop setup 9, take plus set 27) = 36 (10.1%)
- Moving the result back (take 13, set in caller 14) = 27 (7.6%)
- enter_above being a real function call (argument setup, 12 register saves and restores) = 26 (7.3%)
- Register reads for arithmetic = 25.5 (7.2%)
- Register writes for arithmetic, including the release check on the old value = 19.5 (5.5%)
- The arithmetic itself with overflow check, 48-bit range check and re-boxing = 19 (5.4%)
- Operand decode, small-int tag check and untag = 17.5 (4.9%)
- Second indirect branch on the IntOp field = 15 (4.2%)
- Closure env share and release checks on a plain function = 7 (2.0%)
Nothing is unexplained: the trace gives 354.5 per call in steady state and /usr/bin/time gives 355.1. The small gap is the register stack and callers Vec growing, which the trace shows as a handful of CallSelf steps costing 206 to 1347 instructions.

**benches/json/json.sca: where the instructions go (diagnosis only, nothing change.** One parse of the 25,820-byte document costs 93.4 million instructions (28.06 billion for 300 rounds, after startup), which is about 3,600 per input byte; LuaJIT's interpreter spends about 47 million per parse (inferred from the 1.99 ratio). The biggest single cause is the `x <- result::then(...)` style the parser is written in: rewriting those 14,545 chains per parse as plain `match` removes 28.8 million instructions per parse (31%), because each one allocates a closure, makes two extra calls, and keeps the array or map being built shared so every append copies. The rest is spread thin: every simple VM instruction costs about 70 machine instructions and every call, built-in or small allocation costs 200 to 750, so the byte loop (689 per byte), `skip_white_space` (about 870 per call), cutting strings out (about 3,800 per string even written directly) and the Ok/tuple/constructor cells each take 8 to 11%. About 8% of the run is not explained by my probes. Breakdown: Per parse, 93.4M instructions. Measured by ablation on the real benchmark (edit one thing, count again): result::then closure style 28.8M (31%); skip_white_space 7.3 to 7.8M (8%); slice_bytes + to_string inside capture, direct style, 7.9M (8.5%); the '' literal allocated twice per string 1.5M (1.6%). Modelled from micro-probes times call counts (counts from a Python port of the parser, checked against the tally 14,743 characters): scan_string byte loop 12,886 x 689 = 8.9M (9.5%); Ok / tuple / Json constructor cells for results 10.6M (11%); the other 25,600 function calls at about 350 each 9.0M (9.6%); byte_at outside the scan loop and whitespace 15,228 x 212..293 = 3.8M (4%); array append, map set and empty array/map creation 5.2M (5.6%, the map-set and creation parts are guesses); match tests, Int adds and constants about 3M (3%). Sum 86M (92%). Unexplained: about 7.5M (8%), probably tuple destructuring, Drop instructions and releasing the previous tree, none of which I isolated. Fixed cost of compiling and loading json.sca is about 79M once, 0.3% of the run. The categories overlap by mechanism: byte_at as a whole is 38,753 x ~270 = 10.5M (11%) and is counted inside scan, whitespace and the rest.

**benches/list/list.sca (AWFY List): where Scarlet's 21.8 million instructions per.** The benchmark costs 43.67 billion instructions (43.63 billion after startup) against LuaJIT's 6.21 billion with the JIT off, which is the 7.03 ratio. About 80% of it is one loop: the advancing step of is_shorter_than runs 18,050 times per iteration and costs 963 instructions (measured twice, by a micro-probe and by an lldb single-step trace), where LuaJIT's interpreter spends 103 on the same step. That step is 11 VM instructions at about 88 machine instructions each. Of those 11, two are variant tests the exhaustive match does not need, two read a field bound to `_`, and four are the reference-count traffic of walking two owned lists. The rest is spread evenly: a 15-instruction dispatch, heap cells found through `chunks[chunk] -> Option<Chunk> -> words[word..]` with bounds checks on every read (22% of the step), bounds-checked register access, and a 177-instruction self tail call that copies its arguments through a side buffer and clears the whole frame. Breakdown: Measured total is 21.82M instructions per benchmark iteration (43.63B / 2000). The model below is operation counts (from a Python port with counters: 18,050 advancing is_shorter_than steps, 2,809 tail calls, 2,106 next calls, 702 true branches, 34 make_list, 11 length) times per-instruction costs from the lldb trace. It sums to 21.96M, which is 0.7% over the measured figure.

By VM operation:
- is_shorter_than advancing step, 18,050 x 963 = 17.38M, 79.7% of the total. Inside it:
  - 4 JumpUnlessVariant (69+68+69+68 = 274): 22.7%
  - 2 Field reads of the `_` val field (77 each = 154): 12.7%
  - 2 Field reads of the next pointer with a share (96 each = 192): 15.9%
  - 2 Drop of the parent cell (83 each = 166): 13.7%
  - 1 TailCallSelf with 2 moved args (177): 14.6%
- is_shorter_than entry and exit (Call 210, terminal arm 290 or about 630, Ret 106, JumpUnlessCmp 42), 2,809 times: 1.76M, 8.1%
- next(), 2,106 x 649 (Call 159, two variant tests 137, two Fields 173, Drop 83, Ret 97): 1.37M, 6.3%
- tail's own frames (3 CallSelf at 226 to 261, TailCallSelf 229, Drops, Ret 133): about 1.45M, 6.7%. This line is partly estimated: the per-instruction costs are measured, but the mix of 30-instruction and 83-instruction Drops is approximated.
- make_list, length and run: about 0.03M, 0.15%

By mechanism, inside the 963-instruction step (categorised from the trace by inlined source frame):
- dispatch 17.5%
- finding a heap cell 12.5%, plus indexing a word in it 9.3%
- register get, set and take 10.0%
- call_args, again and passed 14.2%
- value tag decode 7.2%
- reference count arithmetic and the release call 8.1%
- handler glue, variant compare and register shuffling in Machine::call 21%

Unexplained: the 0.7% overshoot, and the heap warm-up and free-list traffic for the 31 cells made and freed per iteration, which were not modelled separately.

**benches/mandelbrot/mandelbrot.sca: where the instructions go.** Mandelbrot runs 37.84 billion instructions after startup, and 96% of them are in the inner `escape` loop, which costs 1362 instructions per iteration against LuaJIT's 356 (both measured by changing the iteration limit). Scarlet runs fewer VM instructions per iteration than LuaJIT runs bytecodes (15 against 20), but each one costs about 91 machine instructions against LuaJIT's 18. Just over half of the benchmark is the eight float arithmetic instructions at 92 each, because each one makes three calls that are not inlined (`Machine::float` twice, `float::op` once), passes a Result through memory, and clamps NaN and infinity. The self tail call with six arguments is another 22% at 313 instructions, and the float compare with its separate branch, the two constant reloads and the Int counter make up most of the rest. Nothing allocates and no reference count is touched in this benchmark, so all of it is interpreter overhead. Breakdown: Total 37.84e9 instructions after subtracting the 39M startup. There are 26.64M escape iterations and 812,500 pixels, counted with an instrumented Lua copy.

- Eight float arithmetic instructions (Float Add, Sub, Mul) at 92 each, 736 per iteration: 51.8%
- TailCallSelf with 6 arguments at 313: 21.8%
- Float Gt plus the unfused JumpIfFalse at 140: 9.9%
- Two Const loads (2.0 and 4.0) at 31 each: 4.4%
- IntImm `z + 1` at 59: 4.1%
- JumpUnlessCmpImm `z < 50` at 40: 2.8%
- Unexplained remainder inside the iteration, 12 per iteration (1362 measured against 1350 summed from probes; my guess is two or three more registers cleared by `again` than in the probe): 0.8%
- Per-pixel work in `row` (call and return of escape, two FloatFromInt, three float ops, Int ops, constants, the tail call), measured at 1909 per pixel: 4.1%

These add to 99.7%. `rows` and `main` are below 0.1%.

The same total cut a different way, counted from the disassembly and not measured directly: the dispatch sequence is 15 instructions and runs 15 times per iteration, which is about 16.5% of the benchmark. The NaN and infinity clamp is about 9 instructions on each of 8 float results, about 5%.

**benches/map_churn/map_churn.sca.** The benchmark runs 19.71 billion instructions after startup, against 7.80 billion for LuaJIT with the JIT off, and I can account for 100% of them. About 42% is the key generator `next`, not the map: 22% is a heap-allocated big Int made by `x * 69069` on roughly half the calls because small Ints stop at 48 bits, 10% is the call and return around a four-instruction function, and 10% is its arithmetic. Map delete and set take 34% (about 2,120 instructions per delete-plus-set pair, all on the in-place path), map get takes 9% (about 370 per lookup of a three-level trie), and the tail-call loop skeleton and the other arithmetic take the remaining 15%. Every plain VM instruction costs 30 to 100 machine instructions and a self tail call costs 250 to 280, so the same interpreter overhead seen in fib is underneath all of it. Breakdown: Measured total 19,747M, startup 39M, net 19,708M. By phase (measured by cutting main down): fill 531M (2.7%, 2,654 per step), ten lookup rounds 4,897M (24.8%, 2,449 per step), churn 13,346M (67.7%, 4,448 per step), last two lookup passes 935M (4.7%).

By cause, built from an exact single-stepped instruction trace per VM instruction, multiplied by 200k fill steps, 2.4M lookup steps, 3M churn steps and 8.6M calls to next:
- Big-Int excursion inside next (mul overflows 48 bits, add and rem on a heap cell): 4,386M, 22.2%
- Map delete and set (3M pairs at 2,123, plus 200k growing sets at about 1,330): 6,634M, 33.7%
- Call and Ret around next (135 + 97 per call): 1,995M, 10.1%
- next's own arithmetic on the small path (IntImm, IntImm, Const, Int: 226 per call): 1,944M, 9.9%
- Loop skeleton (TailCallSelf 249 to 277, compare-and-branch 40 to 53, counter 59): 2,074M, 10.5%
- Map get (2 per lookup step, 390 hit and 356 miss): 1,790M, 9.1%
- Hit arithmetic and join-point plumbing in lookups (374 per step): 898M, 4.6%
Sum 19,721M against 19,708M measured, so 100.1%. Nothing is unexplained in instruction count.

One thing is unexplained in time, not instructions: memmove is 29% of time samples (893 of 3,125) but only about 50 instructions per map update, about 1.6% of instructions. I did not find out why; cache misses on the node being edited is a guess.

**benches/nbody/nbody.sca: where Scarlet's 226,400 instructions per time step go (.** nbody retires 22.68 billion instructions, which is 226,400 per time step after startup, against 28,300 for LuaJIT with the JIT off (measured here, ratio 8.0). Four ablated variants of the benchmark split that into Newton square root 29%, the float, field and record work in `pull` 27.5%, array and tuple plumbing in `pull` and `pull_all` 27%, and the `array::map` position update 16.5%, and they add up to within 0.3%. Cut by cause instead, the biggest single item is the Float instruction at 93 instructions each (29%), then walking and rebuilding 5-element arrays (about 30% with map's share), then the tail call that drives the sqrt loop (13%), field reads at 77 each (10.5%), allocating and freeing 25 Body records per step (8.4%) and 15 tuples per step (7.8%). No one thing is pathological: every VM instruction costs 30 to 100 host instructions where LuaJIT pays 18 to 30, and the program also allocates and frees 89 heap cells per step (measured, 0 reused) where the Lua version allocates none. Breakdown: Per time step, 226,400 instructions (22,683M total minus 39M startup, over 100,000 steps). All shares below are of that.

By part of the program (measured by ablating the benchmark, sum 225.9k):
- sqrt (10 calls, 98.97 sqrt_from turns per step): 65.0k to 66.7k, 29%
- pull's float maths, field reads and two Body records per pair: 62.3k, 27.5%
- array and tuple plumbing in pull and pull_all: 60.7k, 27% (tuples 17.7k, pull_all's walk and rebuild 12k, pull's walk and rebuild 29.8k, run loop about 1k)
- array::map position update: 37.4k, 16.5%

By cause (unit cost from micro-probes times a count read off the IR, sum 226.5k):
- Float instruction: 716 per step x 93 = 66.6k, 29%
- array walk and rebuild in pull and pull_all: 41.8k, 18.5% (measured by ablation)
- sqrt_from loop control (TailCallSelf about 195, Const 31, branch about 28 per turn, plus 430 per sqrt entry): 29.5k, 13%
- map's fold, append, two closure calls and captures per body: 26.9k, 12% (by subtraction, not measured directly)
- Field reads: 310 x 77 = 23.9k, 10.5%
- Body ctor plus its free: 25 x 765 = 19.1k, 8.4%
- tuple returned from pull: 15 x 1,183 = 17.7k, 7.8% (measured by ablation)
- run loop: about 1k, 0.4%

Unexplained: nothing large. The two cuts agree to 0.3%, but the 12% for map plumbing is a remainder, and the split of the 41.8k array cost between ArrayLen, ArrayElem, ArrayDrop and ArrayPrepend is only bounded by probes, not pinned per instruction.

**benches/permute/permute.sca (Scarlet 32.46x LuaJIT-joff instructions).** Permute runs 154.9 million instructions per outer iteration (31.0 billion in all, startup off), against 4.77 million for LuaJIT with the JIT off. 72% of that is the recursive `set` function that rebuilds the six-element array, because Scarlet has no array write and the benchmark writes one by taking the array apart with `[h, ..t]` and putting it back with `[x, ..t]`, at about 3,600 instructions per element step. Inside `set`, the biggest single cost is the prepend at the bottom of each walk, which always misses the in-place path and costs about 2,900 instructions for two leaf allocations, a root allocation, two frees and about seven 264-byte SmallVec copies. The other 28% is the ordinary interpreter tax seen in fib: about 330 to 360 instructions per call, about 900 to 950 per `State(..s, ...)` rebuild, about 350 to 420 per `xs[i] or 0`. Breakdown: Per outer iteration, 154.90M instructions, measured by removing one piece at a time from the benchmark (the control flow does not depend on the array's contents, so every variant makes the same 8,660 permute calls). 
- `set` recursion that rebuilds the array (30,998 steps in 20,156 calls): 111.69M = 72.1%. First set of a swap (array shared with the State) 61.27M, second set (array unique) 50.42M.
- Two `v[i] or 0` reads in swap (20,156 reads): 8.46M = 5.5%.
- The swap call itself (10,078 calls): 3.29M = 2.1%.
- Two `State(..s, v: ...)` rebuilds per swaps turn (10,078): 9.17M = 5.9%.
- `State(..s, count: s.count + 1)` in permute (8,660): 8.29M = 5.4%.
- Leaf checksum `State(..counted, check: (check*31 + v[0]) % 1000003)` (5,040): 7.71M = 5.0%.
- Bare call skeleton of permute and swaps (17,319 activations): 6.28M = 4.1%.
These add to 154.89M, 100.0%.

Inside the 72.1% for `set`, the split is estimated by multiplying micro-probe costs by counts, not measured in place: slow-path prepends 20,156 x 2,889 = 58.2M (37.6%); the two-test match on the array 30,998 x 577 = 17.9M (11.5%); `[h, ..t]` drops 30,998 x up to 644 = up to 20.0M (12.9%); calls into set 30,998 x about 330 = 10.2M (6.6%); fast-path prepends 10,842 x 521 = 5.6M (3.6%). That sums to 111.9M against 111.7M measured, with ArrayElem, IntEq, IntSub and returns (roughly 4M) not counted, so the drop figure is an overestimate by about that much. The sampling profile agrees in shape: ArrayPrepend (exec.rs 604-617) is 50% of samples, ArrayDrop 4.5%, ArrayLen 2.5%, ArrayElem 2.4%, ArrayIndexOr 2.6%.

**benches/queens/queens.sca: where the 10.25 million instructions per queens() run.** Scarlet retires 10,247,700 instructions per queens() run (35.906 G total less 38.9 M startup, over 3500 runs) against LuaJIT's 346,400, and 73% of that is the benchmark's hand-written recursive `set`, which rebuilds an array with `[h, ..t]` and `[v, ..rest]` because the language has no indexed array update. `set` runs 2,985 invocations per run where Lua does 767 in-place stores, and each invocation costs about 1,970 instructions, with the prepend alone costing 707 on its fast path and about 3,000 to 3,700 on its fallback. Another 21% is `is_free`, where one `xs[i] or False` costs about 1,025 instructions, roughly half of it making, testing and freeing a `Some` cell. Behind both sits the same thing: the benchmark runs about 68,000 VM instructions per run at about 150 machine instructions each, because every handler makes many bounds-checked, kind-checked cell accesses; even with `set` free, the rest would still be about 7.9 times LuaJIT. Breakdown: Per queens() run, 10,247,700 instructions measured. The model below sums to 10.30 M (100.4%), so nothing sizeable is unexplained.

By source-level cause (measured probe costs times operation counts from a Python simulation of the run):
- `set`, full recursive steps (2,533 at 1,972 each): 4.99 M, 48.7%
- `set`, base invocation including the fallback prepend, the first drop on a shared array and freeing the result (452 calls): 2.53 M, 24.6%
- `is_free` (876 calls, 1,452 array indexes): 2.12 M, 20.7%
- `place_queen` loop and recursion (981 invocations, about 420 each, plus 112 non-tail calls): 0.45 M, 4.4%
- `take` outside its four `set` calls (113 at about 1,170: four Field reads, Board constructor, Board free): 0.13 M, 1.3%
- `filled` x4, Board, `digits`, `run`: 0.075 M, 0.7%

The same instructions by VM instruction, inside the 48.7% of full `set` steps (exact single-step counts, per step): ArrayPrepend 707 (17.5% of the run), dispatch 224 (5.5%), two ArrayLen 204 (5.0%), ArrayElem 186 (4.6%), ArrayDrop in place 184 (4.5%), CallSelf 161 plus Ret 117 (6.9%), three compare-and-jumps, IntImm, Drop and Const 184 (4.5%).

Inside the 24.6% base part (stepped at length 16 with 12 live items, scaled to the measured total): fallback ArrayPrepend about 14.6% of the run, freeing the result array about 3.3%, ArrayDrop on a shared array about 1.9%, the rest (call, return, length, element, dispatch) about 4.8%.

Inside the 20.7% of `is_free`: ArrayIndex 5.7%, Field reads 2.1%, freeing the `Some` 2.7%, JumpUnlessVariant 1.5%, Call plus Ret 2.7%, and about 6% for dispatch, Int adds, branches and the remaining drops.

**benches/rbtree/rbtree.sca: where the 38.8 billion instructions go.** rbtree retires 38.79G instructions after startup, against 16.81G for LuaJIT with the JIT off (2.31x). About 95% is building the tree: 7.55M ins calls, 9.33M is_red calls, 2.38M balance_left calls and 10.5M node constructions. More than half of the count is pattern matching. Every constructor pattern reads all five fields at 69 to 105 instructions each, then drops the unused ones at 38 to 74 each, and the or-patterns and nested patterns make the same node get tested and unpacked two or three times. The rest is calls (about 20%) and building and freeing nodes (about 20%); in-place reuse never fires in ins as written. Rewriting only the source so each node is matched once cut the total by 21.4% (38.83G to 30.50G, 1.81x LuaJIT), which is a measured floor on what the match compilation costs. Breakdown: Model built at n=30000 (measured 3129M after startup) from an IR op counter times probe-measured unit costs. The raw model sums to 3432M, which is 110% of measured, so the shares below are the model's shares scaled to 100%. Field reads in patterns: 30%. Calls and returns, tail calls included: 20%. Explicit Drop instructions: 17%. Fresh constructor plus its later free: 17%. Variant tests: 9%. Jumps, constants, compares, int ops: 4%. Perceus reuse (hollow plus overwrite): 3%. By function: ins 47%, balance_left 31%, is_red 14%, fold plus height plus closures 5%, the rest 3%. Unexplained: the model overshoots by a steady 10 to 16% on all four program variants I checked, and by 11% on a height-only loop (817 predicted, 733 measured per call). The deltas between variants match well (is_red rewrite: model -150M, measured -152M; ins rewrite: model -292M, measured -309M), so the Field, Drop and variant-test costs look right and the overshoot most likely sits in the call and constructor terms, where I guessed frame sizes (10 registers) and how many arguments are shared rather than moved.

**benches/richards/richards.sca: where the instructions go.** One richards iteration costs 1,404 million instructions (35,133M for 25, minus 39M startup), which is about 21,300 per task run and 1,468 per heap cell allocated. The program is written as "copy the record, copy the map, pass it on", and the VM pays full price for every copy: 956,437 cells are allocated per iteration and none are reused in place (both measured), because the task is always still held by the map and the map is always still held by the scheduler when they are rebuilt. About 57% goes to four operations: looking a task up in the map (17%), rebuilding the 8-field Task (17%), map::set on a shared map (14%) and rebuilding the Scheduler (10%). Array queue operations take another 18%, and underneath all of it each simple VM instruction costs about 70 to 80 machine instructions and each small cell about 450 to allocate and free. Breakdown: Per iteration, 1,404M instructions. Two independent accountings.

(A) By scheduler path, all measured with whole-step probes (path cost times measured path count). It sums to 1,397M, which is 99.5% of the real figure:
- handler steps (23,252): 550M, 39%
- device steps (27,884): 429M, 31%
- idle steps (10,000): 171M, 12%
- skipped tasks in schedule (40,814 at 3,587 each): 146M, 10%
- worker steps (4,654): 101M, 7%

(B) By operation, primitive probe cost times count. Counts are measured; items marked ~ use a per-op cost I composed from several probes, so trust them to about 20%. It sums to about 1,405M:
- find_task, a map lookup behind a call (139,849 x 1,711): 239M, 17.0%
- Task record copy (143,209 x ~1,650): 236M, 16.8%
- map::set on a shared map plus freeing the old one (99,029 x 1,951): 193M, 13.8%
- ~array queue work (head/tail match, append, prepend, `!= []`, fill): 250M, 17.8%
- ~calls, tail calls and the schedule loop itself, not counting find_task: 143M, 10.2%
- Scheduler record copy (139,849 x 968): 135M, 9.6%
- ~variant and Bool dispatch (act match, the two gates, priority compares, idle arithmetic): 100M, 7.2%
- Packet copy (44,173 x 998): 44M, 3.1%
- ~small constructors (TaskData 54,169, Some 23,250): 39M, 2.8%
- tuple build and take apart in handle (23,252 x 1,083): 25M, 1.8%

Cutting across (B), not added to it: allocating and freeing the 956,437 cells is about 380M to 430M, 27% to 31% (estimate from about 400 to 450 per cell; the sampling profile independently puts 31.7% of time in Heap::release, Heap::alloc and Heap::free_cell).

Unexplained: nothing large. (A) is short by 0.5%. That (B) lands within 0.1% is partly luck given the ~ items.

**benches/sieve/sieve.sca (Scarlet 328x LuaJIT-joff instructions).** Sieve runs 29.14 billion instructions after startup, and 96.4% of them (measured) are inside the body of `clear`, which the benchmark calls 498,105 times and which costs about 56,400 instructions each time. Scarlet has no write-at-index, so one flag write is two RRB slices, an append and a concat, plus freeing the old path; LuaJIT does the same write as one table store, and its whole benchmark averages about 94 instructions per element operation. The interpreter loop (calls, arithmetic, frames) is only about 4 to 5% here, so the general call cost is not what makes this benchmark slow. Inside the array code the instructions go to copying 32-slot nodes several times by value through SmallVecs, bumping and dropping a refcount on every child one at a time through bounds-checked heap lookups, rebuilding size tables, and malloc'd Vecs in the concat path: about 2,000 to 3,000 instructions per tree node rebuilt, where a memcpy of 66 words would be a few hundred at most. Breakdown: Measured, net of the 39.5M startup, total 29.14e9 (LuaJIT -joff net is 88.7M, ratio 328.6, which matches the table). Per `clear` call (498,105 calls = 45 runs x 11,069 strikes), by duplicating each operation inside the real benchmark and taking the difference: concat `[..x, ..after]` with the frees it causes 34,210 instr = 58.5%; slice `flags[at+1..size]` 10,590 = 18.1%; slice `flags[0..at]` 9,832 = 16.8%; append `[..before, False]` 1,776 = 3.0%. These four sum to 96.4%. The remaining 3.6% (1.04e9) is estimated from micro-probes: strike loop and the call to clear about 1.2% (498k x about 670), sieve loop with its index read about 1.2% (225k x about 1,500), make_flags appends about 1.1% (225k x about 1,440), which sums to about 1.0e9, so nothing meaningful is unexplained. A second view, by sample time rather than instructions (3,665 samples, profiling binary, 300 repeats): array::node 18%, array::rebalance 17% (about 6 points of it malloc/free of Vecs), array::branch 16%, Heap::release + free_cell 18%, array::own_all 9%, Machine::call itself 5%, merge 3%, tree_drop 3%, leaf 2%, Heap::alloc 2%, the rest small. memmove alone is 14% of samples, spread across those functions.

**benches/spectral_norm/spectral_norm.sca: where the instructions go.** The benchmark is one loop: the inner step of av_row/atv_row runs 20.0 million times and costs 1,999 instructions each time, which is 99.8% of the 40.05 billion measured (startup taken off). That step is 20 VM instructions at about 100 machine instructions each, and no single one dominates: eight Int operations take 26%, the array read 18%, the self tail call 15%, the call and return of `a` 14%, three Float operations 14%, and float::from_int 7%. The cost is spread across every instruction as dispatch (14 per instruction), saturating register-index arithmetic with bounds checks, a second jump table on the operator, helpers that are not inlined, and a chunked heap that re-checks bounds on every word read. The 2.89 ratio also flatters Scarlet, because LuaJIT itself spends about 320 of its 692 instructions per step on slow-path mixed int/float arithmetic in `A`; its clean fast paths are about 20 instructions per arithmetic op against Scarlet's 59 to 95. Breakdown: Measured from an lldb single-step trace of one inner iteration (1,999 instructions, exact, period repeated 14 times identically; the benchmark total divided by 20.0M steps gives 2,002). By VM instruction: 8 Int ops (7 in `a`, plus j+1) 519 = 26.0%; ArrayIndexOr x[j] 362 = 18.1%; TailCallSelf 293 = 14.7%; Call of `a` 163 + Ret 115 = 278 = 13.9%; 3 Float ops 280 = 14.0%; FloatFromInt builtin 137 = 6.9%; 2 Const loads 62 = 3.1%; JumpUnlessCmp 50 = 2.5%; Jump 18 = 0.9%. That adds to 1,999 = 100%. Cut the other way, across all 20 instructions: dispatch is 280 = 14.0% and inlined register get/set/take (saturating add plus bounds check plus release check) is about 200 = 10%. Unexplained: about 0.2% (3 instructions per step), which is the outer av/atv rows, ArrayAppend, dot, sqrt and printing; I did not measure those separately.

**benches/storage/storage.sca: where Scarlet's 54.6 million instructions per round.** Storage costs 27.32 billion instructions after startup, which is 54.6 million per round and about 10,000 per tree node, against about 2,600 per node for LuaJIT with the JIT off. Almost half (45%) is the 27,880 array appends per round: an append to an array nothing else holds still costs 790 instructions, because it allocates a new leaf one element longer, copies the old one twice, frees it, and goes through a share-then-release and two small-vector hops on the way in. The rest is spread thin: the tail-call loop step around each append (11%), freeing cells (about 13% between drops and the end-of-round tree free), the Built record used to return four values (about 10%, overlapping the freeing), and plain register instructions at 30 to 80 instructions each. I got these numbers by single-stepping 230,000 machine instructions of the real benchmark under lldb and grouping them by VM instruction; multiplied by exact per-round counts they add up to 99.8% of the measured total. Breakdown: Per round (54.64M measured; the parts below sum to 54.51M, 99.8%). Each line is (measured instructions per VM instruction from the lldb trace) x (exact count per round from the program's shape: 5,461 nodes, 4,096 leaves, 1,365 inner nodes, 27,880 appends).
- ArrayAppend: 45.2% (22,419 in-place appends x 790 = 17.7M; 5,461 first appends to an empty array x 1,284 = 7.0M)
- TailCallSelf (the loop step of empty_slots and children): 10.7% (27,880 x 209; 185 with 2 args, 321 with 6)
- Call + Ret (build, next, empty_slots): 7.3% (13,653 x (176 + 118))
- Drop: 7.3% (70,777 x 56 average; 30 when the register holds no cell, 253 to 341 when it frees a Built)
- IntImm arithmetic: 6.4% (59,281 x 59)
- Freeing the whole tree at the end of each round: 5.5% (16,384 cells x 183)
- Ctor (Node and Built): 5.4% (10,922 x 268)
- Field reads: 3.3% (21,840 x 82)
- Compare-and-jump: 2.8% (38,802 x 40)
- MakeArray (empty array): 2.4% (5,461 x 238)
- Const 1.3%, Int 1.1%, TailCall into children 1.0%
- Unexplained: 0.2%

Cross-cutting views of the same instructions (they overlap the lines above, do not add them): instruction fetch and dispatch is 7.5% (about 12 instructions x 342k VM instructions per round); Heap::alloc + free_cell + release + memmove are 22% of the traced window; bounds-checked heap word access (onward, word, words and their _mut twins) is 6.3%; returning four values through a Built record is about 9.8%.

Measured two ways that agree: probes give 10,115 per leaf and about 9,800 per inner node, so 4,096 x 10,115 + 1,365 x 9,800 = 54.8M (100.3%).

**benches/string_keys/string_keys.sca: where Scarlet's 28.70 billion instructions .** About 74% of the instructions go on building the words and only about 18% on the map. Measured by ablation, the benchmark with the map removed and the words still built costs 23.7 of 28.7 billion. One letter of `spell` costs about 2,250 instructions, and there are 8.5 million letters. Inside a letter the big items are the Concat instruction (about 840), the self tail call that frees two string cells and walks the frame (631), the `''` default literal that is allocated and freed every step and never used (about 355), and the array index (290 plus 63 for the global). A string-keyed map lookup costs about 1,650 to 1,900 instructions against 362 for an Int key, because hashing and comparing each copy the key into a freshly malloc'd Vec. Time is spread differently: MapFind is 27% of samples but about 11% of instructions, since the word-building code runs at about 6.7 instructions per cycle and the map walk misses cache. Breakdown: Share of the 28.70 billion instructions (startup taken off), built from an exact per-VM-instruction trace times how often each runs. The trace ran under lldb, where each malloc/free pair cost about 124 more than in a normal run (see the scratch-Vec finding), so I took that off; with it off, the model gives 14,629 per count iteration against 14,656 measured, and 13,866 per probe iteration against 13,855 measured.

- Concat in `spell` (8.5M): about 24.8%
- TailCallSelf in `spell`, including freeing the old `acc` and the `''` cell (8.5M x 631): 18.7%
- Call overhead and arithmetic for `next`, `word`, the `spell` exit and the two outer loops (2M iterations x about 1,900 to 2,050): 13.8%
- Map get (2M x about 1,600): 11.1%
- ArrayIndexOr (8.5M x 290): 8.6%
- Int arithmetic, compare and jumps inside `spell` (8.5M x 238): 7.0%
- Str `''` allocation (10.5M x 185): 6.8%
- Map set (1M x about 1,930): 6.7%
- GetGlobal `letters` (8.5M x 63): 1.9%
- map::fold, println and teardown: 0.7%

That adds to about 100%. Cut the other way, across those rows: system malloc/free for scratch Vec<u8> is about 14.5%; register-release work plus VM cell free is about 12%; central dispatch (15 instructions x about 58 VM instructions per iteration) is about 6%. Nothing material is unexplained. The one soft number is the 124 malloc correction, which is inferred from the gap and from the profile, not measured on its own.

**benches/strings/strings.sca: where Scarlet's 30.6 billion instructions go (255,0.** The benchmark runs 30.63 billion instructions after startup, which is 255,000 per row. About a third is string concatenation: the batch join and the stdlib `string::replace` are both a fold of `acc + sep + part`, so they copy the whole 2.4 KB text about 100 times per batch, and each copy goes byte by byte out of the heap's u64 words into a growing Vec and back in through a memcpy call per word. About a fifth is `string::split`, which copies both arguments into Rust Strings, validates UTF-8, makes a Rust String per piece and then a heap string per piece and an array. Another fifth is walking arrays with `[x, ..rest]`, which costs about 2,100 instructions per element on the short arrays this benchmark makes, because the drop copies the rest of the tail leaf into a new leaf and root every step. The remainder is ordinary call cost (about 72 calls per row at 350 to 520 each), string literals that allocate a fresh heap string every time they are evaluated, and the other string built-ins, which all copy the string out before looking at it. Breakdown: Total measured: 30,632 M instructions after subtracting startup = 255.3k per row (120,000 rows). LuaJIT -joff measured on the same machine: 8,790 M.

A. Breakdown by part of the program. This is measured by ablation (cumulative variants of the benchmark in the scratch directory, files v0..v4 and r0..r8), and it adds up exactly:
- batch_checksum: 7,974 M = 26.0% (3.32 M per batch of 50 rows)
- fold over fields with field_value: 5,100 M = 16.6%
- array::map(field_text): 4,791 M = 15.6%
- string::replace(row, '=', ': ') + length: 4,753 M = 15.5%
- join(texts, '|') + length + mix: 2,797 M = 9.1%
- row build by interpolation: 1,504 M = 4.9%
- digits (2 to_string, concat, from_string, 2 mix): 958 M = 3.1%
- split(',') of the row: 713 M = 2.3%
- two contains + two count: 616 M = 2.0%
- padded = '  ' + row + ' ': 590 M = 1.9%
- loop, 3 next calls: 313 M = 1.0%
- row_checksum call, trim, batch append: about 530 M = 1.7%

B. Breakdown by cause. This is an estimate: per-operation costs from micro-probes (measured) multiplied by how often each runs per row (counted from the IR). It sums to about 101%, so there is some double counting of a few percent between the rows.
- String concatenation: about 33% (84k per row). 20.5% is the quadratic batch join and batch replace (2.62 M per batch); 12.5% is the 17 small concats per row at about 1.9k each.
- string::split: about 21% (55k per row). 10 splits per row making 25 pieces, about 47k; three big splits per batch, about 7.4k per row.
- Array destructuring `[x, ..rest]`: about 18.5% (47k per row). 20 steps per row at about 2.1k, and about 151 steps per batch.
- Calls and returns: about 10.5% (about 72 per row at 350 to 520).
- int to and from string: about 4.6%.
- String literals allocated on every evaluation: about 3% (about 24 per row at 320 to 430).
- Array building (map's appends, MakeArray, batch append): about 2.6%.
- Interpolation's own concat and the two word lookups: about 2.5%.
- contains: about 1.8%. length: about 1.5%. trim: about 0.6%.
- Arithmetic and the loop itself: about 1.2%.

Not explained: the fixed cost of a small concat is about 1,650 instructions and I can account for roughly 900 to 1,000 of it by reading the code (Vec growth, two is_string calls, cell alloc, cell release). The fixed part of split (about 2,800) is likewise only partly itemised. The sampling profile gives time shares, not instruction shares, so the split inside one built-in is approximate.

**benches/tak/tak.sca: where the instructions go.** Measured: tak costs 38.42 billion instructions after startup for 101,266,770 tak calls, which is 379.4 per call, against LuaJIT's 112 (JIT off). An lldb single-step trace of 79,759 instructions in steady state reproduces this to within 0.1 (379.3) and splits it over the five VM instructions the function uses: CallSelf 35.5%, Ret 24.5%, TailCallSelf 14.6%, JumpUnlessCmp 13.8%, IntImm 11.7%. Nothing touches the heap or reference counts; all of it is frame setup and teardown, copying arguments through bounds-checked register reads and writes, sweeping the whole frame to Nil on every return and tail call, and a 15-instruction dispatch. Scarlet runs only 3.5 VM instructions per tak call where LuaJIT runs 7 bytecodes, but each Scarlet one costs about 108 machine instructions against LuaJIT's 16. Breakdown: Measured from the trace, per tak call (379.3 instructions, 3.5 VM instructions). By VM instruction: CallSelf 0.75 x 179.3 = 134.5 (35.5%); Ret 0.75 x 124 = 93 (24.5%); TailCallSelf 0.25 x 221 = 55.3 (14.6%); JumpUnlessCmp 1.0 x 52.3 = 52.3 (13.8%); IntImm 0.75 x 59 = 44.3 (11.7%). That sums to 379.3 against 379.4 measured with /usr/bin/time, so nothing is unexplained.

The same instructions cut by cause (hand-counted from the per-handler listings, so each line is good to a few instructions): argument passing 73 (19%); sweeping the frame to Nil on Ret and TailCallSelf 62 (16%); dispatch 52.5 (14%); compare-and-branch work other than dispatch 37 (10%); IntImm work other than dispatch 33 (9%); prologue/epilogue and buffer hand-off of the out-of-line enter_above and call_args 30 (8%); pushing and popping the 40-byte Frame on the callers Vec 23 (6%); Ret moving the result out and into the caller's register 20 (5%); enter_above's empty leave, open and zip setup 19.5 (5%); env share/release checks and frame_end 8 (2%); the remaining ~21 (6%) is setup inside `again` and small glue I did not itemise. Across all of these, bounds-checked register access (saturating add, compare with regs.len reloaded from memory, branch, reload regs.ptr) is roughly 5 of every 7 to 9 instructions per register read or write; that share is inferred from the listings, not separately measured.

**benches/towers/towers.sca: where the instructions go.** Towers costs 7,803 instructions per disk move (38.35 billion over 4,914,600 moves, startup taken off), against about 904 for LuaJIT with the JIT off. I single-stepped 390,000 instructions of the running benchmark under lldb (50 whole moves, 7,789 per move, so the trace matches the full run) and attributed every instruction: a move runs 61 VM instructions at 127 machine instructions each. Seen by source construct, 46% is the three `Towers(..t, x: y)` record copies per move (13 field reads, a full free of the old record, a 5-field constructor), 27% is the 7 calls and returns, and the rest is matching, the remaining field reads and drops, and the Disk cell. Seen by mechanism, the same total is 15.5% chunk-table heap addressing, 15.5% frame setup and argument passing, 13.8% reference counting, 11.4% bounds-checked register access, 11.0% dispatch, 6.7% allocator, and 26% handler bodies. Frame size is not a cause: a function with 34 registers costs the same to call as one with 4. Breakdown: Measured from the lldb single-step trace (389,468 instructions, 50 moves, 7,789 per move). Both splits add to 100% by construction.

By source construct, per move:
- Record updates, 3 per move: 13 Field reads (about 1,120) + 3 Drops that free the old record (1,485, which includes about 157 for freeing the popped Disk) + 3 five-field Ctors (1,016) = about 3,620, 46.5%.
- Calls and returns, 7 calls per move: Call 850 + CallSelf 99 + TailCall 445 + TailCallSelf 128 + Ret 593 = 2,115, 27.2%.
- Matching and branches: JumpUnlessInt 263 + JumpUnlessVariant 275 + JumpUnlessCmp/Imm 93 = 631, 8.1%.
- Other Field reads (get_pile's two, and the Disk size/next binds, 6 per move): about 640, 8.2%.
- Other Drops (7 per move, 30 for an empty register or 83 for a decrement): about 300, 3.8%.
- Disk constructor (1 per move, 254): 3.3%.
- Int arithmetic and constants (IntImm 2, Int 1, Const 0.5): 200, 2.6%.

By mechanism, per move:
- Handler bodies (tag tests, kind checks, the argument buffer's Vec pushes, spills; 471 per move has no source line): 2,030, 26.1%.
- Heap addressing (Heap::onward/word/set_word chunk-table lookups): 1,210, 15.5%.
- Frames (enter_above, enter, leave, open, call_args, caller stack push/pop): 1,208, 15.5%.
- Reference counting (share, release, the dying list): 1,072, 13.8%.
- Register access (Machine::get/set/take/at): 890, 11.4%.
- Dispatch (fetch, bounds check, jump table): 856, 11.0%.
- Allocator (alloc, free_cell, free lists, ctor write): 524, 6.7%.

Nothing is unexplained at the instruction level. What I did not measure is LuaJIT's side: its 904 per move is a total from /usr/bin/time with its startup not taken off, and I did not break it down.

**The interpreter loop: Machine::call in crates/scarlet_vm/src/exec.rs and the Ins.** I single-stepped one turn of the bare loop under lldb and got an exact trace with a period of 346 machine instructions, which matches the 346 that /usr/bin/time measures. The loop runs four VM instructions (JumpUnlessCmpImm, IntImm Sub, Int Add, TailCallSelf). The tail call alone is 162 of the 346, dispatch is 60 (15 per VM instruction), register reads and writes with their bounds checks are 59, small-Int tag checks and re-tagging are 32, a second jump table on the IntOp is 18, and the real compare, subtract and add are 9. Nothing here is one big mistake: every step pays for safe-Rust bounds checks, state kept in memory instead of machine registers, a generic argument buffer, and a full clear of the frame on every turn. LuaJIT's interpreter does the same four steps in roughly 105 instructions by my count from its source. Breakdown: Bare loop `go(n - 1, acc + n)`, 346 instructions per step, every one placed from the lldb trace (scratch/fat/loop/iter_annot.txt).

By VM instruction: TailCallSelf 162 (47%); Int Add 55 (16%); IntImm Sub 44 (13%); JumpUnlessCmpImm 25 (7%); dispatch 4 x 15 = 60 (17%).

By cause, adding to 346:
- Tail call, 162 (47%): call_args 75 (17 are its prologue and epilogue, 5 take the `passing` Vec out of self, 11 set up, 2 x 19 per argument, 4 write the Vec back); frame_end and top check 12; slice the frame and test `carried` 13; clear 3 registers 24 (8 each); copy 2 parameters 24 (6 set up, 9 each); `passed` 8; handler entry 6.
- Dispatch, 60 (17%): 15 per VM instruction.
- Register reads, 33 (10%): 4 reads at 7 to 9 each.
- Register writes, 26 (8%): 2 writes at 13 each (7 to index and bounds-check, 6 to store and test whether the old value was a cell).
- Small-Int tag checks and sign-extension 18 (5%), plus 48-bit range check and re-tag 14 (4%).
- Second jump table on IntOp, 18 (5%): 6 per op.
- Loading op, immediate and jump target from the instruction, 6 (2%).
- The actual compare, subtract and add, 9 (3%).

Call and return (probe with a one-argument identity function, 223 extra per step measured and traced): Call 120 + Ret 73 + 2 dispatches 30.

**Calls, returns and frames in crates/scarlet_vm/src/exec.rs (Machine::call, enter.** No single thing is expensive: a fib call is 4.5 VM instructions at about 79 machine instructions each, and a tak call is 3.5 at about 108. I single-stepped the profiling binary under lldb and got exact per-handler counts that add back to the measured totals: fib = CallSelf 124 + Ret 97 + IntImm 59 + JumpUnlessCmpImm 40 + half an Int 70 = 355 (measured 355.5), and tak = 379 (measured 380.1). The call and return machinery itself is 191 of fib's 355 and about 257 of tak's 380, against roughly 55 for LuaJIT's whole CALL + FUNCF + RET1 path (read from its source, not measured); the rest is 15 instructions of dispatch per VM instruction and bounds-checked register reads and writes. Nothing is allocated per call and there is no Rust recursion per Scarlet call (Machine::call is entered only from Machine::run); the cost is argument copying one checked register at a time, clearing every register of the frame on return, a 40-byte Frame pushed to and popped from a Vec, and tail calls that copy arguments out to a side buffer and back. Breakdown: fib, 355 per call (exact, from the lldb trace; 1 CallSelf, 1 Ret, 1 IntImm, 1 JumpUnlessCmpImm, 0.5 Int per call): call side 109 (31%) = enter_above 73 [save/restore 20, no-op leave 11, frame_end+open 6, param loop setup 9, moving the one argument 27] + push Frame and swap 21 + share env, frame_end, call setup 15; return side 82 (23%) = take result 13 + leave 38 (20 fixed + 9 x 2 registers) + release env 3 + pop Frame 14 + store result in caller 12 + 2; dispatch 67.5 (19%) = 15 x 4.5; arithmetic and compare bodies 96.5 (27%) = IntImm 44 + compare 25 + half of Int 55, of which about 43 is bounds-checked register get/set, 15 is the second jump table on the operator, and the rest tag check, overflow and range check, rebox.
tak, 380 per call (0.75 CallSelf at 179, 0.25 TailCallSelf at 221, 0.75 Ret at 124, 0.75 IntImm at 59, 1 JumpUnlessCmp at 52): non-tail calls 123 (32%), tail calls 52 (14%), returns 82 (22%), dispatch 52 (14%), arithmetic and compare 70 (18%).
LuaJIT -joff for the same programs, measured: 122 per fib call, 112 per tak call, everything included.

**Value representation and the heap (crates/scarlet_vm/src/value.rs, word.rs, heap.** A Value is one 8-byte NaN-boxed word, and floats and 48-bit Ints are unboxed, so the representation itself is sound. The cost is in how the word is used. A small Int add costs 70 instructions and a Float add 92, against about 18 for LuaJIT's ADDVV, and only about 20 of the 70 are tag checks, the overflow check and retagging. The heap is the bigger problem: a cell is named by (chunk, word) instead of by address, so every touch of a cell pays 11 to 13 instructions and three bounds checks. A field read costs 77, a release that frees nothing costs 51, freeing a 2-field cell costs about 220, and allocating one costs 252. Tuples, closures and arrays also malloc and free a temporary Vec on every construction, which is 382 of a tuple's 601 instructions. Breakdown: Estimated from time samples of the profiling binary, taken by pid (these are samples, not instruction counts, and only the out-of-line functions can be told apart; everything else is inlined into Machine::call). mandelbrot: about 41% in the out-of-line float helpers (float::op 24%, Machine::float 16%), 10% in call_args, about 48% in Machine::call (dispatch, register access, compares). nbody: 22% float helpers, 15% heap release, alloc and free, 4% memmove, 6% call setup, about 48% Machine::call, the rest array code. binary_trees: 18% heap (release 11%, free_cell 4%, alloc 3%), 6% enter_above, 76% Machine::call, which holds the inlined field reads, shares and constructor writes. richards: 30% heap, 5% memmove, 4% map copy. storage: 35% heap, 9% memmove, 6% array push. For a single small Int add the measured split of its 70 instructions is: 21% main dispatch, 31% register addressing and bounds checks for three operands, 11% second dispatch on the op, 30% tag checks, untag, range check, retag and the release test, 7% the add and jumps.

**Reference counting as the compiler emits it (perceus pass, Drop/DropReuse/reuse,.** Reference counting costs roughly a quarter of the instructions on the allocation-heavy benchmarks: my estimate is about 27% of rbtree's insert phase, 23% of list and 11 to 20% of binary_trees. The compiler emits no dup instruction at all, so the VM adds a reference on every field read and every non-moved argument, and Perceus then emits a Drop to take it back; there is no borrow inference, no drop specialization and no dup/drop fusion, so a unique-path walk that Koka runs with zero count operations runs 210 dups and 444 Drops per rbtree insert here. 59% of those Drops do nothing, because they name a Bool, a field-less enum, a Nil or a register a move already emptied, and each still costs about 35 instructions of dispatch. Reuse is found far less often than in Koka (rbtree makes 33 cells per insert and reuses 2; Koka's fast path allocates about 1), because a nested pattern loses the scrutinee's shape and every `letc` continuation starts with an empty token stack, but a missed reuse only costs about 185 instructions, so it matters less than the count traffic and the repeated field reads that cause it. Breakdown: rbtree insert phase, measured 124,700 instructions per insert and 1,467 VM instructions per insert (exact path model, validated against cells_made). Estimated split, per-op prices from micro-probes: field reads 508 x 77 = 31% (about three quarters of them are re-reads of the same node by alternative match arms, and each read of a child also dups it); dup + non-last release pairs 145 x 72 = 8%; Drop instruction dispatch 444 x 35 = 12% (of which 264 no-op drops = 7.4%); dups that are undone inside a free or hollow about 3%; free + alloc where reuse was possible 23 x 185 = 3.4%; calls and returns (51 per insert) about 10%; constructor building about 6%; variant tests about 6%; the rest (dispatch of other instructions, compares, consts, jumps) about 20%. Reference counting in total (pairs + all Drop dispatch + dup-then-free + missed reuse) is about 27%. list: 21.8M instructions per benchmark() call, 45.1K dup/drop pairs x 107 + 7.9K no-op drops x 35 = about 23%. binary_trees: 1,841 instructions per node, 4 drops and 1.5 dups per node, about 11% avoidable count work plus about 10% for the free and alloc every runtime pays.

**Arrays in the Scarlet VM: crates/scarlet_vm/src/array.rs, the array instructions.** Scarlet has no write-at-index, so sieve pays 58,435 instructions per flag write (two slices, an append and an RRB concat on a 5,000-element tree) where LuaJIT pays about 180 all-in, and that one gap is the whole 328x. In permute, queens and fannkuch about 40% of instructions are ArrayPrepend, because rebuilding with `[h, ..rest]` after a `[h, ..t]` walk always misses the in-place path and copies a leaf (2,300 to 3,400 instructions against 480 in place). Underneath both, every array primitive is 10 to 100 times dearer than it needs to be: a node is copied onto the stack as a 264-byte SmallVec several times per operation (memmove is 9 to 30% of instructions), every heap word is reached through a chunk-index-plus-bounds-check lookup, leaves are exact-sized so even the "in place" append allocates and frees a leaf, and every branch carries a size table that is rebuilt by visiting each child. Even the cheapest operations are heavy: a length is 157 instructions, an index read 238 to 392, an in-place append 823, and one step of a `[h, ..t]` walk 1,045. Breakdown: sieve (29.1G instructions, 498,105 flag writes, 58,435 per write; one `clear` on a 5,000-element array stepped exactly at 55,228): ArrayConcat 51% (28,064), the two ArraySlice 26% (14,503), releasing the intermediate arrays and Ok cells 17% (9,256), ArrayAppend 2% (1,060), Ok/match/dispatch/loop 4%. By function inside that: array::branch 18%, Heap::release 17%, memmove 14%, own_all 7%, node 6%, malloc/free for Vecs in merge/rebalance about 10%. Building the flags and the scan loop are under 2% of the run.
permute (31.0G, 15,383 per swap; from a 120,000-instruction single-stepped window, so approximate): ArrayPrepend 43%, register release and frame clear 10%, dispatch 7.5%, ArrayDrop 5%, ArrayIndexOr 4%, ArrayElem 3%, ArrayLen 3%, record Ctor/Field/DropReuse 8%, calls and returns 11%, the rest 5%.
queens (35.9G; same kind of window): ArrayPrepend 40%, dispatch 10%, ArrayDrop 8%, register release 8%, ArrayElem 6%, ArrayLen 6%, calls and returns 12%, ArrayIndex (Bool, boxed in Some) 3%, the rest 7%.
fannkuch (27.7G; same kind of window): ArrayPrepend 36%, register release 14%, ArraySlice 13%, ArrayConcat 10.5%, dispatch 5%, tail calls 5%, ArrayIndexOr 5%, ArrayAppend 2%, the rest 9%.
All probe programs, the lldb stepping scripts (step.py, iter.py, win.py, linemap.py) and their outputs are in scratch/fat/arrays.

**Strings and binaries in the Scarlet VM: crates/scarlet_vm/src/binary.rs, the str.** Almost every string and binary operation copies its operands out of the heap into a fresh Rust Vec one byte at a time, works on the copy, and copies the result back into a new cell. A string therefore costs about 6 to 18 instructions per byte for work that is a memcpy or a memcmp in Lua or the BEAM, and a binary costs 55 to 313 per byte because each byte goes through a bit-offset reader. On top of that, every call has a fixed cost of 650 to 4,000 instructions even for 8 bytes, most of it system malloc/realloc for the temporary and the cell alloc and free. The two quadratic costs (replace and join) are stdlib functions written as a Scarlet fold of `acc + x + piece`; they should be built-ins, and together with the fold's own per-piece overhead they are about half of the strings benchmark. Binary integer segments (`<<x:8, rest:binary>>`, `<<n:8>>`) go through num_bigint and cost 2,000 to 2,600 instructions for one byte. Breakdown: benches/strings/strings.sca runs 30.67 billion instructions, which is about 256,900 per row after startup; LuaJIT with the JIT off runs 8.79 billion, about 73,000 per row. I measured the split by deleting one statement at a time from a copy and recounting (scratch: scratch/fat/strlib/abl/abl.py). Per row: rebuilding the fields with array::map(field_text) then join('|') is 63,900 (25%); the fold that splits and parses each field is 43,200 (17%); string::replace(row, '=', ': ') is 40,000 (16%); string::replace on the 2.3 KB batch text is 29,400 (11%); the batch join with '\n' is about 27,000 (10%, by subtraction); the two batch splits plus the contains fold are 10,900 (4%); the two row contains calls are 5,700 (2%); the rest of row_checksum (pad, trim, split on ',', the digits round trip) is about 21,000 (8%, by subtraction); and run() itself (the interpolated row, the word lookups, the array append) is about 15,000 (6%, by subtraction). So replace is 27% and join is roughly 20% of the benchmark. A time profile of the same run (not instructions) puts about 55% in read_string, memmove/memcpy, Heap::string, Heap::alloc, Heap::release/free_cell and the system malloc family, and about 16% in Machine::call.

**Maps and hashing in the Scarlet VM (crates/scarlet_vm/src/map.rs, hash.rs, eq.rs.** A unique map is updated in place, but the in-place path is still heavy: on a 200,000-entry Int map a delete costs about 870 instructions and an insert about 905 inside map.rs alone, against 136 to 182 for the delete-and-reinsert pair in LuaJIT's interpreter. String keys are worse because the hash is never cached and both hashing and key equality copy the string into a freshly malloc'd Vec: a string get costs 1,550 to 1,800 instructions, and 43 to 46% of that is the system allocator. Neither benchmark is mostly maps, though: map_churn spends about 40% in its `next()` random-number function (whose product leaves the 48-bit small-Int range and goes through i128 and a heap big-int), and string_keys spends about 80% building the words. Two smaller things I found on the way: `map::set` in tail position is not recognised as owned (so `map::from_list`, `merge`, `filter` and any fold-with-set copy the whole path per entry, 4 to 6 times dearer), and `map::fold` costs about 3,000 instructions per entry, most of it in `array::fold`. Breakdown: map_churn (19.72G instructions after startup, measured by ablation): about 40% is the `next()` generator (8.6M calls at 909 instructions each; 22 points of that is only because x*69069 exceeds 2^47 and takes the i128/big-int path); about 32% is the churn loop's delete+insert (3M pairs at about 2,100); about 10% is map::get (4.8M at about 430); about 1% is the fill; about 17% is the rest of the loops (calls, arithmetic, tuple). string_keys (28.68G after startup, measured by ablation): about 80.5% is word building and loop overhead with no map work at all (23.08G with every map call removed); about 19.5% is map work (5.6G), which I estimate from the stepped profiles splits as roughly 8 points malloc/free of the temporary Vecs in hash and eq, 4 points SipHash and its string copy, 3 to 4 points string equality proper, 3 to 4 points trie walking, in-place insert and instruction dispatch, and 0.6 points map::fold (0.18G, measured).

**Code quality: what the compiler and the loader (crates/scarlet_vm/src/code.rs) h.** For scalar code the instruction stream is already as dense as LuaJIT's: fib runs about 4.5 VM instructions per call against LuaJIT's 6.5 bytecodes, tak 3.5 against 6, and the mandelbrot inner loop 15 against about 16, because the loader already folds small Int constants into IntImm and fuses Int compares into JumpUnlessCmp(Imm) (the `let %1 = c0` lines in `scarlet dis` are Core IR, not what runs). On those three the gap is the price of each instruction (79 to 108 machine instructions per VM instruction against LuaJIT's 19 to 22), which no instruction-set change in my area removes. Code quality is what separates the 7x to 8x benchmarks (bounce, nbody, closures, richards) from the 3x ones: tuples used as multiple values are really allocated (705 instructions to build and drop a pair), a record update on a parameter copies every field into a new cell with no reuse (1297 for a 7-field record), small functions and lambdas passed to known functions are never inlined (223 to 594 per call layer), and a self tail call moves every argument (36 each) even when it does not change. Smaller, real, and cheap to fix: no Float immediates or fused Float compare (12% of mandelbrot), every array pattern loop tests the length twice (165 per element), no common-subexpression removal for field reads (4.3% of nbody), and matches run as a linear chain (40 per failed arm). Breakdown: Measured totals, startup taken off (instructions): fib 10.6G, tak 38.4G, bounce 37.7G, nbody 22.7G, mandelbrot 37.9G, closures 26.4G, richards 35.1G. LuaJIT -joff on the same machine: 3.64G, 11.35G, 4.50G, 2.83G, 9.97G, 3.18G, 4.81G.

mandelbrot (measured by probe, 26.64M inner iterations at 1384 each = 97% of the run): 9 Float instructions about 60%; the 6-argument self tail call plus loop test and z+1 about 31% (433); two Float constant loads 5.6% (78); Bool made by FloatGt then tested by JumpIfFalse instead of one fused instruction about 6.5% (90). Float-from-int and xor builtins under 1%.

bounce (7524 per ball-step; LuaJIT 900): three tuples per ball-step about 35 to 38% (measured: removing the two 3-tuples by hand saves 15.6% net of the extra compares it adds; also inlining `bounce` into `step`, which removes the pair it returns, takes the run from 37.7G to 23.75G, 37% less); persistent array append/drop/len/elem about 30% (sampling: ArrayAppend 19%, ArrayDrop 7%); the call and return of `bounce` about 3.5%; drops of Bools about 1.6% (inferred); the rest is about 40 scalar instructions at 50 to 90 each.

nbody (226k per step): Float instructions 29% of samples; the Newton sqrt loop about 30% (about 100 iterations per step at about 640, measured by probe); arrays about 23% of samples; record rebuilds (25 seven-field constructors per step plus 100 field copies) about 12% (inferred from the 766 and 80 probes); duplicate field reads 4.3% (measured by hand CSE); 15 tuples per step about 5 to 6% (inferred).

closures: the fold wrapper lambda under array::map/filter 9.6% (measured); calling literal lambdas through CallValue instead of inlining them into a specialised loop another 10.4% (measured); ArrayAppend 23% and ArrayDrop 9% of samples; the rest is closure calls the program really asks for (compose pipeline, counter).

richards (sampling only): Heap::release 24%, map builtins about 11%, Field 9.5%, Ctor 6%, dispatch 8%. Statically 181 Field reads and 112 constructors with only 7 reuses, so most of it is records rebuilt field by field; I did not measure a rewritten richards.

fib and tak: nothing to remove in the instruction stream (0 constant loads, all compares fused). 355 and 380 per call are all per-instruction and per-call cost.

**Closures and higher-order calls in the Scarlet VM (crates/scarlet_vm/src/exec.rs.** Making a closure is not the problem: a 1-capture closure costs about 418 instructions to make and 199 to free, which is less than LuaJIT's interpreter spends (about 700), and closure creation is under 1% of the closures benchmark. The cost is in calling them. A closure call and return is 383 instructions against 232 for a known call and about 71 in LuaJIT, each capture read is 70 to 90 against 13, and those three things are about 41% of the benchmark. Another 35% is not closure work at all: it is the persistent-array walk (`[h, ..t]`, about 770 per element) and append (`[..acc, x]`, 800 to 1,000) that array::fold, map and filter are written with. The stdlib adds a further layer, because map and filter are built on fold with a wrapper lambda, so every element pays two closure calls where one would do (about 520 extra per element). Breakdown: closures benchmark, 26.36G instructions after startup (LuaJIT with JIT off: 3.18G, ratio 8.29). By VM instruction, from single-step traces of each phase weighted by that phase's measured size: closure call and return 32% (CallValue 15.3%, TailCallValue 7.7%, Ret 9.1%); capture reads (GetCapture) 8.8%; array walk 20.7% (ArrayDrop 9.5%, ArrayLen 5.9%, ArrayElem 5.3%); ArrayAppend 14.8%; the fold loop's TailCallSelf 6.1%; Int arithmetic 6.4%; Drop 5.6%; compare, const and jump 4.3%; closure and constructor creation about 1%. Cutting the same total another way: instruction dispatch (14 per VM instruction, 188M VM instructions) is 10%, and Heap::release is 9%. By phase: first map 5.41G (3,606 per element), filter 4.95G (3,302), direct fold 2.51G (1,954), map over the composed pipeline 10.39G (8,077 per element, 10 closure calls each), second fold 2.19G (1,705), counter 0.86G (2,863 per step).

