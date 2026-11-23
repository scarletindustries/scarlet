# Speed design: The interpreter core: instruction format, dispatch, registers, calls, returns, tail calls, Int and Float instructions

This is the design for one area of the speed work, written on 23 November 2026. `docs/perf-plan.md` puts its steps in order with the other areas' steps, and settles where the areas disagree. Where this file and the plan differ, the plan is right.

# The interpreter core to build

This is the whole design. I checked every critique in the code or in a probe before changing anything, and the probe for this version is in `perf-scratch/final-core/`.

## The design in eight sentences

1. A frame is a window of 256 registers on one stack. An instruction names a register with a `u8` and the window is a `&mut [Value; 256]`, so the types prove every access is in range and an access is one load or one store.
2. A callee's frame starts where its caller's frame ends, as it does today. The call instruction copies its arguments into the callee's parameter registers and writes one link word there. There is no frame record and no frame list.
3. An instruction is 16 bytes and all code is one array. Each operator is its own instruction, and a constant, a jump target or up to six argument registers sit inside it. One enum, generic over its register type, is used by the loader and by the loop.
4. Int and Float instructions check what they were given when they run. The Int check is a real tag test. The Float check is one test of the result, which the no-NaN rule needs anyway.
5. The compiler decides who owns each reference, once, in Perceus. Core IR says "move" or "copy" on every operand that hands a reference on, and has a `Drop` wherever a value dies. The loader only encodes that.
6. A verifier in the VM checks the encoded instructions at load, and only verified code can run. It checks another crate's work, as the JVM's and WebAssembly's verifiers do.
7. A tail call to a known function is a parallel move into the parameter registers and a jump.
8. The build order keeps reference counting right by construction until the last interpreter step. Windows, typed instructions and tail calls land first with today's sweeps kept, and ownership lands last and can be reverted alone.

## What the probe measures

Each number is machine instructions retired per unit of work, taken as a slope between two sizes. Cycles are from the same runs. "140 arms" is the same loop with 48 more arms of realistic shapes, which is nearer the real loop's size.

| | today | steps 1 to 5 (sweeps kept) | all steps, 92 arms | all steps, 140 arms | LuaJIT, JIT off |
|---|---:|---:|---:|---:|---:|
| fib, instructions a call | 355 | 164 to 168 | 112 | 118 | 122 |
| fib, cycles a call | 51.6 | 35 to 36 | 25.5 | 29.5 | 21.1 |
| tak, instructions a call | 379 | 191 to 195 | 101.5 | 106 | 112 |
| tak, cycles a call | 53.0 | 36 to 37 | 22.5 | 23.7 | 21.4 |
| bare loop `go(n - 1, acc + n)`, arguments scheduled | 346 | 93 to 94 | 87 | 87 | |
| the same loop in today's Core IR order | 346 | 113 to 115 | 102 | 102 | |
| mandelbrot `escape`, a turn | 1,364 | 322 to 328 | 256 | 257 | about 358 |
| call and return of `id(x)`, argument moved (`Call1` 28 + `Ret` 23) | 223 | | 51 | 56 | 71 to 85 |
| call and return of `id(n)`, `n` kept live (`Call1D` 32 + `Ret` 23) | 223 | 112 to 119 | 55 | 60 | |
| call of `fn(x) { x + k }` held in a register | 523 | not measured | 203 | 206 | |
| `x = (x * 69069 + 1) % 2^32`, a step | 967 | 154 to 156 | 140 | 141 | |

So fib and tak end level with LuaJIT with its JIT off, within about 10 instructions either way, and take 1.05 to 1.4 times its time. Adding checks to one arm moved another arm by up to 10 instructions in these builds, so 10 a call is LLVM's noise band and not a design difference.

## Data layouts

### The value word (`value.rs`)

| top bits | holds |
|---|---|
| 13 ones (`0xFFF8` and up) | a small Int, 51-bit two's complement |
| `0x7FF8` | `Nil`, `False`, `True` |
| `0x7FF9` | a function with no captures, by its `FuncIdx` |
| `0x7FFC` | a cell |
| `0x7FFD` | a constructor with no fields |
| anything else | a Float |

- Every tagged word is a quiet NaN. The Float instructions rely on this, and a unit test walks every `Value` constructor to keep it true.
- Two words are both small Ints when `(a & b) >= 0xFFF8_0000_0000_0000`. That is an exact test.
- `(a << 13) as i64` puts the payload at the top of the word, so `checked_add` compiles to `adds` and `b.vs` and the CPU's overflow flag is the range check. For multiply one factor stays unshifted.
- A small Int holds ±2^50. map_churn's `x * 69069` reaches about 2^48.1, so it stays small.
- A function value keeps its `FuncIdx`, so values do not change when code is loaded again, and "a closure on the wire names its code" still reads straight off the value.
- There are two ways to make a value from an `f64`, and both exclude NaN and infinity. `Value::finite(r) -> Option<Value>` holds the test itself and is what Float instructions use. `Value::float(r)` clamps and is what every built-in uses. No unchecked constructor exists. I measured that `finite` costs nothing: `escape` is 256.1 with it and with the hand-written test.
- The header of `value.rs` must say why this matters. In this layout the NaN that ARM produces (`0x7FF8_0000_0000_0000`) is the word for `Nil`, and the NaN that x86 produces (`0xFFF8_0000_0000_0000`) is the word for Int 0. So a stray NaN would be a value of another type, and the two constructors are what stop one being made.

### The stack and a frame

```
stack: Vec<Value>             one per process
frame at `base`:
  register 0        the link: return pc (bits 0 to 31), shift (32 to 39), the caller's destination register (40 to 47), stored as a small Int
  register 1        the closure this frame runs as. Only a call through a value writes it.
  registers 2..     the parameters in order, then locals
  register `nregs`  where a callee's register 0 goes
```

- The callee's base is `base + shift`, and `shift` is the caller's `nregs`. So frames never overlap, which is today's rule.
- A run's whole state is `stack`, `At { pc, base }` and the heap. `struct Frame`, `callers`, `top`, `passing` and `Frame::env` all go.
- The stack starts at 256 slots (2 KB). A call needs 256 slots above the callee's base, so the first call grows it. `grow` takes the larger of what the call needs and one and a half times the current length, so a process that has made one call holds 384 slots, 3 KB. That is more than BEAM's 2.6 KB for a whole fresh process, and it is a decided cost of the fixed window.

### The instruction (`code/ins.rs`)

```rust
pub(crate) struct Reg(u8);    // a real register
pub(crate) struct VReg(u32);  // a virtual register, one per Core local, before allocation
pub(crate) struct Kill(u8);   // which Int operands die here; read only on the cold path
pub(crate) struct Dups(u8);   // bit i: argument i stays live in the caller

pub(crate) enum Ins<R> {
    // Int
    IntAdd { d: R, a: R, b: R, kill: Kill },                 // also Sub Mul Div Rem
    IntAddK { d: R, a: R, kill: Kill, k: i64 },              // also SubK RSubK MulK DivK RemK; k is stored shifted left 13
    IntNeg { d: R, a: R, kill: Kill },
    IntLt { d: R, a: R, b: R, kill: Kill },                  // a Bool that is stored; also Le Eq Ne
    JumpUnlessIntLt { a: R, b: R, kill: Kill, to: u32 },     // also Le Eq Ne
    JumpUnlessIntLtK { a: R, kill: Kill, to: u32, k: i64 },  // also Le Gt Ge Eq Ne
    // Float
    FloatAdd { d: R, a: R, b: R },                           // also Sub Mul Div Rem
    FloatAddK { d: R, a: R, k: f64 },                        // also SubK RSubK MulK DivK RDivK
    FloatNeg { d: R, a: R },
    FloatFromInt { d: R, a: R, kill: Kill },
    FloatLt { d: R, a: R, b: R },                            // also Le
    JumpUnlessFloatLt { a: R, b: R, to: u32 },               // also Le
    JumpUnlessFloatLtK { a: R, to: u32, k: f64 },            // also LeK GtK GeK
    // Moving values
    Const { d: R, v: Value },
    Move { d: R, s: R },
    Move2 { d: [R; 2], s: [R; 2] },                          // parallel: every source is read before any write
    Move3 { d: [R; 3], s: [R; 3] },
    Dup { d: R, s: R },                                      // copy, and share if it is a cell
    Drop { n: u8, r: [R; 13] },                              // give up each register's reference
    // Control
    Jump { to: u32 },
    JumpUnlessTrue { a: R, to: u32 },                        // anything but True or False stops
    Call1 { d: R, shift: u8, a0: R, to: u32 },               // also Call2, Call3: every argument is moved or scalar
    Call1D { d: R, shift: u8, a0: R, to: u32 },              // the argument stays live in the caller
    Call3D { d: R, shift: u8, a: [R; 3], dups: Dups, to: u32 },  // also Call2D
    Call { d: R, shift: u8, n: u8, dups: Dups, args: [R; 6], to: u32 },
    CallWide { d: R, shift: u8, n: u8, list: u32, to: u32 },
    CallValue { d: R, shift: u8, f: R, n: u8, dups: Dups, args: [R; 6] },
    CallValueWide { d: R, shift: u8, f: R, n: u8, list: u32 },
    TailCallValue { f: R, n: u8, dups: Dups, args: [R; 6] },
    TailCallValueWide { f: R, n: u8, list: u32 },
    Ret { s: R },
    Halt { s: R },
    Capture { d: R, index: u16 },
    Bad { why: u32 },
    // Ctor, Tuple, Closure, Array, Field, the variant tests, Builtin and the
    // rest keep their meaning: up to 8 operand registers inline, or a `list`
    // index into `Code::lists` when there are more.
}
const _: () = assert!(size_of::<Ins<Reg>>() == 16);
```

- The loader works on `Ins<VReg>` and the loop runs `Ins<Reg>`. Allocation is `try_map` from one to the other. Today's `Instr` goes, so there is one enum to add an instruction to. I compiled this shape in `final-core/ins-generic/`: it is 16 bytes over `Reg` and 56 over `VReg`.
- `Ins::operands` lists every register an instruction names, each with a role: looked at, killed, taken, copied or written. It is written once for any `R`. Liveness, allocation and the verifier all walk it, so they cannot disagree about an instruction.
- On a compare-and-jump, `Kill` has four bits: operand `a` or `b` dies when the jump is not taken, and `a` or `b` dies when it is. Elsewhere it has two. A register gets at most one kill bit, so `n * n` with a dying `n` sets the bit for `a` only.
- The call forms are split by whether anything needs sharing, because a share test in the hot one-argument arm cost fib 3 to 23 instructions a call depending on how LLVM placed it. `Call1`, `Call2` and `Call3` contain no share code. The `D` forms carry the mask and test each kept argument for a cell. This is BEAM's loader idea: the loader knows the fact, so it picks the instruction.
- An entry in a wide form's list is a register and its own copy bit.
- Strings, binaries, `Broken` values and `VariantRef`s move to side tables in `Code`, indexed by `u32`. Numbers stay in the instruction.
- `Num`, the operator typed only as "a number", stays one generic instruction.

### Loaded code

```rust
pub(crate) struct Code {
    ins: Vec<Ins<Reg>>,                 // every function one after another; `Halt` is at index 0
    funcs: TiVec<FuncIdx, Entry>,       // Entry { pc: u32, arity: u8, name }: read by CallValue, show, wire and error messages
    lists: Box<[Operand]>,              // operand lists too long for an instruction
    maps: Maps,                         // which registers own a reference at each call site; written by verify
    ...
}
pub(crate) struct Checked(Code);        // only `verify` makes one, and `Machine::new` takes nothing else
```

- A call by name holds its target's `pc`, so the loop never looks a function up.
- A `pc` is an index and stays valid for the life of the run. Code is only ever appended. Loading a module again appends its new functions and repoints `funcs[idx].pc`, and old instructions are never moved or reused while a link or a call can name them.
- A function that needs something not built yet is one `Bad` at its entry, so it loads and stops only when called. A function that needs more than 254 registers is one of these until the spill step.

## The contract with the compiler

Ownership is one fact, and one pass states it. Today Perceus places drops for some types and the VM's sweeps cover the rest. The earlier idea of a second ownership pass in the loader would have derived the same fact twice, which is the mistake `CLAUDE.md` warns about, and it would have blocked borrowed parameters. Lean does it this way too: `RC.lean` is a compiler pass, and its interpreter only runs the `inc` and `dec` it is handed.

What Core IR carries after Perceus:

1. **A class for every local.** `scalar` can never be a cell: Float, Bool, Nil, and any type the compiler marks never-a-cell. `int` is a cell only when big. Everything else is `any`. The never-a-cell bit is computed in the compiler where full type definitions are known and stored per `RTy` in `ResolvedPool`. The loader cannot work it out from `Program::types`, which lists only the types a program builds or matches, and where an Array or a handle type has no constructors at all.
2. **Move or copy on every operand that hands a reference on.** Which operands those are is fixed per atom in `scarlet_ir`, in one function. A call argument, a constructor field, a capture, a returned value and the array of an owned push are taken. Arithmetic, compares, a `Field`'s source and a variant test only look.
3. **A `Drop` wherever a non-scalar local dies without being moved.** That means after its last looking read, at the head of a branch where it is dead, at the head of the body for a parameter nothing reads, and inside joins. Int, String and type-variable locals are included.
4. **`DropReuse` and `reuse` as today.** The two rules the VM now depends on are written into the doc comment on `Atom::Ctor::reuse`: only a token present at every tail call to self is carried round a loop, and a carried token pairs only with its own slot.
5. **Room for borrowed parameters.** The verifier's register states include `Lent`, a value that may be read and never taken, killed or dropped, so borrow inference adds a state it already knows.

What the loader does with it is encoding, with no decisions:

- A copied call argument sets a `dups` bit and so picks a `D` form. A copied operand elsewhere uses the instruction's mask if it has one, or a `Dup` into a fresh register.
- A `Drop` straight after an Int instruction that looks at the same local becomes that instruction's kill bit. A `Drop` at the head of either side of an Int compare-and-jump becomes a direction bit. Any other run of drops becomes one `Drop { n, r }`.
- A scalar local never gets a dup, a drop or a kill bit.

**The rule this gives the loop.** A register owns a reference exactly from the instruction that writes it to the instruction that takes or kills it. A register nothing owns holds stale bits, and nothing reads it before writing it. So a write is a plain store, `Ret` and a tail call sweep nothing, and moving a value out needs no `Nil` store.

## The loader's passes

`code.rs` splits into `code/{ins, translate, live, alloc, moves, emit, verify}.rs`. Each pass is a function from one value to the next.

1. **translate** is today's `Loader::expr`. It emits `Ins<VReg>`, one virtual register per Core local, and picks the typed operator from the `PrimOp`. It folds a constant into the K forms on either side and in tail position, fuses a compare with its jump, and encodes moves, copies and drops as above.
2. **live** is backward liveness over `Ins::operands`. Only allocation uses it.
3. **alloc** is today's colouring `allocate`, with parameters pinned to registers 2 and up, registers 0 and 1 never handed out, and one preference: the value a tail call passes as parameter *i* takes parameter *i*'s register when the two never clash.
4. **moves** turns each tail call to a known function into a parallel move and a `Jump`.
5. **emit** resolves labels and entries to positions in the one array.
6. **verify** walks what `emit` produced and returns `Checked`.

One small pass belongs in `scarlet_lower`, before Perceus, because it changes where values die: **tail-argument scheduling**. Today `go(n - 1, acc + n)` lowers to `%5 = n - 1` and then `%6 = acc + n`, so the new `n` is made while the old one is still read, and the loop keeps a `Move` (102 instructions a step against 87). The pass moves a pure Int or Float `let` whose only use is an argument of a tail call to a known function down to just before the call. It orders those lets so the one that replaces parameter *i* comes after the others that read parameter *i*, and leaves a cycle as written. Scarlet arithmetic is total, so the reordering cannot change what a program does.

I scanned all 90 self tail calls in the 23 benchmarks' own functions (`final-core/scan/tailsched.py`). In 73 every argument can already be computed in place, 6 more can after this pass (4 are in spectral_norm, including `av_row`, which is 99.8% of that benchmark), 9 keep a `Move` for good (tak and nbody's `sqrt_from` among them), and 2 permute their parameters. The scan does not check values bound by a pattern.

## The loop

```rust
pub(crate) fn run(code: &Checked, stack: &mut Vec<Value>, rest: &mut Rest) -> Result<Value, Stop> {
    loop {
        match steps(code.ins(), code.funcs(), stack, rest)? {
            Pause::Done(v) => return Ok(v),
            Pause::NeedStack => grow(stack, rest.at.base)?,
        }
    }
}

fn steps(code: &[Ins<Reg>], funcs: &[Entry], stack: &mut [Value], rest: &mut Rest) -> Result<Pause, Stop> {
    let (mut pc, mut base) = (rest.at.pc, rest.at.base);
    let len = stack.len();
    let mut win = window(stack, base).ok_or_else(lost_window)?;
    loop {
        let Some(ins) = code.get(pc) else { return Err(fail(rest, pc, base, ran_past_end())) };
        pc = pc.wrapping_add(1);
        match *ins {                                   // in place: never copy the instruction first
            Ins::IntAdd { d, a, b, kill } => {
                let (x, y) = (win.look(a), win.look(b));
                let v = match int::add(x, y) {
                    Some(v) => v,
                    None => match int::slow(rest, IntOp::Add, x, y, kill) {
                        Ok(v) => v,
                        Err(e) => return Err(fail(rest, pc, base, e)),
                    },
                };
                win.put(d, v);
            }
            Ins::FloatMul { d, a, b } => {
                let r = win.look(a).as_f64() * win.look(b).as_f64();
                let v = match Value::finite(r) {
                    Some(v) => v,
                    None => /* float::slow(FloatOp::Mul, win.look(a), win.look(b)), through `fail` */,
                };
                win.put(d, v);
            }
            Ins::Call1 { d, shift, a0, to } => {
                let up = base.wrapping_add(u32::from(shift));
                if up as usize + 256 > len {           // ask first; nothing is shared or written yet
                    rest.at = At { pc: pc - 1, base };
                    return Ok(Pause::NeedStack);
                }
                let v0 = win.take(a0);
                win = window(stack, up).ok_or_else(lost_window)?;
                base = up;
                win.put(Reg::LINK, Value::link(pc, shift, d));
                win.put(Reg(2), v0);
                pc = to as usize;
            }
            Ins::Ret { s } => {
                let (v, link) = (win.take(s), win.look(Reg::LINK));
                pc = link.link_pc();
                base = base.wrapping_sub(link.link_shift());
                win = window(stack, base).ok_or_else(lost_window)?;
                win.put(link.link_dst(), v);
            }
            Ins::Jump { to } => pc = to as usize,
            Ins::Halt { s } => return Ok(Pause::Done(win.take(s))),
            ...
        }
    }
}
```

Six things in this shape were measured.

- **Match on `*ins` through the reference.** Copying the instruction out first makes LLVM decode every field before the jump, and dispatch is 14 or 15 instructions instead of 9.
- **The inner loop borrows the stack as a slice.** A slice cannot grow, so `len` is a loop constant and a call that needs room returns `NeedStack` to the outer loop. This is also the door a yield will use.
- **A call asks for room before it shares or writes anything.** The outer loop grows the stack and the whole call instruction runs again, so there is no half-done call to resume.
- **`win.look(r)`, `win.take(r)` and `win.put(r, v)` are `self.0.get(usize::from(r.0))` with a fallback arm.** Each compiles to `ldrb; ldr` or `ldrb; str` with no compare. `look` and `take` are the same load. In test builds they record which role the handler used, and a test fails if that differs from what `Ins::operands` says.
- **Every failing arm leaves through one cold function, `fail`,** which stores `At { pc, base }` in `rest` before returning the `Stop`. So a stop knows its frame, and an error trace is a walk down the links from there, with each pc mapped to a function by binary search over `Code::funcs`. This did not change any count in the probe.
- **Cold state sits behind one `&mut Rest`** (heap, host, globals, `at`). Any arm longer than a few lines calls an `#[inline(never)]` function.

Dispatch is 9 instructions plus one branch back (`cmp; b.hs; add; add; ldrb; adr; ldrh; add; br`), and `steps` has one indirect branch. I read this in objdump for the 92-arm and the 140-arm builds.

## Int instructions

```rust
fn add(x: Value, y: Value) -> Option<Value> {
    if x.bits() & y.bits() < INT_TAG { return None }                 // one is a big Int, or not an Int at all
    let r = ((x.bits() << 13) as i64).checked_add((y.bits() << 13) as i64)?;
    Some(Value::from_bits((r as u64 >> 13) | INT_TAG))
}
```

- `int::slow` is the one cold function behind every Int instruction. It reads each operand as an `i128` when it is small or a one-word big Int and falls back to `num-bigint`. An operand that is not an Int is `Broken::IntOperation`, as today.
- `int::slow` works in a fixed order: read both operands, compute, build a fresh value, then release the registers `kill` names. It never returns one of its operands, so a release cannot free the answer.
- A small Int owns nothing, so a dying Int costs nothing on the fast path.
- `x / 0` is 0 and `x % 0` is `x`, tested before the divide. A small divide or negate is range-tested before it is tagged, because `-2^50 / -1` fits an `i64` and does not fit a small Int.
- The exact tag test costs 2 to 3 instructions more per Int instruction than a one-bit test that trusts the loader. I kept it, because a wrong register then stops with a typed error instead of computing a wrong number.

## Float instructions

- The fast path is two loads into FP registers, the operation, `Value::finite` on the result, and one store. It measures 22 to 23 instructions with dispatch, against 92 today.
- The operands are not checked on the way in. A tagged word is a NaN, a NaN operand gives a NaN result, and `x / 0.0` gives an infinity or a NaN. So the one test of the result catches a non-Float operand, a divide by zero and an overflow together.
- **The rule for what may be a Float instruction:** a NaN operand always gives a NaN result, and the result goes through `Value::finite` before it is stored. Add, subtract, multiply, divide, remainder and negate qualify. `FloatNeg` needs its test more than any: negating a small Int's word gives a word with the cell tag. `min`, `max` and `pow` do not qualify, because they can return the other operand or 1, so they stay built-ins that check their operands.
- `float::slow` is cold and reads the two operand words again. A tagged operand is `Broken::FloatOperation`. A divide by `0.0` is `0.0`, `x % 0.0` is `x`, an answer with no value is `0.0`, and an overflow stops at the largest Float.
- Every Float compare, stored or fused, uses `partial_cmp`. `None` means an operand was a NaN, so it was not a Float, and the run stops with `Broken::FloatOperation`. A plain `<` would quietly answer False.
- The macro that writes the handlers also writes a test for each one that hands it every tagged kind of word and expects a stop.

## Calls, returns, closures, tail calls

- **Call.** The handler asks for room, reads the arguments, shares the kept ones (in a `D` form only), moves the window up by `shift`, writes the link and stores the arguments in registers 2 and up. `Call` does this for up to six in a loop, and the wide forms read a list.
- **Return.** `Ret` reads the result and the link, moves the window back, and stores the result in the register the link names. It releases nothing. The first frame's link returns to the `Halt` at index 0, so there is no "last frame" test.
- **A call through a value.** `CallValue` reads the function value. A function word gives its `FuncIdx`, and a closure cell holds its `FuncIdx` in its first word. `funcs[idx]` gives the entry and the arity, and an arity that differs from `n` is `Broken::CallArity`. The function value goes in the callee's register 1. This measures 90 instructions, and a whole closure call 203.
- **Register 1 is an owned parameter of a function that some `Closure` instruction builds with captures,** and of no other function. `Capture` reads through it and adds a reference. Perceus treats it as a parameter, so it is dropped after its last read, or at the head of the body if nothing reads it. `emit` refuses a call by name to such a function.
- **A function's entry has two labels.** `entry:` runs once per activation and stores `Nil` into each register that carries a reuse token round the loop. `loop:` is the body, which starts with the drops of parameters nothing reads. A tail call to self jumps to `loop`. A call or a tail call from anywhere else goes to `entry`. A drop that must run every turn is therefore never in the skipped part.
- **A tail call to a known function** is a parallel move into the parameter registers and a `Jump`. `Move2` and `Move3` read all their sources before they write, so a swap is one `Move2` and a rotation of three is one `Move3`. Only a cycle longer than three uses a scratch register. An argument already in its parameter's register costs nothing.
- **`TailCallValue`** reads its arguments into locals, writes register 1 and the parameters, and jumps.
- **Arity.** A function takes at most 250 parameters. The compiler reports more as a diagnostic, and the loader refuses it with a typed error.
- **Stack depth** for `internal::stack_depth` is a walk down the links.

## The verifier

`verify` is one forward walk per function over the emitted `Ins<Reg>`. Each register is `Empty`, `Scalar`, `Int`, `Any` or `Token`. It rejects the function unless:

- every operand an instruction reads holds a value;
- a taken or killed operand becomes `Empty`, in the order `Ins::operands` gives, so a register killed twice is rejected;
- no instruction writes over an `Int`, `Any` or `Token` register, unless the same instruction takes or kills it;
- a `DropReuse` turns `Any` into `Token`, a `Ctor` with `reuse` finds a `Token` on every path in, and a join of `Token` with anything else is rejected;
- at `Ret`, at a `Jump` to an entry and at `TailCallValue`, no `Int`, `Any` or `Token` register is left except the ones passed on (and the carried tokens, at a jump to `loop`);
- where two paths join they agree on every register that is not `Empty` or `Scalar`;
- at `entry` only the parameters (and register 1, for a closure body) hold values;
- no instruction but a call and `Ret` touches register 0;
- every call fits (`shift + 2 + n <= 256`) and passes as many arguments as its callee takes;
- every target is an instruction and every function ends in a terminator.

A failure is `Stop::broken(Broken::Unverified { func, pc, reg, why })` at load.

`verify` writes one thing out: for each call site, the set of registers that own a reference. Every frame below the top is paused at a call, so these maps say what is live on a whole stack. Nothing reads them while a program runs. They are what a debugger, an honest crash dump, heap compaction on hibernate and a JIT's stack maps need, and the test audit below uses them. The map at any other pc comes from running the walk again over that one function.

**What the verifier cannot see, and what covers it.** The verifier checks that the instructions agree with `Ins::operands`. It does not check that `Ins::operands` agrees with what each handler in `exec.rs` does. Three things cover that gap.

1. A generated test per instruction and per built-in puts a fresh cell with count 1 in each operand, runs the one instruction, and asserts each count changed by what the role says.
2. `Heap::release` and `Heap::share` return a typed `Broken` when the count is already 0. Today a doubled release falls silently into the free path a second time. The live-cell counter stops using a saturating subtract, so one leak and one doubled release cannot cancel in `cells_left_after`.
3. A test-only audit run recounts every cell's owners after each instruction, from the owned registers, the cells' own fields and the globals, and compares with the header count. The golden programs run under it.

The `Ctor` handler keeps its `fits_ctor` test on a reuse token. It costs a few instructions on a path that costs hundreds.

## Big functions

A function that colours to more than 254 registers needs somewhere else to keep values. Nothing in the repository does: I bounded all 1,352 functions that `scarlet dis` prints for the 85 programs in `benches`, `examples` and the test programs, and the largest needs at most about 133 registers, for a 132-element array literal (`final-core/scan/maxlive.py`).

So until its own step, such a function loads as one `Bad` at its entry. The need is cut first: an operand list may name a constant directly, and `translate` builds a long literal in chunks, as Lua's `closelistfield` flushes a table constructor every 50 items. When spilling is built it is a second stack in `rest` with `Spill` and `Unspill` instructions, and the verifier treats a slot as one more register. It is built after ownership so it is written once against the final rules.

## Worked example: fib

Today (machine instructions per VM instruction from the diagnosis's trace; a call runs 4.5 of them and costs 355):

```
0 JumpUnlessCmpImm Lt r0, 2 -> 2      40
1 Ret r0                              97
2 IntImm r1 = r0 - 1                  59
3 CallSelf r1 = (r1 moved)           124
4 IntImm r2 = r0 - 2                  59
5 CallSelf r2 = (r2 moved)           124
6 Int r3 = r1 + r2                    70
7 Ret r3                              97
```

After (register 2 is `n`, register 3 a temporary, `shift` 4; counts from single-stepping the probe):

```
1 JumpUnlessIntLtK  a:2, k:2, to:3                   22
2 Ret               s:2                              23
3 IntSubK           d:3, a:2, k:1                    25
4 Call1             d:3, shift:4, a0:3, to:1         28
5 IntSubK           d:2, a:2, k:2, kill:a            25
6 Call1             d:2, shift:4, a0:2, to:1         28
7 IntAdd            d:2, a:3, b:2, kill:ab           28
8 Ret               s:2                              23
```

That is 22 + 23 + 0.5 x (25 + 28 + 25 + 28 + 28) = 112 a call, and the whole run measures 112.0 (117.5 with 140 arms). Both arguments are fresh temporaries, so both calls are the plain `Call1`. No instruction releases anything.

## Worked example: the bare loop and `av_row`

`go(n - 1, acc + n)` after tail-argument scheduling (register 2 is `n`, 3 is `acc`):

```
1 JumpUnlessIntEqK  a:2, k:0, to:5                   22
2 IntAdd            d:3, a:3, b:2, kill:a            28
3 IntSubK           d:2, a:2, k:1, kill:a            25
4 Jump              to:1                             12
5 Ret               s:3
```

It measures 87 a step against 346 today. In today's Core IR order the subtract comes first, needs a third register and a `Move` (15), and the step is 102.

spectral_norm's `av_row` is the same case on a real benchmark. Core IR computes `j + 1` first, then reads `x[j]` and calls `a(i, j)`. After scheduling, the call is a `Call2D` that keeps `i` and `j`, the new `acc` is computed into `acc`'s register, `j + 1` into `j`'s, and the tail call is one `Jump`.

## Worked example: mandelbrot's `escape`

Today it is 15 instructions and 1,364 machine instructions a turn. After (2 `z`, 3 `zrzr`, 4 `zizi`, 5 `zi`, 6 `cr`, 7 `ci`, temporaries 8 and 9):

```
 1 JumpUnlessIntLtK   a:2, k:50, to:out        22
 2 FloatSub           d:8, a:3, b:4            23
 3 FloatAdd           d:8, a:8, b:6            23      zr
 4 FloatMulK          d:9, a:8, k:2.0          22
 5 FloatMul           d:9, a:9, b:5            23
 6 FloatAdd           d:5, a:9, b:7            23      the new zi, in zi's register
 7 FloatMul           d:3, a:8, b:8            23      the new zrzr, in its parameter
 8 FloatMul           d:4, a:5, b:5            23      the new zizi, in its parameter
 9 FloatAdd           d:8, a:3, b:4            23
10 JumpUnlessFloatLeK a:8, k:4.0, to:escaped   15
11 IntAddK            d:2, a:2, k:1, kill:a    25
12 Jump               to:1                     12
```

It measures 256 a turn and 66 cycles. This one needs no scheduling: in today's Core IR each new argument is already made after the last read of the parameter it replaces, so the allocator's preference is enough. The preference itself is still argued and not run, because the listing was assembled by hand.

## Worked example: tak

```
 1 JumpUnlessIntLt  a:3, b:2, to:10, kill: a and b when the jump is taken     25
 2 IntSubK          d:5, a:2, k:1                                              25
 3 Call3D           d:5, shift:8, a: 5 3 4, dups: y z                          47
 4 IntSubK          d:6, a:3, k:1                                              25
 5 Call3D           d:6, shift:8, a: 6 4 2, dups: z x                          47
 6 IntSubK          d:7, a:4, k:1, kill:a                                      25
 7 Call3            d:7, shift:8, a: 7 2 3                                     33
 8 Move3            2<-5, 3<-6, 4<-7                                           23
 9 Jump             to:1                                                       12
10 Ret              s:4                                                        23
```

It measures 101.5 a call against 379 today (106 with 140 arms). The three results cannot be computed in place, because each call still reads the old parameters, so the `Move3` stays. Sending the two calls that keep arguments through the general `Call` instead of `Call3D` costs 110.

## The invariants, and what upholds each

| invariant | made true by | checked by |
|---|---|---|
| A register index is in the window. | `Reg(u8)` into `[Value; 256]`. | The Rust types. |
| The loop's window is the 256 slots at `base`. | `window()`, the only constructor of `Window`, called at entry to `steps`, in each call arm and in `Ret`. | Its own bounds check. |
| A call writes and shares nothing until it has room. | The room test is the first statement of every call arm. | A test that runs deep recursion from a 256-slot stack and counts cells. |
| `pc` names an instruction, for the life of the run. | `emit`; code is only appended. | `verify`, and `code.get(pc)` on every fetch. |
| Who owns a reference is decided once. | Perceus, in `scarlet_lower`. | `verify`, over the emitted instructions. |
| A register owns a reference from its write to its take or kill. | `translate` encodes Perceus's moves, copies and drops one for one. | `verify`, the audit run and the cell-count tests. |
| A handler does to each operand what `Ins::operands` says. | The handler uses `look`, `take` or `put`. | The generated test per instruction, and the role check in test builds. |
| A doubled release cannot pass unseen. | `Heap::release` and `share` stop on a count of 0. | The exact live-cell count. |
| A scalar register never holds a cell. | The compiler's never-a-cell bit per type. | Nothing at run time, so anything the compiler cannot prove is `any`. |
| A constructor reuses only a cell kept for it. | Perceus's two token rules. | `verify`'s `Token` state, and `fits_ctor` in the handler. |
| Register 0 is the link until `Ret`. | `alloc` never hands out registers 0 and 1. | `verify`. |
| A function that owns register 1 is entered only through a function value. | `emit` refuses a call by name to it. | `emit`. |
| Code that must run every turn is not skipped by a self tail call. | The `entry:` and `loop:` layout. | `verify`'s state at `loop`, and a cell-count test of a loop with a dead cell parameter and a carried token. |
| A known call passes what its callee takes and fits the window. | `alloc`, `emit`. | `verify`. |
| A call through a value passes what its callee takes. | The type checker. | The arity compare in `CallValue`. |
| An Int instruction's operands are Ints. | `translate`, from the `PrimOp`. | The tag test in every Int fast path, then `int::slow`. |
| A Float instruction's operands are Floats. | `translate`. | `Value::finite` on the result or `partial_cmp`, then `float::slow`. |
| Every tagged word is a NaN. | The constants in `value.rs`. | A unit test over every `Value` constructor. |
| No stored Float is NaN or infinite. | `Value::finite` and `Value::float` are the only ways in from an `f64`. | The Rust types, and the generated Float tests. |
| One number has one form. | `int::slow` returns a small Int whenever the answer fits. | The bigint tests. |
| A stop knows where it happened. | Every failing arm returns through `fail`. | A test of a trace from a nested call. |
| No Rust recursion and no panic. | Frames are stack slots and every failure arm returns a `Stop`. | The lints. |

Without `unsafe`, a bug in Perceus, `alloc` or `verify` can leak a cell or free one twice, and the heap check then stops the run with a typed error. It cannot cause undefined behaviour.

## What each diagnosed cause becomes

| cause | today | after | what does it |
|---|---|---|---|
| `dispatch-and-instr-format` | 15 to 17 per instruction, 48-byte instructions | 9 plus 1, 16-byte instructions | an enum matched in place, one code array, cold state behind one pointer |
| `register-access-checks` | 7 to 8 per read, 14 per write | 2 per read, 2 per write | the window, `u8` registers, plain stores once ownership lands |
| `call-return-frames` | 223 for one argument | 51 to 60 | the link in register 0, arguments copied by the call, no sweep, no `env` |
| `self-tail-call-buffer-and-sweep` | 177 for two arguments, 313 for six | 12, plus a move in about one loop in eight | scheduling, parameter-register preference, parallel moves, `Jump` |
| `int-op-inner-dispatch-and-48-bit-ints` | 59 to 72, big past 2^47 | 22 to 28, big past 2^50 | one opcode per operator, overflow flag as the range check, 51 bits |
| `float-ops-out-of-line` | 92 to 97, 140 for compare and branch | 22 to 23, 15 for compare and branch | inline op, one test of the result, K operands, fused compare |

## What to expect, benchmark by benchmark

Three rows are measured: the probe's count with 140 arms, times 1.1 for the real loop and heap. The other twenty are arithmetic. For each I took the share of today's run that the diagnosis gives to calls, closure calls, self tail calls, Float instructions and Int instructions, and removed the fraction the probe measures for that kind (calls 0.72, closure calls 0.58, tail calls 0.82, Float 0.72, Int 0.50). For every other VM instruction I removed 0.38 of its dispatch and 0.65 of its register access. Before ownership lands the call and register fractions are lower (0.48 and 0.40), from the build that keeps the sweeps. The script is `final-core/scan/table.py`.

| benchmark | today, billions | after steps 1 to 5 | after step 6 | ratio to LuaJIT after | |
|---|---:|---:|---:|---:|---|
| binary_trees | 27.6 | 20.9 | 18.6 | 1.71 | arithmetic |
| bounce | 37.5 | 33.2 | 32.1 | 7.14 | arithmetic |
| cfold | 43.1 | 33.7 | 30.3 | 1.30 | arithmetic |
| closures | 26.4 | 18.6 | 16.8 | 5.28 | arithmetic |
| deriv | 39.2 | 31.6 | 29.0 | 2.17 | arithmetic |
| fannkuch | 27.7 | 25.1 | 24.6 | 24.6 | arithmetic |
| fib | 10.6 | 5.5 | 3.9 | 1.06 | measured |
| json | 28.1 | 24.0 | 22.7 | 1.61 | arithmetic |
| list | 43.6 | 34.4 | 32.1 | 5.17 | arithmetic |
| mandelbrot | 37.8 | 10.2 | 8.1 | 0.82 | measured |
| map_churn | 19.7 | 11.3 | 10.4 | 1.34 | arithmetic |
| nbody | 22.7 | 13.7 | 12.9 | 4.55 | arithmetic |
| permute | 31.0 | 27.4 | 26.2 | 27.5 | arithmetic |
| queens | 35.9 | 32.4 | 31.4 | 25.9 | arithmetic |
| rbtree | 38.8 | 31.8 | 29.3 | 1.76 | arithmetic |
| richards | 35.1 | 31.5 | 30.4 | 6.33 | arithmetic |
| sieve | 29.1 | 28.6 | 28.5 | 321 | arithmetic |
| spectral_norm | 40.0 | 21.3 | 18.6 | 1.34 | arithmetic |
| storage | 27.3 | 23.2 | 22.2 | 3.09 | arithmetic |
| string_keys | 28.7 | 25.5 | 24.9 | 2.54 | arithmetic |
| strings | 30.6 | 27.8 | 26.8 | 3.06 | arithmetic |
| tak | 38.4 | 21.7 | 11.8 | 1.04 | measured |
| towers | 38.4 | 29.8 | 26.4 | 6.11 | arithmetic |

The geomean of 6.17 goes to 4.5 after steps 1 to 5 and to 4.1 after step 6. With every modelled fraction cut by a quarter for what I cannot see, it is 4.8 and 4.4. So I would plan on **4.5 to 4.8 with today's ownership kept, and 4.1 to 4.4 once ownership lands**, for this area alone.

By the clock expect about half the gain in logs, because the instructions this removes are cheap ones. fib's instructions fall 3.0 to 3.2 times and its cycles 1.75 to 2.0 times. Today's geomean by the clock is 2.83, and I would expect about 2.3 to 2.4.

**One ledger across areas.** These numbers must be counted once.

- The big-Int excursion in map_churn (cause 16) is booked here, in step 1.
- Dead `Field` reads and drops that can never free (cause 15) belong to the compiler's work and are not in the table.
- Step 6 here is the VM's half of the new ownership contract: plain stores and no sweeps. The compiler area's probe measures the same change from its side (fib 0.776). It is one gain, booked here. Borrowed parameters and drop specialisation are the compiler's and are not in the table.
- An inliner removes calls this table speeds up. With the calls its measured rewrites remove taken out of closures, spectral_norm, map_churn and towers, this area's geomean is 4.6 before ownership and 4.2 after, instead of 4.5 and 4.1.
- In permute, queens, sieve and fannkuch most of what this area speeds up is inside the hand-written array `set`, which the array work deletes.
- The closure figure (203) was measured on a toy heap where a share costs about 5 instructions. Part of the real gain there is the heap work's.


## Steps

### 1. 1. Small Ints of 51 bits, with every tagged word a NaN

**Changes.** crates/scarlet_vm/src/value.rs: the tag table in the design (Int is 13 ones; immediates, function, cell and nullary move to 0x7FF8, 0x7FF9, 0x7FFC, 0x7FFD), `Value::int`, `small_int`, `as_cell`, `view`, SMALL_INT_MIN and MAX as -(1<<50) and (1<<50)-1. `Value::finite(f64) -> Option<Value>` and the clamping `Value::float` become the only constructors from an f64, and the file's header says which word a stray NaN would be on ARM and on x86. A new test checks that every `Value` constructor makes a NaN or a Float. bigint.rs and heap.rs: the 'fits a small Int' bound wherever an Int is put back in its one form. Tests that name 2^47, plus divide and negate at both ends of the new range. benches/run.py: print cycles next to instructions, so every later step is judged on both.

**Expected gain.** map_churn: the diagnosis measured 19,747M falling to 15,372M when the generator's product stays small, which is 22.2%. `x * 69069` is about 2^48.1 and the new limit is 2^50. No other benchmark is known to cross 2^47. Geomean factor exp(ln(0.778)/23) = 0.989, so 6.17 to about 6.10. This gain is cause 16 and is booked here and nowhere else.

**Risk.** Low. Anything that builds or takes apart a tagged word by hand must use the constants and not the number 48 or 16. The new layout makes 'every tagged word is a NaN' load-bearing for step 2, so its unit test lands here.

### 2. 2. One instruction per Int and Float operator, with constants, fused compares and inline Float

**Changes.** crates/scarlet_vm/src/code.rs, still in today's 48-byte format: IntAdd/Sub/Mul/Div/Rem and their K forms (constant on either side, and in tail position), JumpUnlessInt* and their K forms, FloatAdd/Sub/Mul/Div/Rem and K forms, FloatLt/Le, JumpUnlessFloat* and K forms, FloatFromInt; `Loader::immediate` takes Float constants and a left-hand constant; `take_comparison` takes Float compares. exec.rs: Int fast paths inline with the shift-by-13 and the overflow flag, one `#[cold]` `int::slow` that always builds a fresh value; Float fast paths inline with `Value::finite` on the result and a `#[cold]` `float::slow` that re-reads the operand words; every Float compare, stored or fused, uses `partial_cmp` and stops on `None`. Delete `Machine::float`, `float::op`'s operator match and `bigint::small_op`. A macro writes each handler family once, and the same macro writes a test per Float instruction that hands it each tagged kind of word and expects a stop, and an edge-value test per operator against today's `float::op` and `bigint::op`. `min`, `max` and `pow` stay built-ins.

**Expected gain.** From the sceptic's estimate of an inlined Float op in today's format (45 to 55 of 92): mandelbrot per turn saves 8 x about 42 = 336, the fused compare about 90 of 140, two constants about 70, and two Int instructions 6 each for the inner jump table, so about 508 of 1,364, 37%. nbody about 17% (Float is 29% and loses 45% of it, plus about 4% for the sqrt loop's constant and compare). spectral_norm about 15%. fib 2.5 Int instructions x 6 = 15 of 355, 4%. tak about 3%. About 1% on ten others. Geomean factor about 0.96, so about 5.86. The full 92 to 22 arrives only with steps 3 and 4.

**Risk.** Low to medium. The Float check moves from the operands to the result and relies on step 1's test. There are many near-identical handlers, which is why the macro writes both the handler and its test. Behaviour does not change, and the golden tests cover it.

### 3. 3. One 16-byte instruction enum in one code array, matched in place

**Changes.** crates/scarlet_vm/src/code/ins.rs: `Ins<R>` with the size assert on `Ins<Reg>`, `Reg(u8)`, `VReg(u32)`, `Ins::operands` (the one list of each instruction's registers and roles) and `try_map`; operand lists inline (6 or 8 registers) or in `Code::lists`, whose entries may name a constant; strings, binaries, `Broken` and `VariantRef` in side tables. Today's `Instr` is deleted, and translate, liveness and allocation move to `Ins<VReg>` and `Ins::operands`. code/emit.rs: every function into one `Vec<Ins<Reg>>` with absolute targets, `Halt` at index 0, `Code::funcs` with entry and arity, and a `Bad` at the entry of a function that is not built yet or that colours past 254 registers; `translate` builds a long literal in chunks. The loader refuses more than 250 parameters with a typed error. exec.rs: fetch with `code.get(pc)` and `match *ins`; cold state behind one `&mut Rest`; long arms out of line; every failing arm returns through one cold `fail` that stores `At`. Frames, calls, the sweep and the release on write are untouched, so `Frame` holds an entry pc where it held a body. scarlet/src (dis): print the instruction listing next to the Core IR.

**Expected gain.** Dispatch falls from 15 to 9, plus the branch back that is there today, so 6 on every VM instruction (the sceptic's corrected figure, and what I read in objdump for the 92-arm and the 140-arm probe). fib 4.5 x 6 = 27 of 355, 7.6%. mandelbrot about 5%. deriv and cfold about 7%, list, binary_trees and spectral_norm 6% to 7%, most others 3% to 5%, sieve 0. Geomean factor about 0.955, so about 5.6. Acceptance: the dispatch block in objdump is 9 instructions and `steps` has one indirect branch.

**Risk.** Low for meaning, medium for size: it rewrites the instruction enum and every arm. The trap is measured: bind the instruction by value and dispatch is 15 again. A function wider than 254 registers stops when called until step 7. I bounded every function in the repository's 85 programs at about 133 registers, so nothing there is affected, but a user program could be.

### 4. 4. The frame window, the link in register 0, and calls that copy their arguments, with today's ownership kept

**Changes.** New crates/scarlet_vm/src/frame.rs: `Window` over `&mut [Value; 256]` with `look`, `take` and `put`, `window()`, the link word. code/alloc.rs: parameters pinned at registers 2 and up, registers 0 and 1 never handed out, `shift` on every call. code/emit.rs: Call1/2/3, Call1D/2D/3D, Call, CallWide, CallValue, CallValueWide, Ret, Halt, Capture reading register 1, the refusal of a call by name to a function that owns register 1. A `dups` bit is the opposite of today's `moved` flag from `moves_on_last_use`. exec.rs: `run` and `steps` with `Pause::{Done, NeedStack}`, the room test first in every call arm, `InternalStackDepth` and error traces walking links; delete `Frame`, `callers`, `top`, `enter_above`, `open`, `callee` and `Frame::env`. Ownership stays as it is today: `put` releases what the register held, a moved operand leaves `Nil`, and `Ret` carries the frame's register count and sweeps it. Tail calls keep `call_args` and `again` over the window for this step. Tests: a seven-parameter function passed as a value and called, and deep recursion from a 256-slot stack with a cell count.

**Expected gain.** Measured in the probe build that keeps the sweeps: fib is 164 to 168 a call against 355, and about 313 of that was left after steps 2 and 3, so this step is about 147 of 355, 41%. Call and return of `id(n)` is 112 to 119 against 223. tak goes from about 341 to about 231 here (29%) and to 193 with step 5. For the modelled rows, steps 4 and 5 together are a geomean factor of 0.81 to 0.86, of which this step is about 0.88, so about 4.9 to 5.0. Cycles: fib 51.6 to about 35 a call.

**Risk.** The largest change in size, but reference counting is still right by construction, so a mistake shows as a failing test and not as a leak. Things to get right: no share or write before the room test, the window retaken after every change of `base`, register 1 owned only by closure bodies, and LLVM keeping `win`, `pc` and `base` in machine registers. Read `Call1`, `Ret` and the dispatch block in objdump after the change, and check that no register saves sit on a call's fast path because of a cold call.

### 5. 5. Tail calls as moves and a jump, with today's ownership kept

**Changes.** crates/scarlet_lower: the tail-argument scheduling pass, before Perceus (a pure Int or Float `let` whose only use is an argument of a tail call to a known function moves to just before the call, ordered so the one replacing parameter i follows the other reads of parameter i). crates/scarlet_vm/src/code/moves.rs: a tail call to a known function becomes a parallel move into the parameter registers, packed into Move, Move2 and Move3 (each reads all sources before it writes, so only a cycle longer than three uses a scratch register), then a temporary `Clear { from, n }` that releases the frame's other registers, then `Jump`. code/alloc.rs: the value feeding parameter i prefers parameter i's register when they never clash. The `entry:` and `loop:` layout: token `Nil` stores at `entry`, and a self tail call jumps to `loop`. exec.rs: TailCallValue and TailCallValueWide read their arguments into locals; delete TailCall, TailCallSelf, `call_args`, `again`, `enter`, `passed` and `passing`. Test: a loop with a dead cell parameter and a carried reuse token, with a cell count.

**Expected gain.** Measured in the probe build that keeps the sweeps: the bare loop is 93 to 94 a step against 346 (113 to 115 without the scheduling pass), `escape` is 322 to 328 a turn against 1,364, and tak is 191 to 195 a call. My scan of the 90 self tail calls in the benchmarks finds 73 need no move, 6 more need none after scheduling (4 of them in spectral_norm), 9 keep one and 2 permute. Geomean factor for this step about 0.93 to 0.95, so the product of steps 1 to 5 is 0.734 to 0.777 and 6.17 goes to 4.5 to 4.8.

**Risk.** Medium. Parallel moves are easy to get wrong on a cycle, and the allocator preference was not built by anyone: every probe placed `escape` by hand. A weak preference costs moves (15 for a `Move`, 23 for a `Move3`) and never correctness. `Clear` is thrown away in step 6; it is about 15 lines.

### 6. 6. Ownership stated by the compiler and verified at load, in four landings

**Changes.** (a) crates/scarlet_vm/src/heap.rs: `release` and `share` return a typed `Broken` when the count is 0, on the cold side of the existing `c > 1` test; the live-cell counter stops saturating. (b) crates/scarlet_ir and scarlet_lower (the compiler area's work, and this step waits on it): a never-a-cell bit per `RTy` in `ResolvedPool`; one function in scarlet_ir saying which operands of each atom are taken; Perceus writes move or copy on each of those and a `Drop` for every non-scalar local that dies unmoved, Int, String and type variables included, inside joins too; the two token rules written on `Atom::Ctor::reuse`. (c) crates/scarlet_vm/src/code/translate.rs encodes them as `dups` bits, kill bits and `Drop { n, r }`; code/verify.rs: the forward walk with `Empty`, `Scalar`, `Int`, `Any`, `Token` (and `Lent` reserved), `Broken::Unverified` in stop.rs, `Checked`, and the per-call-site maps; the generated test per instruction and per built-in, the role check behind `look` and `take` in test builds, and the test-only audit run. In this landing every move, kill and drop still stores `Nil` and the sweeps stay but count what they release, and a test asserts that count is zero on every golden program. `moves_on_last_use` is deleted. (d) exec.rs: `put` becomes a plain store and `take` a plain read; delete the `Ret` sweep, `Clear`, the `Nil` stores and `Body::carried`. Tests: `n * n`, `n - n` and `n == n` on a big dying `n`.

**Expected gain.** Measured as the difference between the two probe builds: fib 164 to 168 becomes 112 to 118, tak 191 to 195 becomes 101 to 106, `escape` 322 to 328 becomes 256, the bare loop 93 becomes 87, and a call and return of `id(n)` 112 to 119 becomes 55 to 60. Cycles: fib about 35 to 25.5 to 29.5 a call, tak about 36 to 22.5 to 23.7, against LuaJIT's 21.1 and 21.4. For the modelled rows a write loses its test of the old value and a return its sweep: geomean factor 0.90 to 0.91, so 4.5 to 4.8 becomes 4.1 to 4.4. This is the same gain the compiler area's probe measures as 'old contract to new', and it is booked here once. Dead `Field` reads and drops that never free are the compiler's and are not counted.

**Risk.** The highest of all steps, which is why it is last and in four landings that can each be reverted alone. After (d), reference counting is right because Perceus is right and no longer by construction. What guards it: the verifier checks another crate's output at every load, the heap stops on a doubled release, the zero-sweep test in (c) proves the sweeps had nothing left to do before (d) deletes them, and the audit run and per-instruction tests cover a handler that disagrees with its roles. A String or type-variable value that dies after a read that only looks at it costs a dispatched `Drop` (about 12 plus the release, per run of drops) where the sweep cost 9 a register. Acceptance for (c): VM instructions per iteration, counted with a counter feature, must not rise on list, rbtree, deriv, cfold and binary_trees.

### 7. 7. Functions wider than the window

**Changes.** crates/scarlet_vm/src/code/alloc.rs: a function that still colours past 254 registers, after constants in operand lists and chunked literals, keeps the extra values in a spill stack in `rest`, with `SpillEnter`, `SpillLeave`, `Spill` and `Unspill` and three scratch registers; a wide form's list may name a slot. verify treats a slot as one more register, and the maps list slots. Tests: 300 values live at once, and a 300-field constructor.

**Expected gain.** None for speed. It removes a `Bad` stop for a function no program in the repository has: the widest of 1,352 functions needs at most about 133 registers.

**Risk.** Low for the benchmarks, because nothing reaches it. It is a second storage class that only its own tests exercise, so it is built once, after the rules it must follow have stopped changing.

### 8. 8. Later, with processes: count reductions at calls and loops

**Changes.** exec.rs: a `fuel` counter in the loop, decremented in every call arm and in a new `Loop` instruction, which a tail call to a known function emits in place of `Jump`; `Pause::OutOfFuel` returned through the same door as `NeedStack`, with the call left to run again. code/moves.rs: emit `Loop`. Decide the stack's growth rule and measure spawn. Not part of the speed work.

**Expected gain.** A cost. Measured in the probe with the counter as a local in `steps`: fib 112 to 116 (118 to 125 with 140 arms), tak 101.5 to 106 (106 to 112), the bare loop 87 to 91 (94), `escape` 256 to 260. So with a scheduler fib is 116 to 125 against LuaJIT's 122.

**Risk.** Low for meaning. The cost depends on where LLVM puts the counter, so measure it when it lands.

## Experiments

**What I checked first.** I re-ran the earlier merged probe and both critics' copies as they left them, and every number reproduces: merged fib 112.0, tak 106.7, loop 88.0, escape 257.2; the third critic's builds give fib 121.0 with a `dups` test inline in `Call1`, 135.1 with it out of line, 132.0 "as written", and 170.0 / 180.3 / 92.0 / 283.1 with today's ownership on the new frames. I re-ran fib and tak on today's binary (10.64e9 and 38.45e9 instructions, 1.55e9 and 5.37e9 cycles) and on LuaJIT with its JIT off (3.66e9 and 11.36e9 instructions, 0.639e9 and 2.17e9 cycles).

**The probe for this design** is `perf-scratch/final-core/` (`#![forbid(unsafe_code)]`, Rust 1.97.1, its own target directories). `src/main.rs` is the loop, `src/progs.rs` the hand-assembled programs, `build.sh NAME FEATURES` builds a variant, `all.sh` prints instructions and cycles per unit, and `tr.sh` single-steps under lldb. I started from the second critic's copy and changed it to what this design says: `Call1/2/3` with no share code and `Call1D`/`Call3D` with exact per-argument cell tests, the link written as a tagged small Int, `CallValue` through a `funcs` table with an arity compare, every slow path's stop routed through one cold function that stores `At`, `JumpUnlessTrue` stopping on a non-Bool, a stored Float compare through `partial_cmp`, and `finite()` as the only Float constructor. Every program prints the right answer in every build.

| build | fib | tak | loop | loop in Core order | escape | closure step | generator |
|---|---:|---:|---:|---:|---:|---:|---:|
| final, 92 arms | 112.0 (25.5 cyc) | 101.5 (22.5) | 87.0 | 102.0 | 256.1 (65.8) | 290.1 | 140.0 |
| final, 140 arms | 117.5 (29.5) | 106.3 (23.7) | 87.0 | 102.0 | 257.1 (66.7) | 293.0 | 141.1 |
| share test inline in `Call1` and `Call3` | 115.1 | 103.0 | 87.0 | 102.1 | 256.2 | 290.1 | 140.1 |
| one combined test and a cold share | 113.5 | 106.1 | 86.0 | 101.1 | 256.1 | 290.1 | 140.1 |
| today's ownership on the new frames, 92 arms | 163.6 (35.9) | 191.3 (37.5) | 94.0 | 115.0 | 328.2 | | 156.1 |
| the same, 140 arms | 168.1 | 195.3 | 93.0 | 113.0 | 322.2 | | 154.0 |
| fuel counted, 92 arms | 116.1 | 105.6 | 91.0 | | 260.1 | 298.1 | |
| fuel counted, 140 arms | 125.1 | 111.8 | 94.0 | | 261.1 | 308.1 | |

Per VM instruction in the final 92-arm build, from the lldb trace with its one-per-step overcount taken off: Int compare with a constant and jump 22, Int compare of two registers and jump 25, Int op with a constant 25, Int add 28, Float op 22 to 23, `Jump` 12, `Move` 15, `Move3` 23, `Call1` 28, `Call1D` 32, `Call3` 33, `Call3D` 47, `Ret` 23, `CallValue` 90, `Capture` 34, `Drop` 28. With 140 arms `Call1` is 30 and `Ret` 26. Dispatch is the same 9 instructions in both builds and `steps` has one indirect branch.

**What the experiments decided.**

- *The call forms.* A share test in `Call1` costs fib 3 a call here and 9 to 23 in the third critic's builds, so the plain forms hold no share code. For the two tak calls that keep arguments, `Call3D` gives 101.5 and the general `Call` gives 110.1, so the `D` forms exist.
- *The build order.* With today's ownership kept on the new frames (a write tests the old value, a move leaves `Nil`, `Ret` sweeps, and a tail call clears its temporaries), fib is already 164 to 168 and the loop 93 to 94. So ownership is worth about 50 a call on fib, 90 on tak and 65 a turn on `escape`, and it can land last.
- *The stop location, the Bool check, the tagged link and `finite()`* cost nothing measurable: fib stayed at 112.0 and `escape` at 256.1.
- *The cold paths.* A Float add handed an Int stops, a stored Float compare handed an Int stops, a fused compare handed an Int stops, `1.5 / 0.0` is 0, `1e300 * 1e300` is the largest Float, and a jump on an Int stops.

**Scans of real Core IR** (`final-core/scan/`, each reads `scarlet dis`).

- `tailsched.py`: of the 90 self tail calls in the benchmarks' own functions, 73 can compute every argument in place in today's order, 6 more can after tail-argument scheduling, 9 keep a move and 2 permute parameters. The second critic's count of 48 clashes treated a `drop` line as a read. Most of those drops sit right after the instruction that takes the value, and under the new contract they are that instruction's move. I also confirmed his example: `go(n - 1, acc + n)` does lower to the subtract first.
- `maxlive.py`: over the 1,352 functions printed for the 85 programs in `benches`, `examples` and the test programs, the most registers any needs is about 133, and that function is a 132-element array literal. Sixteen functions have over 254 locals before colouring and none comes near 254 live at once. The bound is exact for straight-line code and approximate across join points.
- `table.py`: the per-benchmark table and its geomean (0.734 before ownership and 0.659 after; 0.777 and 0.710 with every modelled fraction cut by a quarter; 0.747 and 0.677 with the inliner's calls removed).

**One enum for loader and loop.** `final-core/ins-generic/` compiles `Ins<R>` with a single `operands` role function and a `try_map`. It is 16 bytes over `Reg(u8)` and 56 over `VReg(u32)`.

**Claims I read in the source and did not run.** `Heap::release` (heap.rs 281 to 310) has no branch for a count of 0 and `free_cell` uses `saturating_sub` on the live count, so a doubled release is silent today. `allocate`'s doc comment (code.rs 2104 to 2114) says register sharing relies on a write releasing the old value. `perceus.rs` 113 to 121 says a temporary inside a join is never dropped because the VM sweeps. `ResolvedPool::is_heap` answers false for Int, String and type variables.

**Limits.** The probe has no loader: scheduling, the allocator's preference, `moves`, `verify` and the spill path exist as rules, and the programs were assembled by hand as those passes should emit them. Its heap is a toy, so the closure figure is the least sure, and I did not get a trustworthy closure number for the build that keeps the sweeps. Only fib, tak, the loop, `escape`, an identity call, one closure call and the map_churn generator were run. The other twenty benchmark rows are arithmetic on the diagnosis's shares.

## Prior art

**Read in this session.**

- Lean 4, `src/library/compiler/ir_interpreter.cpp` at v4.12.0, https://raw.githubusercontent.com/leanprover/lean4/v4.12.0/src/library/compiler/ir_interpreter.cpp (through a summarising fetch that quoted the lines). Its header says "IR variables are mapped to stack slots by adding the current base pointer to the variable index". Its `Inc` and `Dec` cases only run what the IR holds: `inc(var(fn_body_inc_var(b)).m_obj, ...)` and a loop of `dec(...)`. It adds counts of its own only at its edges (cached constants, boxed calls into native code, literals). This is the model for "the compiler decides, the VM executes": I moved ownership out of the loader because of it.
- Lua 5.4.6 `lparser.c`, https://raw.githubusercontent.com/lua/lua/v5.4.6/lparser.c (same kind of fetch). `closelistfield` flushes a table constructor with `luaK_setlist` every `LFIELDS_PER_FLUSH` items, so a long literal never holds all its items in registers. `MAXVARS` is 200, with the comment "must be smaller than 250, due to the bytecode format", and parameters count against it. I took chunked literals as the way to keep functions inside the window, and the parameter limit.
- The probes in `perf-scratch/`: `merged-core`, the critics' `numbers-core-attack` and `simple-future-attack`, and the compiler area's `perceus-passes`, whose README measures the old ownership contract against the new one (fib 0.776, the bare loop 0.739, list 0.683 owned and 0.454 with borrow inference) and whose new contract already uses a per-argument `copy` mask. I matched that encoding.
- `docs/vm-design.md` and `docs/perf-diagnosis.md`, and `crates/scarlet_vm/src/{exec, code, heap, value}.rs`, `crates/scarlet_lower/src/perceus.rs` and `crates/scarlet_ir/src/rty.rs` for what the loader is given and what today's rules rely on.

**Read by the earlier designers and critics, not re-read by me.** I rely on their reports for these.

- Lean 4, `src/Lean/Compiler/IR/RC.lean`, https://github.com/leanprover/lean4/blob/v4.12.0/src/Lean/Compiler/IR/RC.lean. An owned argument gets an `inc` unless this is its last use and it is not passed again in the same call; `addDecForAlt` adds a `dec` for a variable dead in one alternative; `addDecForDeadParams` drops an owned parameter nothing reads. These are the rules Perceus must follow to meet the contract.
- LuaJIT 2.1 `src/lj_frame.h`, https://github.com/LuaJIT/LuaJIT/blob/v2.1/src/lj_frame.h. A frame keeps its link in the slots below `base`. I took the link in a stack slot, and put the shift and the destination register in it so `Ret` does not read the caller's code.
- LuaJIT 2.1 `vm_arm64.dasc`, https://github.com/LuaJIT/LuaJIT/blob/v2.1/src/vm_arm64.dasc. `ins_NEXT` is 6 instructions copied into every handler, which is the yardstick dispatch misses by 3 to 4. `ins_arithdn` does `adds` then `bvs`, which is the overflow flag as range check.
- Lua 5.4.6 `lopcodes.h` and `lvm.c`, https://github.com/lua/lua/blob/v5.4.6/lopcodes.h and https://github.com/lua/lua/blob/v5.4.6/lvm.c, for the K operand forms and compare-against-immediate forms.
- Erlang/OTP `beam_makeops.md`, https://github.com/erlang/otp/blob/master/erts/emulator/internal_doc/beam_makeops.md. The loader turns a generic instruction into a specific one by what it knows about the operands. That is the model for choosing `Call1` or `Call1D`, and `IntAdd` or `IntAddK`.
- The Java VM specification, section 4.10, https://docs.oracle.com/javase/specs/jvms/se21/html/jvms-4.html#jvms-4.10, and WebAssembly's instruction validation, https://webassembly.github.io/spec/core/valid/instructions.html. Both check code another program wrote, once, at load. With ownership decided in the compiler, `verify` is now that kind of check.
- wasmi 0.40, https://github.com/wasmi-labs/wasmi/blob/v0.40.0/crates/wasmi/src/engine/executor/instrs.rs, and Pulley, https://github.com/bytecodealliance/wasmtime/blob/main/pulley/src/interp/match_loop.rs: stable-Rust match loops over an enum matched in place.
- Matt Keeter, "A tail-call interpreter in (nightly) Rust", https://www.mattkeeter.com/blog/2026-04-05-tailcall/, for the `u8` index into a 256-slot array. Rust tracking issue 112788, https://github.com/rust-lang/rust/issues/112788: `become` is refused by 1.97.1.
- Reinking, Xie, de Moura and Leijen, "Perceus", https://www.microsoft.com/en-us/research/publication/perceus-garbage-free-reference-counting-with-reuse/, and Ullrich and de Moura, "Counting Immutable Beans", https://arxiv.org/abs/1908.05647, for the ownership rule and borrow inference.
- Rideau, Serpette and Leroy, "Tilting at windmills with Coq", https://xavierleroy.org/publi/parallel-move.pdf, for the parallel-move algorithm. Nobody fetched it successfully, so treat it as a pointer.

## What it gives up, and what the critics found

**What the design gives up.**

- **After the last step, reference counting is right because Perceus is right.** Today a register releases whatever it held, whatever the compiler did. The verifier, the heap's stop on a doubled release, the audit run and the per-instruction tests guard the change, and it lands last so it can be reverted alone. It is still the part to review hardest.
- **The interpreter's last step waits on the compiler.** Step 6 needs Perceus to emit complete moves, copies and drops and a never-a-cell bit per type. Until then the VM stops at 4.5 to 4.8 on the geomean.
- **About 2 to 3 instructions on every Int instruction**, for the exact tag test.
- **Every call copies its arguments**, about 4 instructions each.
- **More opcodes.** There are about 115, with constant-operand forms, compare-and-jump forms and call forms split by whether anything is shared. `docs/vm-design.md` says "no fused super-instructions", and that line needs rewording.
- **A fixed window.** A function names 254 registers and takes at most 250 parameters. A wider function stops when called until the spill step. A process's stack is 2 KB at spawn and 3 KB after its first call.
- **Dead registers hold stale words.** The per-call-site maps say which registers are live, and `scarlet dis` has to print the instruction listing, or a frame will be hard to read.
- **One dispatch site.** Dispatch is 9 instructions and a branch back against LuaJIT's 6 copied into each handler. fib retires fewer instructions than LuaJIT and still takes 1.2 to 1.4 times as long.
- **A closure owns itself** until borrow inference lends it: a closure call is about 203 instructions.
- **Two things are thrown away on the way**: the `Clear` instruction and the sweeping `Ret`, which exist only between steps 4 and 6.

**Where it is weakest.**

- No loader pass is built. Scheduling, the allocator's preference, `moves` and `verify` are rules, and every probe ran hand-assembled programs. My scans of real Core IR cover tail calls and frame width only.
- Twenty of the 23 benchmark rows are arithmetic, and the shares for several were read off the diagnosis's gain lists. After step 3 lands, re-derive them from real traces of binary_trees and list.
- The loop's speed depends on LLVM's register allocation. The same source moved by up to 10 instructions a call between builds.
- The probe's heap is a toy, so anything that touches cells will cost more until the heap work lands.

**What the critics found, and what I did.**

1. *A share test in `Call1` and a table lookup in `CallValue` were specified and not measured; fib was 116 to 135, not 112.* True. I split the call forms so the plain ones hold no share code, built everything the text specifies into the probe, and re-measured: fib 112 (118 with 140 arms), closure call 203. The headline is now "level with LuaJIT within 10".
2. *The bare loop's 88 depended on an instruction order no pass produces.* True, and I confirmed it in `scarlet dis`. I added tail-argument scheduling before Perceus and quote both numbers (87 and 102). His count of 48 clashing tail calls was too high, because it read drops as reads: my scan finds 73 of 90 already in place, 6 fixed by scheduling and 9 that keep a move.
3. *Ownership was derived twice, in Perceus and in the loader.* True. Perceus now states it once, the loader's `own` pass is gone, and the verifier checks the compiler's output. A `Lent` state is reserved for borrowed parameters.
4. *The riskiest step sat in the middle.* True. Ownership is now the last interpreter step, in four landings, and I measured the stage before it (fib 164 to 168).
5. *"Land the verifier first against code that still sweeps" could not work.* True. The landing where both rules hold stores `Nil` on every move, kill and drop and asserts the sweeps release nothing.
6. *The verifier cannot see a role table that disagrees with a handler, and a doubled release is silent in today's heap.* True, and I read the heap code. I added a typed stop on a count of 0, an exact live count, a generated test per instruction, `look` and `take` accessors checked in tests, and an audit run.
7. *Two instruction enums could not share "one table".* True. There is one enum, `Ins<R>`, and I compiled it.
8. *One "prologue" mixed code that runs once with code that runs every turn.* True. An entry has an `entry:` and a `loop:` label, and the dead-parameter drops are part of the body.
9. *A reuse token had no verifier state, and the loader relied on unwritten Perceus rules.* True. I added `Token`, kept `fits_ctor`, and the rules go on `Atom::Ctor::reuse`.
10. *The scalar class had no sound source.* True. The compiler supplies a never-a-cell bit per type.
11. *Step 4 added dispatched drops while counting drops as a gain.* True. `Drop` takes up to 13 registers, removed no-op drops are booked to the compiler, and the acceptance check counts VM instructions on five cell-heavy benchmarks.
12. *The geomean of 4.3 multiplied overlapping factors and step 5's 0.86 did not follow from its own list.* True. I replaced the per-step products with one row per benchmark, and I quote 4.5 to 4.8 with today's ownership and 4.1 to 4.4 after.
13. *Gains were booked in two areas.* True. The design has a ledger, and quotes the geomean with the inliner's calls removed as well (4.6 and 4.2).
14. *Every gain was in instructions.* True. The scoreboard has cycles, each step is judged on both, and the time goal is stated (about 2.3 to 2.4 from 2.83).
15. *The spill path was a second storage class nothing needs.* True: no function in the repository needs more than about 133 registers. It is deferred to its own step, and a wide function is a `Bad` until then. I kept the second stack as its eventual shape, because slots above the window on the same stack need a shift wider than the link word holds.
16. *Stale registers leave nothing able to walk a stack.* True. `verify` writes the owned registers at every call site.
17. *Smaller points, all taken:* one kill bit per register and a fresh value from `int::slow`; the divide and negate range test; a rule for which Float operations may be instructions, and `partial_cmp` in stored compares; no unchecked Float constructor; a stop that records where it happened; wide forms for calls through a value; a Bool check in `JumpUnlessTrue`; `kill:a` on the listings that write over their own operand; `Move2` and `Move3` as parallel moves; a stack of 3 KB and not 2 KB after one call; and code that is only ever appended, so a pc stays valid.

## Processes and a JIT

**Processes.** The design is shaped for them and builds none of them.

- A run's whole state is its `stack: Vec<Value>`, `At { pc, base }` and its heap (and later, rarely, a spill stack). No Scarlet frame is on the host stack, nothing points into the register stack, and there is no frame list to save. A process is that struct, and moving it to another scheduler thread moves those values.
- `steps` already returns to an outer loop when it needs something it cannot do itself (`Pause::NeedStack`). Running out of fuel, waiting on a mailbox and blocking on IO are further variants of `Pause`, and the scheduler is the outer loop.
- A call that cannot go ahead has shared and written nothing, and `at.pc` names the call itself. So a yield at a call resumes by running that instruction again, with no half-done state to describe.
- Reductions are counted at the call arms and at a `Loop` instruction, which a tail call to a known function emits in place of `Jump` when step 8 lands. So a loop written as tail recursion yields too. Measured: fib +4 to +8 instructions a call, tak +4 to +6, the bare loop +4 to +7, `escape` +4. With fuel counted fib is 116 to 125 against LuaJIT's 122.
- A stop records `At { pc, base }` before it returns, so a dead process can say why and where straight away, and its trace is a walk down the links.
- A process's stack is 256 slots (2 KB) at spawn and 384 slots (3 KB) once it has made a call, because a callee needs a full window above its base. That is more than BEAM's 2.6 KB for a whole process. A million processes that have each made a call hold 3 GB of register stack. The growth rule and the cost of spawn should be measured when processes land. Once ownership has landed, fresh slots need no particular value, so the stack can be allocated zeroed.
- Nothing scans a stack while a program runs, and a process that dies returns its heap in whole chunks. When something does need the live values of a stack, the verifier's map at each call site gives them: every frame below the top is paused at a call. That keeps compacting a heap on hibernate possible, which `docs/vm-design.md` lists as BEAM's answer to fragmentation.
- A function value holds a `FuncIdx`, which `Code::funcs` maps to its entry and its name. So the decision that a closure on the wire names its code is untouched.
- Loading code again appends the new functions and repoints `funcs[idx]`. No pc in any link or call instruction changes, and no value in any heap is rewritten. Calls by name inside old code keep running the old code, and calls through a function value reach the new code, which is close to how BEAM treats local and remote calls across a reload.

**A JIT.** The instructions are a good input for one.

- They are fixed-width, each operator is its own instruction, and jump targets are indexes into one array. A template compiler maps each instruction to a short run of machine code and each `to` to a label.
- Ownership is explicit in the instruction stream (`Dup`, `Drop`, `dups` and `kill` bits, and the choice of call form), so compiled code and the interpreter release at the same points by construction.
- The frame layout is data on the register stack and is fixed: the link in register 0, parameters from register 2, the callee's window at `base + shift`. Compiled and interpreted code can call each other through the same link word if the link grows a bit saying which kind of code the return position names.
- Compiled code keeps the run-time checks the interpreter has (the Int tag test and the Float result test) and calls the same cold functions. A JIT that wants to drop the Int test can use the verifier's register classes. That is the place to decide whether the compiler's typing is trusted, with the interpreter as the reference.
- The verifier's maps are the stack maps a JIT needs at a call or a yield.
- `Ins::operands` gives a JIT the same per-instruction list of reads, writes and deaths that liveness and the verifier use.

**What this design does not decide.** The heap's cell format, the header a constructor writes and the cost of `share` and `release` belong to the heap work. This design needs a cell test on the value word, `share` and `release` entry points that stop on a count of 0, and a release that copes with a hollowed cell. Where Perceus places each move, copy and drop, borrowed parameters, and lending a closure to its callee belong to the compiler's work. This design fixes only the form those facts arrive in and the check the VM runs on them.

