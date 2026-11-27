# Speed design: What the compiler hands the VM: inlining, multiple values without tuples, match as a switch, dead binds and no-op drops, borrow inference, reuse and record update

This is the design for one area of the speed work, written on 23 November 2026. `docs/perf-plan.md` puts its steps in order with the other areas' steps, and settles where the areas disagree. Where this file and the plan differ, the plan is right.

# Counted Core in two halves

## The decision

The compiler will write every reference-count change into the Core IR, a checker will prove the counts balance, and the VM will stop counting by itself. The work is in two halves, and the plan of record is the first half plus the smallest version of the second.

**The first half (steps 0 to 8) is a classic middle end on today's contract.** A record update becomes one instruction. A match becomes a decision tree of switches that reads a field only where it is used. A simplifier with an environment inlines small functions and known lambdas, and a recursive function is specialised on a known lambda. The VM keeps its sweeps and its release on write the whole time, so a compiler mistake in this half costs time and never a wrong answer. By my arithmetic this half is a factor of 0.845 on the geomean (6.17 to about 5.2).

**The second half (steps 9 and 10) is the smallest ownership contract that lets the VM stop sweeping.** Every local is owned. Every operand carries one of three marks. A checker proves each owned local ends exactly once on every path, and the loader accepts only what the checker returned. Step 9 states the counts while the VM still checks them. Step 10 removes the VM's own counting. The speed this buys belongs to the call, register and tail-call items of the diagnosis, which cannot be fixed soundly without it.

**Three more parts are designed here and wait for the scoreboard after step 10.** They are `Open` (take a dying cell apart in one instruction), borrowed parameters, and a record update that takes a field out first. Together they claim about 0.97, and they hold most of the risk nobody has measured.

**Four things are left out until a measurement asks for them.** They are reuse tokens, borrows rooted in a local of the same frame, several results in registers, and kept-field masks. Each has a number below that says why it can wait.

One correction to the brief: there is no `crates/scarlet_elaborate`. The elaborator is `crates/scarlet_lower/src/translate.rs` and `typed_ir.rs`, and the type checker is `crates/scarlet_check`.

## The pipeline

```
typed IR
  lower + matching   ANF. A match is a decision tree of Switch and Probe. A field is read
                     where it is used. R(..base, f: x) is one Update.
  immediates         as today: Bool and Nil are atoms, a Bool switch is an If
  simplify           inside one function: copy, known field, common subexpression,
                     known constructor, flatten a jump, dead code
  inline             a small callee, a callee with one reference, a known lambda
  specialise         a self-recursive function cloned for a known lambda
  ---- memory: a Raw body goes in, a Counted body comes out ----
  borrow             which parameters are borrowed (step 12; before it, none)
  rc                 a mark on every operand, Drop, and later Open
  verify             the ownership checker, in scarlet_ir; the loader takes only its result
```

`build_program` in `crates/scarlet_lower/src/program.rs` stays the only place the passes run. It lowers every function first, then runs the whole-program passes, then the memory passes.

## Core IR after the change

Everything here is in `crates/scarlet_ir/src/core_ir/`.

### How a value is held

```rust
/// Whether a value of a type can ever be a heap cell.
pub enum Repr {
    /// Never a cell: Float, and a type with at least one constructor and no fields on any.
    Word,
    /// May be a cell. Everything else: Int (a big one is a cell), String, Array, Map,
    /// a type variable, a function, a type with no constructors, any type with a field.
    Counted,
}
impl Program { pub fn repr(&self, ty: RTy) -> Repr }
```

`Program::repr` is one function over the type table. Passes, the checker and the loader all call it, so a bind carries its type and nothing can disagree with it. A type the table does not know is `Counted`. A wrong `Counted` costs a tag test. A wrong `Word` leaks. Neither can release a cell twice.

There is no "always a cell" value. A standalone probe measured a drop of a live cell at 21 instructions with no tag test and 24 with one, so the third value would save 3 instructions and add a way to decrement the wrong header.

### Stages and marks

```rust
/// Which side of the `rc` pass a body is on.
pub trait Stage { type Use; type Rc; type Hold; }
pub enum Raw {}      // Use = LocalId, Rc = Infallible, Hold = ()
pub enum Counted {}  // Use = Use,     Rc = (),         Hold = Hold

pub struct Use { pub local: LocalId, pub pass: Pass }

/// What an operand does with the frame's reference.
pub enum Pass {
    /// Look. The count does not change.
    Lend,
    /// The frame's reference ends here. A position that keeps the value takes it over.
    /// A position that only looks releases it after reading.
    Give,
    /// Make one more reference and hand that over. Only where the position keeps it.
    Copy,
}

/// Arrives with step 12. Before that every bind is owned and the field does not exist.
pub enum Hold {
    /// The frame holds one reference and ends it exactly once on every path.
    Owned,
    /// No reference. It was read out of a borrowed parameter or a global, so a
    /// suspended caller or the program holds it for as long as this frame runs.
    Borrowed,
}

pub struct CoreBind<S: Stage> { pub id: LocalId, pub ty: RTy, pub hold: S::Hold }
```

A borrowed local never points into something this frame owns. That is the rule Lean and Koka both keep, and it is what lets a borrow need no root and no lifetime: it is valid from its bind to the end of the frame.

### Positions

One table, in a new `sig.rs`, says what every operand position does. The checker and the `rc` pass both read it.

```rust
pub enum Takes {
    /// Reads the operand. The count is as it was afterwards.
    Looks,
    /// Reads the operand, and the result holds one more reference to it, which the
    /// handler adds. For views: a binary slice, an array slice.
    Shares,
    /// Takes one reference over: stores it, changes the value in place, or releases it.
    Keeps,
}
pub enum Gives { Fresh, PartOf(u8), Word }   // a new reference; a part of operand k; never a cell
impl PrimOp    { pub fn sig(self) -> Sig }
impl Intrinsic { pub fn sig(self) -> Sig }
impl ProbeOp   { pub fn sig(self) -> Sig }
```

| Position | Takes |
|---|---|
| a `Ctor` field, a `Closure` capture, an `Update` base and each value it sets | Keeps |
| `Ret`, `SetGlobal`, a `Jump` argument to an owned join parameter | Keeps |
| a call argument to an owned parameter, every argument of a call through a value, the callee of a call through a value | Keeps |
| a call argument to a borrowed parameter, a `Jump` argument to a borrowed join parameter | Looks, and only `Lend` |
| a `Switch` scrutinee, an `If` condition | Looks |
| a primitive or built-in operand | `sig()`: Looks unless listed |

The built-ins that keep are a short named list: `MakeTuple`, `MakeArray`, `ArrayAppend`, `ArrayPrepend`, `ArrayDrop`, `ArrayConcat`, `MapSet`, `MapDelete`, binary append, and the `ArrayUncons` probe. `PartOf` is allowed only for `Field`, `TupleField` and `Capture`, which read a fixed slot of a cell. `ArrayElem` is `Fresh`, because an element of a lazy range is computed and can be a new big Int.

### Nodes

```rust
pub enum Atom<S: Stage> {
    Copy(S::Use), Const(ConstId), Global(GlobalSlot), Nil, Bool(bool),
    Ctor    { variant: VariantRef, fields: Vec<S::Use> },
    /// `base` with some fields replaced: `R(..base, f: x)`.
    Update  { base: S::Use, variant: VariantRef, sets: Vec<(u16, S::Use)> },
    Prim    { op: PrimOp, args: Vec<S::Use> },
    Intrinsic { intrinsic: Intrinsic, args: Vec<S::Use> },
    Closure { func: FuncIdx, captures: Vec<S::Use> },
    Call    { callee: Callee<S>, args: Vec<S::Use> },
}
pub enum Callee<S: Stage> { Known(FuncIdx), Value(S::Use) }

pub enum CoreExpr<S: Stage> {
    Let    { bind: CoreBind<S>, rhs: Atom<S>, body: Box<Self> },
    /// The one place control merges. `body` may jump to it; `cont` runs when it does.
    Join   { id: JoinId, params: Vec<CoreBind<S>>, cont: Box<Self>, body: Box<Self> },
    Jump   { to: JoinId, args: Vec<S::Use> },
    /// A test and nothing else. It binds no fields.
    Switch { scrut: S::Use, cases: Cases<S>, default: Option<Box<Self>> },
    If     { cond: S::Use, then: Box<Self>, els: Box<Self> },
    /// A test that binds. On a hit it binds its results and runs `hit`. On a miss
    /// nothing is bound.
    Probe  { op: ProbeOp, args: Vec<S::Use>,
             hit: (Vec<CoreBind<S>>, Box<Self>), miss: Box<Self> },
    /// Write a module-level slot. Only in a toplevel, once for each slot.
    SetGlobal { slot: GlobalSlot, value: S::Use, body: Box<Self> },
    Ret(S::Use),
    TailCall { callee: Callee<S>, args: Vec<S::Use> },
    /// Counted bodies only (`rc: S::Rc` cannot be made for Raw).
    Drop   { rc: S::Rc, locals: Vec<LocalId>, body: Box<Self> },
    /// Step 11. Take a dying cell apart. Unique: the listed fields move into their
    /// binds with no count change, the others are released, and the cell is freed.
    /// Shared: the listed fields gain a reference and the cell loses one.
    Open   { rc: S::Rc, cell: LocalId, shape: Opened,
             fields: Vec<(u16, CoreBind<S>)>, body: Box<Self> },
}
pub enum ProbeOp { MapGet, ArrayGet, ArrayUncons, BinaryReadUtf8 }
pub enum Opened  { Variant(VariantRef), Tuple(u16), Env }

pub enum Cases<S: Stage> {
    /// Several variants may share an arm: an or-pattern that binds nothing.
    Variant { ty: TypeId, arms: Vec<(Vec<u16>, CoreExpr<S>)> },
    Int(Vec<(i64, CoreExpr<S>)>),
    Lit(Vec<(LitKey, CoreExpr<S>)>),
}
/// A literal as `==` sees it. `-0.0` and `0.0` are one key.
pub enum LitKey { Float(u64), Text(ConstId), Bits(ConstId) }

pub struct CoreFn<S: Stage> {
    /// A lambda with captures takes its own closure as `params[0]`.
    pub env: bool,
    pub captures: u16,
    pub params: Vec<CoreBind<S>>,
    pub result: RTy,
    pub body: CoreExpr<S>,
}
```

What goes, and why:

- `Tail(Atom)` is `Ret` or `TailCall`, so no pass special-cases a constructor in tail position.
- `LetJoin` (one value) and `LetCont` with `Goto` (no values) are one `Join`. An or-pattern that binds names jumps with them, so it is no longer lowered once for each alternative.
- `Match` and `CorePat` go. A `Switch` arm is a tag. A field is read by `let x = Field(i)(s)` on the paths that use it.
- `CoreBind::global` goes. A bind pinned to a slot has no way to end, and a simplifier would delete it, because nothing reads the local. Today's toplevel for three `const` lines is three `let %n@gK = ..` binds and `ret nil`. `SetGlobal` is an ordinary keeping position.
- `Drop { shape }`, `ReuseShape` and `Ctor { reuse }` go at step 9. `Open` replaces them at step 11.
- `Load::Capture` and `Load::SelfClosure` go. A capture is `Capture(i)(env)`, a part read out of `params[0]`. A lambda that calls itself passes its `env` on like any argument.
- A built-in that returns a found flag and a value does not exist. On a miss the value would be a Nil in a register the checker thinks is owned, and no state means "present only when the flag is true". `Probe` binds the value only in the arm where it was found. BEAM's `get_map_elements` and `bs_get_utf8` take a fail label for the same reason.

A `LitKey::Text` or `Bits` is a `ConstId` because `build_program` already interns constants by content (`const_ids: HashMap<Const, ConstId>`), so the id is the identity of the bytes. A float cannot use a `ConstId`, because `Const` compares floats by bits and a match compares them with `==`. I ran the case: `(0.0, 1) -> "A"`, `(-0.0, 2) -> "B"`, `_ -> "C"` prints B for `(0.0, 2)` today, and a tree keyed by `ConstId` would print C.

### The checker's witness

```rust
pub struct Verified<'p>(&'p Program<Counted>);      // only `verify` makes one
pub fn verify(p: &Program<Counted>) -> Result<Verified<'_>, OwnershipError>;
// crates/scarlet_vm/src/code.rs
pub(crate) fn load(program: Verified<'_>) -> Result<Code, Stop>
```

A body cannot reach the VM uncounted, because the loader takes `Program<Counted>`. It cannot reach the VM unchecked, because the loader takes `Verified`. `OwnershipError` is an enum that names the function, the local and the rule.

## The first half: the middle end

### Matching (`scarlet_lower/src/matching.rs`, new)

`lower` hands each `match` to a decision-tree compiler. It is the algorithm in Gleam's `compiler-core/src/exhaustiveness.rs` (Jules Jacobs' note and Maranget's decision trees). A row is a list of checks `(local, pattern)` and a body. The compiler takes the first row's leftmost check, emits one `Switch` on that local with an arm for each constructor that appears, and splits the rows under each arm. A row with no check on that local goes to every arm. Gleam picks the check that appears in the most rows, and that is the knob to turn if tests get copied too much.

- A sub-value is read into a local with `Field` the first time a path tests or binds it, and every later row on that path shares the local. A field nothing names is never read.
- A body reached from more than one leaf is a `Join` whose parameters are the names the arm binds. No body is lowered twice.
- A guard is `if g { body } else { the tree for the rows below }`.
- The type checker has proved the match exhaustive, so a `Variant` switch that lists every variant has no default and no last test.
- Literal rows are grouped by `LitKey`, so `0.0` and `-0.0` land in one case with both rows in it.
- An Int case is keyed by its value. The `SwitchInt` instruction takes a small Int through a table or a compare chain. A big Int scrutinee is a cell, and takes a slow path that compares it by value with the cases outside the small range. `match big { 1000000000000000 -> .. }` works today through `is_int`, and must keep working.
- An array pattern is a `Probe`. `[h, ..t]` is `ArrayUncons`, which tests the length once, reads the head and advances.
- A `<<>>` pattern stays a left-to-right read, treated as one check that can fail, because a later segment's size reads an earlier binding.

`R(..base, f: x)` lowers to `Update` when the base is known to be the variant being built, which is always true for a one-constructor record. A base whose variant is not known keeps today's expansion into field reads.

### Simplify inside one function (`simplify.rs`, new)

It is one forward walk with an environment, in the style of MLton's `shrink`. A function is walked at most five times, and stops early when a walk changes nothing.

1. **Copy.** `let x = y` makes `x` a name for `y`.
2. **Known field.** `Field(i)(x)` or `TupleField(i)(x)`, where `x` is bound to a constructor or tuple in scope, is that operand.
3. **Common subexpression.** A part read, a compare or an arithmetic primitive with the same operator and operands as one in scope is that local. A primitive that allocates is never merged, because two cells are not one cell once in-place update exists.
4. **Known constructor.** A `Switch` on a local bound to a constructor in scope takes that arm. Inside an arm the scrutinee is known to be that variant, so a second switch on it folds too. The same holds for `If` on a known Bool.
5. **Flatten a jump.** When a join's parameter is only read through `TupleField` and every jump passes a tuple built in scope, the join takes the parts.
6. **Jump with a known constructor.** When a join starts by switching on its parameter and a jump passes a known constructor or Bool, the jump goes straight to that arm, which becomes its own join taking the fields. Nothing is copied.
7. **Rebuilt scrutinee.** In an arm where `r` is known to be `Err`, `Err(Field(0)(r))` is `r`.
8. **Dead code.** A pure `Let` nothing reads is deleted. Only primitives are pure, and no call is. A join nothing jumps to is deleted, and one jumped to once is put at the jump.

`scarlet dis --stats` prints how often each rule fired in each function, so a missing gain can be pinned on one rule.

### Inline (`inline.rs` and `callgraph.rs`, new)

The rules are the ones the prototype ran over the real Core IR of all 23 benchmarks. They are simpler than choosing loop breakers, and they are the ones with evidence.

- Functions are visited callees first over the call graph's strongly connected components. An edge is a call by name or a `Closure` of the function.
- **Nothing in a call cycle is inlined**, with one exception below.
- **A call by name** to a function outside any cycle is replaced by its body when the callee is 12 nodes or fewer, or has one reference in the code reachable from the program's entry points, or the call hands it a known lambda and it is 60 nodes or fewer.
- **A call of a known lambda** (`call f(args)` where `f` is bound to `closure fn#K(caps)` in scope) is replaced by the lambda's body, with `Capture(i)(env)` replaced by `caps[i]`, when the lambda is outside any cycle and 60 nodes or fewer.
- **The exception:** a lambda that is made once and called once is inlined even inside a cycle. That inline removes the only `Closure` of it in reachable code, so it cannot happen twice. In json, `read_elements` makes `closure fn#82` and `fn#82` calls `read_elements` back, so without the exception every `then` closure stays. The prototype measured 18 closure sites staying 18 without it and going to 0 with it.
- A node is a `Let` that does work, or a terminator. A `Let` of a constant, Nil, a Bool, a global or a copy does not count. Under the first count I tried, which counted constants, spectral_norm's `a` was 13 nodes and missed the limit by one.
- A body past 3,000 nodes takes no more inlines.
- In tail position the callee's body replaces the `TailCall`. Elsewhere the rest of the caller becomes `join k(x) = rest`, and each `Ret` in the copy becomes `jump k(v)`. Every binder and join in the copy gets a new id.

**The policy is one function.** `same_unit(caller_module, callee_module)` answers yes for everything today. When hot reload arrives it can narrow to "same module or stdlib" in one place, and step 12 asks the same function before it calls a borrowed entry in another module. Each function records which functions were inlined into it, so a stop can still name them.

**What is never dropped.** Every lambda and every public function keeps its body in the table, even when no closure of it is made in this program, because a closure on the wire names its module and function and another program running the same module may send one. Only a private declared function nothing references, and the compiler's own clones, are dropped.

### Specialise (`specialise.rs`, new, called from the inliner)

A self-recursive function that receives a known lambda is cloned for that lambda (Koka's `Core/Specialize.hs`). The rules are again the prototype's:

- A parameter is specialised only when every use of it in the body is a direct call of it or the unchanged argument of a self call in the same position.
- The lambda's shape is its `FuncIdx` and, for each capture, either "unknown" or another shape, to a depth of three. The lambda must be outside any cycle and 60 nodes or fewer.
- The clone drops the parameter and takes the lambda's unknown captures in its place. The inliner then inlines the lambda into the clone.
- Clones are kept in a map keyed by `(FuncIdx, [(parameter index, shape)])`. That key is the identity of "this function over that code", so two call sites with the same lambda share a clone.
- A clone that would still build its lambda on every turn is thrown away.
- A clone records its generation. A clone of a clone may be made, and a clone of that may not. I measured why the limit is two and not one: with no re-specialisation at all, closures keeps 8 of 9 closure sites where the prototype reaches 6, and nbody, strings and string_keys each lose one. With a limit of two, all 23 benchmarks come out the same as with no limit, and no benchmark reaches a third generation.

## The second half: counts the VM obeys

### Count insertion, owned only (`rc.rs`, replacing `perceus.rs`)

It is the backward liveness walk `drop_pass` is today, over every local that is not `Word`. Ints, Strings and type variables are tracked from here on.

- An operand in a keeping position is `Give` when the local is dead after, and `Copy` otherwise.
- An operand in a looking position is `Lend`. When it is the local's last use, a primitive, built-in, `Switch`, `If` or `Probe` carries `Give` and releases after reading. A call cannot, so it gets `Lend` and a `Drop` after it.
- A local that is given by a node is not also lent by it. In that rare case (`x * x` at the last use of a big `x`) the pass writes two lends and a `Drop`.
- At a branch each arm drops what the others consume. Every jump to a join arrives with the same owned locals.
- A part read (`Field`, `TupleField`, `Capture`) binds an owned copy: one count raised. This is what Lean does for a projection out of an owned variable and what Koka does for the pattern variables of an owned scrutinee. `Open` later removes the count where the parent dies.
- Inside a `Switch` arm that proves the scrutinee is a field-less variant, the scrutinee is a word, and gets no drop.
- An Int whose last use has no reader gets a `Drop`. When no call lies between that point and the block's `Ret` or `TailCall`, the drop is placed just before the terminator, where the loader folds it into the instruction. A model of the pipeline measured this on tak: naive Int drops took it from 445,276 VM instructions to 540,681, and placing them on the return brought it back to 445,267.
- Reuse is off from this step. The second reuse walk, tokens carried round a self tail call and `Body.carried` are deleted, because "nothing owned is left at a tail call" has to hold with no exceptions for the checker to be simple.

### Verify (`scarlet_ir/src/core_ir/verify.rs`, new)

It is one forward walk for each function, with one state for each tracked local: `Owned`, `Borrowed` or `Gone`. `Word` locals are not tracked. The checker grows with the contract: rules about borrows arrive with step 12 and the `Open` rule with step 11.

- **V1, positions.** A looking or sharing position takes `Lend` or `Give`. A keeping position takes `Give` or `Copy`. A looking argument of a `Call` or `TailCall` takes `Lend` only.
- **V2, live.** Every operand names a local that is `Owned` or `Borrowed` when the node starts. So `IntAdd(give x, give x)` and a copy of something already given both fail.
- **V3, one end.** The node's gives are applied one at a time in operand order. Each needs `Owned` and leaves `Gone`. A local a node gives is not lent by the same node.
- **V4, borrows.** A `Borrowed` local is never given, dropped or opened.
- **V5, drop and open.** `Drop` needs each local `Owned` and leaves it `Gone`. `Open` needs the cell `Owned` and leaves it `Gone`. Its shape must be proved: an enclosing `Switch` arm on that local, a `Ctor` bind, a one-constructor type, a tuple type, or this function's own `env`. Its field indexes are distinct and in range, and its binds are `Owned`.
- **V6, binds.** The bind of a `PartOf(k)` result may be `Borrowed` only when operand `k` is a `Borrowed` local. A read of a global is `Borrowed`. Every other bind is `Owned`.
- **V7, branches and joins.** The arms of a `Switch`, `If` or `Probe` are checked separately from the same state. Every jump to a join arrives with the same set of `Owned` locals, and the join's `cont` is checked from that set.
- **V8, ends.** At a `Ret` or `TailCall`, once its gives are applied, nothing is `Owned`. A `TailCall` lends only `Borrowed` locals.
- **V9, forward only.** A `Jump` names an enclosing `Join` and is not inside that join's own `cont`.
- **V10, calls.** A call by name passes as many arguments as the callee has parameters, with `Lend` exactly where the parameter is `Borrowed`. Every `Closure` target owns all its parameters. It has `env` exactly when the closure has captures, and `captures` matches.
- **V11, shapes.** A `Field`, `TupleField` or `Capture` index is in range for the proved shape, or for every variant of the operand's type. A `Variant` switch names distinct variants of its type and has no default exactly when it names them all. Case keys are distinct.
- **V12, word arms.** In a `Switch` arm that proves the scrutinee field-less, the scrutinee is no longer tracked.
- **V13, globals.** `SetGlobal` appears only in a toplevel, and at most once for each slot in the whole program.
- **V14, waiting.** A built-in that can wait has only looking positions.

The checker is tested by mutation. For every `Counted` body the compiler makes from `tests/programs` and `benches`, the test flips each mark, deletes each `Drop` and doubles each `Drop`. Every mutant that changes a count must be rejected. The survivors are run under the audit heap below, and must not stop.

### What the loader and the VM do with it

The loader only encodes. It picks register numbers, builds switch tables, lays out jumps, folds a constant operand into an immediate as it does today, picks `CallSelf` for a call to the function being loaded, and folds a `Drop` into the `Ret` or `TailCall` right after it. It derives no ownership fact. `moves_on_last_use`, `reused`, `Body.carried`, the `MapSetOwned` and `MapDeleteOwned` matches, `option_arms` and `Loader::pattern` all go.

The count behaviour of each instruction is this contract:

| Instruction | What it does to counts |
|---|---|
| any register write | A plain store. Nothing is tested and nothing is released. |
| `Ret`, `TailCall` | `Ret` copies its result and pops. A tail call moves its arguments and jumps. Each releases the registers its folded `Drop` names. Nothing sweeps. |
| `Ctor`, `Closure`, `Call`, `Jump`, `SetGlobal` | One copy bit for each operand. A given operand moves. |
| `Update` | If the base was given and its count is one, store the new fields and release the old ones. Otherwise allocate, copy the other fields with a count each, and give up the base. |
| a part read | A load and a count. From step 12, a plain load when the bind is borrowed. |
| a looking instruction | Reads its operands, does its work, then releases each given operand, on the path where it is a cell. Giving up a small Int costs nothing. |
| `Drop` | Test the tag. If it is a cell, decrement, and free through the VM's own work list at zero. |
| `Open` | One count test, then the unique half or the shared half. |
| `Switch` | One tag read and a table jump. For a type with one field-less constructor and one with fields (List, Option, Pile, Tree) it is a single "is it a cell" branch on the value's bits. |
| `Probe` | A hit binds and falls through. A miss jumps. An operand it keeps is consumed on both paths. |

Four rules hold for every handler, and each has one place that upholds it:

- **H1. A handler reads every operand's word into a local before it writes any register, and works only from those locals.** The loader gives a result the register of an operand that dies at the same instruction (`allocate` adds a clash only between a write and what is live after it), so a handler that wrote its result and then re-read a given operand would release the result. `Machine::operands` is the only way a handler gets its operands, and a loader test forces the result and a given operand into one register for each instruction kind.
- **H2. The order is: make the copies, do the work, write the result, release what was given at a looking position.** One wrapper applies this around every primitive and built-in from its `Sig`, so a handler body touches no count. The exceptions are the short list of keeping built-ins, each written by hand.
- **H3. The `Sig` table and the handlers agree.** A table-driven test runs every `PrimOp`, `Intrinsic` and `ProbeOp` on a counting heap under each legal mark, on its success path and its failure path, and compares every count change with the row.
- **H4. A built-in that can wait changes no count before the point it can no longer yield.** It has only looking positions (V14), and the wrapper releases given operands after the work.

Two cell states get a fixed meaning in every count instruction:

- **A frozen cell** has the sticky count the heap already has (`STICKY`, `heap.rs` line 48). `Copy` and `Drop` do not write it, and the unique test in `Update`, `Open` and the keeping built-ins fails. Constants and everything a global reaches must be frozen before a second process exists, so no process ever writes a count another can read.
- **A dead cell** has count 0. Today a second release of a freed cell is silent: it takes one off `live` and pushes the cell on the size-0 free list, so a leak and a double release can cancel in the "no cell is live at exit" check. From step 0, `release` and the unique test return `Broken::DeadCell` when they find count 0, in every build. That costs nothing on the fast path: my probe compiles the live path to 13 instructions with the test and 13 without. In debug builds, which is what `cargo test` runs, a freed cell is quarantined and never handed out again, every share and every read checks the count, and a `Give` leaves a poison word that names a chunk the heap never has. So a read through a stale reference stops at once with a typed `Broken`.

## Designed, and waiting for the scoreboard after step 10

### Open, with no token (step 11)

Where an owned local of proved shape is used only through part reads from some point on, and it dies there, those reads and the drop become one `Open`. This is Koka's drop specialisation and Lean's `ExpandResetReuse` as a single instruction. An `Open` lists the fields some path uses, and a path that does not want one drops it.

A unique cell is freed inside the `Open`, and the constructor that follows allocates. The free list is last in, first out, so that constructor gets the same cell back. On the toy VM a match-and-rebuild of a list cell costs 583 instructions under today's contract, 366 with `Open` and a fresh constructor, and 345 with a reuse token. `Open` is worth 217 of the 238, and the token 21. The token would need a third local state, tokens as join parameters, a pairing pass, a free-the-shell drop and a size check, so it waits.

### Borrowed parameters, with an owned twin (step 12)

It is Lean's inference (`IR/Borrow.lean`) to a fixed point. Every parameter that is not `Word` starts borrowed. A parameter becomes owned, and the reason is recorded for `scarlet dis`, when:

- **O1, it is consumed.** It sits in a keeping position: stored in a constructor, tuple, array or closure, returned, passed to an owned parameter, passed to any call through a value, or the base of an `Update`. A sharing position does not count, so slicing a binary does not make a parser own its input.
- **O2, a part of it is consumed.** A local read out of it meets O1. This is Lean's projection rule, and it keeps `map`, `ins` and `set_black` in place.
- **O3, its own loop hands it something owned.** A self tail call passes, in that parameter's position, something the frame owns. This is Lean's `preserveTailCall`, which is also for self calls only.
- **O4, a call can follow its last use.** Borrowing never keeps a value alive across a call that does not use it or a part of it. Without this, `fn process(input) { summary = digest(input); serve_forever(summary) }` would borrow `input`, and its caller would hold the input for as long as the server runs. A tail call and a built-in that can wait both count as a call. This is the restriction "Reference Counting with Frame Limited Reuse" asks for.
- **O5, it is an Int.** Handing a small Int on is free, and giving one up inside an arithmetic instruction is free, while a borrowed Int makes the caller drop its temporary after the call.

So a function's holds depend on its own body and on its callees. No caller can change them.

**The twin.** A function with a borrowed parameter may need a second entry where every parameter is owned. `f$own` is the same `Raw` body, counted again by `rc` with every parameter owned. It is made only on demand, in two cases. A `Closure` of `f` points at `f$own`, so a call through a value needs no knowledge of the callee. And a tail call that would have to lend something its own frame owns goes to `f$own` and gives it, so every tail call is still a jump. Lean has the same second entry (`requiresBoxedVersion` is true when any parameter is borrowed), but Lean's is a wrapper that calls and then returns, which Scarlet cannot use because the wrapper's frame would stay on the stack for each turn of a loop that runs through function values.

I counted what this does on 16 benchmarks with the census model. Of 429 parameters that can hold a cell and are not Ints, 76 stay borrowed, against 61 under a rule that lets any tail call anywhere make a parameter owned for every caller. 15 of 416 functions need a twin: 13 because a closure is made of them, and 2 for a tail call (list's `length` and spectral_norm's `atv`).

A read out of a borrowed local, and a read of a global, binds `Borrowed`. A join parameter is borrowed when every jump passes a borrowed local and the join does not consume it.

### An update that takes a field (step 13)

In `Scheduler(..s, tasks: map::set(s.tasks, k, v))` the field is read before the update, so the map has two holders and `map::set` copies its path. When an `Update`'s base dies there, and a field it sets was read earlier and consumed, `rc` opens the base at that earlier read and builds a fresh constructor in place of the `Update`:

```
open s as Scheduler [0 -> tasks, 1 -> current, 2 -> queue, 3 -> count]
let tasks2 = MapSet(give tasks, give id, give task)     ; one holder, so it is set in place
let s2 = Ctor Scheduler(give tasks2, give current, give queue, give count)
```

Every other `Update` stays an `Update`. Turning every rebuild into one made permute worse in a model of the pipeline (15,126 cells to 45,360), so `Update` comes from the source's `..base` only.

## Left out until a measurement asks

- **Reuse tokens.** They are worth 21 instructions for each rebuilt cell on the toy VM, and 32 on a five-field record (311 against 279). If they come back, a token must pair by cell kind and size in words, never by field count, because a 2-tuple is 3 words and a 2-field constructor is 4, and the constructor must rewrite the whole header.
- **Borrows rooted in a local.** A read out of an owned local could skip its count if the parent outlives the child. My probe prices that at 47 instructions for each looked-at cell field when the drop is its own instruction (174 against 127). It needs a root on every borrow, liveness that extends a parent to its children's last look, and a memory rule like O4 for locals. Neither Lean nor Koka does it.
- **Several results in registers.** A worker that returns a tuple's parts, with a wrapper that boxes them, is worth 7.8% of nbody and 1.8% of richards and nothing else measured, which is 0.996 on the geomean. It changes the VM's call and return path. It should wait until a result inside `Ok` can be unboxed too, which is where json's tuples are. If it is built, the rule is the prototype's: inside a worker every call to a function that has a worker goes to the worker, a call that is not in tail position is followed by a `MakeTuple` that the known-field rule cancels, and a wrapper is always inlined except in tail position of a function that is not a worker. nbody's `pull`, which calls itself outside tail position, is the test.
- **Kept fields.** A mask of fields a rebuild leaves in place is worth 52 of 279 on a five-field record with tokens. It needs tokens first.

## Invariants and what upholds each

| Invariant | Who relies on it | What upholds it |
|---|---|---|
| Every owned local is ended exactly once on every path, by a `Give`, a `Drop` or an `Open`. At a `Ret` or `TailCall` nothing owned is left. | The VM never sweeps and never tests a register it writes. Nothing leaks. | `rc.rs` makes it true in its backward walk. `verify` V2, V3, V5, V7 and V8 check it. The mutation test checks the checker. The golden harness asserts no cell is live at exit. |
| A borrowed local is valid for the whole frame. | A part read with no count is safe. | V6: a borrow is only ever read out of a borrowed parameter or a global. V10 and V8: a caller lends only what it holds across the call, and a tail call lends only what an older frame holds. |
| A cell is written in place only through a given reference whose count is one. | `Update` and the keeping built-ins. | The count test is one line in each handler. A lender is suspended while its borrow lives, and a borrow adds nothing to a count, so a count of one in an owner's hands means nobody else can reach the cell. A frozen cell never has a count of one. |
| A register that is written holds nothing owned. | A write is a plain store. | The loader gives two locals one register only when they are never live together, with `Drop` and `Open` counted as reads. H1 covers a result that lands in a given operand's register. |
| A handler changes counts only as its `Sig` row says. | The checker, which reads the table and not the handler. | H2's wrapper, and H3's table-driven test. |
| A `Word` local never holds a cell. | No drop is emitted for it. | `Program::repr`, the only place a `Repr` is made, with a test for each kind of type. A wrong answer either way is a leak or a tag test. |
| A released or freed cell is never used again in silence. | Step 10, where the VM's own counting goes. | `Broken::DeadCell` on release and on the unique test in every build. Quarantine, read checks and the poison word in debug builds. |
| No jump goes backward, so every loop passes through a call. | Counting reductions at calls. | `lower` and `simplify` only build jumps forward to an enclosing join. V9. |
| Every tail call is a jump. | Loops written as recursion. | O3 for a function's own loop, and the owned twin for every other tail call that hands over something the frame owns. V8. |
| A function reached through a value owns every parameter. | A call through a value needs no knowledge of the callee. | A `Closure` points at the twin. V10. |
| Inlining ends. | The compiler. | Nothing in a cycle is inlined. The one exception removes the only closure of a lambda. The 3,000 node cap bounds growth. |
| Specialisation ends. | The compiler. | A clone's key is drawn from a finite set (a function, its parameters, shapes three deep over the program's lambdas), and no clone is made past the second generation. |
| A slot is written once. | A read of a global is a borrow. | V13. |

## Worked example: fib

`scarlet dis benches/fib/fib.sca` today has a `Let` for each constant, two calls and an add. Counted, with `n` owned (it is an Int):

```
fn main.fib(n: own Int) -> Int
  let c = IntLt(lend n, 2)
  if c
    ret give n
  else
    let a = IntSub(lend n, 1)
    let x = call self(give a)
    let b = IntSub(give n, 2)
    let y = call self(give b)
    let r = IntAdd(give x, give y)
    ret give r
```

The loader makes the same instructions as today and a call still runs 4.5 of them. Nothing in this area removes one. What changes is what each no longer does: the return does not sweep, the call does not clear the frame above it, and a register write does not test the old value. That gain belongs to the call, register and tail-call items of the diagnosis. Two models disagree on its size for fib: 7.4% in one, and 59 of 355 instructions (17%) by the trace arithmetic for the sweep, the write tests and the call's clear.

## Worked example: towers

Towers is the benchmark this area helps most. Today a disk move runs 61 VM instructions and makes 4 cells, and three `Towers(..t, x: y)` updates are 46.5% of the run. `set_pile` today, once for each arm:

```
| c0 ->
  let %3:7 = Field(1)(%0)
  let %4:7 = Field(2)(%0)
  let %5:0 = Field(3)(%0)
  let %6:0 = Field(4)(%0)
  drop %0
  ret ctor 10.0(%2, %3, %4, %5, %6)
```

After steps 2 to 10, with `get_pile` and `set_pile` inlined and `push_disk` left as a call:

```
fn main.move_top_disk(t: own Towers, from: own Int, to: own Int) -> Towers
  join k(p: own Pile) =
    switch lend p                                 ; one test: is it a cell
    | Empty ->                                    ; p is a word in this arm
      let e  = Field(4)(lend t)                   ; an Int: the count is a tag test
      let e1 = IntAdd(give e, 1)
      let r  = Update(give t) { 4: give e1 }
      drop [from, to]                             ; folded into the ret
      ret give r
    | Disk ->
      let size = Field(0)(lend p)
      let next = Field(1)(give p)                 ; one count for next; p's reference ends here
      join j(t2: own Towers) =
        let pushed = call push_disk(give t2, give size, give to)
        let m  = Field(3)(lend pushed)
        let m1 = IntAdd(give m, 1)
        let r  = Update(give pushed) { 3: give m1 }
        ret give r
      switch give from
      | 0 -> let t2 = Update(give t) { 0: give next }
             jump j(give t2)
      | 1 -> let t2 = Update(give t) { 1: give next }
             jump j(give t2)
      | _ -> let t2 = Update(give t) { 2: give next }
             jump j(give t2)
  switch lend from
  | 0 -> let p = Field(0)(lend t)                 ; one count for the pile
         jump k(give p)
  | 1 -> let p = Field(1)(lend t)
         jump k(give p)
  | _ -> let p = Field(2)(lend t)
         jump k(give p)
```

`Update(give t) { 0: give next }` on a unique `t` stores `next` and releases the old field. The old field is the popped Disk, which the VM frees in its own loop, and that puts `next` back to one reference.

A hand-written version of this function on a toy VM, with the two pile reads as borrows, ran a move in 29.5 VM instructions against 61.2, with 1 cell against 4, and 1,526 machine instructions against 4,487. The owned reads above add one count up and one count down to that. A model of the pipeline with the real loader's rules gets 6.01M VM instructions down to 2.62M and 393,516 cells down to 98,472.

## Shorter examples

**list.** `is_shorter_than` today runs 11 instructions a step and 963 machine instructions: four variant tests, two reads of a field bound to `_`, two reads that share, two drops and the tail call. After step 4 it is two one-branch tests, two reads and the tail call. After step 12 both parameters are borrowed, because the only call uses a part of each:

```
fn main.is_shorter_than(x: borrow, y: borrow) -> Bool
  switch lend y
  | Empty   -> ret false
  | Element ->
    let y_tail: borrow = Field(1)(lend y)
    switch lend x
    | Empty   -> ret true
    | Element ->
      let x_tail: borrow = Field(1)(lend x)
      tailcall self(lend x_tail, lend y_tail)
```

That is five instructions and no count changes. At today's unit costs it is two cell tests at 40, two reads at 77 and a tail call at 177, so 411 against 963.

`length` is the twin case. `benchmark` ends in `ret call fn#0(%6)`, a tail call that passes a list its own frame made. `tail`'s callers still get the borrowed entry:

```
fn main.length(t: borrow List) -> Int
  switch lend t
  | Empty   -> ret 0
  | Element -> let next: borrow = Field(1)(lend t)
               let n = call length(lend next)
               let r = IntAdd(give n, 1)
               ret give r

fn main.length$own(t: own List) -> Int            ; what benchmark's tail call jumps to
  switch lend t
  | Empty   -> ret 0
  | Element -> open t as Element [1 -> next]
               let n = call length(lend next)
               drop [next]
               let r = IntAdd(give n, 1)
               ret give r
```

**rbtree.** `ins` on a Red node after step 11:

```
fn main.ins(t: own Tree, k: own Int, v: Bool) -> Tree
  switch lend t
  | Node ->
    open t as Node [0 -> c, 1 -> l, 2 -> key, 3 -> val, 4 -> r]
    switch lend c
    | Red ->
      let lt = IntLt(lend k, lend key)
      if lt
        let l2 = call self(give l, give k, lend v)
        let n  = Ctor Node(lend c, give l2, give key, give val, give r)
        ret give n
```

The node is opened and freed, its left child is given to the recursive call so that call sees a unique node too, and the constructor takes the freed cell back off the free list. `is_red` on a black node goes from 29 instructions to 4.

**json.** `read_elements` today is `closure fn#82(%0, %2)` then `ret call fn#31(%4, %5)`, where `fn#31` is `result::then`. Inlining `then`, and then the lambda (made once and called once, so allowed inside the cycle), gives a `switch` on the result with `items` still owned by the frame. `ArrayAppend(give items, give value)` then appends in place, where today the closure holds a second reference and the append copies.

**bounce.** `(x, x_vel, hit_x) = if .. { (500, .., True) } else ..` today is a `letj` of three `MakeTuple`s, three `TupleField`s and a drop. Flatten-a-jump makes it `join kx(x, x_vel, hit_x)` with three jumps. `bounce` has one call site, so it is inlined into `step` and its `(Ball, Bool)` result is never built. The prototype did this mechanically on the real listing: `MakeTuple` 10 to 0, `TupleField` 16 to 0.

## What this does not do

It does not unbox a tuple inside an `Ok` (json's `Ok((value, index))`), a returned tuple or record, or make an array element unique when it is read out (bounce's Ball is still shared with its array). It does not unroll or inline recursive functions, so fib and tak get nothing from the compiler but the guarantee that their frames own nothing at a return. It does not claim json's input stays borrowed: in the census model `capture` and `read_string` own it under O4 and the main parse chain owns it too, and nobody has run the borrow pass over the prototype's inlined json. Tail recursion modulo cons and a `fip`-style promise of in-place update are left for later.

## Steps

### 1. 0. Instruments, and a heap that notices a dead cell

**Changes.** crates/scarlet/src/dis.rs: `scarlet dis --instrs` prints the loaded instructions beside the Core IR. crates/scarlet_vm/src/exec.rs: a per-opcode run counter, read through `scarlet/internal`, off by default. crates/scarlet_vm/src/heap.rs: `release` and the unique test return `Broken::DeadCell` when they find count 0, in every build; in debug builds a freed cell is quarantined and every share and read checks the count. The golden harness asserts no cell is live when a program exits. The scoreboard gets a column for what this area has claimed so far for each benchmark, beside the measured change, so a double count shows as a row that claims more than it measured.

**Expected gain.** None. The scoreboard must not move: my probe compiles `release`'s live path to 13 instructions with the dead-cell test and 13 without. The counters make every later step measurable in VM instructions and cells as well as machine instructions.

**Risk.** Low. Today a second release of a freed cell is silent (heap.rs 281 to 312), so this step may turn up a bug that already exists. The debug-only checks must not change what a release build does.

### 2. 1. Repr, and no drop for a value that can never be a cell

**Changes.** crates/scarlet_ir: `Repr { Word, Counted }` and `Program::repr(RTy)`, one function over the type table. A type with at least one constructor and no fields on any is `Word`; Float is `Word`; everything else, including Map, Pid, Subject and every other type with no constructors, is `Counted`. crates/scarlet_lower/src/perceus.rs: `owns_cell` becomes `is_heap(ty)` and not `Word`, so Ints, Strings and type variables stay untracked exactly as today (`is_heap` is deleted in step 9, not here); `drop_match` does not drop the scrutinee in an arm that proves a field-less variant. No VM change.

**Expected gain.** rbtree 5%: 187.1 drops of never-cell values for each insert at 30 to 38 each is 5.6k to 7.1k of 129.3k. bounce 2.4%: 6 Bool drops for each ball-step at 30 is 180 of 7,493. storage about 1%, binary_trees 0.8%, list 0.4%. Geomean factor 0.996. fib and tak must not move, because no Int becomes tracked.

**Risk.** Low. The VM still releases everything at frame exit. A Map must stay `Counted`: the loader picks `MapSetOwned` only when the next node is the map's `Drop` (code.rs 1355 to 1370), so a Map with no drop would take the copying path on every set. `repr` gets a test for each kind of type, with `scarlet/map.Map` among them.

### 3. 2. `Update`: a record update is one instruction that writes in place

**Changes.** crates/scarlet_ir: `Atom::Update { base, variant, sets }`. crates/scarlet_lower/src/translate.rs `construct` builds a typed update when the base is known to be the variant being built, in place of the field reads; lower.rs passes it through; perceus.rs treats the base as an operand. crates/scarlet_vm: `Instr::Update { dst, base, moved, sets }`. `moved` comes from liveness in `moves_on_last_use`, the path `ArrayAppend` takes, and not from the `let; drop` shape `MapSet` needs, so an update in tail position is in place too. If `moved` and the count is one, release the old field values and store the new ones; otherwise copy the cell, count each kept cell field, store, and release the base. The handler and one probe land first, and the unit cost is measured on the real VM before the lowering is finished.

**Expected gain.** towers 40%: 14.75M updates at 1,281 each are 18.9G of 38.35G (measured by the sceptic), and an in-place write at 150 to 250 saves 15.2G to 16.7G. On a toy VM one update of a unique 5-field record fell from 564 to 55. richards about 13%: the Scheduler copy is 9.6% and is unique (about 7.5 saved), the Task copy is 16.8% and is shared with the map, so it takes the copying path, which still skips eight Field reads (616 of about 1,650, about 5), and the Packet copy about 1. permute about 8% (State rebuilds are 11.3%). nbody about 5%. Geomean factor 0.966. If the measured in-place cost is over 300, towers is redone before anything else is built.

**Risk.** Medium. The in-place path must release each overwritten field exactly once. The uniqueness test is at run time, so a wrong `moved` bit costs a copy and never a wrong answer. An update whose new value reads a field of the base still copies that field's holder, as today; step 13 fixes that.

### 4. 3. One join with parameters, `Ret` and `TailCall`, `SetGlobal`

**Changes.** crates/scarlet_ir/src/core_ir/mod.rs: `Join { id, params, cont, body }` and `Jump { to, args }` replace `LetJoin`, `LetCont` and `Goto`; `Tail(Atom)` splits into `Ret` and `TailCall`; `SetGlobal { slot, value, body }` replaces `CoreBind::global`. lower.rs `join_operand`, `short_circuit` and the or-pattern code emit joins, and an or-pattern that binds names jumps with them, so `ArmLeaf::OrBody` goes; `let_pinned` emits a `SetGlobal`. immediates.rs and perceus.rs follow: perceus treats join parameters as owned binds, walks into a join, and loses its `pinned` set, because the VM's `SetGlobal` already takes its own reference. code.rs: `Dest::Join` goes; a `Jump` is moves then a jump.

**Expected gain.** None by itself. The scoreboard should not move by more than about 0.5%, which is the test that the reshaping changed nothing. Steps 4, 6 and 7 are written in it.

**Risk.** Medium. It touches every pass and every golden snapshot. No behaviour changes, so the whole test suite is the check. A program with module constants (three `const` lines and a function that reads them) goes into tests/programs.

### 5. 4. A match is a decision tree that reads a field where it is used, with a `Switch` instruction

**Changes.** New crates/scarlet_lower/src/matching.rs after Gleam's exhaustiveness.rs; lower.rs `match_arms`, `pattern`, `nested_walk` and `or_alternatives` are replaced by calls into it, and the `<<>>` walk stays as one check that can fail. core_ir: `Switch`, `Cases` and `LitKey`; `Match` and `CorePat` go. immediates.rs: a Bool switch becomes `If`. perceus.rs takes a dying scrutinee's shape from the switch arm. crates/scarlet_vm: `Instr::Switch { src, table }`, `SwitchInt` with a slow path for a big Int scrutinee, a compare chain for literal cases, and a one-branch form for types with one field-less and one fielded constructor; `JumpUnlessVariant` and `Loader::pattern` go. The `Switch` handler lands first with a match on one type, and its unit cost is measured before the tree compiler is finished.

**Expected gain.** list about 25%: two dead Field reads at 77 and four tests at 68.5 become two one-branch tests at about 45, which is about 340 of the 963-instruction step, times its 79.7% share, trimmed for what a one-branch test really costs. rbtree about 26%: a source rewrite that matches each node once measured 21.4%, and about a third of the remaining dead reads (2.45M at 77 plus 1.04M shares undone at 83 is 295M of 1,960M, 15%) is on top. deriv about 21%: 1.93M failed tests at 68 and 0.98M chain jumps at 18 are 149M of 769M on the scaled run, plus dead reads in or-pattern chains, less 2 points because a toy measured a `Switch` at 3 more than a passing test (about 71), not the 58 to 62 a model assumed. binary_trees about 10% (dead `Node(..)` reads and their drops are 7.4% by trace). cfold about 8% (41.9M tests beyond one for each match at 69 is 2.9B of 43.1B, less 2 points for the same reason, plus dead binders at 3.9%). richards 4%, towers 3%, storage, json, permute and queens about 2% each. Geomean factor 0.951. If `Switch` on a cell measures over 75, deriv, rbtree and cfold are redone.

**Risk.** Medium to high. It rewrites the densest part of lower.rs. Guards must run in source order and binary patterns must keep their left-to-right reads. It lands with a differential test that runs every match in tests/programs through both lowerings, and that test includes the float probe (`(0.0, 1)`, `(-0.0, 2)`, `_`) and the big-Int probe (`1000000000000000 -> ..`). A row with no check on the chosen local is copied into every arm, so code size needs watching; bodies are never copied because they are joins. Fewer shares happen, so the allocation-count tests need reading, not only re-blessing.

### 6. 5. `Probe`: a test that binds

**Changes.** core_ir: `Probe { op, args, hit, miss }` with `ProbeOp { MapGet, ArrayGet, ArrayUncons, BinaryReadUtf8 }`. matching.rs lowers `[h, ..t]` to `ArrayUncons` and a `match` on `map::get` or `xs[i]` to a probe; `index_or` goes. perceus.rs treats the hit binds like any bind. crates/scarlet_vm: the instructions `MapFind` already is, for all four; the loader's `option_arms` and `MapFind` recognition go, because the shape is now in the IR.

**Expected gain.** None is claimed here. What it removes (the second `ArrayLen` in every `[h, ..t]`, the `Some` cell built and matched at once) is counted by the array-walk, array-index and map items of the diagnosis. Its job in this area is that no built-in ever returns a value that exists only when a flag is true, which the checker in step 9 could not describe.

**Risk.** Low to medium. `ArrayUncons` keeps its array, so on a miss it must still release it; that goes in the built-in table test of step 9.

### 7. 6. The simplifier inside one function

**Changes.** New crates/scarlet_lower/src/simplify.rs with the rules that need no other function: copy, known field, common subexpression (non-allocating primitives only), known constructor and known Bool, flatten a jump, jump with a known constructor, rebuilt scrutinee, dead code. At most five walks for each function. program.rs `build_program` lowers every function, then simplifies, then runs perceus. `scarlet dis --stats` prints how often each rule fired.

**Expected gain.** bounce 15.6%: the two triples carried out of an `if` (37.7G to 31.8G, measured by a source rewrite). nbody 4.3%: duplicate field reads, measured by hand. Geomean factor 0.991. The jump-with-a-known-constructor and rebuilt-scrutinee rules have no number of their own: they fire after inlining, and their counters in step 7 say how often.

**Risk.** Low to medium. Only primitives are pure; no call is. Merging two allocating primitives would change which cells are unique, so that is refused.

### 8. 7. Inlining over the call graph

**Changes.** New crates/scarlet_lower/src/callgraph.rs (references counted over code reachable from the entry points, strongly connected components, callees-first order) and inline.rs, called from the simplifier's walk: a small callee (12 nodes), a callee with one reference, a callee handed a known lambda (60 nodes), a known lambda called in place (60 nodes), and a lambda made once and called once even inside a cycle. Nothing else in a cycle is inlined. A constant load is not a node. `same_unit(caller_module, callee_module)` is the one policy function. `LoweredFn` records which functions were inlined into it. Every lambda and every public function keeps its body.

**Expected gain.** Measured on the real VM by hand rewrites: json 28.8% (28,089M to 19,998M with every `then` chain and `capture`'s pipe chain as a match), bounce a further 21.4% (31.8G to 23.75G of the original 37.7G with `bounce` inlined into `step`; I re-ran the two together at 37,577M to 23,705M), spectral_norm 12.9%, towers 12.2%, map_churn 11.6%. The prototype reproduces each mechanically: json 18 closure sites to 0 and 91 known calls to 39; spectral_norm's `a` is inlined at both call sites once constants are not counted (it missed by one node before, and the other 22 benchmarks make the same decisions under either count). Claimed and not measured: strings 4%, richards 3%. Geomean factor 0.956.

**Risk.** Medium. Renaming on inline must make every binder and join new. Code grows (json 596 to 692 nodes, strings 205 to 289, closures 103 to 177 in the prototype). Lowering and Perceus for json's 56 functions are within the noise of an 83M-instruction compile today, so the passes are cheap, but the `startup` row must be watched. Inlining across modules ties them together, which matters for hot reload later. These gains are deleted calls and closures, so they shrink by a third to a half once calls and allocation are cheaper.

### 9. 8. Specialise a self-recursive function on a known lambda

**Changes.** New crates/scarlet_lower/src/specialise.rs, called from the inliner, with clones in a map keyed by `(FuncIdx, [(parameter index, shape)])` and appended to the program's functions. A parameter is specialised only when its every use is a direct call or the unchanged self-call argument. Shapes stop at depth three. A clone records its generation and none is made past the second. A clone that still builds its lambda each turn is thrown away. No stdlib change.

**Expected gain.** closures 20.1% (26,385M to 21,087M, measured by the sceptic for exactly this rewrite: 9.6% for the fold wrapper and 10.4% for the lambdas). nbody about 6% (map plumbing is a 12% remainder; not re-measured). strings about 3% (not measured). Geomean factor 0.986. Steps 1 to 8 together are 0.845 by summing each benchmark's shares, 6.17 to about 5.2.

**Risk.** Medium. Code grows by one loop for each distinct lambda. The key must be the code's identity, never a span. The generation limit of two is measured on 23 benchmarks and is a guess beyond them; raising it is one constant.

### 10. 9. The compiler states every count, and the VM still checks

**Changes.** core_ir: `Stage`, `Use`, `Pass`, sig.rs with `sig()` for `PrimOp`, `Intrinsic` and `ProbeOp`; `CoreFn::env` (captures become part reads out of `params[0]`); `Drop { shape }`, `ReuseShape` and `Ctor { reuse }` go. New crates/scarlet_lower/src/rc.rs replaces perceus.rs: every local that is not `Word` is tracked, every bind is owned, every part read is an owned copy, Int drops are placed on the `Ret` or `TailCall`, and there is no reuse. `ResolvedPool::is_heap` goes. New crates/scarlet_ir/src/core_ir/verify.rs with the owned-only rules; `load` takes `Verified`. code.rs reads the marks; `moves_on_last_use`, `reused`, `Body.carried` and the `MapSetOwned` and `MapDeleteOwned` matches go. exec.rs: `Machine::operands` reads every operand before any write; one wrapper applies each `Sig`; a `Give` empties its register, and leaves the poison word in debug builds; `DropReuse`, `hollow`, `fits_ctor` go. The VM keeps its release on write and its sweeps, and counts every time one of them releases a live cell (`net_caught`). Three tests land with it: the mutation test of the checker, the table-driven test of every built-in against its `Sig` row, and the loader test that forces a result into a given operand's register. The suite and all 23 benchmarks must run with `net_caught == 0` and no `DeadCell`.

**Expected gain.** Direct: drops of registers a call already emptied go. cfold 3.2% (1.11M of them at 30 is 33M of 1,040M), deriv 2.4%, towers 2.0%, binary_trees 1.6%, rbtree 1.2%, list 0.6%. Geomean factor 0.995. Against that, cfold loses reuse until step 11: 16.3M reuses at about 111 each is 1.8G of 43.1G, so cfold may be up to 4% worse for two steps; permute (2.0M reuses) and rbtree (0.6M) lose under 1%. The point of the step is the proof: a zero on `net_caught` shows the sweep and the write test have nothing left to do, and a clean audit run shows nothing was freed early.

**Risk.** Medium. A missed release cannot go wrong yet, because the VM's own counting covers it and counts it. A release done too early can: a wrong `Give`, or a handler that disagrees with its `Sig` row. The audit heap, the poison word and the table test are there for that, and they are why this step is not as safe as it first looks. Joins with parameters, guards and binary patterns are where the pass is hardest, and no prototype has run them. The VM tests that assert a one-allocation loop change to assert the peak number of live cells.

### 11. 10. The VM obeys

**Changes.** crates/scarlet_vm/src/exec.rs: `set` is a plain store, `leave` and `again` no longer sweep, a `Give` at a keeping position stores nothing, `Drop` tests the tag and decrements; `net_caught` goes. code.rs: `allocate` no longer relies on overwrite to release. The debug-build poison, quarantine and read checks stay for good.

**Expected gain.** This is the gain the diagnosis counts under call-return-frames, register-access-checks and self-tail-call, so it is not added to this area's: 9 a register on every return, 8 a register on every self tail call, 6 on every register write. For fib that is between 7.4% (a model of the pipeline) and 17% (38 for the sweep and 21 for write tests of the 355-instruction call, by the trace). tak about 10.8% (model). This step is where the plan of record ends.

**Risk.** High. This is where a compiler mistake becomes a leak or a stale read. That is a wrong answer or a typed stop, never memory unsafety, but it is a bug. What stands against it: step 9 ran clean first, the checker runs on every load and is itself tested by mutation, a release of a dead cell stops in every build, and the suite runs on the audit heap.

### 12. 11. `Open`, with no token (decide from the scoreboard after step 10)

**Changes.** core_ir: `Open { cell, shape, fields, body }`. rc.rs: where an owned local of proved shape is used only through part reads from some point on and dies there, the reads and the drop become one `Open`. verify.rs: rule V5 for `Open`. crates/scarlet_vm: `Instr::Open`, which moves the listed fields out and frees the cell when the count is one, and otherwise counts the listed fields and decrements. The handler and one consumed match land first, and the unit cost is measured against a build without it.

**Expected gain.** cfold about 6%: the hot arm's two sharing reads (96 each), `DropReuse` (218) and a constructor over the reused cell (206) are 616 of 1,021 on 33.9% of the run, against an Open at about 150 and a fresh constructor at 252. That also wins back what step 9 took. rbtree about 6% (count pairs around consumed nodes are 8% and counts undone inside a free 3%). binary_trees about 4%, deriv 2%, towers 1%. Geomean factor 0.992. On the toy VM a match-and-rebuild of a list cell costs 583 under today's contract and 366 with an Open and a fresh constructor.

**Risk.** Medium. The diagnosis found reuse does not pay today, so this must be measured, not assumed. The free and the allocation that a token would save are 21 instructions on the toy and about 129 on today's allocator, so this step is worth more once the allocation work has landed.

### 13. 12. Borrowed parameters, with an owned twin (decide from the scoreboard)

**Changes.** core_ir: `Hold { Owned, Borrowed }` on binds and parameters. New crates/scarlet_lower/src/borrow.rs: the fixed point with rules O1 to O5 and borrowed join parameters. rc.rs takes a vector of parameter holds, is run once with the inferred holds and, on demand, once with every parameter owned to make `f$own`; a `Closure` points at the twin, and a tail call that would lend something its frame owns goes to the twin. A read out of a borrowed local, and a read of a global, binds `Borrowed`. verify.rs: rules V4, V6 and the borrowed half of V8 and V10. `scarlet dis` prints each hold, the rule that made a parameter owned, and why a twin exists.

**Expected gain.** list about 16%: two shares at 19 and two live Drops at 83 are 204 of the 963-instruction step, times 79.7%. deriv about 6% (the `count` walk's traffic is 8.6%, part of it already taken by step 11). binary_trees about 3% (`check`), rbtree 2%, richards 2%, closures 2%, towers 1%. Geomean factor 0.985. My census of 16 benchmarks has 76 of 429 parameters borrowed and 15 of 416 functions needing a twin. Rule O4 costs at most 0.2% on any of 17 benchmarks in a critic's run of the model.

**Risk.** Medium. The gain sits in a handful of look-only functions (list has 4 borrowed parameters of 7, rbtree 2 of 12, richards 6 of 81). A twin is a second body, so code grows by about 4% of functions. A wrong hold is a checker error, not a wrong run. The census copies join bodies, so borrowed join parameters have not been exercised.

### 14. 13. An update that takes a field (decide from the scoreboard; needs step 11)

**Changes.** rc.rs: an `Update` whose base dies there, and one of whose set fields was read earlier and consumed, becomes an `Open` at that read and a fresh `Ctor`. No IR or VM change.

**Expected gain.** richards about 9%: 99,029 map sets an iteration move from the shared path (1,951) to the unique one (441), 149.5M of 1,404M, less about 22M because each `Scheduler` update turns from an in-place write (55 on the toy) into an open and a rebuild (311 with no token). Geomean factor 0.996. The same rule is what lets an array held in a record reach an in-place array set unique (permute's State, fannkuch's Next), and that gain is counted by the array-set item. Across all of steps 1 to 13 richards is about 31%, which is what the one whole-pipeline model supports once its 23% shortfall against the real run is taken off. The whole area is about 0.81 on today's VM, and 0.84 to 0.88 once calls and allocation are cheaper.

**Risk.** Low to medium. It is a rewrite inside one pass, and the checker sees an ordinary `Open` and `Ctor`. richards is the benchmark where the model and the arithmetic disagree most, and nobody has measured a rewritten richards, because the source language cannot express the in-place forms.

### 15. 14. Optional, each on its own measurement

**Changes.** Four separate pieces, none in the plan. (a) Several results in registers: `CoreFn::results`, a worker and wrapper split in a new unbox.rs with the prototype's rule for calls inside a worker, and a VM call with several result registers. (b) Reuse tokens: a `Token` local state, tokens as join parameters, a pairing pass, paired by cell kind and size in words. (c) Borrows rooted in a local: a root on each borrow and liveness that extends a parent to its children's last look. (d) Kept fields: a mask on a constructor over a token.

**Expected gain.** (a) nbody 7.8% (15 tuples a step, by ablation) and richards 1.8%: geomean factor 0.996. (b) 21 instructions for each rebuilt cell on the toy (366 against 345), under 0.5% of rbtree. (c) 47 instructions for each looked-at cell field read on the toy (174 against 127), about 3% of a towers move. (d) 52 of 279 on a five-field rebuild.

**Risk.** (a) changes the call and return path the frame work also changes, and should wait until a tuple inside `Ok` can be unboxed, which is where json's are. (b) and (c) each add a state to the checker. Build none of them unless the scoreboard after step 13 points at it.

## Experiments

Everything I built is in perf-scratch/handover-final/, with its own Cargo target directories. I edited nothing in the repository and ran neither cargo nor git there.

**1. A token against no token, re-run and extended.** I copied the toy VM a critic had extended (safe Rust, one interpreter loop) and ran its probes again. A match-and-rebuild of a unique list cell costs 583 machine instructions under today's contract, 420 with field reads and an unpack, 345 with one `Open` and a reuse token, and 366 with an `Open` that frees the shell and a fresh constructor. So `Open` is worth 217 of the 238 and the token 21. I added the same comparison for the update that takes a field (`go(n - 1, R(..r, a: Box(..r.a, v: n)))` on a unique 5-field record, 10M steps, over the bare loop): a blind update 326, a hole 157, an open with a token 279, an open with no token 311. The token is worth 32 there. `./run.sh` in the scratch directory repeats all of it, and every run checks its answer and that cells made equals cells freed.

**2. What a borrow rooted in a local is worth.** On the same toy I read a cell field out of a record the frame owns, looked at it, and updated the record in place. With the read as a borrow the step costs 127 over the bare loop. With an owned copy and a drop it costs 174. So leaving local-rooted borrows out costs 47 instructions for each such read when the drop is its own instruction, and less when it is folded into the last look.

**3. The inline size count.** I copied the middle-end prototype (Python, over the real `scarlet dis` output of all 23 benchmarks) and changed its node count so a `Let` of a constant, Nil, a Bool, a global or a copy is not a node. spectral_norm's `a` is then inlined at both call sites (known calls 15 to 13, where the old count left 15), and `av_row` shows the body in place with the duplicate `IntAdd(%1, %2)` that the common-subexpression rule is for. The other 22 benchmarks end with the same number of known calls and closure sites under either count.

**4. Whether a clone may be specialised again.** In the same copy I refused to specialise a clone. closures then keeps 8 of its 9 closure sites where the prototype reaches 6, nbody keeps 1 of 2, and strings and string_keys each keep a call through a value. With a limit of two generations all 23 benchmarks match the unlimited prototype, and the deepest clone any benchmark makes is generation two.

**5. Who owns what under the two tail-call rules.** I copied the census model (a Python model of the memory passes over 17 benchmarks' real IR) and made its tail-call rule switchable, running inference only. Over 16 benchmarks there are 429 parameters that can hold a cell and are not Ints. Under the rule "any tail call that passes something the frame owns makes the callee's parameter owned for every caller, and a closure target owns everything", 61 are borrowed. Under "only a self tail call does that, and everything else goes to an owned twin", 76 are. 15 of 416 functions need a twin: 13 because a closure is made of them and 2 for a tail call (list's `length`, spectral_norm's `atv`). `census/twin.py` prints the list. In json, rule O4 makes `capture` and `read_string` own the input, and the main parse chain owns it with or without O4, so I do not claim json's input stays borrowed.

**6. What a dead-cell test costs.** A standalone safe-Rust crate (`counts/`), read with objdump. `release`'s live path is 13 instructions today and 13 with one compare that also tells a dead cell (count 0) from a last reference. `share` is 8 instructions today and 12 with the same test, which is why the test on share is in debug builds only.

**7. Critics' claims I checked in the code or by running.**
- `scarlet dis benches/list/list.sca`: `benchmark` ends in `ret call fn#0(%6)`, a tail call to `length` with a local of that frame. Held.
- `scarlet dis benches/nbody/nbody.sca`: `pull` has `let %70 = call self(..)` outside tail position and `ret MakeTuple(..)` at its tails. Held.
- `scarlet dis benches/spectral_norm/spectral_norm.sca`: `a` is 12 `Let`s and a return. Held.
- The constants probe: the toplevel is three binds pinned to slots and `ret nil`, and no local reads them. Held.
- The float probe prints B, B, A, so `(0.0, 2)` matches the `-0.0` row today; and `big(1000000000000000)` matches its literal. Held.
- `map.sca` line 6 is `pub type Map(k, v)`, with no constructors. Held.
- heap.rs: `release` on a freed cell reads header 0 and falls through to `free_cell` with no error; `held` starts a tuple's fields at word 1 and a constructor's at word 2; `STICKY` already makes a count that `share` and `release` leave alone. Held.
- code.rs: `allocate` adds a clash only between a write and what is live after it; `moves_on_last_use` sets `moved` for `ArrayAppend` from liveness; the `MapSetOwned` match needs the map's `Drop` as the next node. Held.
- perceus.rs: `back_edge_seed` and the second reuse walk carry tokens round a self tail call; `owns_cell` asks `is_heap`, which is false for Int, String and type variables. Held.
- program.rs line 218 interns constants by content. So a `ConstId` is the identity of a string's bytes, and of a float's bits, which is the wrong identity for a float case.
- Compile cost: `scarlet check benches/json/json.sca` retires 87.9M instructions and `scarlet dis` 83.0M on this run, so lowering, Perceus and printing for the program are inside the noise.

**8. The gain arithmetic.** I gave each benchmark's share to one step and computed each factor as exp of the mean of ln(1 - share) over the 23. The steps come out at 0.996, 0.966, 0.951, 0.991, 0.956, 0.986, 0.995, 0.992, 0.985 and 0.996. Summing shares within a benchmark, steps 1 to 8 are 0.845 and steps 1 to 13 are 0.809. With every share cut by a quarter for overlap the whole is 0.861, and with only the inlining and tuple gains cut by 40% it is 0.835.

**What I did not do.** I built no pass in Rust. Ownership across joins with parameters, or-patterns that bind, guards and binary patterns has not been exercised anywhere, because the one model of the memory passes copies join bodies. The twin was counted, not run. The mutation test and the built-in table test are specified, not built. `Probe` has no prototype. The unit costs of `Switch`, `Open` and `Update` on the real VM are estimates from toy VMs and from today's unit costs, which is why steps 2, 4 and 11 each start by landing the handler and measuring it.

## Prior art

Fetched and read in this session (raw sources):

- Lean 4, `src/Lean/Compiler/IR/Borrow.lean` at v4.12.0 (https://github.com/leanprover/lean4/blob/v4.12.0/src/Lean/Compiler/IR/Borrow.lean). `collectExpr` is the source of rules O1 and O2: a constructor owns its arguments that are parameters, a full application owns the arguments its callee's parameters own, `ap` and `pap` own everything, and for a projection "if z is owned then own x", which is "a part of it is consumed". `preserveTailCall` carries the note "we currently support TCO for self-calls only" and tests `ctx.currFn == g`, which is rule O3. A jump runs both `ownArgsUsingParams` and `ownParamsUsingArgs`, so join points are treated as local functions.
- Lean 4, `src/Lean/Compiler/IR/RC.lean` at v4.12.0 (https://github.com/leanprover/lean4/blob/v4.12.0/src/Lean/Compiler/IR/RC.lean). `processVDecl` for `Expr.proj _ x` adds an increment to the result when `x` is consumed, under the comment "We do not need to consume the projection of a variable that is not consumed". So Lean counts a read out of an owned variable and borrows only out of a borrowed one. That is the rule this design keeps, and why borrows rooted in a local are left out.
- Lean 4, `src/Lean/Compiler/IR/Boxing.lean` at v4.12.0 (https://github.com/leanprover/lean4/blob/v4.12.0/src/Lean/Compiler/IR/Boxing.lean). `requiresBoxedVersion` is true when any parameter is borrowed, and `mkBoxedVersion` makes an entry whose parameters are all owned, which `pap` points at. That is the owned twin. Lean's is a wrapper that calls and returns; Scarlet's is a second counting of the body, because Scarlet promises every tail call.
- Koka, `src/Backend/C/Parc.hs` on dev (https://github.com/koka-lang/koka/blob/dev/src/Backend/C/Parc.hs). `parcGuard` computes `dups = S.intersection ownedPvs liveInThisBranch`: the pattern variables of an owned scrutinee are dupped, and the one place that skips it is marked "unsafe in general". `specializeDrop` then fuses those dups with the parent's drop, which is `Open`.
- Koka, `src/Backend/C/ParcReuse.hs` on dev (https://github.com/koka-lang/koka/blob/dev/src/Backend/C/ParcReuse.hs). `ruTryReuseCon` looks a token up with `M.lookup size available`, in a map keyed by allocation size. So a token pairs by size, never by field count, which is the rule recorded for tokens if they come back.
- Erlang/OTP, `lib/compiler/src/genop.tab` on master (https://github.com/erlang/otp/blob/master/lib/compiler/src/genop.tab). `get_map_elements/3` and `bs_get_utf8/5` take a fail label and bind only on success, which is `Probe`. `select_val Arg FailLabel Destinations` is `Switch`. `update_record Hint Size Src Dst Updates` is `Update`, with a hint where this design has a run-time uniqueness test.

Read in earlier rounds of this work and relied on here without re-reading:

- Gleam, `compiler-core/src/exhaustiveness.rs` (https://github.com/gleam-lang/gleam/blob/main/compiler-core/src/exhaustiveness.rs): the row-of-checks decision-tree compiler, after Jules Jacobs, "How to compile pattern matching" (https://julesjacobs.com/notes/patternmatching/patternmatching.pdf) and Maranget, "Compiling Pattern Matching to Good Decision Trees" (https://www.cs.tufts.edu/~nr/cs257/archive/luc-maranget/jun08.pdf). Its `find_pivot_check` picks the check in the most clauses. It cites Gustafsson and Sagonas (https://user.it.uu.se/~kostis/Papers/JFP_06.pdf) for bit-array patterns in the tree, which is the later path for `<<>>`.
- Koka `Core/Specialize.hs` (https://github.com/koka-lang/koka/blob/dev/src/Core/Specialize.hs): specialising a recursive function on a parameter it passes on unchanged.
- Lean 4 `ExpandResetReuse.lean` (https://github.com/leanprover/lean4/blob/v4.9.0/src/Lean/Compiler/IR/ExpandResetReuse.lean): the fast and slow path on `isShared`, which are the two halves of `Open`.
- "Reference Counting with Frame Limited Reuse" (https://www.microsoft.com/en-us/research/uploads/prod/2021/11/flreuse-tr.pdf): borrowing "can increase the memory usage of a program by an arbitrary amount". Rule O4 comes from this.
- "Perceus: Garbage Free Reference Counting with Reuse" (https://www.microsoft.com/en-us/research/uploads/prod/2020/11/perceus-tr-v1.pdf) and "Counting Immutable Beans" (https://arxiv.org/abs/1908.05647): without borrow inference deriv is 1.16 times slower and binarytrees 1.14.
- MLton `ssa/shrink.fun` and `ssa/inline.fun` (https://github.com/MLton/mlton/tree/master/mlton/ssa): one small simplifier run between bigger passes, and a size threshold with "one reference is always fine". `ssa/flatten.fun` is flattening a tuple passed to a jump.
- GHC `OccurAnal.hs`, Note [Choosing loop breakers] (https://github.com/ghc/ghc/blob/master/compiler/GHC/Core/Opt/OccurAnal.hs). I read it and did not take it: "nothing in a cycle is inlined, except a lambda made once and called once" is simpler and is the rule that was run. `CprAnal.hs`, Note [Constructed Product Result] (https://github.com/ghc/ghc/blob/master/compiler/GHC/Core/Opt/CprAnal.hs), is the worker and wrapper split in the optional step.
- Erlang/OTP `beam_ssa_destructive_update.erl` (https://github.com/erlang/otp/blob/master/lib/compiler/src/beam_ssa_destructive_update.erl): the in-place form of `update_record`.
- Maurer, Downen, Ariola and Peyton Jones, "Compiling without Continuations" (https://www.microsoft.com/en-us/research/wp-content/uploads/2016/11/join-points-pldi17.pdf), and OxCaml Flambda 2 `flambda.mli` (https://github.com/oxcaml/oxcaml/blob/main/middle_end/flambda2/terms/flambda.mli): joins with parameters, jumps and switch as the only control flow.

## What it gives up, and what the critics found

## What the critics found, and what I did

I checked every problem in the code or by experiment. All of them held. None is answered with "it does not hold".

**The correctness critic.**
- The checker's rules accepted unbalanced bodies (a `Lend` in a keeping position, no operand order, a rule that covered only calls, an `Open` that could list a field twice). I rewrote the rules around one table of positions (V1 to V14), with gives applied one at a time and "a local a node gives is not lent by it" for every node. The checker is tested by mutation.
- A found flag with a value has no ownership state on a miss. `ArrayGet`, `MapGet`, `ArrayUncons` and `BinaryReadUtf8` are now a control node, `Probe`, that binds only on a hit.
- `Repr` was undefined for types with no constructors, and `Cell` bought 3 instructions. `Repr` has two values, and `Counted` is the default for everything the table does not prove a word.
- The simplifier would delete module constants, and a pinned bind could not end. `SetGlobal` is an explicit keeping node, and `CoreBind::global` goes.
- Literal cases keyed by `ConstId` miscompile floats, and Int cases had no path for a big Int. Cases are keyed by `LitKey`, `SwitchInt` has a slow path for a cell, and both probes go into step 4's differential test.
- A token paired by field count could be the wrong size. Tokens are out of the plan. The rule for bringing them back (pair by kind and words, rewrite the header) is written down.
- A double release and a dead borrow were silent. A release of a dead cell is a typed stop in every build from step 0, and debug builds quarantine freed cells, check reads and poison given registers.
- The built-in table was an unchecked second statement of 150 handlers. One wrapper applies each row, only a short named list keeps its operands, `PartOf` is limited to three fixed-slot reads, and a table-driven test compares every handler with its row.
- One tail call anywhere could make a parameter owned for every caller. A function's holds now depend only on its body and its callees, and an owned twin takes the tail calls and closures. I counted it: 76 borrowed against 61, and 15 twins in 416 functions.
- A result can share a register with a given operand. Rule H1 and a loader test cover it.
- The smaller points are taken too: a waiting built-in has only looking positions, a closure target has `env` exactly when it has captures, and every lambda and public function keeps its body for the wire.

**The numbers critic.**
- The worker and wrapper split contradicted the loop-breaker rule on nbody's `pull`. There is no loop-breaker choice any more, and the split is an optional step that states the prototype's rule and names `pull` as its test.
- spectral_norm's `a` missed the inline limit by one node. A constant load is no longer a node, and a common-subexpression rule is added. I re-ran the prototype: `a` is inlined and nothing else changes.
- Two steps counted closures and nbody twice. Each benchmark's share now belongs to one step, and step 0 adds a scoreboard column that shows a double count.
- richards added up to 52.5%. It is 31% across the area, and the update that takes a field is 9%, not 10.7%.
- The token was credited with `Open`'s gain. `Open` lands with no token. I re-ran the probe (366 against 345).
- The headline was the same numbers multiplied twice. It is now 0.81 on today's VM by my recount and 0.84 to 0.88 after the call and allocation work, and the three new instructions are measured on the real VM before their passes are built.
- Rule B4 and B5's cost (at most 0.2%) and the `moved` path for `Update` are taken as measured.

**The simplicity critic.**
- Step 9 could not land while reuse kept its old form. Reuse is switched off in step 9, and the checker grows with the contract.
- Step 1 would have started tracking Ints. It keeps `is_heap` until step 9.
- Steps 1 to 8 carry most of the gain and none of the ownership risk. They are the plan of record with steps 9 and 10, and the rest is gated.
- Borrows rooted in a local go beyond Lean and Koka. I read both sources, removed them, and measured what that costs (47 instructions a read on the toy).
- The middle end in the text was not the one the prototype ran. The text is now the prototype's rules, with the generation limit measured.
- "Behaviour cannot go wrong yet" was true only for a missed release. The poison word and the audit heap are in from steps 0 and 9.
- Most of the tuple step's gain was the flatten-a-jump rule. It is counted in step 6, and the worker and wrapper split is 0.996 and optional.
- Frozen cells had no stated behaviour. The heap's sticky count already gives it, and the contract now says so.
- The IR let illegal states through. Holds have two values, there are no token binds, `env` is a parameter, `Open` carries its shape, and `Repr` is a function and not a field.
- Three steps bundled mechanisms. The middle end is three steps with a counter for each rule, the memory work is five, and the old dead-bind step is folded into the decision tree that replaces its code.

## What this design gives up

**The safety net goes at step 10.** Today a Perceus mistake costs time. After step 10 a wrong mark is a leak or a freed cell read again. In safe Rust over an indexed heap that is a wrong answer or a typed stop, never memory unsafety, but it is a bug. The checker proves ownership, not that the passes chose well, and it has to be right itself. Someone who values the net more than the speed can stop after step 8 and keep 0.845.

**The passes are not built.** The worked outputs come from a hand-written toy, a Python prototype of the simplifier that cannot run its result, and a Python model of the memory passes that copies join bodies. Ownership across joins with parameters is the hardest code in the plan and nothing has exercised it. That is where I expect step 9 to take longest.

**Owned-only reads cost a count.** A cell field that is only looked at, read out of a record the frame owns, costs a count up and a count down. That is about 3% of a towers move on the toy. Lean and Koka pay the same.

**No tokens means a free and an allocation for each rebuild.** That is 21 to 32 instructions on the toy, and about 129 on today's allocator until the allocation work lands. cfold may be up to 4% worse between steps 9 and 11.

**Borrowing is the least certain gain.** Only 76 of 429 parameters are borrowed in the census, the gain sits in a few look-only functions, and the twin doubles 15 bodies in 416. Rule O1 follows Lean in making a stored parameter owned, and views are the one exception. That choice is a heuristic.

**It is not garbage-free.** A borrowed argument is kept until the last call of the callee that uses it. A loop over a borrowed list keeps the whole list until the loop ends, where an owning loop would free it as it went.

**In-place update depends on a count at run time.** A record that happens to be shared takes the copying path. The fast path is a property of the run and not of the source.

**Tuples stay boxed across calls.** `Ok((value, index))`, which json returns everywhere, still allocates both cells (a modelled 11% of json). nbody keeps 7.8% in tuples until the optional step.

**Whole-program inlining fights hot reload.** Inlining `scarlet/result.then` into user code bakes the callee into the caller, so replacing one module at run time would need its dependents recompiled. `same_unit` takes the two modules so the policy can narrow in one place. BEAM does not inline across modules for this reason. This is the design's weakest point for what comes later.

**Code gets bigger, and stack traces lose inlined frames.** In the prototype json grows 16% in nodes, strings 41% and closures 72%. A stop inside inlined code names the caller and the list of functions inlined into it.

**The gains are estimates and they overlap with other areas.** About 0.81 on today's VM is my recount, with each benchmark's share in one step. Inlining saves 223 a call today and about 110 once calls are cheaper, so json, bounce and the other inlining gains shrink by a third to a half. The larger effect of step 10 is that it makes the call, register and tail-call fixes sound, and those gains belong to those items.

## Processes and a JIT

**Processes.** Nothing here crosses a heap. A count stays a plain integer, because every cell a process can write a count on is in its own heap. The one thing shared between processes is what the program holds: constants and everything a global reaches. Those must be frozen with the sticky count the heap already has before a second process exists. A frozen cell answers every count instruction without a write: `Copy` and `Drop` leave it alone, and the unique test in `Update`, `Open` and the keeping built-ins fails. So a read of a global is a borrow with no cost, and a copy of one into a process's own cell writes nothing another process can see.

A borrow is always backed by a reference a suspended frame of the same process holds, or by the program. So switching processes at a call cannot invalidate one. A message send is a keeping position: the sender gives or copies into it, and the copy to the other heap happens there. A process that dies returns its chunks whole and never walks its frames. With explicit ownership there is no sweep to skip: a suspended frame is its registers and its return record, and what it owns is exactly the owned locals live at its program counter.

Rule O4 exists for processes. A server loop is a chain of tail calls that never returns, and without O4 a borrowed argument its caller no longer needs would be held for the life of the process. A built-in that can wait counts as a call for O4. It also has only looking positions, and a given operand is released after the work, so an instruction that yields and runs again never makes a copy twice.

**Reductions.** A call is still where the VM can yield, and frames still live on the VM's own stack. Inlining and specialisation remove calls, so one reduction covers more work than before. Every loop still passes through a call: a jump only goes forward to an enclosing join, the checker enforces that, nothing in a call cycle is inlined except a lambda that is then gone, and a specialised `fold` keeps its self tail call. Straight-line code between two calls is bounded by the 3,000 node cap. If reductions should track work more closely, the loader can charge a function's entry by its instruction count. Every tail call stays a jump, through the function's own loop rule or through its owned twin, so a loop through function values runs in constant stack.

**Closures on the wire.** A closure names its function, and every lambda and every public function keeps its body, so a closure another program sends can always be decoded by the same module code. A closure's target in the VM is the function's owned twin when it has one. The twin has the source function's module and name, and the wire carries those, never a `FuncIdx`. A specialised clone is never a closure's target. Every closure target owns all its parameters, so its calling convention does not depend on inference.

**Hot reload.** A function's holds depend only on its own body and its callees, never on its callers. So a module's compiled code does not change when another module starts calling it. What does cross modules is a direct call to a borrowed entry and an inlined body. Both ask `same_unit(caller_module, callee_module)`, which says yes to everything today. Narrowing it later makes a call across a module boundary go to the owned twin and stops inlining across it, and then a module's code depends on no other module's inference. The cost of whole-program inlining for reload is in the trade-offs.

**JIT.** The JIT compiles the same instructions, and this contract is the one Koka and Lean compile to C. `Lend` compiles to nothing. `Copy` is a tag test and an increment that skips a frozen count. `Give` at a keeping position is nothing, and at a looking position it is a tag test with a call on the cold path. `Open` and `Update` are a load, a compare of the count with one, and two straight-line blocks. `Switch` is a bounds-checked indirect jump, or one branch for a type like List. `Probe` is a call with two exits. No hidden behaviour is left for the JIT to copy: today it would have to emit the release-on-write test and the frame sweep to agree with the interpreter, and after step 10 neither exists. Because a register is a plain word, a value can live in a machine register across instructions. `Repr` tells the JIT which registers never need a tag test, and parameter holds are a calling convention it can use directly. The debug-build poison word and read checks are interpreter-only and change no instruction's meaning.

**What was left out stays possible.** Tokens, borrows rooted in a local and several results in registers each add to this contract without changing it: a third local state, a root on a borrow, a result count on a function. None of them is needed by processes or by the JIT.

