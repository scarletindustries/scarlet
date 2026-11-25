//! The instruction the loader makes and the interpreter runs.
//!
//! There is one enum, [`Ins`], for both. The loader works on `Ins<VReg>`, in
//! which a register is a value's own number, a jump names a label and a call
//! names a function. The interpreter runs `Ins<Reg>`, in which a register is a
//! place in the frame, and a jump and a call name an instruction. Giving the
//! values registers turns one into the other ([`Place`]).
//!
//! Each field's type says what the instruction does with it. A [`Dst`] is
//! written, a [`Src`] is only looked at, and a [`Taken`] is emptied. So there
//! is one list of the registers an instruction names and of what happens to
//! each, which is the fields themselves ([`Ins::operands`]). Liveness, register
//! allocation and the listing all read it. The interpreter has a method for
//! each of these types (`exec.rs`). A handler that looks at a register before
//! it takes or shares it reads the number inside.
//!
//! An instruction is 16 bytes. Operands sit in it, and the few forms whose
//! operands may not fit have a second instruction with `Wide` at the end of
//! its name, whose operands are a list kept beside the code ([`Lists`]).

use scarlet_ir::core_ir::{ConstId, FuncIdx, GlobalSlot};
use scarlet_ir::intrinsic::Intrinsic;

use crate::heap::Ctor;
use crate::ops::NumOp;
use crate::value::{Float, SmallInt, Tag, Value};

mod fields;

pub(crate) use fields::{
    Arg, ArgList, Args, BinaryId, BitsOp, BlockId, Dead, Dst, DstList, EntryPc, Hands, Hit, Last,
    Lists, LookList, Looks, MaybeDst, Names, Newline, Part, Pc, Place, Places, Reg, Role, Room,
    ShareList, Shared, Shares, Src, Taken, TextId, Unplaced, VReg, WhyId,
};
pub(super) use fields::{Callee, Label, LastReg, OwnEntry, OwnRoom};

/// Declares [`Ins`] and everything that is one case for each of its kinds. An
/// instruction is added here and in the interpreter's `match`, and nowhere
/// else. One marked `ends` is never followed by the instruction after it.
macro_rules! instructions {
    (@falls) => { true };
    (@falls ends) => { false };
    ($(
        $(#[$doc:meta])* $name:ident $({ $($field:ident: $ty:ty),* $(,)? })? $(=> $end:ident)?
    ),* $(,)?) => {
        /// One instruction. Its fields are in the order its handler reads
        /// them, after the registers it writes.
        #[derive(Debug, Clone, Copy)]
        pub(crate) enum Ins<R: Names> {
            $($(#[$doc])* $name $({ $($field: $ty),* })?),*
        }

        /// Which kind of instruction an [`Ins`] is, without its operands. The
        /// listing names an instruction by it and a counted run counts by it,
        /// so the two use the same words.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub(crate) enum Op {
            $($name),*
        }

        impl Op {
            /// Every kind, in the order they are declared, which is also each
            /// one's number.
            pub(crate) const ALL: &[Op] = &[$(Op::$name),*];

            pub(crate) fn name(self) -> &'static str {
                match self {
                    $(Op::$name => stringify!($name)),*
                }
            }
        }

        impl<R: Names> Ins<R> {
            pub(crate) fn op(&self) -> Op {
                match self {
                    $(Ins::$name { .. } => Op::$name),*
                }
            }

            /// Whether the next instruction may run after this one.
            pub(crate) fn falls_through(&self) -> bool {
                match self {
                    $(Ins::$name { .. } => instructions!(@falls $($end)?)),*
                }
            }

            /// Call `f` with every register the instruction names and what it
            /// does with it. The registers it reads come in the order it
            /// reads them, after the ones it writes.
            pub(crate) fn operands(&self, lists: &Lists<R>, f: &mut impl FnMut(R, Role)) {
                match *self {
                    $(Ins::$name $({ $($field),* })? => {
                        $($($field.operands(lists, f);)*)?
                    })*
                }
            }

            /// Make each register the instruction hands on one it takes, or
            /// one it shares, as `moved` says. They come in the order
            /// [`Self::operands`] has them.
            pub(crate) fn decide(
                &mut self,
                lists: &mut Lists<R>,
                moved: &mut impl FnMut(R) -> bool,
            ) {
                match self {
                    $(Ins::$name $({ $($field),* })? => {
                        $($(Part::<R>::decide($field, lists, moved);)*)?
                    })*
                }
            }

            /// Where the instruction may jump, if it may.
            pub(crate) fn target(&self) -> Option<R::To> {
                match *self {
                    $(Ins::$name $({ $($field),* })? => {
                        None$($(.or(Part::<R>::target($field)))*)?
                    })*
                }
            }
        }

        impl Ins<VReg> {
            /// The instruction with everything it names in its place.
            pub(crate) fn place(&self, at: &impl Places) -> Result<Ins<Reg>, Unplaced> {
                Ok(match *self {
                    $(Ins::$name $({ $($field),* })? => {
                        Ins::$name $({ $($field: $field.place(at)?),* })?
                    })*
                })
            }
        }

        impl Ins<Reg> {
            /// Add each field to `line`, for the listing.
            pub(crate) fn show(&self, line: &mut crate::listing::Line<'_>) {
                use crate::listing::Show;
                match *self {
                    $(Ins::$name $({ $($field),* })? => {
                        $($($field.show(line);)*)?
                    })*
                }
            }
        }
    };
}

instructions! {
    /// The end of a run, and no function's. The first frame returns here, and
    /// the run's result is in `src`.
    Halt { src: Taken<R> } => ends,
    /// Count one more run of basic block `block`. Only a counted load has
    /// these, one at the head of each block ([`super::emit`]).
    Count { block: BlockId },
    /// Check the whole heap ([`Heap::audit`](crate::heap::Heap::audit)). Only
    /// an audited load has these, one in front of every instruction.
    Audit,
    Const { dst: Dst<R>, value: Value },
    /// A new string. It is made each time the instruction runs, until
    /// constants get a shared area of their own.
    Str { dst: Dst<R>, text: TextId },
    /// A new binary, made each time like [`Ins::Str`].
    BinaryConst { dst: Dst<R>, bits: BinaryId },
    /// A binary operation ([`BitsOp`]).
    Bits { dst: Dst<R>, op: BitsOp, args: Looks<R, 8> },
    BitsWide { dst: Dst<R>, op: BitsOp, args: LookList },
    /// The big int `n`, for a constant past a small Int's range. Its cell is
    /// made the first time the constant is asked for and kept.
    BigInt { dst: Dst<R>, n: i64, id: ConstId },
    /// `dst` gets what `src` holds.
    Move { dst: Dst<R>, src: Arg<R> },
    GetGlobal { dst: Dst<R>, slot: GlobalSlot },
    SetGlobal { slot: GlobalSlot, src: Shared<R> },
    // Each operator on two Ints is an instruction of its own, and so is each
    // on two Floats but `==` and `!=`, which are the ones for any two values
    // ([`Ins::Equal`]). So the one jump that finds an instruction finds its
    // operator too. A name with a `K` at its end holds the second operand,
    // for a constant the program spells there, and one with an `R` before the
    // operator holds the first. Not every operator has one of each. `>` and
    // `>=` of two registers are `<` and `<=` with the registers swapped. A
    // comparison gives a Bool.
    IntAdd { dst: Dst<R>, a: Src<R>, b: Src<R> },
    IntSub { dst: Dst<R>, a: Src<R>, b: Src<R> },
    IntMul { dst: Dst<R>, a: Src<R>, b: Src<R> },
    IntDiv { dst: Dst<R>, a: Src<R>, b: Src<R> },
    IntRem { dst: Dst<R>, a: Src<R>, b: Src<R> },
    IntAddK { dst: Dst<R>, a: Src<R>, k: SmallInt },
    IntSubK { dst: Dst<R>, a: Src<R>, k: SmallInt },
    IntMulK { dst: Dst<R>, a: Src<R>, k: SmallInt },
    IntDivK { dst: Dst<R>, a: Src<R>, k: SmallInt },
    IntRemK { dst: Dst<R>, a: Src<R>, k: SmallInt },
    IntRSubK { dst: Dst<R>, k: SmallInt, a: Src<R> },
    IntEq { dst: Dst<R>, a: Src<R>, b: Src<R> },
    IntNe { dst: Dst<R>, a: Src<R>, b: Src<R> },
    IntLt { dst: Dst<R>, a: Src<R>, b: Src<R> },
    IntLe { dst: Dst<R>, a: Src<R>, b: Src<R> },
    IntEqK { dst: Dst<R>, a: Src<R>, k: SmallInt },
    IntNeK { dst: Dst<R>, a: Src<R>, k: SmallInt },
    IntLtK { dst: Dst<R>, a: Src<R>, k: SmallInt },
    IntLeK { dst: Dst<R>, a: Src<R>, k: SmallInt },
    IntGtK { dst: Dst<R>, a: Src<R>, k: SmallInt },
    IntGeK { dst: Dst<R>, a: Src<R>, k: SmallInt },
    IntNeg { dst: Dst<R>, a: Src<R> },
    FloatAdd { dst: Dst<R>, a: Src<R>, b: Src<R> },
    FloatSub { dst: Dst<R>, a: Src<R>, b: Src<R> },
    FloatMul { dst: Dst<R>, a: Src<R>, b: Src<R> },
    FloatDiv { dst: Dst<R>, a: Src<R>, b: Src<R> },
    FloatRem { dst: Dst<R>, a: Src<R>, b: Src<R> },
    FloatAddK { dst: Dst<R>, a: Src<R>, k: Float },
    FloatSubK { dst: Dst<R>, a: Src<R>, k: Float },
    FloatMulK { dst: Dst<R>, a: Src<R>, k: Float },
    FloatDivK { dst: Dst<R>, a: Src<R>, k: Float },
    FloatRSubK { dst: Dst<R>, k: Float, a: Src<R> },
    FloatRDivK { dst: Dst<R>, k: Float, a: Src<R> },
    FloatLt { dst: Dst<R>, a: Src<R>, b: Src<R> },
    FloatLe { dst: Dst<R>, a: Src<R>, b: Src<R> },
    FloatLtK { dst: Dst<R>, a: Src<R>, k: Float },
    FloatLeK { dst: Dst<R>, a: Src<R>, k: Float },
    FloatGtK { dst: Dst<R>, a: Src<R>, k: Float },
    FloatGeK { dst: Dst<R>, a: Src<R>, k: Float },
    FloatNeg { dst: Dst<R>, a: Src<R> },
    /// The Float nearest the Int in `a`.
    FloatFromInt { dst: Dst<R>, a: Src<R> },
    /// An operator whose operands were only known to be "a number" when the
    /// program compiled. It does Int or Float arithmetic, picked when it
    /// runs, and `+` of two strings.
    Num { dst: Dst<R>, op: NumOp, a: Src<R>, b: Src<R> },
    Neg { dst: Dst<R>, a: Src<R> },
    /// Write what `a` shows to the output, and a newline after it if
    /// `newline`.
    Print { dst: Dst<R>, a: Src<R>, newline: Newline },
    /// The text `${a}` shows. A string shows as itself.
    ToString { dst: Dst<R>, a: Shared<R> },
    /// Every string in `parts`, joined in order.
    Concat { dst: Dst<R>, parts: Looks<R, 8> },
    ConcatWide { dst: Dst<R>, parts: LookList },
    /// A new cell for constructor `ctor`, holding `fields`. A constructor with
    /// no fields needs no cell, so it is an [`Ins::Const`] instead. One with a
    /// number at its end has that many fields and takes each of them, so it has
    /// nothing to share ([`Ins::taking`]).
    Ctor1 { dst: Dst<R>, ctor: Ctor, fields: [Taken<R>; 1] },
    Ctor2 { dst: Dst<R>, ctor: Ctor, fields: [Taken<R>; 2] },
    Ctor3 { dst: Dst<R>, ctor: Ctor, fields: [Taken<R>; 3] },
    Ctor4 { dst: Dst<R>, ctor: Ctor, fields: [Taken<R>; 4] },
    Ctor5 { dst: Dst<R>, ctor: Ctor, fields: [Taken<R>; 5] },
    Ctor6 { dst: Dst<R>, ctor: Ctor, fields: [Taken<R>; 6] },
    Ctor7 { dst: Dst<R>, ctor: Ctor, fields: [Taken<R>; 7] },
    Ctor8 { dst: Dst<R>, ctor: Ctor, fields: [Taken<R>; 8] },
    /// The same, for a constructor that shares a field.
    Ctor { dst: Dst<R>, ctor: Ctor, fields: Args<R, 8> },
    CtorWide { dst: Dst<R>, ctor: Ctor, fields: ArgList },
    /// Take the constructor in `src` apart. `src` gives up its reference to the
    /// cell, and each field goes to its register with a reference of its own.
    /// When `src` held the only one, the fields' references move and the cell
    /// is freed, and a field that nothing reads, which has no register, is given
    /// up. The constructor is `ctor`, as the [`Ins::JumpUnlessTag`] in front
    /// found. One with a number at its end has that many fields.
    Open1 { dsts: [MaybeDst<R>; 1], src: Taken<R>, ctor: Ctor },
    Open2 { dsts: [MaybeDst<R>; 2], src: Taken<R>, ctor: Ctor },
    Open3 { dsts: [MaybeDst<R>; 3], src: Taken<R>, ctor: Ctor },
    Open4 { dsts: [MaybeDst<R>; 4], src: Taken<R>, ctor: Ctor },
    Open5 { dsts: [MaybeDst<R>; 5], src: Taken<R>, ctor: Ctor },
    Open6 { dsts: [MaybeDst<R>; 6], src: Taken<R>, ctor: Ctor },
    Open7 { dsts: [MaybeDst<R>; 7], src: Taken<R>, ctor: Ctor },
    Open8 { dsts: [MaybeDst<R>; 8], src: Taken<R>, ctor: Ctor },
    OpenWide { dsts: DstList, src: Taken<R>, ctor: Ctor },
    /// Perceus's last use of `reg`. It gives up the reference now instead of
    /// at the frame's end.
    Drop { reg: Dead<R> },
    /// A call to the function at `to`. `own` is this frame's size, which is
    /// how far above it the callee's frame starts. One with a number at its
    /// end has that many arguments and takes each of them, so it has nothing to
    /// share ([`Ins::taking`]).
    Call0 { dst: Dst<R>, own: R::Own, to: R::Entry },
    Call1 { dst: Dst<R>, own: R::Own, to: R::Entry, a: Taken<R> },
    Call2 { dst: Dst<R>, own: R::Own, to: R::Entry, a: Taken<R>, b: Taken<R> },
    Call3 { dst: Dst<R>, own: R::Own, to: R::Entry, a: Taken<R>, b: Taken<R>, c: Taken<R> },
    /// The same, for a call that shares an argument.
    Call1Share { dst: Dst<R>, own: R::Own, to: R::Entry, a: Arg<R> },
    Call2Share { dst: Dst<R>, own: R::Own, to: R::Entry, a: Arg<R>, b: Arg<R> },
    Call3Share { dst: Dst<R>, own: R::Own, to: R::Entry, a: Arg<R>, b: Arg<R>, c: Arg<R> },
    Call { dst: Dst<R>, own: R::Own, to: R::Entry, args: Args<R, 6> },
    CallWide { dst: Dst<R>, own: R::Own, to: R::Entry, args: ArgList },
    TailCall { to: R::Entry, args: Args<R, 6>, own: R::Own } => ends,
    TailCallWide { to: R::Entry, args: ArgList, own: R::Own } => ends,
    /// A call to the function running now, as the same closure. One that keeps
    /// no closure is called by name.
    CallSelf { dst: Dst<R>, own: R::Own, to: R::OwnEntry, args: Args<R, 6> },
    CallSelfWide { dst: Dst<R>, own: R::Own, to: R::OwnEntry, args: ArgList },
    TailCallSelf { to: R::OwnEntry, args: Args<R, 6>, own: R::Own } => ends,
    TailCallSelfWide { to: R::OwnEntry, args: ArgList, own: R::Own } => ends,
    /// A call to the function value in `callee`, with or without captures.
    CallValue { dst: Dst<R>, callee: Shared<R>, own: R::Own, args: Args<R, 6> },
    CallValueWide { dst: Dst<R>, callee: Shared<R>, own: R::Own, args: ArgList },
    TailCallValue { callee: Shared<R>, args: Args<R, 6>, own: R::Own } => ends,
    TailCallValueWide { callee: Shared<R>, args: ArgList, own: R::Own } => ends,
    /// A new closure cell running `func` over `captures`. A function with no
    /// captures needs no cell, so it is an [`Ins::Const`] instead.
    Closure { dst: Dst<R>, func: FuncIdx, captures: Shares<R, 8> },
    ClosureWide { dst: Dst<R>, func: FuncIdx, captures: ShareList },
    /// Capture `index` of the closure running now.
    GetCapture { dst: Dst<R>, index: u32 },
    /// The closure running now, as a value. `this` is its function, for when
    /// it runs as a plain function, with no closure cell.
    GetSelf { dst: Dst<R>, this: FuncIdx },
    /// The value in `src`, for the caller. The registers from the first
    /// parameter to `last` are given up.
    Ret { src: Taken<R>, last: R::Last } => ends,
    /// [`Ins::Ret`], for a function that keeps the closure it runs as, which is
    /// given up after the rest.
    RetClosure { src: Taken<R>, last: R::Last } => ends,
    Jump { to: R::To } => ends,
    /// Continue at `to` when `cond` is `False`, else at the next instruction.
    JumpIfFalse { cond: Src<R>, to: R::To },
    // Continue at `to` unless a comparison holds, for an `if` on a comparison
    // nothing else reads. Each is the comparison above with its name.
    JumpUnlessIntEq { a: Src<R>, b: Src<R>, to: R::To },
    JumpUnlessIntNe { a: Src<R>, b: Src<R>, to: R::To },
    JumpUnlessIntLt { a: Src<R>, b: Src<R>, to: R::To },
    JumpUnlessIntLe { a: Src<R>, b: Src<R>, to: R::To },
    JumpUnlessIntEqK { a: Src<R>, k: SmallInt, to: R::To },
    JumpUnlessIntNeK { a: Src<R>, k: SmallInt, to: R::To },
    JumpUnlessIntLtK { a: Src<R>, k: SmallInt, to: R::To },
    JumpUnlessIntLeK { a: Src<R>, k: SmallInt, to: R::To },
    JumpUnlessIntGtK { a: Src<R>, k: SmallInt, to: R::To },
    JumpUnlessIntGeK { a: Src<R>, k: SmallInt, to: R::To },
    JumpUnlessFloatLt { a: Src<R>, b: Src<R>, to: R::To },
    JumpUnlessFloatLe { a: Src<R>, b: Src<R>, to: R::To },
    JumpUnlessFloatLtK { a: Src<R>, k: Float, to: R::To },
    JumpUnlessFloatLeK { a: Src<R>, k: Float, to: R::To },
    JumpUnlessFloatGtK { a: Src<R>, k: Float, to: R::To },
    JumpUnlessFloatGeK { a: Src<R>, k: Float, to: R::To },
    /// `map::set` for a map whose register gives up its reference, so the map
    /// can change in place when nothing else holds it.
    MapSetOwned { dst: Dst<R>, map: Taken<R>, key: Src<R>, value: Src<R> },
    /// `map::get` whose `Option` is only matched on: the value goes straight
    /// to `dst`, the `Some` arm's binder, and a missing key continues at `to`,
    /// the `None` arm. No `Some` cell is made.
    MapFind { dst: Hit<R>, map: Src<R>, key: Src<R>, to: R::To },
    /// `map::delete`, as [`Ins::MapSetOwned`] is `map::set`.
    MapDeleteOwned { dst: Dst<R>, map: Taken<R>, key: Src<R> },
    /// Continue at `to` unless the top half of the word in `src` is `tag`,
    /// which says which variant of its type the value is. No cell is looked at.
    JumpUnlessTag { src: Src<R>, tag: Tag, to: R::To },
    /// Continue at `to` unless `src` is the Int `n`.
    JumpUnlessInt { src: Src<R>, n: i64, to: R::To },
    /// Continue at `to` unless `src` is the Float `x`, compared as `==` does,
    /// so the two zeros match each other.
    JumpUnlessFloat { src: Src<R>, x: f64, to: R::To },
    /// Continue at `to` unless `src` is the string `text`.
    JumpUnlessStr { src: Src<R>, text: TextId, to: R::To },
    /// Continue at `to` unless `src` is the binary `bits`.
    JumpUnlessBinary { src: Src<R>, bits: BinaryId, to: R::To },
    /// A new tuple holding `elements`. One with a number at its end has that
    /// many.
    Tuple2 { dst: Dst<R>, elements: [Shared<R>; 2] },
    Tuple3 { dst: Dst<R>, elements: [Shared<R>; 3] },
    Tuple { dst: Dst<R>, elements: Shares<R, 8> },
    TupleWide { dst: Dst<R>, elements: ShareList },
    /// A call to a `@vm` built-in the VM runs ([`super::built`]).
    Builtin { dst: Dst<R>, intrinsic: Intrinsic, args: Looks<R, 3> },
    /// How many frames there are. A tail call reuses its frame, so a million
    /// of them still count one.
    StackDepth { dst: Dst<R> },
    /// `a == b`, structurally ([`crate::eq`]).
    Equal { dst: Dst<R>, a: Src<R>, b: Src<R> },
    /// `a != b`.
    NotEqual { dst: Dst<R>, a: Src<R>, b: Src<R> },
    /// `!a`, on a Bool.
    Not { dst: Dst<R>, a: Src<R> },
    /// `start..end`, a range of Ints that stores only its two ends.
    Range { dst: Dst<R>, start: Src<R>, end: Src<R> },
    /// A new array of `elements`.
    Array { dst: Dst<R>, elements: Shares<R, 8> },
    ArrayWide { dst: Dst<R>, elements: ShareList },
    /// How many elements the array in `src` has.
    ArrayLen { dst: Dst<R>, src: Src<R> },
    /// Element `index` of the array in `src`, which a pattern has already
    /// proved it has.
    ArrayElem { dst: Dst<R>, src: Src<R>, index: u16 },
    /// The array in `src` without its first `n` elements. When it takes `src`
    /// it may change the array in place.
    ArrayDrop { dst: Dst<R>, src: Arg<R>, n: Src<R> },
    /// `[x, y, ..xs]`, which is the array in `rest` with `front` before it.
    /// When it takes `rest` it may change the array in place.
    ArrayPrepend { dst: Dst<R>, rest: Arg<R>, front: Shares<R, 8> },
    ArrayPrependWide { dst: Dst<R>, rest: Arg<R>, front: ShareList },
    /// `[..xs, x, y]`, which is the array in `rest` with `back` after it.
    ArrayAppend { dst: Dst<R>, rest: Arg<R>, back: Shares<R, 8> },
    ArrayAppendWide { dst: Dst<R>, rest: Arg<R>, back: ShareList },
    /// `[..a, ..b]`.
    ArrayConcat { dst: Dst<R>, a: Src<R>, b: Src<R> },
    /// `xs[a..b]`. It gives `Ok` of the elements of the array in `src` from
    /// `start` up to `end`, or `Err(Nil)` when that range is not inside it.
    ArraySlice { dst: Dst<R>, src: Src<R>, start: Src<R>, end: Src<R> },
    /// The array in `src` with `value` at `index`, for `array::put` and
    /// `array::set`. When the array has no such index, nothing is written
    /// and nothing is taken, and the run continues at `miss`. When it takes
    /// `src` the write may change the array in place.
    ArraySet { dst: Hit<R>, src: Arg<R>, index: Src<R>, value: Arg<R>, miss: R::To },
    /// `xs[i]`. It gives `Some` of element `index` of the array in `src`, or
    /// `None` when it has no such element.
    ArrayIndex { dst: Dst<R>, src: Src<R>, index: Src<R> },
    /// `xs[i] or d`. It gives element `index` of the array in `src`, or
    /// `default` when it has no such element.
    ArrayIndexOr { dst: Dst<R>, src: Src<R>, index: Src<R>, default: Shared<R> },
    /// Field `index` of the constructor in `src`, or element `index` of the
    /// tuple there.
    Field { dst: Dst<R>, src: Src<R>, index: u16 },
    /// Stop the run, because the program broke a promise the compiler makes,
    /// or reached something the VM does not run yet. `why` says which. A
    /// function that needs something not built is one of these and nothing
    /// else.
    Bad { why: WhyId } => ends,
}

const _: () = assert!(size_of::<Ins<Reg>>() == 16);

impl<R: Names> Ins<R> {
    /// The call or the constructor that does the same as this one and has
    /// nothing in it to share with, when this one has few operands to hand on
    /// and takes them all.
    pub(super) fn taking(&self) -> Option<Ins<R>> {
        if let Ins::Ctor { dst, ctor, fields } = *self {
            // The one of these with as many fields.
            macro_rules! one_of {
                ($($name:ident)*) => {
                    None$(.or_else(|| {
                        fields.taken().map(|fields| Ins::$name { dst, ctor, fields })
                    }))*
                };
            }
            return one_of!(Ctor1 Ctor2 Ctor3 Ctor4 Ctor5 Ctor6 Ctor7 Ctor8);
        }
        let took = |a: Arg<R>| a.moved.then_some(Taken(a.reg));
        if let Ins::Call1Share { dst, own, to, a } = *self {
            let a = took(a)?;
            return Some(Ins::Call1 { dst, own, to, a });
        }
        if let Ins::Call2Share { dst, own, to, a, b } = *self {
            let (a, b) = (took(a)?, took(b)?);
            return Some(Ins::Call2 { dst, own, to, a, b });
        }
        if let Ins::Call3Share {
            dst,
            own,
            to,
            a,
            b,
            c,
        } = *self
        {
            let (a, b, c) = (took(a)?, took(b)?, took(c)?);
            return Some(Ins::Call3 {
                dst,
                own,
                to,
                a,
                b,
                c,
            });
        }
        None
    }
}

// The operator instructions by family, each beside the operator it does. This
// is the one list of them. `int_arith!(then)` hands the list to the macro
// `then`, in which the loader finds the instruction for an operator
// (`instr_for!`) and the tests every instruction there is. The interpreter
// needs an arm for each, so it names each with its operator again, and the
// tests hold it to this list. A comparison comes with a second name, the jump
// it becomes.
macro_rules! families {
    ($d:tt $($family:ident { $($row:tt)* })*) => {$(
        macro_rules! $family {
            ($d then:ident $d($d with:tt)*) => {
                $d then! { [$d($d with)*] $($row)* }
            };
        }
    )*};
}

families! { $
    int_arith {
        IntAdd Arith::Add, IntSub Arith::Sub, IntMul Arith::Mul, IntDiv Arith::Div,
        IntRem Arith::Rem
    }
    int_arith_k {
        IntAddK Arith::Add, IntSubK Arith::Sub, IntMulK Arith::Mul, IntDivK Arith::Div,
        IntRemK Arith::Rem
    }
    int_arith_k_first { IntRSubK Arith::Sub }
    int_compare {
        IntEq JumpUnlessIntEq Compare::Eq, IntNe JumpUnlessIntNe Compare::Ne,
        IntLt JumpUnlessIntLt Compare::LT, IntLe JumpUnlessIntLe Compare::LE
    }
    int_compare_k {
        IntEqK JumpUnlessIntEqK Compare::Eq, IntNeK JumpUnlessIntNeK Compare::Ne,
        IntLtK JumpUnlessIntLtK Compare::LT, IntLeK JumpUnlessIntLeK Compare::LE,
        IntGtK JumpUnlessIntGtK Compare::GT, IntGeK JumpUnlessIntGeK Compare::GE
    }
    float_arith {
        FloatAdd Arith::Add, FloatSub Arith::Sub, FloatMul Arith::Mul, FloatDiv Arith::Div,
        FloatRem Arith::Rem
    }
    float_arith_k {
        FloatAddK Arith::Add, FloatSubK Arith::Sub, FloatMulK Arith::Mul, FloatDivK Arith::Div
    }
    float_arith_k_first { FloatRSubK Arith::Sub, FloatRDivK Arith::Div }
    float_compare {
        FloatLt JumpUnlessFloatLt Order::Lt, FloatLe JumpUnlessFloatLe Order::Le
    }
    float_compare_k {
        FloatLtK JumpUnlessFloatLtK Order::Lt, FloatLeK JumpUnlessFloatLeK Order::Le,
        FloatGtK JumpUnlessFloatGtK Order::Gt, FloatGeK JumpUnlessFloatGeK Order::Ge
    }
}

#[cfg(test)]
mod tests;
