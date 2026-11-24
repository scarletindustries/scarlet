//! The VM's instructions, and the loader that makes them from Core IR.
//!
//! When the program loads, each function's Core IR becomes a flat list of
//! register instructions, once. A register is a Core IR local, so
//! `let %2 = IntAdd(%0, %1)` becomes `Int { dst: 2, op: Add, a: 0, b: 1 }`.
//! Each construct maps to one instruction or a few, so the listing reads like
//! the Core IR it came from.
//!
//! The VM is being built one feature at a time. A function that uses
//! something not built yet still loads, marked with what it needs, and only a
//! call to it stops the run. So a program runs as far as the VM has come.
//!
//! A `match` becomes one test per arm, in order. Each test jumps to the next
//! arm's test when the value does not fit the arm's pattern. Otherwise it
//! falls into the arm, which reads out the fields the pattern binds. A
//! `LetCont`'s continuation is placed after its body, and a `Goto` is a jump
//! to it.
//!
//! A call names its callee in one of three ways, and each has its own
//! instruction. `Call` is for a function known when the program loads.
//! `CallSelf` is for the function running now, and keeps the closure it runs
//! as, so a lambda calling itself still sees its captures. `CallValue` is for
//! a function value in a register. Each has a tail form that replaces the
//! frame.

use std::collections::{BTreeSet, HashMap};

use scarlet_ir::core_ir::{
    Abi, Atom, Callee, Const, ConstId, CoreBind, CoreExpr, CoreFn, CorePat, FuncIdx, GlobalSlot,
    JoinId, Load, LocalId, LoweredFn, PrimOp, Program, VariantRef,
};
use scarlet_ir::intrinsic::Intrinsic;
use scarlet_ir::tivec::{Idx, TiVec};

use crate::Audit;
use crate::float::NumOp;
use crate::show::Types;
use crate::stop::{Arity, Broken, Missing, Stop};
use crate::value::Value;

/// A register, which is an index into the running function's frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Reg(pub(crate) u32);

impl Reg {
    fn of(local: LocalId) -> Reg {
        Reg(local.0)
    }
}

/// An argument a call passes.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Arg {
    pub(crate) reg: Reg,
    /// Whether the call takes the reference the register holds instead of
    /// adding one. It does when this is the last thing the frame does with
    /// the register ([`moves_on_last_use`]).
    pub(crate) moved: bool,
}

impl Arg {
    fn shared(reg: Reg) -> Arg {
        Arg { reg, moved: false }
    }
}

/// One instruction. `dst` is the register it writes.
#[derive(Debug, Clone)]
pub(crate) enum Instr {
    /// Count one more run of basic block `block`. Only a counted load has
    /// these, one at the head of each block ([`count_blocks`]).
    Count {
        block: u32,
    },
    /// Check the whole heap ([`Heap::audit`](crate::heap::Heap::audit)). Only
    /// an audited load has these, one in front of every instruction
    /// ([`audit_each`]).
    Audit,
    Const {
        dst: Reg,
        value: Value,
    },
    /// A new string holding `text`. It is made each time the instruction
    /// runs, until constants get a shared area of their own.
    Str {
        dst: Reg,
        text: Box<[u8]>,
    },
    /// A new binary holding the first `len` bits of `bytes`. It is made each
    /// time it runs, like [`Instr::Str`].
    BinaryConst {
        dst: Reg,
        bytes: Box<[u8]>,
        len: u64,
    },
    /// A binary operation ([`BitsOp`]) on `args`.
    Bits {
        dst: Reg,
        op: BitsOp,
        args: Box<[Reg]>,
    },
    /// The big int `n`, for a constant past a small Int's range. Its cell is
    /// made the first time the constant is asked for and kept.
    BigInt {
        dst: Reg,
        n: i64,
        id: ConstId,
    },
    /// `dst` gets what `src` holds. `moved` says nothing reads `src` after
    /// this, so `dst` takes its reference and no count changes.
    Move {
        dst: Reg,
        src: Reg,
        moved: bool,
    },
    GetGlobal {
        dst: Reg,
        slot: GlobalSlot,
    },
    SetGlobal {
        slot: GlobalSlot,
        src: Reg,
    },
    Int {
        dst: Reg,
        op: IntOp,
        a: Reg,
        b: Reg,
    },
    /// `a op b` for a `b` the program spells as a small Int constant, which
    /// saves the instruction that would put it in a register.
    IntImm {
        dst: Reg,
        op: IntOp,
        a: Reg,
        b: i64,
    },
    IntNeg {
        dst: Reg,
        a: Reg,
    },
    Float {
        dst: Reg,
        op: NumOp,
        a: Reg,
        b: Reg,
    },
    FloatNeg {
        dst: Reg,
        a: Reg,
    },
    /// An operator whose operands were only known to be "a number" when the
    /// program compiled. It does Int or Float arithmetic, picked when it
    /// runs, and `+` of two strings.
    Num {
        dst: Reg,
        op: NumOp,
        a: Reg,
        b: Reg,
    },
    Neg {
        dst: Reg,
        a: Reg,
    },
    /// Write what `arg` shows to the output, and a newline after it if
    /// `newline`.
    Print {
        dst: Reg,
        arg: Reg,
        newline: bool,
    },
    /// The text `${a}` shows.
    ToString {
        dst: Reg,
        a: Reg,
    },
    /// Every string in `parts`, joined in order.
    Concat {
        dst: Reg,
        parts: Box<[Reg]>,
    },
    /// A new cell for constructor `variant`, holding `fields`. A constructor
    /// with no fields needs no cell, so it is an [`Instr::Const`] instead.
    Ctor {
        dst: Reg,
        variant: VariantRef,
        fields: Box<[Arg]>,
        /// Perceus reuse. This is a register where an [`Instr::DropReuse`]
        /// above left a hollowed cell, which this constructor overwrites
        /// instead of allocating. If the register holds anything else, the
        /// constructor allocates.
        reuse: Option<Reg>,
    },
    /// Perceus's last use of `reg`. It gives up the reference now instead of
    /// at the frame's end.
    Drop {
        reg: Reg,
    },
    /// An [`Instr::Drop`] of a cell that a constructor below is paired with.
    /// When this is the last reference, the allocation stays in `reg` to be
    /// overwritten instead of freed.
    DropReuse {
        reg: Reg,
    },
    Call {
        dst: Reg,
        func: FuncIdx,
        args: Box<[Arg]>,
    },
    TailCall {
        func: FuncIdx,
        args: Box<[Arg]>,
    },
    /// A call to the function running now, as the same closure.
    CallSelf {
        dst: Reg,
        args: Box<[Arg]>,
    },
    TailCallSelf {
        args: Box<[Arg]>,
    },
    /// A call to the function value in `callee`, with or without captures.
    CallValue {
        dst: Reg,
        callee: Reg,
        args: Box<[Arg]>,
    },
    TailCallValue {
        callee: Reg,
        args: Box<[Arg]>,
    },
    /// A new closure cell running `func` over `captures`. A function with no
    /// captures needs no cell, so it is an [`Instr::Const`] instead.
    Closure {
        dst: Reg,
        func: FuncIdx,
        captures: Box<[Reg]>,
    },
    /// Capture `index` of the closure running now.
    GetCapture {
        dst: Reg,
        index: u32,
    },
    /// The closure running now, as a value. `this` is its function, for when
    /// it runs as a plain function, with no closure cell.
    GetSelf {
        dst: Reg,
        this: FuncIdx,
    },
    Ret {
        src: Reg,
    },
    /// Continue at instruction `to` of this function.
    Jump {
        to: u32,
    },
    /// Continue at `to` when `cond` is `False`, else at the next instruction.
    JumpIfFalse {
        cond: Reg,
        to: u32,
    },
    /// Continue at `to` unless `a op b` holds, for an `if` on a comparison
    /// nothing else reads.
    JumpUnlessCmp {
        op: IntOp,
        a: Reg,
        b: Reg,
        to: u32,
    },
    /// `map::set` for a map whose register gives up its reference, so the map
    /// can change in place when nothing else holds it.
    MapSetOwned {
        dst: Reg,
        map: Reg,
        key: Reg,
        value: Reg,
    },
    /// `map::get` whose `Option` is only matched on: the value goes straight
    /// to `dst`, the `Some` arm's binder, and a missing key continues at `to`,
    /// the `None` arm. No `Some` cell is made.
    MapFind {
        dst: Reg,
        map: Reg,
        key: Reg,
        to: u32,
    },
    /// `map::delete`, as [`Instr::MapSetOwned`] is `map::set`.
    MapDeleteOwned {
        dst: Reg,
        map: Reg,
        key: Reg,
    },
    /// [`Instr::JumpUnlessCmp`] against a small Int constant.
    JumpUnlessCmpImm {
        op: IntOp,
        a: Reg,
        b: i64,
        to: u32,
    },
    /// Continue at `to` unless `src` is constructor `variant`.
    JumpUnlessVariant {
        src: Reg,
        variant: VariantRef,
        to: u32,
    },
    /// Continue at `to` unless `src` is the Int `n`.
    JumpUnlessInt {
        src: Reg,
        n: i64,
        to: u32,
    },
    /// Continue at `to` unless `src` is the Float `x`, compared as `==` does,
    /// so the two zeros match each other.
    JumpUnlessFloat {
        src: Reg,
        x: f64,
        to: u32,
    },
    /// Continue at `to` unless `src` is the string `text`.
    JumpUnlessStr {
        src: Reg,
        text: Box<[u8]>,
        to: u32,
    },
    /// Continue at `to` unless `src` is the binary of the first `len` bits of
    /// `bytes`.
    JumpUnlessBinary {
        src: Reg,
        bytes: Box<[u8]>,
        len: u64,
        to: u32,
    },
    /// A new tuple holding `elements`.
    Tuple {
        dst: Reg,
        elements: Box<[Reg]>,
    },
    /// Element `index` of the tuple in `src`.
    Element {
        dst: Reg,
        src: Reg,
        index: u16,
    },
    /// A call to a `@vm` built-in the VM runs ([`built`]).
    Builtin {
        dst: Reg,
        intrinsic: Intrinsic,
        args: Box<[Reg]>,
    },
    /// `a == b`, structurally ([`crate::eq`]).
    Equal {
        dst: Reg,
        a: Reg,
        b: Reg,
    },
    /// `a != b`.
    NotEqual {
        dst: Reg,
        a: Reg,
        b: Reg,
    },
    /// `!a`, on a Bool.
    Not {
        dst: Reg,
        a: Reg,
    },
    /// `start..end`, a range of Ints that stores only its two ends.
    Range {
        dst: Reg,
        start: Reg,
        end: Reg,
    },
    /// A new array of `elements`.
    Array {
        dst: Reg,
        elements: Box<[Reg]>,
    },
    /// How many elements the array in `src` has.
    ArrayLen {
        dst: Reg,
        src: Reg,
    },
    /// Element `index` of the array in `src`, which a pattern has already
    /// proved it has.
    ArrayElem {
        dst: Reg,
        src: Reg,
        index: u16,
    },
    /// The array in `src` without its first `n` elements.
    /// `moved` says nothing reads `src` after this, so the drop takes its
    /// reference and may change the array in place.
    ArrayDrop {
        dst: Reg,
        src: Reg,
        n: Reg,
        moved: bool,
    },
    /// `[x, y, ..xs]`, which is the array in `rest` with `front` before it.
    /// `moved` says nothing reads `rest` after this, so the push takes its
    /// reference and may change the array in place.
    ArrayPrepend {
        dst: Reg,
        front: Box<[Reg]>,
        rest: Reg,
        moved: bool,
    },
    /// `[..xs, x, y]`, which is the array in `rest` with `back` after it.
    ArrayAppend {
        dst: Reg,
        rest: Reg,
        back: Box<[Reg]>,
        moved: bool,
    },
    /// `[..a, ..b]`.
    ArrayConcat {
        dst: Reg,
        a: Reg,
        b: Reg,
    },
    /// `xs[a..b]`. It gives `Ok` of the elements of the array in `src` from
    /// `start` up to `end`, or `Err(Nil)` when that range is not inside it.
    ArraySlice {
        dst: Reg,
        src: Reg,
        start: Reg,
        end: Reg,
    },
    /// The array in `src` with `value` at `index`, for `array::put` and
    /// `array::set`. When the array has no such index, nothing is written
    /// and nothing is taken, and the run continues at `miss`. `moved` says
    /// nothing reads `src` after a write, so the write takes the register's
    /// reference and may change the array in place.
    ArraySet {
        dst: Reg,
        src: Reg,
        index: Reg,
        value: Arg,
        moved: bool,
        miss: u32,
    },
    /// `xs[i]`. It gives `Some` of element `index` of the array in `src`, or
    /// `None` when it has no such element.
    ArrayIndex {
        dst: Reg,
        src: Reg,
        index: Reg,
    },
    /// `xs[i] or d`. It gives element `index` of the array in `src`, or
    /// `default` when it has no such element.
    ArrayIndexOr {
        dst: Reg,
        src: Reg,
        index: Reg,
        default: Reg,
    },
    /// Field `index` of the constructor in `src`.
    Field {
        dst: Reg,
        src: Reg,
        index: u16,
    },
    /// Stop the run, because the program broke a promise the compiler makes.
    /// `why` says which. The loader puts this where only such a program can
    /// reach, like after a `match`'s last arm.
    Bad {
        why: Broken,
    },
}

impl Instr {
    /// Each register this instruction names, read or written, to renumber.
    fn regs_mut(&mut self, f: &mut impl FnMut(&mut Reg)) {
        let args = |args: &mut [Arg], f: &mut dyn FnMut(&mut Reg)| {
            args.iter_mut().for_each(|a| f(&mut a.reg));
        };
        match self {
            Instr::Bad { .. } | Instr::Jump { .. } | Instr::Count { .. } | Instr::Audit => {}
            Instr::Const { dst, .. }
            | Instr::Str { dst, .. }
            | Instr::BinaryConst { dst, .. }
            | Instr::BigInt { dst, .. }
            | Instr::GetGlobal { dst, .. }
            | Instr::GetCapture { dst, .. }
            | Instr::GetSelf { dst, .. }
            | Instr::SetGlobal { src: dst, .. }
            | Instr::Drop { reg: dst }
            | Instr::DropReuse { reg: dst }
            | Instr::Ret { src: dst }
            | Instr::JumpIfFalse { cond: dst, .. }
            | Instr::JumpUnlessCmpImm { a: dst, .. }
            | Instr::JumpUnlessVariant { src: dst, .. }
            | Instr::JumpUnlessInt { src: dst, .. }
            | Instr::JumpUnlessFloat { src: dst, .. }
            | Instr::JumpUnlessStr { src: dst, .. }
            | Instr::JumpUnlessBinary { src: dst, .. } => f(dst),
            Instr::Move { dst, src: a, .. }
            | Instr::IntImm { dst, a, .. }
            | Instr::IntNeg { dst, a }
            | Instr::FloatNeg { dst, a }
            | Instr::Neg { dst, a }
            | Instr::Print { dst, arg: a, .. }
            | Instr::ToString { dst, a }
            | Instr::Element { dst, src: a, .. }
            | Instr::Not { dst, a }
            | Instr::ArrayLen { dst, src: a }
            | Instr::ArrayElem { dst, src: a, .. }
            | Instr::Field { dst, src: a, .. }
            | Instr::JumpUnlessCmp { a: dst, b: a, .. } => {
                f(dst);
                f(a);
            }
            Instr::Int { dst, a, b, .. }
            | Instr::Float { dst, a, b, .. }
            | Instr::Num { dst, a, b, .. }
            | Instr::Equal { dst, a, b }
            | Instr::NotEqual { dst, a, b }
            | Instr::Range {
                dst,
                start: a,
                end: b,
            }
            | Instr::ArrayDrop {
                dst, src: a, n: b, ..
            }
            | Instr::ArrayConcat { dst, a, b }
            | Instr::ArrayIndex {
                dst,
                src: a,
                index: b,
            }
            | Instr::MapFind {
                dst,
                map: a,
                key: b,
                ..
            }
            | Instr::MapDeleteOwned {
                dst,
                map: a,
                key: b,
            } => {
                f(dst);
                f(a);
                f(b);
            }
            Instr::ArraySlice {
                dst,
                src,
                start,
                end,
            } => {
                for r in [dst, src, start, end] {
                    f(r);
                }
            }
            Instr::ArrayIndexOr {
                dst,
                src,
                index,
                default,
            } => {
                for r in [dst, src, index, default] {
                    f(r);
                }
            }
            Instr::ArraySet {
                dst,
                src,
                index,
                value,
                ..
            } => {
                for r in [dst, src, index, &mut value.reg] {
                    f(r);
                }
            }
            Instr::MapSetOwned {
                dst,
                map,
                key,
                value,
            } => {
                for r in [dst, map, key, value] {
                    f(r);
                }
            }
            Instr::Bits {
                dst, args: more, ..
            }
            | Instr::Concat { dst, parts: more }
            | Instr::Closure {
                dst,
                captures: more,
                ..
            }
            | Instr::Tuple {
                dst,
                elements: more,
            }
            | Instr::Builtin {
                dst, args: more, ..
            }
            | Instr::Array {
                dst,
                elements: more,
            } => {
                f(dst);
                more.iter_mut().for_each(f);
            }
            Instr::Ctor {
                dst, fields, reuse, ..
            } => {
                f(dst);
                fields.iter_mut().for_each(|a| f(&mut a.reg));
                reuse.iter_mut().for_each(f);
            }
            Instr::ArrayPrepend {
                dst,
                front: more,
                rest,
                ..
            }
            | Instr::ArrayAppend {
                dst,
                back: more,
                rest,
                ..
            } => {
                f(dst);
                more.iter_mut().for_each(&mut *f);
                f(rest);
            }
            Instr::Call { dst, args: a, .. } | Instr::CallSelf { dst, args: a } => {
                f(dst);
                args(a, f);
            }
            Instr::CallValue {
                dst,
                callee,
                args: a,
            } => {
                f(dst);
                f(callee);
                args(a, f);
            }
            Instr::TailCall { args: a, .. } | Instr::TailCallSelf { args: a } => args(a, f),
            Instr::TailCallValue { callee, args: a } => {
                f(callee);
                args(a, f);
            }
        }
    }

    /// Each register this instruction reads, for liveness. A plain `Drop`
    /// reads nothing, because dropping an emptied register does nothing.
    fn reads(&self, mut f: impl FnMut(Reg)) {
        let args = |args: &[Arg], f: &mut dyn FnMut(Reg)| args.iter().for_each(|a| f(a.reg));
        match self {
            Instr::Const { .. }
            | Instr::Str { .. }
            | Instr::BinaryConst { .. }
            | Instr::BigInt { .. }
            | Instr::GetGlobal { .. }
            | Instr::GetCapture { .. }
            | Instr::GetSelf { .. }
            | Instr::Drop { .. }
            | Instr::Jump { .. }
            | Instr::Count { .. }
            | Instr::Audit
            | Instr::Bad { .. } => {}
            Instr::Bits { args, .. }
            | Instr::Concat { parts: args, .. }
            | Instr::Closure { captures: args, .. }
            | Instr::Tuple { elements: args, .. }
            | Instr::Builtin { args, .. }
            | Instr::Array { elements: args, .. } => args.iter().copied().for_each(f),
            Instr::Move { src: a, .. }
            | Instr::SetGlobal { src: a, .. }
            | Instr::IntImm { a, .. }
            | Instr::IntNeg { a, .. }
            | Instr::FloatNeg { a, .. }
            | Instr::Neg { a, .. }
            | Instr::Print { arg: a, .. }
            | Instr::ToString { a, .. }
            | Instr::DropReuse { reg: a }
            | Instr::Ret { src: a }
            | Instr::JumpIfFalse { cond: a, .. }
            | Instr::JumpUnlessCmpImm { a, .. }
            | Instr::JumpUnlessVariant { src: a, .. }
            | Instr::JumpUnlessInt { src: a, .. }
            | Instr::JumpUnlessFloat { src: a, .. }
            | Instr::JumpUnlessStr { src: a, .. }
            | Instr::JumpUnlessBinary { src: a, .. }
            | Instr::Element { src: a, .. }
            | Instr::Not { a, .. }
            | Instr::ArrayLen { src: a, .. }
            | Instr::ArrayElem { src: a, .. }
            | Instr::Field { src: a, .. } => f(*a),
            Instr::Int { a, b, .. }
            | Instr::Float { a, b, .. }
            | Instr::Num { a, b, .. }
            | Instr::JumpUnlessCmp { a, b, .. }
            | Instr::Equal { a, b, .. }
            | Instr::NotEqual { a, b, .. }
            | Instr::Range {
                start: a, end: b, ..
            }
            | Instr::ArrayDrop { src: a, n: b, .. }
            | Instr::ArrayConcat { a, b, .. }
            | Instr::ArrayIndex {
                src: a, index: b, ..
            } => {
                f(*a);
                f(*b);
            }
            Instr::ArraySlice {
                src, start, end, ..
            } => {
                f(*src);
                f(*start);
                f(*end);
            }
            Instr::ArrayIndexOr {
                src,
                index,
                default,
                ..
            } => {
                f(*src);
                f(*index);
                f(*default);
            }
            Instr::ArraySet {
                src, index, value, ..
            } => {
                f(*src);
                f(*index);
                f(value.reg);
            }
            Instr::MapSetOwned {
                map, key, value, ..
            } => {
                f(*map);
                f(*key);
                f(*value);
            }
            Instr::MapFind { map, key, .. } | Instr::MapDeleteOwned { map, key, .. } => {
                f(*map);
                f(*key);
            }
            Instr::Ctor { fields, reuse, .. } => {
                fields.iter().for_each(|a| f(a.reg));
                reuse.iter().copied().for_each(f);
            }
            Instr::ArrayPrepend {
                front: more, rest, ..
            }
            | Instr::ArrayAppend {
                back: more, rest, ..
            } => {
                more.iter().copied().for_each(&mut f);
                f(*rest);
            }
            Instr::Call { args: a, .. }
            | Instr::CallSelf { args: a, .. }
            | Instr::TailCall { args: a, .. }
            | Instr::TailCallSelf { args: a } => args(a, &mut f),
            Instr::CallValue {
                callee, args: a, ..
            }
            | Instr::TailCallValue { callee, args: a } => {
                f(*callee);
                args(a, &mut f);
            }
        }
    }

    /// The register this instruction writes, if any, and whether it writes
    /// it every time. A `Drop` empties its register, which ends what it held
    /// as surely as a write.
    fn writes(&self) -> Option<Write> {
        let dst = match self {
            // A lookup of a key the map does not have, or a write at an index
            // the array does not have, has no answer to write.
            Instr::MapFind { dst, to, .. } | Instr::ArraySet { dst, miss: to, .. } => {
                return Some(Write::OnHit {
                    reg: *dst,
                    miss: *to,
                });
            }
            Instr::Const { dst, .. }
            | Instr::Str { dst, .. }
            | Instr::BinaryConst { dst, .. }
            | Instr::Bits { dst, .. }
            | Instr::BigInt { dst, .. }
            | Instr::Move { dst, .. }
            | Instr::GetGlobal { dst, .. }
            | Instr::Int { dst, .. }
            | Instr::IntImm { dst, .. }
            | Instr::IntNeg { dst, .. }
            | Instr::Float { dst, .. }
            | Instr::FloatNeg { dst, .. }
            | Instr::Num { dst, .. }
            | Instr::Neg { dst, .. }
            | Instr::Print { dst, .. }
            | Instr::ToString { dst, .. }
            | Instr::Concat { dst, .. }
            | Instr::Ctor { dst, .. }
            | Instr::Call { dst, .. }
            | Instr::CallSelf { dst, .. }
            | Instr::CallValue { dst, .. }
            | Instr::Closure { dst, .. }
            | Instr::GetCapture { dst, .. }
            | Instr::GetSelf { dst, .. }
            | Instr::MapSetOwned { dst, .. }
            | Instr::MapDeleteOwned { dst, .. }
            | Instr::Tuple { dst, .. }
            | Instr::Element { dst, .. }
            | Instr::Builtin { dst, .. }
            | Instr::Equal { dst, .. }
            | Instr::NotEqual { dst, .. }
            | Instr::Not { dst, .. }
            | Instr::Range { dst, .. }
            | Instr::Array { dst, .. }
            | Instr::ArrayLen { dst, .. }
            | Instr::ArrayElem { dst, .. }
            | Instr::ArrayDrop { dst, .. }
            | Instr::ArrayPrepend { dst, .. }
            | Instr::ArrayAppend { dst, .. }
            | Instr::ArrayConcat { dst, .. }
            | Instr::ArraySlice { dst, .. }
            | Instr::ArrayIndex { dst, .. }
            | Instr::ArrayIndexOr { dst, .. }
            | Instr::Field { dst, .. }
            | Instr::Drop { reg: dst } => *dst,
            Instr::SetGlobal { .. }
            | Instr::DropReuse { .. }
            | Instr::TailCall { .. }
            | Instr::TailCallSelf { .. }
            | Instr::TailCallValue { .. }
            | Instr::Ret { .. }
            | Instr::Jump { .. }
            | Instr::JumpIfFalse { .. }
            | Instr::JumpUnlessCmp { .. }
            | Instr::JumpUnlessCmpImm { .. }
            | Instr::JumpUnlessVariant { .. }
            | Instr::JumpUnlessInt { .. }
            | Instr::JumpUnlessFloat { .. }
            | Instr::JumpUnlessStr { .. }
            | Instr::JumpUnlessBinary { .. }
            | Instr::Count { .. }
            | Instr::Audit
            | Instr::Bad { .. } => return None,
        };
        Some(Write::Always(dst))
    }

    /// The instructions that may run after this one, which is at `i`.
    fn successors(&self, i: usize) -> impl Iterator<Item = usize> {
        let next = i.checked_add(1);
        let (fall, jump) = match self {
            Instr::Jump { to } => (None, Some(*to)),
            Instr::JumpIfFalse { to, .. }
            | Instr::JumpUnlessCmp { to, .. }
            | Instr::JumpUnlessCmpImm { to, .. }
            | Instr::MapFind { to, .. }
            | Instr::ArraySet { miss: to, .. }
            | Instr::JumpUnlessVariant { to, .. }
            | Instr::JumpUnlessInt { to, .. }
            | Instr::JumpUnlessFloat { to, .. }
            | Instr::JumpUnlessStr { to, .. }
            | Instr::JumpUnlessBinary { to, .. } => (next, Some(*to)),
            Instr::TailCall { .. }
            | Instr::TailCallSelf { .. }
            | Instr::TailCallValue { .. }
            | Instr::Ret { .. }
            | Instr::Bad { .. } => (None, None),
            Instr::Count { .. }
            | Instr::Audit
            | Instr::Const { .. }
            | Instr::Str { .. }
            | Instr::BinaryConst { .. }
            | Instr::Bits { .. }
            | Instr::BigInt { .. }
            | Instr::Move { .. }
            | Instr::GetGlobal { .. }
            | Instr::SetGlobal { .. }
            | Instr::Int { .. }
            | Instr::IntImm { .. }
            | Instr::IntNeg { .. }
            | Instr::Float { .. }
            | Instr::FloatNeg { .. }
            | Instr::Num { .. }
            | Instr::Neg { .. }
            | Instr::Print { .. }
            | Instr::ToString { .. }
            | Instr::Concat { .. }
            | Instr::Ctor { .. }
            | Instr::Drop { .. }
            | Instr::DropReuse { .. }
            | Instr::Call { .. }
            | Instr::CallSelf { .. }
            | Instr::CallValue { .. }
            | Instr::Closure { .. }
            | Instr::GetCapture { .. }
            | Instr::GetSelf { .. }
            | Instr::MapSetOwned { .. }
            | Instr::MapDeleteOwned { .. }
            | Instr::Tuple { .. }
            | Instr::Element { .. }
            | Instr::Builtin { .. }
            | Instr::Equal { .. }
            | Instr::NotEqual { .. }
            | Instr::Not { .. }
            | Instr::Range { .. }
            | Instr::Array { .. }
            | Instr::ArrayLen { .. }
            | Instr::ArrayElem { .. }
            | Instr::ArrayDrop { .. }
            | Instr::ArrayPrepend { .. }
            | Instr::ArrayAppend { .. }
            | Instr::ArrayConcat { .. }
            | Instr::ArraySlice { .. }
            | Instr::ArrayIndex { .. }
            | Instr::ArrayIndexOr { .. }
            | Instr::Field { .. } => (next, None),
        };
        fall.into_iter().chain(jump.map(|to| to as usize))
    }

    /// Where this instruction may jump, for a jump still waiting on its label.
    fn target(&mut self) -> Option<&mut u32> {
        match self {
            Instr::Jump { to }
            | Instr::JumpIfFalse { to, .. }
            | Instr::JumpUnlessCmp { to, .. }
            | Instr::JumpUnlessCmpImm { to, .. }
            | Instr::MapFind { to, .. }
            | Instr::ArraySet { miss: to, .. }
            | Instr::JumpUnlessVariant { to, .. }
            | Instr::JumpUnlessInt { to, .. }
            | Instr::JumpUnlessFloat { to, .. }
            | Instr::JumpUnlessStr { to, .. }
            | Instr::JumpUnlessBinary { to, .. } => Some(to),
            Instr::Count { .. }
            | Instr::Audit
            | Instr::Const { .. }
            | Instr::Str { .. }
            | Instr::BinaryConst { .. }
            | Instr::Bits { .. }
            | Instr::BigInt { .. }
            | Instr::Move { .. }
            | Instr::GetGlobal { .. }
            | Instr::SetGlobal { .. }
            | Instr::Int { .. }
            | Instr::IntImm { .. }
            | Instr::MapSetOwned { .. }
            | Instr::MapDeleteOwned { .. }
            | Instr::IntNeg { .. }
            | Instr::Float { .. }
            | Instr::FloatNeg { .. }
            | Instr::Num { .. }
            | Instr::Neg { .. }
            | Instr::Print { .. }
            | Instr::ToString { .. }
            | Instr::Concat { .. }
            | Instr::Ctor { .. }
            | Instr::Drop { .. }
            | Instr::DropReuse { .. }
            | Instr::Call { .. }
            | Instr::TailCall { .. }
            | Instr::CallSelf { .. }
            | Instr::TailCallSelf { .. }
            | Instr::CallValue { .. }
            | Instr::TailCallValue { .. }
            | Instr::Closure { .. }
            | Instr::GetCapture { .. }
            | Instr::GetSelf { .. }
            | Instr::Ret { .. }
            | Instr::Field { .. }
            | Instr::Tuple { .. }
            | Instr::Element { .. }
            | Instr::Builtin { .. }
            | Instr::Equal { .. }
            | Instr::NotEqual { .. }
            | Instr::Not { .. }
            | Instr::Range { .. }
            | Instr::Array { .. }
            | Instr::ArrayLen { .. }
            | Instr::ArrayElem { .. }
            | Instr::ArrayDrop { .. }
            | Instr::ArrayPrepend { .. }
            | Instr::ArrayAppend { .. }
            | Instr::ArrayConcat { .. }
            | Instr::ArraySlice { .. }
            | Instr::ArrayIndex { .. }
            | Instr::ArrayIndexOr { .. }
            | Instr::Bad { .. } => None,
        }
    }
}

/// What an instruction writes, and when. Liveness needs both: a register
/// that is only written on one way out still holds its old value on the
/// other.
#[derive(Clone, Copy)]
enum Write {
    /// The register is written every time the instruction runs.
    Always(Reg),
    /// The register is written only when the instruction has an answer. When
    /// it has none it writes nothing and continues at `miss`.
    OnHit { reg: Reg, miss: u32 },
}

impl Write {
    fn reg(self) -> Reg {
        match self {
            Write::Always(reg) | Write::OnHit { reg, .. } => reg,
        }
    }
}

/// Declares [`Op`] with one variant per kind of instruction, and
/// [`Instr::op`]. Every [`Instr`] variant must be named here: the `match`
/// this writes does not compile without it.
macro_rules! ops {
    ($($name:ident),* $(,)?) => {
        /// Which kind of instruction an [`Instr`] is, without its operands.
        /// The listing names an instruction by it and a counted run counts
        /// by it, so the two use the same words.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub(crate) enum Op {
            $($name),*
        }

        impl Op {
            /// Every kind, in the order they are declared, which is also
            /// each one's number.
            pub(crate) const ALL: &[Op] = &[$(Op::$name),*];

            pub(crate) fn name(self) -> &'static str {
                match self {
                    $(Op::$name => stringify!($name)),*
                }
            }
        }

        impl Instr {
            pub(crate) fn op(&self) -> Op {
                match self {
                    $(Instr::$name { .. } => Op::$name),*
                }
            }
        }
    };
}

ops! {
    Count,
    Audit,
    Const,
    Str,
    BinaryConst,
    Bits,
    BigInt,
    Move,
    GetGlobal,
    SetGlobal,
    Int,
    IntImm,
    IntNeg,
    Float,
    FloatNeg,
    Num,
    Neg,
    Print,
    ToString,
    Concat,
    Ctor,
    Drop,
    DropReuse,
    Call,
    TailCall,
    CallSelf,
    TailCallSelf,
    CallValue,
    TailCallValue,
    Closure,
    GetCapture,
    GetSelf,
    Ret,
    Jump,
    JumpIfFalse,
    JumpUnlessCmp,
    MapSetOwned,
    MapFind,
    MapDeleteOwned,
    JumpUnlessCmpImm,
    JumpUnlessVariant,
    JumpUnlessInt,
    JumpUnlessFloat,
    JumpUnlessStr,
    JumpUnlessBinary,
    Tuple,
    Element,
    Builtin,
    Equal,
    NotEqual,
    Not,
    Range,
    Array,
    ArrayLen,
    ArrayElem,
    ArrayDrop,
    ArrayPrepend,
    ArrayAppend,
    ArrayConcat,
    ArraySlice,
    ArraySet,
    ArrayIndex,
    ArrayIndexOr,
    Field,
    Bad,
}

/// Where a Core IR expression's value goes.
#[derive(Clone, Copy)]
enum Dest {
    /// Returned from the function, so a `Tail` is a return or a tail call.
    Return,
    /// Written to `reg`, then on to the code after a `LetJoin`. A `Tail` here
    /// is a value, and a call in it is an ordinary call.
    Join { reg: Reg, after: Label },
}

/// A jump target not placed yet. Every jump to it is patched when it is.
#[derive(Clone, Copy)]
struct Label(usize);

/// A binary operation, one per Core IR binary `PrimOp`, with its arguments
/// in the `PrimOp`'s order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BitsOp {
    /// `<<v:size(b)>>` of an Int, which is its low `b` bits.
    FromInt,
    /// `<<v:size(b)>>` of a binary, which is its first `b` bits.
    Take,
    /// `<<s:utf8>>`, which is a string's bytes.
    FromString,
    /// A literal's segments, joined.
    Concat,
    BitSize,
    /// `len` bits from bit `at`, shared, for a pattern.
    View,
    /// Whether the binary holds a prefix at bit `at`, for a pattern.
    MatchPrefix,
    /// The code point at bit `at` and how many bits it took, for a pattern.
    ReadUtf8,
    /// `width` bits from bit `at` as an unsigned Int, for a pattern.
    ReadInt,
}

/// A two-operand Int operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IntOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl IntOp {
    /// Whether the op gives a Bool.
    fn compares(self) -> bool {
        match self {
            IntOp::Eq | IntOp::Ne | IntOp::Lt | IntOp::Le | IntOp::Gt | IntOp::Ge => true,
            IntOp::Add | IntOp::Sub | IntOp::Mul | IntOp::Div | IntOp::Rem => false,
        }
    }

    /// The op that gives the same answer with its operands swapped, or `None`
    /// when there is none.
    fn swapped(self) -> Option<IntOp> {
        Some(match self {
            IntOp::Add => IntOp::Add,
            IntOp::Mul => IntOp::Mul,
            IntOp::Eq => IntOp::Eq,
            IntOp::Ne => IntOp::Ne,
            IntOp::Lt => IntOp::Gt,
            IntOp::Le => IntOp::Ge,
            IntOp::Gt => IntOp::Lt,
            IntOp::Ge => IntOp::Le,
            IntOp::Sub | IntOp::Div | IntOp::Rem => return None,
        })
    }
}

/// One loaded function.
#[derive(Debug)]
pub(crate) enum Func {
    Ready(Body),
    /// The function uses something the VM does not run yet, and this says
    /// what.
    NotBuiltYet(Missing),
}

#[derive(Debug)]
pub(crate) struct Body {
    pub(crate) name: String,
    /// Registers a frame for this function needs.
    pub(crate) regs: u32,
    /// Where each argument goes, in order.
    pub(crate) params: Box<[Reg]>,
    pub(crate) instrs: Box<[Instr]>,
    /// Registers a constructor reuses. A call to self in tail position keeps
    /// these, so a loop can overwrite the cell its last turn gave up. Every
    /// other register is released.
    pub(crate) carried: Box<[Reg]>,
}

/// A whole loaded program.
pub(crate) struct Code {
    pub(crate) fns: TiVec<FuncIdx, Func>,
    /// Each module's toplevel in the order they run, with the entry file's
    /// last.
    pub(crate) toplevels: Vec<Func>,
    pub(crate) main: Option<FuncIdx>,
    pub(crate) globals: u32,
    pub(crate) types: Types,
    /// Each function's source name, for showing a function value.
    pub(crate) names: TiVec<FuncIdx, String>,
    /// The stdlib constructors the VM builds on its own.
    pub(crate) abi: Abi,
}

/// A program loaded so that a run of it can say how often each instruction
/// ran ([`load_counted`]).
pub(crate) struct Counted {
    /// The program, with an [`Instr::Count`] at the head of every basic
    /// block.
    pub(crate) code: Code,
    /// The kinds of the instructions in each block, by the number the
    /// block's `Count` holds.
    pub(crate) blocks: Vec<Box<[Op]>>,
}

/// Load `program` for a run that checks its heap when `audit` says. One that
/// is to check before every instruction gets an [`Instr::Audit`] in front of
/// each, and any other gets none.
pub(crate) fn load(program: &Program, audit: Audit) -> Result<Code, Stop> {
    load_as(program, Extra { count: None, audit })
}

/// [`load`], with an [`Instr::Count`] at the head of every basic block. The
/// counts leave the audits out.
pub(crate) fn load_counted(program: &Program, audit: Audit) -> Result<Counted, Stop> {
    let mut blocks = Vec::new();
    let extra = Extra {
        count: Some(&mut blocks),
        audit,
    };
    let code = load_as(program, extra)?;
    Ok(Counted { code, blocks })
}

/// The instructions a load puts in that the program does not have.
struct Extra<'a> {
    /// Where to list the blocks numbered so far, for a load that counts them.
    count: Option<&'a mut Vec<Box<[Op]>>>,
    /// When the run is to audit its heap.
    audit: Audit,
}

fn load_as(program: &Program, mut extra: Extra<'_>) -> Result<Code, Stop> {
    let full = |_| Stop::broken(Broken::TooManyFunctions);
    let consts = &program.consts;
    let mut fns = TiVec::new();
    let mut names = TiVec::new();
    for f in &program.fns {
        let this = fns.next_idx().map_err(full)?;
        fns.push(load_fn(f, consts, &program.abi, Some(this), &mut extra)?)
            .map_err(full)?;
        names.push(f.name.clone()).map_err(full)?;
    }
    let mut toplevels = Vec::new();
    for f in program.inits.iter().chain([&program.toplevel]) {
        toplevels.push(load_fn(f, consts, &program.abi, None, &mut extra)?);
    }
    Ok(Code {
        fns,
        toplevels,
        main: program.main,
        globals: program.globals,
        types: program.types.clone(),
        names,
        abi: program.abi,
    })
}

/// `this` is the function's own index, which a call to `self` means. A
/// toplevel has none, and cannot call itself.
/// Why a function didn't load.
enum Unloadable {
    /// It needs something the VM doesn't run yet. The function still loads,
    /// and stops with this if it's ever called.
    Missing(Missing),
    /// It has more instructions than a jump target can name.
    TooLong,
    /// It needs more registers than a register number can name.
    TooManyRegisters,
}

impl From<Missing> for Unloadable {
    fn from(m: Missing) -> Self {
        Unloadable::Missing(m)
    }
}

/// A function that's too long for the VM's jumps stops the whole load, as a
/// heap too big for its addresses stops a run: it's a limit of the machine.
fn load_fn(
    f: &LoweredFn,
    consts: &[Const],
    abi: &Abi,
    this: Option<FuncIdx>,
    extra: &mut Extra<'_>,
) -> Result<Func, Stop> {
    let name = || format!("{}.{}", f.module, f.name);
    let loaded = Loader::new(consts, abi, &f.core, this)
        .and_then(|l| l.body(&f.core))
        .and_then(|(regs, mut instrs)| {
            let mut params: Box<[Reg]> = f.core.params.iter().map(|p| Reg::of(p.id)).collect();
            let mut carried: Box<[Reg]> = reused_locals(&f.core.body)
                .into_iter()
                .map(Reg::of)
                .collect();
            let regs = allocate(&mut instrs, &mut params, &mut carried, regs);
            let instrs = match &mut extra.count {
                Some(blocks) => count_blocks(instrs, blocks)?,
                None => instrs,
            };
            let instrs = match extra.audit {
                Audit::EachInstruction => audit_each(instrs)?,
                Audit::Never | Audit::AtEnd => instrs,
            };
            Ok((regs, params, carried, instrs))
        });
    Ok(match loaded {
        Ok((regs, params, carried, instrs)) => Func::Ready(Body {
            name: name(),
            regs,
            params,
            carried,
            instrs: instrs.into_boxed_slice(),
        }),
        Err(Unloadable::Missing(what)) => Func::NotBuiltYet(what),
        Err(Unloadable::TooLong) => return Err(Stop::FunctionTooLong { name: name() }),
        Err(Unloadable::TooManyRegisters) => {
            return Err(Stop::TooManyRegisters { name: name() });
        }
    })
}

struct Loader<'c> {
    consts: &'c [Const],
    this: Option<FuncIdx>,
    instrs: Vec<Instr>,
    /// Each label's instruction index once placed, and the jumps still
    /// waiting for it.
    labels: Vec<(Option<u32>, Vec<usize>)>,
    /// The label of each `LetCont`'s continuation, for the `Goto`s to it.
    conts: HashMap<JoinId, Label>,
    /// One past the highest register the function names. A value that no
    /// local holds, like a tail expression's, gets a register from here.
    next: u32,
    /// Locals a constructor reuses, so their `Drop` hollows instead of frees.
    reused: BTreeSet<LocalId>,
    /// How many times each local is read.
    reads: HashMap<LocalId, u32>,
    /// The stdlib's own constructors, such as `Some` and `None`.
    abi: &'c Abi,
}

impl<'c> Loader<'c> {
    fn new(
        consts: &'c [Const],
        abi: &'c Abi,
        f: &CoreFn,
        this: Option<FuncIdx>,
    ) -> Result<Self, Unloadable> {
        let highest = f
            .params
            .iter()
            .map(|p| p.id.0)
            .chain(highest_local(&f.body))
            .max();
        let next = match highest {
            None => 0,
            Some(h) => h.checked_add(1).ok_or(Unloadable::TooManyRegisters)?,
        };
        Ok(Loader {
            consts,
            abi,
            this,
            reused: reused_locals(&f.body),
            reads: reads(&f.body),
            instrs: Vec::new(),
            labels: Vec::new(),
            conts: HashMap::new(),
            next,
        })
    }

    fn body(mut self, f: &CoreFn) -> Result<(u32, Vec<Instr>), Unloadable> {
        self.expr(&f.body, Dest::Return)?;
        moves_on_last_use(&mut self.instrs, self.next);
        Ok((self.next, self.instrs))
    }

    fn label(&mut self) -> Label {
        let l = Label(self.labels.len());
        self.labels.push((None, Vec::new()));
        l
    }

    /// Put `l` at the next instruction, and point every jump to it there.
    fn place(&mut self, l: Label) -> Result<(), Unloadable> {
        let here = u32::try_from(self.instrs.len()).map_err(|_| Unloadable::TooLong)?;
        let waiting = match self.labels.get_mut(l.0) {
            Some((at, waiting)) => {
                *at = Some(here);
                std::mem::take(waiting)
            }
            None => Vec::new(),
        };
        for i in waiting {
            if let Some(to) = self.instrs.get_mut(i).and_then(Instr::target) {
                *to = here;
            }
        }
        Ok(())
    }

    /// Push a jump to `l`, patched when `l` is placed if it is not yet.
    fn jump(&mut self, l: Label, make: impl FnOnce(u32) -> Instr) {
        let at = self.labels.get(l.0).and_then(|(at, _)| *at);
        let i = self.instrs.len();
        self.instrs.push(make(at.unwrap_or(0)));
        if at.is_none()
            && let Some((_, waiting)) = self.labels.get_mut(l.0)
        {
            waiting.push(i);
        }
    }

    /// The call instruction for `callee`, in tail position or not.
    fn call(&self, dst: Option<Reg>, callee: &Callee, args: &[LocalId]) -> Instr {
        let args: Box<[Arg]> = args
            .iter()
            .copied()
            .map(|a| Arg::shared(Reg::of(a)))
            .collect();
        match (callee, dst) {
            (Callee::Known(func), Some(dst)) => Instr::Call {
                dst,
                func: *func,
                args,
            },
            (Callee::Known(func), None) => Instr::TailCall { func: *func, args },
            (Callee::Self_, _) if self.this.is_none() => Instr::Bad {
                why: Broken::ToplevelCallsSelf,
            },
            (Callee::Self_, Some(dst)) => Instr::CallSelf { dst, args },
            (Callee::Self_, None) => Instr::TailCallSelf { args },
            (Callee::Local(f), Some(dst)) => Instr::CallValue {
                dst,
                callee: Reg::of(*f),
                args,
            },
            (Callee::Local(f), None) => Instr::TailCallValue {
                callee: Reg::of(*f),
                args,
            },
        }
    }

    /// `let k = <small Int>` read only by the `let` right after it, which does
    /// Int arithmetic or a comparison on it. The two load as one
    /// [`Instr::IntImm`], and what comes after them is returned too.
    fn immediate<'e>(
        &self,
        bind: &CoreBind,
        rhs: &Atom,
        body: &'e CoreExpr,
    ) -> Option<(Imm, &'e CoreExpr)> {
        let Atom::Const(c) = rhs else { return None };
        let Some(&Const::Int(n)) = self.consts.get(c.index()) else {
            return None;
        };
        let CoreExpr::Let {
            bind: next,
            rhs: Atom::PrimOp { op, args },
            body: rest,
        } = body
        else {
            return None;
        };
        if bind.global.is_some() || Value::int(n).is_none() || self.reads.get(&bind.id) != Some(&1)
        {
            return None;
        }
        let op = int_op(*op)?;
        let (a, op) = match args.as_slice() {
            [a, k] if *k == bind.id && *a != bind.id => (*a, op),
            [k, a] if *k == bind.id && *a != bind.id => (*a, op.swapped()?),
            _ => return None,
        };
        let instr = Instr::IntImm {
            dst: Reg::of(next.id),
            op,
            a: Reg::of(a),
            b: n,
        };
        Some((
            Imm {
                instr,
                global: next.global,
            },
            rest,
        ))
    }

    /// When the last instruction compares two Ints into `cond`, and the `if`
    /// on `cond` is all that reads it, take that instruction back, so the
    /// `if` can compare and jump in one. Nothing may jump to the `if` itself,
    /// or that jump would skip the comparison.
    fn take_comparison(&mut self, cond: LocalId) -> Option<Test> {
        let here = u32::try_from(self.instrs.len()).ok()?;
        if self.reads.get(&cond) != Some(&1) || self.labels.iter().any(|(at, _)| *at == Some(here))
        {
            return None;
        }
        let test = match self.instrs.last()? {
            Instr::Int { dst, op, a, b } if *dst == Reg::of(cond) && op.compares() => {
                Test::Regs(*op, *a, *b)
            }
            Instr::IntImm { dst, op, a, b } if *dst == Reg::of(cond) && op.compares() => {
                Test::Imm(*op, *a, *b)
            }
            _ => return None,
        };
        self.instrs.pop();
        Some(test)
    }

    fn scratch(&mut self) -> Result<Reg, Unloadable> {
        let r = Reg(self.next);
        self.next = self
            .next
            .checked_add(1)
            .ok_or(Unloadable::TooManyRegisters)?;
        Ok(r)
    }

    fn expr(&mut self, e: &CoreExpr, dest: Dest) -> Result<(), Unloadable> {
        let mut e = e;
        loop {
            match e {
                CoreExpr::Let { bind, rhs, body }
                    if let Some((imm, rest)) = self.immediate(bind, rhs, body) =>
                {
                    let dst = imm.dst();
                    self.instrs.push(imm.instr);
                    if let Some(slot) = imm.global {
                        self.instrs.push(Instr::SetGlobal { slot, src: dst });
                    }
                    e = rest;
                }
                // A map dropped right after it is set gives its reference to
                // the set.
                CoreExpr::Let {
                    bind,
                    rhs:
                        Atom::Intrinsic {
                            intrinsic: Intrinsic::MapSet,
                            args,
                        },
                    body,
                } if let [m, k, v] = args.as_slice()
                    && k != m
                    && v != m
                    && let CoreExpr::Drop {
                        local, body: rest, ..
                    } = &**body
                    && local == m
                    && !self.reused.contains(m) =>
                {
                    let dst = Reg::of(bind.id);
                    self.instrs.push(Instr::MapSetOwned {
                        dst,
                        map: Reg::of(*m),
                        key: Reg::of(*k),
                        value: Reg::of(*v),
                    });
                    if let Some(slot) = bind.global {
                        self.instrs.push(Instr::SetGlobal { slot, src: dst });
                    }
                    e = rest;
                }
                CoreExpr::Let {
                    bind,
                    rhs:
                        Atom::Intrinsic {
                            intrinsic: Intrinsic::MapGet,
                            args,
                        },
                    body,
                } if let [m, k] = args.as_slice()
                    && bind.global.is_none()
                    && self.reads.get(&bind.id) == Some(&1)
                    && let CoreExpr::Match { scrut, arms, .. } = &**body
                    && *scrut == bind.id
                    && let Some((value, found, missing)) = option_arms(self.abi, arms) =>
                {
                    let none = self.label();
                    let (map, key) = (Reg::of(*m), Reg::of(*k));
                    self.jump(none, |to| Instr::MapFind {
                        dst: Reg::of(value),
                        map,
                        key,
                        to,
                    });
                    self.expr(found, dest)?;
                    self.place(none)?;
                    e = missing;
                }
                CoreExpr::Let {
                    bind,
                    rhs:
                        Atom::Intrinsic {
                            intrinsic: Intrinsic::MapDelete,
                            args,
                        },
                    body,
                } if let [m, k] = args.as_slice()
                    && k != m
                    && let CoreExpr::Drop {
                        local, body: rest, ..
                    } = &**body
                    && local == m
                    && !self.reused.contains(m) =>
                {
                    let dst = Reg::of(bind.id);
                    self.instrs.push(Instr::MapDeleteOwned {
                        dst,
                        map: Reg::of(*m),
                        key: Reg::of(*k),
                    });
                    if let Some(slot) = bind.global {
                        self.instrs.push(Instr::SetGlobal { slot, src: dst });
                    }
                    e = rest;
                }
                CoreExpr::Let { bind, rhs, body } => {
                    let dst = Reg::of(bind.id);
                    self.atom(dst, rhs)?;
                    if let Some(slot) = bind.global {
                        self.instrs.push(Instr::SetGlobal { slot, src: dst });
                    }
                    e = body;
                }
                CoreExpr::Drop { local, body, .. } => {
                    let reg = Reg::of(*local);
                    self.instrs.push(if self.reused.contains(local) {
                        Instr::DropReuse { reg }
                    } else {
                        Instr::Drop { reg }
                    });
                    e = body;
                }
                CoreExpr::Tail(atom) => {
                    match (dest, atom) {
                        (Dest::Return, Atom::Call { callee, args }) => {
                            let call = self.call(None, callee, args);
                            self.instrs.push(call);
                        }
                        (Dest::Return, Atom::Local(local)) => {
                            self.instrs.push(Instr::Ret {
                                src: Reg::of(*local),
                            });
                        }
                        (Dest::Return, atom) => {
                            let src = self.scratch()?;
                            self.atom(src, atom)?;
                            self.instrs.push(Instr::Ret { src });
                        }
                        (Dest::Join { reg, after }, atom) => {
                            self.atom(reg, atom)?;
                            self.jump(after, |to| Instr::Jump { to });
                        }
                    }
                    return Ok(());
                }
                // The branch's value lands in `bind`, and every path through
                // it jumps to the code after it.
                CoreExpr::LetJoin { bind, join, body } => {
                    let after = self.label();
                    let reg = Reg::of(bind.id);
                    self.expr(join, Dest::Join { reg, after })?;
                    self.place(after)?;
                    if let Some(slot) = bind.global {
                        self.instrs.push(Instr::SetGlobal { slot, src: reg });
                    }
                    e = body;
                }
                CoreExpr::If {
                    cond, then, els, ..
                } => {
                    let otherwise = self.label();
                    match self.take_comparison(*cond) {
                        Some(Test::Regs(op, a, b)) => {
                            self.jump(otherwise, |to| Instr::JumpUnlessCmp { op, a, b, to });
                        }
                        Some(Test::Imm(op, a, b)) => {
                            self.jump(otherwise, |to| Instr::JumpUnlessCmpImm { op, a, b, to });
                        }
                        None => {
                            let cond = Reg::of(*cond);
                            self.jump(otherwise, |to| Instr::JumpIfFalse { cond, to });
                        }
                    }
                    self.expr(then, dest)?;
                    self.place(otherwise)?;
                    e = els;
                }
                CoreExpr::Match { scrut, arms, .. } => {
                    let src = Reg::of(*scrut);
                    for (pat, arm) in arms {
                        let next = self.label();
                        self.pattern(src, pat, next)?;
                        self.expr(arm, dest)?;
                        self.place(next)?;
                    }
                    self.instrs.push(Instr::Bad { why: Broken::NoArm });
                    return Ok(());
                }
                // Every path through `body` ends in a return or a jump, so
                // the continuation after it is reached only by its `Goto`s.
                CoreExpr::LetCont { id, cont, body } => {
                    let l = self.label();
                    self.conts.insert(*id, l);
                    self.expr(body, dest)?;
                    self.place(l)?;
                    e = cont;
                }
                CoreExpr::Goto(id) => {
                    match self.conts.get(id) {
                        Some(&l) => self.jump(l, |to| Instr::Jump { to }),
                        None => self.instrs.push(Instr::Bad {
                            why: Broken::UndeclaredCase,
                        }),
                    }
                    return Ok(());
                }
            }
        }
    }

    /// Test `src` against `pat`, jumping to `next` when it does not fit, and
    /// read out what the pattern binds when it does.
    fn pattern(&mut self, src: Reg, pat: &CorePat, next: Label) -> Result<(), Unloadable> {
        match pat {
            CorePat::Wild => {}
            CorePat::Bind(b) => self.instrs.push(Instr::Move {
                dst: Reg::of(b.id),
                src,
                moved: false,
            }),
            CorePat::Ctor { variant, fields } => {
                let variant = *variant;
                self.jump(next, |to| Instr::JumpUnlessVariant { src, variant, to });
                for (i, f) in fields.iter().enumerate() {
                    let index = u16::try_from(i).map_err(|_| Missing::TooManyFields)?;
                    self.instrs.push(Instr::Field {
                        dst: Reg::of(f.id),
                        src,
                        index,
                    });
                }
            }
            CorePat::Lit(c) => match self.consts.get(c.index()) {
                Some(Const::Int(n)) => {
                    let n = *n;
                    self.jump(next, |to| Instr::JumpUnlessInt { src, n, to });
                }
                Some(Const::String(text)) => {
                    let text: Box<[u8]> = text.as_bytes().into();
                    self.jump(next, |to| Instr::JumpUnlessStr { src, text, to });
                }
                Some(Const::Float(x)) => {
                    let x = *x;
                    self.jump(next, |to| Instr::JumpUnlessFloat { src, x, to });
                }
                Some(Const::Binary { bytes, bit_len }) => {
                    let (bytes, len): (Box<[u8]>, u64) = (bytes.as_slice().into(), *bit_len);
                    self.jump(next, |to| Instr::JumpUnlessBinary {
                        src,
                        bytes,
                        len,
                        to,
                    });
                }
                None => {
                    return Err(Missing::NoConst(c.index()).into());
                }
            },
        }
        Ok(())
    }

    fn atom(&mut self, dst: Reg, atom: &Atom) -> Result<(), Unloadable> {
        let instr = match atom {
            Atom::Local(src) => Instr::Move {
                dst,
                src: Reg::of(*src),
                moved: false,
            },
            Atom::Const(c) => match self.consts.get(c.index()) {
                Some(Const::String(text)) => Instr::Str {
                    dst,
                    text: text.as_bytes().into(),
                },
                Some(Const::Int(n)) if Value::int(*n).is_none() => {
                    Instr::BigInt { dst, n: *n, id: *c }
                }
                Some(Const::Binary { bytes, bit_len }) => Instr::BinaryConst {
                    dst,
                    bytes: bytes.as_slice().into(),
                    len: *bit_len,
                },
                _ => Instr::Const {
                    dst,
                    value: self.constant(c.index())?,
                },
            },
            Atom::Nil => Instr::Const {
                dst,
                value: Value::NIL,
            },
            Atom::Bool(b) => Instr::Const {
                dst,
                value: Value::bool(*b),
            },
            Atom::Load(Load::Global(slot)) => Instr::GetGlobal { dst, slot: *slot },
            Atom::Load(Load::Capture(i)) => match u32::try_from(i.0) {
                Ok(index) => Instr::GetCapture { dst, index },
                Err(_) => Instr::Bad {
                    why: Broken::NegativeCapture,
                },
            },
            Atom::Load(Load::SelfClosure) => match self.this {
                Some(this) => Instr::GetSelf { dst, this },
                None => Instr::Bad {
                    why: Broken::ToplevelSelf,
                },
            },
            Atom::Closure { func_idx, captures } if captures.is_empty() => Instr::Const {
                dst,
                value: Value::func(*func_idx),
            },
            Atom::Closure { func_idx, captures } => Instr::Closure {
                dst,
                func: *func_idx,
                captures: captures.iter().copied().map(Reg::of).collect(),
            },
            Atom::Call { callee, args } => self.call(Some(dst), callee, args),
            Atom::PrimOp { op, args } => self.prim(dst, *op, args)?,
            Atom::Intrinsic {
                intrinsic: intrinsic @ (Intrinsic::Print | Intrinsic::Println),
                args,
            } => match args.as_slice() {
                [arg] => Instr::Print {
                    dst,
                    arg: Reg::of(*arg),
                    newline: *intrinsic == Intrinsic::Println,
                },
                _ => return Err(Missing::PrintArity(*intrinsic).into()),
            },
            // `array::put` gives the array back as it was when it has no
            // such index.
            Atom::Intrinsic {
                intrinsic: Intrinsic::ArrayPut,
                args,
            } if let [xs, i, x] = args.as_slice() => {
                let (miss, after) = (self.label(), self.label());
                self.array_set(dst, [*xs, *i, *x], miss);
                self.jump(after, |to| Instr::Jump { to });
                self.place(miss)?;
                self.instrs.push(Instr::Move {
                    dst,
                    src: Reg::of(*xs),
                    moved: false,
                });
                return self.place(after);
            }
            // `array::set` says which it was: `Ok` of the new array, or
            // `Err(Nil)`.
            Atom::Intrinsic {
                intrinsic: Intrinsic::ArraySet,
                args,
            } if let [xs, i, x] = args.as_slice() => {
                let (miss, after) = (self.label(), self.label());
                let (made, nil) = (self.scratch()?, self.scratch()?);
                self.array_set(made, [*xs, *i, *x], miss);
                self.instrs.push(Instr::Ctor {
                    dst,
                    variant: self.abi.ok,
                    fields: Box::new([Arg::shared(made)]),
                    reuse: None,
                });
                self.jump(after, |to| Instr::Jump { to });
                self.place(miss)?;
                self.instrs.push(Instr::Const {
                    dst: nil,
                    value: Value::NIL,
                });
                self.instrs.push(Instr::Ctor {
                    dst,
                    variant: self.abi.err,
                    fields: Box::new([Arg::shared(nil)]),
                    reuse: None,
                });
                return self.place(after);
            }
            Atom::Intrinsic { intrinsic, args } if built(*intrinsic, args.len()) => {
                Instr::Builtin {
                    dst,
                    intrinsic: *intrinsic,
                    args: args.iter().copied().map(Reg::of).collect(),
                }
            }
            Atom::Intrinsic { intrinsic, .. } => return Err(Missing::Intrinsic(*intrinsic).into()),
            // A constructor with no fields is a value word, so there is no
            // allocation for Perceus to reuse.
            Atom::Ctor {
                variant, fields, ..
            } if fields.is_empty() => Instr::Const {
                dst,
                value: Value::nullary(*variant),
            },
            Atom::Ctor {
                variant,
                fields,
                reuse,
            } => Instr::Ctor {
                dst,
                variant: *variant,
                fields: fields
                    .iter()
                    .copied()
                    .map(|l| Arg::shared(Reg::of(l)))
                    .collect(),
                reuse: reuse.map(Reg::of),
            },
        };
        self.instrs.push(instr);
        Ok(())
    }

    /// The write that `array::put` and `array::set` both are: `xs` with `x`
    /// at index `i` goes to `dst`, and an `i` that `xs` does not have
    /// continues at `miss`.
    fn array_set(&mut self, dst: Reg, [xs, i, x]: [LocalId; 3], miss: Label) {
        let (src, index, value) = (Reg::of(xs), Reg::of(i), Arg::shared(Reg::of(x)));
        self.jump(miss, |to| Instr::ArraySet {
            dst,
            src,
            index,
            value,
            moved: false,
            miss: to,
        });
    }

    fn prim(&mut self, dst: Reg, op: PrimOp, args: &[LocalId]) -> Result<Instr, Missing> {
        let prim = op;
        let arity = |want| Missing::Arity { op: prim, want };
        let int = |op| -> Result<Instr, Missing> {
            match args {
                [a, b] => Ok(Instr::Int {
                    dst,
                    op,
                    a: Reg::of(*a),
                    b: Reg::of(*b),
                }),
                _ => Err(arity(Arity::Two)),
            }
        };
        let bits = |op, want: Arity| -> Result<Instr, Missing> {
            if !want.fits(args.len()) {
                return Err(arity(want));
            }
            Ok(Instr::Bits {
                dst,
                op,
                args: args.iter().copied().map(Reg::of).collect(),
            })
        };
        // A Float operation (`float`), or one on either kind of number.
        let two = |op, float| -> Result<Instr, Missing> {
            match args {
                [a, b] => {
                    let (a, b) = (Reg::of(*a), Reg::of(*b));
                    Ok(if float {
                        Instr::Float { dst, op, a, b }
                    } else {
                        Instr::Num { dst, op, a, b }
                    })
                }
                _ => Err(arity(Arity::Two)),
            }
        };
        match op {
            PrimOp::IntAdd => int(IntOp::Add),
            PrimOp::IntSub => int(IntOp::Sub),
            PrimOp::IntMul => int(IntOp::Mul),
            PrimOp::IntDiv => int(IntOp::Div),
            PrimOp::IntRem => int(IntOp::Rem),
            PrimOp::IntEq => int(IntOp::Eq),
            PrimOp::IntNe => int(IntOp::Ne),
            PrimOp::IntLt => int(IntOp::Lt),
            PrimOp::IntLe => int(IntOp::Le),
            PrimOp::IntGt => int(IntOp::Gt),
            PrimOp::IntGe => int(IntOp::Ge),
            PrimOp::FloatAdd => two(NumOp::Add, true),
            PrimOp::FloatSub => two(NumOp::Sub, true),
            PrimOp::FloatMul => two(NumOp::Mul, true),
            PrimOp::FloatDiv => two(NumOp::Div, true),
            PrimOp::FloatLt => two(NumOp::Lt, true),
            PrimOp::FloatLe => two(NumOp::Le, true),
            PrimOp::FloatGt => two(NumOp::Gt, true),
            PrimOp::FloatGe => two(NumOp::Ge, true),
            PrimOp::Add => two(NumOp::Add, false),
            PrimOp::Sub => two(NumOp::Sub, false),
            PrimOp::Mul => two(NumOp::Mul, false),
            PrimOp::Div => two(NumOp::Div, false),
            PrimOp::Rem => two(NumOp::Rem, false),
            PrimOp::Lt => two(NumOp::Lt, false),
            PrimOp::Le => two(NumOp::Le, false),
            PrimOp::Gt => two(NumOp::Gt, false),
            PrimOp::Ge => two(NumOp::Ge, false),
            PrimOp::FloatNeg | PrimOp::Neg => match args {
                [a] => {
                    let a = Reg::of(*a);
                    Ok(if op == PrimOp::FloatNeg {
                        Instr::FloatNeg { dst, a }
                    } else {
                        Instr::Neg { dst, a }
                    })
                }
                _ => Err(arity(Arity::One)),
            },
            PrimOp::ToString => match args {
                [a] => Ok(Instr::ToString {
                    dst,
                    a: Reg::of(*a),
                }),
                _ => Err(arity(Arity::One)),
            },
            PrimOp::StringConcat | PrimOp::StringConcatMany => Ok(Instr::Concat {
                dst,
                parts: args.iter().copied().map(Reg::of).collect(),
            }),
            PrimOp::IntNeg => match args {
                [a] => Ok(Instr::IntNeg {
                    dst,
                    a: Reg::of(*a),
                }),
                _ => Err(arity(Arity::One)),
            },
            PrimOp::Eq | PrimOp::Ne => match args {
                [a, b] => {
                    let (a, b) = (Reg::of(*a), Reg::of(*b));
                    Ok(if op == PrimOp::Eq {
                        Instr::Equal { dst, a, b }
                    } else {
                        Instr::NotEqual { dst, a, b }
                    })
                }
                _ => Err(arity(Arity::Two)),
            },
            PrimOp::Not => match args {
                [a] => Ok(Instr::Not {
                    dst,
                    a: Reg::of(*a),
                }),
                _ => Err(arity(Arity::One)),
            },
            PrimOp::BinaryFromInt => bits(BitsOp::FromInt, Arity::Two),
            PrimOp::BinaryTake => bits(BitsOp::Take, Arity::Two),
            PrimOp::BinaryFromString => bits(BitsOp::FromString, Arity::One),
            PrimOp::BinaryConcatMany => Ok(Instr::Bits {
                dst,
                op: BitsOp::Concat,
                args: args.iter().copied().map(Reg::of).collect(),
            }),
            PrimOp::BinaryBitSize => bits(BitsOp::BitSize, Arity::One),
            PrimOp::BinaryView => bits(BitsOp::View, Arity::Three),
            PrimOp::BinaryMatchPrefix => bits(BitsOp::MatchPrefix, Arity::Three),
            PrimOp::BinaryReadUtf8 => bits(BitsOp::ReadUtf8, Arity::Two),
            PrimOp::BinaryReadInt => bits(BitsOp::ReadInt, Arity::Three),
            PrimOp::MakeRange => match args {
                [start, end] => Ok(Instr::Range {
                    dst,
                    start: Reg::of(*start),
                    end: Reg::of(*end),
                }),
                _ => Err(arity(Arity::Two)),
            },
            PrimOp::MakeArray => Ok(Instr::Array {
                dst,
                elements: args.iter().copied().map(Reg::of).collect(),
            }),
            PrimOp::ArrayLen => match args {
                [src] => Ok(Instr::ArrayLen {
                    dst,
                    src: Reg::of(*src),
                }),
                _ => Err(arity(Arity::One)),
            },
            PrimOp::ArrayElem(index) => match args {
                [src] => Ok(Instr::ArrayElem {
                    dst,
                    src: Reg::of(*src),
                    index,
                }),
                _ => Err(arity(Arity::One)),
            },
            PrimOp::ArrayDrop => match args {
                [src, n] => Ok(Instr::ArrayDrop {
                    dst,
                    src: Reg::of(*src),
                    n: Reg::of(*n),
                    moved: false,
                }),
                _ => Err(arity(Arity::Two)),
            },
            PrimOp::ArrayPrepend => match args.split_last() {
                Some((rest, front)) => Ok(Instr::ArrayPrepend {
                    dst,
                    front: front.iter().copied().map(Reg::of).collect(),
                    rest: Reg::of(*rest),
                    moved: false,
                }),
                None => Err(arity(Arity::AtLeastOne)),
            },
            PrimOp::ArrayAppend => match args.split_first() {
                Some((rest, back)) => Ok(Instr::ArrayAppend {
                    dst,
                    rest: Reg::of(*rest),
                    back: back.iter().copied().map(Reg::of).collect(),
                    moved: false,
                }),
                None => Err(arity(Arity::AtLeastOne)),
            },
            PrimOp::ArrayConcat => match args {
                [a, b] => Ok(Instr::ArrayConcat {
                    dst,
                    a: Reg::of(*a),
                    b: Reg::of(*b),
                }),
                _ => Err(arity(Arity::Two)),
            },
            PrimOp::ArraySlice => match args {
                [src, start, end] => Ok(Instr::ArraySlice {
                    dst,
                    src: Reg::of(*src),
                    start: Reg::of(*start),
                    end: Reg::of(*end),
                }),
                _ => Err(arity(Arity::Three)),
            },
            PrimOp::ArrayIndex => match args {
                [src, index] => Ok(Instr::ArrayIndex {
                    dst,
                    src: Reg::of(*src),
                    index: Reg::of(*index),
                }),
                _ => Err(arity(Arity::Two)),
            },
            PrimOp::ArrayIndexOr => match args {
                [src, index, default] => Ok(Instr::ArrayIndexOr {
                    dst,
                    src: Reg::of(*src),
                    index: Reg::of(*index),
                    default: Reg::of(*default),
                }),
                _ => Err(arity(Arity::Three)),
            },
            PrimOp::MakeTuple => Ok(Instr::Tuple {
                dst,
                elements: args.iter().copied().map(Reg::of).collect(),
            }),
            PrimOp::TupleField(index) => match args {
                [src] => Ok(Instr::Element {
                    dst,
                    src: Reg::of(*src),
                    index,
                }),
                _ => Err(arity(Arity::One)),
            },
            PrimOp::Field(index) => match args {
                [src] => Ok(Instr::Field {
                    dst,
                    src: Reg::of(*src),
                    index,
                }),
                _ => Err(arity(Arity::One)),
            },
        }
    }

    fn constant(&self, i: usize) -> Result<Value, Missing> {
        match self.consts.get(i) {
            Some(Const::Int(n)) => Value::int(*n).ok_or(Missing::BigConst(*n)),
            Some(Const::Float(f)) => Ok(Value::float(*f)),
            Some(Const::String(_)) => Err(Missing::StringConst),
            Some(Const::Binary { .. }) => Err(Missing::BinaryConst),
            None => Err(Missing::NoConst(i)),
        }
    }
}

/// Whether the VM runs built-in `i` when called with `argc` arguments. A
/// function calling any other still loads, and stops only when it runs.
fn built(i: Intrinsic, argc: usize) -> bool {
    let arity = match i {
        Intrinsic::MapNew
        | Intrinsic::InternalStackDepth
        | Intrinsic::InternalCellsMade
        | Intrinsic::InternalCellsReused
        | Intrinsic::TimeMonotonic
        | Intrinsic::TimeEpochMs
        | Intrinsic::OsArgv
        | Intrinsic::OsEnv
        | Intrinsic::OsTerminalSize => 0,
        Intrinsic::CryptoRandomBytes
        | Intrinsic::ProcessSleep
        | Intrinsic::IoReadFile
        | Intrinsic::CryptoSha1
        | Intrinsic::CryptoSha256
        | Intrinsic::CryptoSha512 => 1,
        Intrinsic::JsonParseBinary
        | Intrinsic::JsonKind
        | Intrinsic::JsonLen
        | Intrinsic::JsonEntries
        | Intrinsic::JsonElements
        | Intrinsic::JsonString
        | Intrinsic::JsonInt
        | Intrinsic::JsonIntText
        | Intrinsic::JsonFloat
        | Intrinsic::JsonBool
        | Intrinsic::JsonEncode => 1,
        Intrinsic::JsonField | Intrinsic::JsonIndex => 2,
        Intrinsic::HttpFraming | Intrinsic::HttpHeadersValid => 1,
        Intrinsic::HttpParseHead
        | Intrinsic::HttpParseResponseHead
        | Intrinsic::HttpHeaderGet
        | Intrinsic::HttpHeaderHas => 2,
        Intrinsic::HttpChunkDecode | Intrinsic::HttpSerializeHead => 3,
        Intrinsic::IoWriteFile | Intrinsic::CryptoHmacSha256 | Intrinsic::CryptoConstEq => 2,
        Intrinsic::CryptoP256Verify | Intrinsic::CryptoEd25519Verify => 3,
        Intrinsic::MapKeys | Intrinsic::MapValues | Intrinsic::MapSize | Intrinsic::MapToList => 1,
        Intrinsic::MapGet | Intrinsic::MapHas | Intrinsic::MapDelete => 2,
        Intrinsic::MapSet => 3,
        Intrinsic::StringInspect
        | Intrinsic::StringLength
        | Intrinsic::ArrayLength
        | Intrinsic::IntToString
        | Intrinsic::FloatFloor
        | Intrinsic::FloatCeil
        | Intrinsic::FloatRound
        | Intrinsic::FloatTruncate
        | Intrinsic::FloatFromInt
        | Intrinsic::FloatToString
        | Intrinsic::BinaryFromString
        | Intrinsic::BinaryToString
        | Intrinsic::BinaryBitSize
        | Intrinsic::BinaryByteSize => 1,
        Intrinsic::BinaryToAsciiLower
        | Intrinsic::StringTrim
        | Intrinsic::StringToGraphemes
        | Intrinsic::IntFromString
        | Intrinsic::IntBitwiseNot => 1,
        Intrinsic::StringSplit
        | Intrinsic::FloatFromDecimal
        | Intrinsic::StringContains
        | Intrinsic::IntBitwiseAnd
        | Intrinsic::IntBitwiseOr
        | Intrinsic::IntBitwiseXor
        | Intrinsic::IntBitwiseShiftLeft
        | Intrinsic::IntBitwiseShiftRight => 2,
        Intrinsic::BinaryAppend
        | Intrinsic::BinaryByteAt
        | Intrinsic::BinaryParseInt
        | Intrinsic::BinaryEqIgnoreAsciiCase
        | Intrinsic::BinaryFromIntAscii => 2,
        Intrinsic::BinarySliceBits | Intrinsic::BinaryIndexOf => 3,
        Intrinsic::AddressParse
        | Intrinsic::NetResolve
        | Intrinsic::NetListen
        | Intrinsic::NetAccept
        | Intrinsic::NetConnect
        | Intrinsic::NetLocalAddr
        | Intrinsic::NetClose
        | Intrinsic::SocketClose
        | Intrinsic::TlsClose => 1,
        Intrinsic::NetResolveUntil
        | Intrinsic::NetConnectUntil
        | Intrinsic::SocketRead
        | Intrinsic::SocketWrite
        | Intrinsic::SocketWriteParts
        | Intrinsic::TlsHandshake
        | Intrinsic::TlsRead
        | Intrinsic::TlsWrite => 2,
        Intrinsic::SocketReadUntil | Intrinsic::TlsHandshakeUntil | Intrinsic::TlsReadUntil => 3,
        _ => return false,
    };
    argc == arity
}

/// A call hands the callee the reference an argument's register holds,
/// instead of adding one, when nothing the frame runs later reads that
/// register. A constructor takes its fields' references the same way, an
/// array push or drop takes its array's, a write at an index takes its
/// array's and its value's, and a `Move` takes its source's.
/// Whatever receives the value then holds the reference the register held,
/// so a value nothing else holds still has a count of 1, which is what lets
/// it be changed in place or its cell overwritten.
///
/// Which registers are read later comes from liveness over the function's
/// instructions, jumps and all. A plain `Drop` does not count as a read: by
/// the time it runs on a register an instruction emptied, it does nothing,
/// and on a path where that instruction never ran it still finds the value
/// and releases it. A `DropReuse` does count, so a cell this frame has
/// already paired with a constructor of its own stays here.
fn moves_on_last_use(instrs: &mut [Instr], regs: u32) {
    let (before, live) = liveness(instrs, regs, false);
    for (i, (instr, after)) in instrs.iter_mut().zip(&live).enumerate() {
        // A write at an index takes its array and its value only when it
        // does not miss, and then the next instruction runs. So what counts
        // is what is read from there on, and a read on the way from a miss
        // keeps neither register. The array is read before the value is
        // taken, so the value may name its register too, and then only the
        // value takes it.
        if let Instr::ArraySet {
            src,
            index,
            value,
            moved,
            ..
        } = instr
        {
            if let Some(hit) = before.get(i.saturating_add(1)) {
                *moved = !hit.has(*src) && value.reg != *src && index != src;
                value.moved = !hit.has(value.reg);
            }
            continue;
        }
        // A push whose array is read by nothing later takes it over. Its own
        // items are read before the array is, so they may name it too.
        if let Instr::ArrayPrepend {
            front: items,
            rest,
            moved,
            ..
        }
        | Instr::ArrayAppend {
            back: items,
            rest,
            moved,
            ..
        } = instr
        {
            *moved = !after.has(*rest) && !items.contains(rest);
            continue;
        }
        if let Instr::ArrayDrop { src, n, moved, .. } = instr {
            *moved = !after.has(*src) && n != src;
            continue;
        }
        if let Instr::Move { src, moved, .. } = instr {
            *moved = !after.has(*src);
            continue;
        }
        let args = match instr {
            Instr::Ctor { fields: args, .. }
            | Instr::Call { args, .. }
            | Instr::CallSelf { args, .. }
            | Instr::CallValue { args, .. }
            | Instr::TailCall { args, .. }
            | Instr::TailCallSelf { args }
            | Instr::TailCallValue { args, .. } => args,
            _ => continue,
        };
        // Only the last position holding a register may take what it holds,
        // because an earlier one still reads it.
        for k in (0..args.len()).rev() {
            let Some(reg) = args.get(k).map(|a| a.reg) else {
                continue;
            };
            let later = args.get(k.saturating_add(1)..).unwrap_or_default();
            if after.has(reg) || later.iter().any(|a| a.reg == reg) {
                continue;
            }
            if let Some(a) = args.get_mut(k) {
                a.moved = true;
            }
        }
    }
}

/// `instrs` with an [`Instr::Count`] put in at the head of each basic block,
/// and every jump moved to follow. The kinds of each block's instructions
/// are added to `blocks`, at the number its `Count` holds.
///
/// A block starts at the function's first instruction, at every instruction
/// a jump names, and after every instruction that may continue anywhere but
/// at the next one. So an instruction runs only when its block's head has,
/// and when it has run, the rest of the block does, unless the run stops.
fn count_blocks(instrs: Vec<Instr>, blocks: &mut Vec<Box<[Op]>>) -> Result<Vec<Instr>, Unloadable> {
    let mut heads = vec![false; instrs.len()];
    let mut head = |i: usize| {
        if let Some(h) = heads.get_mut(i) {
            *h = true;
        }
    };
    head(0);
    for (i, instr) in instrs.iter().enumerate() {
        let next = i.saturating_add(1);
        let mut falls_through = false;
        let mut leaves = false;
        for to in instr.successors(i) {
            if to == next {
                falls_through = true;
            } else {
                leaves = true;
                head(to);
            }
        }
        if leaves || !falls_through {
            head(next);
        }
    }
    let mut block: Vec<Op> = Vec::new();
    let close = |block: &mut Vec<Op>, blocks: &mut Vec<Box<[Op]>>| {
        if !block.is_empty() {
            blocks.push(std::mem::take(block).into_boxed_slice());
        }
    };
    let mut heads = heads.into_iter();
    let out = put_before(instrs, |instr| {
        let count = if heads.next().unwrap_or(false) {
            close(&mut block, blocks);
            let number = u32::try_from(blocks.len()).map_err(|_| Unloadable::TooLong)?;
            Some(Instr::Count { block: number })
        } else {
            None
        };
        block.push(instr.op());
        Ok(count)
    })?;
    close(&mut block, blocks);
    Ok(out)
}

/// `instrs` with an [`Instr::Audit`] put in front of each.
fn audit_each(instrs: Vec<Instr>) -> Result<Vec<Instr>, Unloadable> {
    put_before(instrs, |_| Ok(Some(Instr::Audit)))
}

/// `instrs` with what `extra` gives for each put in front of it, and every
/// jump moved to follow. A jump to an instruction goes to what was put in
/// front of it.
fn put_before(
    instrs: Vec<Instr>,
    mut extra: impl FnMut(&Instr) -> Result<Option<Instr>, Unloadable>,
) -> Result<Vec<Instr>, Unloadable> {
    let mut lands = Vec::with_capacity(instrs.len());
    let mut out = Vec::with_capacity(instrs.len());
    for instr in instrs {
        lands.push(u32::try_from(out.len()).map_err(|_| Unloadable::TooLong)?);
        out.extend(extra(&instr)?);
        out.push(instr);
    }
    u32::try_from(out.len()).map_err(|_| Unloadable::TooLong)?;
    for instr in &mut out {
        if let Some(to) = instr.target()
            && let Some(moved) = lands.get(*to as usize)
        {
            *to = *moved;
        }
    }
    Ok(out)
}

/// Give the function's values registers, two values sharing one whenever
/// they are never live at once, and say how many registers that takes.
/// Every frame clears its registers when it returns and every loop when it
/// goes round, so fewer of them is less work at each.
///
/// Sharing a register changes when a value is released, never whether: the
/// value a register held is released when the next one is written over it,
/// instead of when the frame ends, and by then nothing reads it. A `Drop`
/// names its register, so a value keeps it until its `Drop`. The registers
/// Perceus parks a cell in for a constructor to reuse survive a loop going
/// round, so none of them is shared.
fn allocate(instrs: &mut [Instr], params: &mut [Reg], carried: &mut [Reg], regs: u32) -> u32 {
    let (before, after) = liveness(instrs, regs, true);
    let n = regs as usize;
    let mut clash = vec![Regs::new(regs); n];
    let edge = |clash: &mut Vec<Regs>, a: Reg, b: Reg| {
        if a != b {
            if let Some(x) = clash.get_mut(a.0 as usize) {
                x.add(b);
            }
            if let Some(y) = clash.get_mut(b.0 as usize) {
                y.add(a);
            }
        }
    };
    for (instr, out) in instrs.iter().zip(&after) {
        if let Some(w) = instr.writes() {
            for r in out.iter() {
                edge(&mut clash, w.reg(), r);
            }
        }
    }
    // The parameters are all written on entry, with whatever is live there.
    let entry = before.first().cloned().unwrap_or_else(|| Regs::new(regs));
    for &p in params.iter() {
        for &q in params.iter() {
            edge(&mut clash, p, q);
        }
        for r in entry.iter() {
            edge(&mut clash, p, r);
        }
    }
    let mut used = BTreeSet::new();
    let mut note = |r: &mut Reg| {
        used.insert(r.0);
    };
    params.iter_mut().for_each(&mut note);
    carried.iter_mut().for_each(&mut note);
    instrs.iter_mut().for_each(|i| i.regs_mut(&mut note));
    let mut color: HashMap<u32, u32> = HashMap::new();
    let mut count = 0u32;
    // A parked cell's register is its own, and no other value takes it.
    for r in carried.iter() {
        if let std::collections::hash_map::Entry::Vacant(e) = color.entry(r.0) {
            e.insert(count);
            count = count.saturating_add(1);
        }
    }
    let reserved = count;
    let order = params.iter().map(|p| p.0).chain(used.iter().copied());
    for r in order.collect::<Vec<_>>() {
        if color.contains_key(&r) {
            continue;
        }
        let taken: BTreeSet<u32> = clash
            .get(r as usize)
            .into_iter()
            .flat_map(Regs::iter)
            .filter_map(|q| color.get(&q.0).copied())
            .collect();
        let c = (reserved..).find(|c| !taken.contains(c)).unwrap_or(count);
        color.insert(r, c);
        count = count.max(c.saturating_add(1));
    }
    let mut map = |r: &mut Reg| {
        if let Some(&c) = color.get(&r.0) {
            r.0 = c;
        }
    };
    params.iter_mut().for_each(&mut map);
    carried.iter_mut().for_each(&mut map);
    instrs.iter_mut().for_each(|i| i.regs_mut(&mut map));
    count
}

/// A set of registers, one bit each.
#[derive(Clone, PartialEq, Eq)]
struct Regs(Vec<u64>);

impl Regs {
    fn new(regs: u32) -> Regs {
        Regs(vec![0; (regs as usize).div_ceil(64)])
    }

    fn has(&self, r: Reg) -> bool {
        let r = r.0 as usize;
        self.0.get(r / 64).is_some_and(|w| w >> (r % 64) & 1 == 1)
    }

    fn add(&mut self, r: Reg) {
        let r = r.0 as usize;
        if let Some(w) = self.0.get_mut(r / 64) {
            *w |= 1 << (r % 64);
        }
    }

    fn remove(&mut self, r: Reg) {
        let r = r.0 as usize;
        if let Some(w) = self.0.get_mut(r / 64) {
            *w &= !(1 << (r % 64));
        }
    }

    fn iter(&self) -> impl Iterator<Item = Reg> + '_ {
        self.0.iter().zip(0u32..).flat_map(|(&w, i)| {
            (0..64u32)
                .filter(move |b| w >> b & 1 == 1)
                .map(move |b| Reg(i.saturating_mul(64).saturating_add(b)))
        })
    }

    fn union(&mut self, other: &Regs) {
        for (w, o) in self.0.iter_mut().zip(&other.0) {
            *w |= o;
        }
    }
}

/// The registers live before and after each instruction: the ones some
/// later instruction may read before anything writes them again. With
/// `drops_read`, a plain `Drop` counts as a read, so a register stays its
/// value's until the `Drop` that names it.
fn liveness(instrs: &[Instr], regs: u32, drops_read: bool) -> (Vec<Regs>, Vec<Regs>) {
    let empty = Regs::new(regs);
    let mut before = vec![empty.clone(); instrs.len()];
    let mut after = vec![empty.clone(); instrs.len()];
    // Backward liveness, run to a fixed point. Loops only come from jumps
    // back, which are rare, so this settles in a pass or two.
    loop {
        let mut changed = false;
        for i in (0..instrs.len()).rev() {
            let Some(instr) = instrs.get(i) else { continue };
            let mut out = empty.clone();
            for next in instr.successors(i) {
                if let Some(b) = before.get(next) {
                    out.union(b);
                }
            }
            let mut live = out.clone();
            match instr.writes() {
                Some(Write::Always(reg)) => live.remove(reg),
                // A miss writes nothing, so a register live where it lands is
                // live here too, even the one a hit writes.
                Some(Write::OnHit { reg, miss }) => {
                    live.remove(reg);
                    if let Some(b) = before.get(miss as usize) {
                        live.union(b);
                    }
                }
                None => {}
            }
            instr.reads(|r| live.add(r));
            if drops_read && let Instr::Drop { reg } = instr {
                live.add(*reg);
            }
            if let Some(slot) = after.get_mut(i) {
                *slot = out;
            }
            if let Some(slot) = before.get_mut(i)
                && *slot != live
            {
                *slot = live;
                changed = true;
            }
        }
        if !changed {
            return (before, after);
        }
    }
}

/// Every local a constructor in `e` overwrites in place. `Atom`'s operand walk
/// leaves `reuse` out on purpose, because it names a cell to write over, not
/// a value read. So this reads the atoms itself.
fn reused_locals(e: &CoreExpr) -> BTreeSet<LocalId> {
    let mut out = BTreeSet::new();
    let mut note = |a: &Atom| {
        if let Atom::Ctor {
            reuse: Some(local), ..
        } = a
        {
            out.insert(*local);
        }
    };
    let mut stack = vec![e];
    while let Some(e) = stack.pop() {
        match e {
            CoreExpr::Let { rhs, body, .. } => {
                note(rhs);
                stack.push(body);
            }
            CoreExpr::LetJoin { join, body, .. } => {
                stack.push(join);
                stack.push(body);
            }
            CoreExpr::LetCont { cont, body, .. } => {
                stack.push(cont);
                stack.push(body);
            }
            CoreExpr::Drop { body, .. } => stack.push(body),
            CoreExpr::Match { arms, .. } => stack.extend(arms.iter().map(|(_, arm)| arm)),
            CoreExpr::If { then, els, .. } => {
                stack.push(then);
                stack.push(els);
            }
            CoreExpr::Tail(atom) => note(atom),
            CoreExpr::Goto(_) => {}
        }
    }
    out
}

/// The arms of a `match` on an `Option` that names both variants and binds
/// `Some`'s value to a plain name: that name, the `Some` arm and the `None`
/// arm.
fn option_arms<'e>(
    abi: &Abi,
    arms: &'e [(CorePat, CoreExpr)],
) -> Option<(LocalId, &'e CoreExpr, &'e CoreExpr)> {
    let [(a, x), (b, y)] = arms else {
        return None;
    };
    let some = |p: &CorePat| match p {
        CorePat::Ctor { variant, fields } if *variant == abi.some => match fields.as_slice() {
            [v] if v.global.is_none() => Some(v.id),
            _ => None,
        },
        CorePat::Wild | CorePat::Bind(_) | CorePat::Lit(_) | CorePat::Ctor { .. } => None,
    };
    let none = |p: &CorePat| matches!(p, CorePat::Ctor { variant, fields } if *variant == abi.none && fields.is_empty());
    match (some(a), some(b)) {
        (Some(v), _) if none(b) => Some((v, x, y)),
        (_, Some(v)) if none(a) => Some((v, y, x)),
        _ => None,
    }
}

/// A comparison an `if` jumps on: two registers, or one and a constant.
enum Test {
    Regs(IntOp, Reg, Reg),
    Imm(IntOp, Reg, i64),
}

/// An [`Instr::IntImm`], and the global its result is stored in, if any.
struct Imm {
    instr: Instr,
    global: Option<GlobalSlot>,
}

impl Imm {
    fn dst(&self) -> Reg {
        match self.instr {
            Instr::IntImm { dst, .. } => dst,
            _ => Reg(0),
        }
    }
}

/// The [`IntOp`] an Int primitive is, or `None` for any other primitive.
fn int_op(op: PrimOp) -> Option<IntOp> {
    Some(match op {
        PrimOp::IntAdd => IntOp::Add,
        PrimOp::IntSub => IntOp::Sub,
        PrimOp::IntMul => IntOp::Mul,
        PrimOp::IntDiv => IntOp::Div,
        PrimOp::IntRem => IntOp::Rem,
        PrimOp::IntEq => IntOp::Eq,
        PrimOp::IntNe => IntOp::Ne,
        PrimOp::IntLt => IntOp::Lt,
        PrimOp::IntLe => IntOp::Le,
        PrimOp::IntGt => IntOp::Gt,
        PrimOp::IntGe => IntOp::Ge,
        _ => return None,
    })
}

/// How many times `e` reads each local.
fn reads(e: &CoreExpr) -> HashMap<LocalId, u32> {
    let mut out: HashMap<LocalId, u32> = HashMap::new();
    let mut note = |l: LocalId| {
        let n = out.entry(l).or_default();
        *n = n.saturating_add(1);
    };
    let mut stack = vec![e];
    while let Some(e) = stack.pop() {
        match e {
            CoreExpr::Let { rhs, body, .. } => {
                rhs.for_each_operand(&mut note);
                stack.push(body);
            }
            CoreExpr::LetJoin { join, body, .. } => {
                stack.push(join);
                stack.push(body);
            }
            CoreExpr::LetCont { cont, body, .. } => {
                stack.push(cont);
                stack.push(body);
            }
            CoreExpr::Drop { local, body, .. } => {
                note(*local);
                stack.push(body);
            }
            CoreExpr::Match { scrut, arms, .. } => {
                note(*scrut);
                stack.extend(arms.iter().map(|(_, arm)| arm));
            }
            CoreExpr::If {
                cond, then, els, ..
            } => {
                note(*cond);
                stack.push(then);
                stack.push(els);
            }
            CoreExpr::Tail(atom) => atom.for_each_operand(&mut note),
            CoreExpr::Goto(_) => {}
        }
    }
    out
}

/// The highest local `e` binds or reads, or `None` when it names none.
fn highest_local(e: &CoreExpr) -> Option<u32> {
    let mut top = None;
    let mut note = |l: LocalId| top = top.max(Some(l.0));
    let mut stack = vec![e];
    while let Some(e) = stack.pop() {
        match e {
            CoreExpr::Let { bind, rhs, body } => {
                note(bind.id);
                rhs.for_each_operand(&mut note);
                stack.push(body);
            }
            CoreExpr::LetJoin { bind, join, body } => {
                note(bind.id);
                stack.push(join);
                stack.push(body);
            }
            CoreExpr::LetCont { cont, body, .. } => {
                stack.push(cont);
                stack.push(body);
            }
            CoreExpr::Drop { local, body, .. } => {
                note(*local);
                stack.push(body);
            }
            CoreExpr::Match { scrut, arms, .. } => {
                note(*scrut);
                for (pat, arm) in arms {
                    pat.binds().for_each(|b| note(b.id));
                    stack.push(arm);
                }
            }
            CoreExpr::If {
                cond, then, els, ..
            } => {
                note(*cond);
                stack.push(then);
                stack.push(els);
            }
            CoreExpr::Tail(atom) => atom.for_each_operand(&mut note),
            CoreExpr::Goto(_) => {}
        }
    }
    top
}

#[cfg(test)]
mod tests;
