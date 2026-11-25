//! What the fields of an instruction are: the registers it names, each by what
//! it does with it, the places in the code it names, and the rest.

use std::fmt;

use scarlet_ir::core_ir::{ConstId, FuncIdx, GlobalSlot};
use scarlet_ir::intrinsic::Intrinsic;

use crate::heap::Ctor;
use crate::ops::NumOp;
use crate::value::{Float, SmallInt, Tag, Value};

/// A register, which is a place in the running function's frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Reg(pub(crate) u8);

/// A value the loader has not given a register yet. One per Core IR local,
/// and one for each value no local holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct VReg(pub(crate) u32);

/// What an instruction names other things by: the registers, and what stands
/// for a place in the code and for a frame's size beside them.
pub(crate) trait Names: Copy + PartialEq + fmt::Debug {
    /// Where a jump goes.
    type To: Part<Self> + fmt::Debug;
    /// The function a call by name runs.
    type Entry: Part<Self> + fmt::Debug;
    /// The running function, where a call to itself runs it again.
    type OwnEntry: Part<Self> + fmt::Debug;
    /// How many registers the running function has.
    type Own: Part<Self> + fmt::Debug;
    /// The last of them.
    type Last: Part<Self> + fmt::Debug;
    /// What fills a place for an operand that an instruction does not use.
    const UNUSED: Self;
    /// What is no register an instruction writes. The loader gives no value
    /// this number, and no value's register is a frame's link.
    const NONE: Self;
}

impl Names for VReg {
    type To = Label;
    type Entry = Callee;
    type OwnEntry = OwnEntry;
    type Own = OwnRoom;
    type Last = LastReg;
    const UNUSED: VReg = VReg(0);
    const NONE: VReg = VReg(u32::MAX);
}

impl Names for Reg {
    type To = Pc;
    type Entry = EntryPc;
    type OwnEntry = EntryPc;
    type Own = Room;
    type Last = Last;
    const UNUSED: Reg = Reg(0);
    const NONE: Reg = Reg(0);
}

/// A place in a function the loader is still making.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Label(pub(crate) u32);

/// An instruction, by where it is in the program's code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Pc(pub(crate) u32);

impl Pc {
    pub(crate) fn index(self) -> usize {
        self.0 as usize
    }
}

/// The function a call by name runs, before its code has a place.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Callee(pub(crate) FuncIdx);

/// A function's first instruction.
#[derive(Debug, Clone, Copy)]
pub(crate) struct EntryPc(pub(crate) u32);

impl EntryPc {
    pub(crate) fn index(self) -> usize {
        self.0 as usize
    }
}

/// This function's first instruction, before that is known.
#[derive(Debug, Clone, Copy)]
pub(crate) struct OwnEntry;

/// How many registers this function has, before that is known.
#[derive(Debug, Clone, Copy)]
pub(crate) struct OwnRoom;

/// This function's last register, before that is known.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LastReg;

/// A frame's last register.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Last(pub(crate) Reg);

/// How many registers a frame has, the two that are for its link and its
/// closure among them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Room(pub(crate) u16);

impl Room {
    /// The last of them. A frame has no more than a window of them
    /// (`super::alloc`).
    pub(crate) fn last(self) -> Last {
        let [last, _] = self.0.wrapping_sub(1).to_le_bytes();
        Last(Reg(last))
    }

    /// How far above a frame of this size its callee's starts.
    #[inline(always)]
    pub(crate) fn below(self) -> u32 {
        u32::from(self.0)
    }

    pub(crate) fn len(self) -> usize {
        usize::from(self.0)
    }
}

/// What an instruction does with a register.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    /// Reads it, and leaves it as it was.
    Look,
    /// Reads it and adds a reference to what it holds, for somewhere new.
    Share,
    /// Hands what it holds on. When `moved` the reference goes too and the
    /// register is left empty, and otherwise this is a [`Role::Share`].
    Pass { moved: bool },
    /// Takes what it holds and leaves it empty.
    Take,
    /// Gives up the reference it holds and leaves it empty.
    Drop,
    /// Writes it.
    Write,
    /// Writes it, unless the instruction jumps.
    WriteOnHit,
}

/// One field of an instruction.
pub(crate) trait Part<R: Names>: Copy {
    /// Call `f` with each register here, and what the instruction does with it.
    fn operands(self, _: &Lists<R>, _: &mut impl FnMut(R, Role)) {}

    /// Make each [`Arg`] here one the instruction takes, or not, as `moved`
    /// says of its register.
    fn decide(&mut self, _: &mut Lists<R>, _: &mut impl FnMut(R) -> bool) {}

    /// Where the instruction may jump, when this is what says so.
    fn target(self) -> Option<R::To> {
        None
    }
}

/// A field of an instruction on [`VReg`]s, as it is once the registers, the
/// labels and the functions have their places.
pub(crate) trait Place {
    type Placed;
    fn place(self, at: &impl Places) -> Result<Self::Placed, Unplaced>;
}

/// Why an instruction gets no place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Unplaced {
    /// It names a register, a label, a list or a function that has none.
    NoPlace,
    /// It calls this function by name, which can run only as a closure.
    NeedsClosure(FuncIdx),
}

/// Where everything an instruction names ended up.
pub(crate) trait Places {
    fn reg(&self, r: VReg) -> Result<Reg, Unplaced>;
    fn label(&self, l: Label) -> Result<Pc, Unplaced>;
    /// Where a call by name to `f` goes.
    fn entry(&self, f: FuncIdx) -> Result<EntryPc, Unplaced>;
    /// Where a function's call to itself goes.
    fn own_entry(&self) -> EntryPc;
    fn own(&self) -> Room;
    /// Where the function's own list `i` is among the program's.
    fn reg_list(&self, i: u32) -> Result<u32, Unplaced>;
    fn arg_list(&self, i: u32) -> Result<u32, Unplaced>;
    fn dst_list(&self, i: u32) -> Result<u32, Unplaced>;
}

impl Part<VReg> for Label {
    fn target(self) -> Option<Label> {
        Some(self)
    }
}

impl Part<Reg> for Pc {
    fn target(self) -> Option<Pc> {
        Some(self)
    }
}

impl Part<VReg> for Callee {}
impl Part<VReg> for OwnEntry {}
impl Part<VReg> for OwnRoom {}
impl Part<VReg> for LastReg {}
impl Part<Reg> for Last {}
impl Part<Reg> for EntryPc {}
impl Part<Reg> for Room {}

impl Place for Label {
    type Placed = Pc;
    fn place(self, at: &impl Places) -> Result<Pc, Unplaced> {
        at.label(self)
    }
}

impl Place for Callee {
    type Placed = EntryPc;
    fn place(self, at: &impl Places) -> Result<EntryPc, Unplaced> {
        at.entry(self.0)
    }
}

impl Place for OwnEntry {
    type Placed = EntryPc;
    fn place(self, at: &impl Places) -> Result<EntryPc, Unplaced> {
        Ok(at.own_entry())
    }
}

impl Place for OwnRoom {
    type Placed = Room;
    fn place(self, at: &impl Places) -> Result<Room, Unplaced> {
        Ok(at.own())
    }
}

impl Place for LastReg {
    type Placed = Last;
    fn place(self, at: &impl Places) -> Result<Last, Unplaced> {
        Ok(at.own().last())
    }
}

/// Declares the field that is one register and has one role.
macro_rules! registers {
    ($($(#[$doc:meta])* $name:ident $role:expr;)*) => {$(
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub(crate) struct $name<R>(pub(crate) R);

        impl<R: Names> Part<R> for $name<R> {
            fn operands(self, _: &Lists<R>, f: &mut impl FnMut(R, Role)) {
                f(self.0, $role);
            }
        }

        impl Place for $name<VReg> {
            type Placed = $name<Reg>;
            fn place(self, at: &impl Places) -> Result<$name<Reg>, Unplaced> {
                at.reg(self.0).map($name)
            }
        }
    )*};
}

registers! {
    /// A register the instruction writes.
    Dst Role::Write;
    /// A register the instruction writes unless it jumps.
    Hit Role::WriteOnHit;
    /// A register the instruction only looks at.
    Src Role::Look;
    /// A register whose value the instruction shares.
    Shared Role::Share;
    /// A register the instruction takes from.
    Taken Role::Take;
    /// A register given up ([`Ins::Drop`]).
    Dead Role::Drop;
}

/// Declares the field that is `N` registers with one role.
macro_rules! registers_of_one_role {
    ($($name:ident $role:expr;)*) => {$(
        impl<R: Names, const N: usize> Part<R> for [$name<R>; N] {
            fn operands(self, _: &Lists<R>, f: &mut impl FnMut(R, Role)) {
                self.iter().for_each(|r| f(r.0, $role));
            }
        }

        impl<const N: usize> Place for [$name<VReg>; N] {
            type Placed = [$name<Reg>; N];
            fn place(self, at: &impl Places) -> Result<[$name<Reg>; N], Unplaced> {
                place_each(self.map(|r| r.0), at).map(|regs| regs.map($name))
            }
        }
    )*};
}

registers_of_one_role! {
    Dst Role::Write;
    Shared Role::Share;
    Taken Role::Take;
}

/// Each of `regs` as the register it was given.
fn place_each<const N: usize>(regs: [VReg; N], at: &impl Places) -> Result<[Reg; N], Unplaced> {
    let mut placed = [Reg::UNUSED; N];
    for (to, from) in placed.iter_mut().zip(regs) {
        *to = at.reg(from)?;
    }
    Ok(placed)
}

/// A register the instruction writes, or none, for what it has nowhere to put.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MaybeDst<R>(R);

impl<R: Names> MaybeDst<R> {
    pub(crate) fn new(dst: Option<R>) -> MaybeDst<R> {
        MaybeDst(dst.unwrap_or(R::NONE))
    }

    #[inline(always)]
    pub(crate) fn dst(self) -> Option<Dst<R>> {
        (self.0 != R::NONE).then_some(Dst(self.0))
    }
}

impl<R: Names> Part<R> for MaybeDst<R> {
    fn operands(self, lists: &Lists<R>, f: &mut impl FnMut(R, Role)) {
        if let Some(dst) = self.dst() {
            dst.operands(lists, f);
        }
    }
}

impl Place for MaybeDst<VReg> {
    type Placed = MaybeDst<Reg>;
    fn place(self, at: &impl Places) -> Result<MaybeDst<Reg>, Unplaced> {
        let dst = self.dst().map(|dst| at.reg(dst.0)).transpose()?;
        Ok(MaybeDst::new(dst))
    }
}

impl<R: Names, const N: usize> Part<R> for [MaybeDst<R>; N] {
    fn operands(self, lists: &Lists<R>, f: &mut impl FnMut(R, Role)) {
        for dst in self {
            dst.operands(lists, f);
        }
    }
}

impl<const N: usize> Place for [MaybeDst<VReg>; N] {
    type Placed = [MaybeDst<Reg>; N];
    fn place(self, at: &impl Places) -> Result<[MaybeDst<Reg>; N], Unplaced> {
        let mut placed = [MaybeDst(Reg::NONE); N];
        for (to, from) in placed.iter_mut().zip(self) {
            *to = from.place(at)?;
        }
        Ok(placed)
    }
}

/// A register whose value the instruction hands on, to a callee, a cell or
/// another register.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Arg<R> {
    pub(crate) reg: R,
    /// Whether the reference the register holds goes too, and no count
    /// changes. It does when the frame does nothing with the register after
    /// this ([`super::live`]).
    pub(crate) moved: bool,
}

impl<R> Arg<R> {
    pub(crate) fn shared(reg: R) -> Arg<R> {
        Arg { reg, moved: false }
    }

    pub(crate) fn moved(reg: R) -> Arg<R> {
        Arg { reg, moved: true }
    }
}

impl<R: Names> Part<R> for Arg<R> {
    fn operands(self, _: &Lists<R>, f: &mut impl FnMut(R, Role)) {
        f(self.reg, Role::Pass { moved: self.moved });
    }

    fn decide(&mut self, _: &mut Lists<R>, moved: &mut impl FnMut(R) -> bool) {
        self.moved = moved(self.reg);
    }
}

impl Place for Arg<VReg> {
    type Placed = Arg<Reg>;
    fn place(self, at: &impl Places) -> Result<Arg<Reg>, Unplaced> {
        Ok(Arg {
            reg: at.reg(self.reg)?,
            moved: self.moved,
        })
    }
}

/// Up to `N` registers, held in the instruction.
#[derive(Debug, Clone, Copy)]
struct Few<R, const N: usize> {
    n: u8,
    regs: [R; N],
}

impl<R: Names, const N: usize> Few<R, N> {
    /// `of`, or `None` when there is no room for it here.
    fn new(of: &[R]) -> Option<Self> {
        let mut regs = [R::UNUSED; N];
        regs.get_mut(..of.len())?.copy_from_slice(of);
        Some(Few {
            n: u8::try_from(of.len()).ok()?,
            regs,
        })
    }
}

impl<R, const N: usize> Few<R, N> {
    #[inline(always)]
    fn regs(&self) -> &[R] {
        // There are never more than `N`. With that said, the compiler leaves
        // out the test of whether the slice is there.
        let n = usize::from(self.n).min(N);
        self.regs.get(..n).unwrap_or_default()
    }
}

impl<const N: usize> Few<VReg, N> {
    fn place(self, at: &impl Places) -> Result<Few<Reg, N>, Unplaced> {
        let mut regs = [Reg::UNUSED; N];
        for (to, from) in regs.iter_mut().zip(self.regs()) {
            *to = at.reg(*from)?;
        }
        Ok(Few { n: self.n, regs })
    }
}

/// Declares the field that names a list of registers with one role.
macro_rules! lists {
    ($($(#[$doc:meta])* $list:ident $role:expr;)*) => {$(
        $(#[$doc])*
        #[derive(Debug, Clone, Copy)]
        pub(crate) struct $list(pub(crate) u32);

        impl $list {
            pub(crate) fn regs<R>(self, lists: &Lists<R>) -> &[R] {
                lists.regs(self.0)
            }
        }

        impl<R: Names> Part<R> for $list {
            fn operands(self, lists: &Lists<R>, f: &mut impl FnMut(R, Role)) {
                self.regs(lists).iter().for_each(|r| f(*r, $role));
            }
        }

        impl Place for $list {
            type Placed = $list;
            fn place(self, at: &impl Places) -> Result<$list, Unplaced> {
                at.reg_list(self.0).map($list)
            }
        }
    )*};
}

lists! {
    /// Registers given up ([`Ins::DropWide`]).
    DeadList Role::Drop;
}

/// Declares the two fields that are several registers with one role: one that
/// holds them, and one that names the list that does.
macro_rules! several {
    ($($(#[$doc:meta])* $few:ident $list:ident $role:expr;)*) => {$(
        $(#[$doc])*
        #[derive(Debug, Clone, Copy)]
        pub(crate) struct $few<R, const N: usize>(Few<R, N>);

        lists! {
            /// The same, in a list of registers kept beside the code.
            $list $role;
        }

        impl<R: Names, const N: usize> $few<R, N> {
            pub(crate) fn new(of: &[R]) -> Option<Self> {
                Few::new(of).map($few)
            }
        }

        impl<R, const N: usize> $few<R, N> {
            #[inline(always)]
            pub(crate) fn regs(&self) -> &[R] {
                self.0.regs()
            }
        }

        impl<R: Names, const N: usize> Part<R> for $few<R, N> {
            fn operands(self, _: &Lists<R>, f: &mut impl FnMut(R, Role)) {
                self.regs().iter().for_each(|r| f(*r, $role));
            }
        }

        impl<const N: usize> Place for $few<VReg, N> {
            type Placed = $few<Reg, N>;
            fn place(self, at: &impl Places) -> Result<$few<Reg, N>, Unplaced> {
                self.0.place(at).map($few)
            }
        }

    )*};
}

several! {
    /// Registers the instruction only looks at.
    Looks LookList Role::Look;
    /// Registers whose values the instruction shares.
    Shares ShareList Role::Share;
}

/// Up to `N` [`Arg`]s, held in the instruction.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Args<R, const N: usize> {
    regs: Few<R, N>,
    /// Bit `i` is the `i`th argument's [`Arg::moved`].
    moved: u8,
}

impl<R: Names, const N: usize> Args<R, N> {
    /// Each bit of `moved` is an argument's, so an instruction that holds
    /// more than eight does not compile.
    const FITS: () = assert!(N <= 8);

    /// `of`, each shared, or `None` when there is no room for it here.
    pub(crate) fn new(of: &[R]) -> Option<Self> {
        let () = Self::FITS;
        Some(Args {
            regs: Few::new(of)?,
            moved: 0,
        })
    }
}

impl<R: Copy, const N: usize> Args<R, N> {
    /// The registers, when there are `K` of them and the instruction takes each.
    pub(super) fn taken<const K: usize>(&self) -> Option<[Taken<R>; K]> {
        let regs: &[R; K] = self.regs.regs().try_into().ok()?;
        self.iter().all(|a| a.moved).then(|| regs.map(Taken))
    }

    pub(crate) fn len(&self) -> usize {
        self.regs.regs().len()
    }

    #[inline(always)]
    pub(crate) fn iter(&self) -> impl ExactSizeIterator<Item = Arg<R>> {
        let mut moved = self.moved;
        self.regs.regs().iter().map(move |reg| {
            let arg = Arg {
                reg: *reg,
                moved: moved & 1 == 1,
            };
            moved >>= 1;
            arg
        })
    }
}

impl<R: Names, const N: usize> Part<R> for Args<R, N> {
    fn operands(self, _: &Lists<R>, f: &mut impl FnMut(R, Role)) {
        for a in self.iter() {
            f(a.reg, Role::Pass { moved: a.moved });
        }
    }

    fn decide(&mut self, _: &mut Lists<R>, moved: &mut impl FnMut(R) -> bool) {
        let mut bit = 1u8;
        let mut bits = 0u8;
        for reg in self.regs.regs() {
            if moved(*reg) {
                bits |= bit;
            }
            bit = bit.wrapping_shl(1);
        }
        self.moved = bits;
    }
}

impl<const N: usize> Place for Args<VReg, N> {
    type Placed = Args<Reg, N>;
    fn place(self, at: &impl Places) -> Result<Args<Reg, N>, Unplaced> {
        Ok(Args {
            regs: self.regs.place(at)?,
            moved: self.moved,
        })
    }
}

/// The operands an instruction hands to a new cell, in order.
pub(crate) trait Hands {
    fn hands(&self) -> impl ExactSizeIterator<Item = Arg<Reg>>;
}

impl<const N: usize> Hands for [Taken<Reg>; N] {
    #[inline(always)]
    fn hands(&self) -> impl ExactSizeIterator<Item = Arg<Reg>> {
        self.iter().map(|r| Arg {
            reg: r.0,
            moved: true,
        })
    }
}

impl<const N: usize> Hands for [Shared<Reg>; N] {
    #[inline(always)]
    fn hands(&self) -> impl ExactSizeIterator<Item = Arg<Reg>> {
        self.iter().map(|r| Arg::shared(r.0))
    }
}

impl<const N: usize> Hands for Args<Reg, N> {
    #[inline(always)]
    fn hands(&self) -> impl ExactSizeIterator<Item = Arg<Reg>> {
        self.iter()
    }
}

impl Hands for [Arg<Reg>] {
    #[inline(always)]
    fn hands(&self) -> impl ExactSizeIterator<Item = Arg<Reg>> {
        self.iter().copied()
    }
}

impl Hands for [Reg] {
    #[inline(always)]
    fn hands(&self) -> impl ExactSizeIterator<Item = Arg<Reg>> {
        self.iter().map(|r| Arg::shared(*r))
    }
}

/// A list of [`Arg`]s kept beside the code.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ArgList(pub(crate) u32);

impl ArgList {
    pub(crate) fn args<R>(self, lists: &Lists<R>) -> &[Arg<R>] {
        lists.args(self.0)
    }
}

impl<R: Names> Part<R> for ArgList {
    fn operands(self, lists: &Lists<R>, f: &mut impl FnMut(R, Role)) {
        for a in self.args(lists) {
            a.operands(lists, f);
        }
    }

    fn decide(&mut self, lists: &mut Lists<R>, moved: &mut impl FnMut(R) -> bool) {
        if let Some(args) = lists.args.get_mut(self.0 as usize) {
            for a in args {
                a.moved = moved(a.reg);
            }
        }
    }
}

impl Place for ArgList {
    type Placed = ArgList;
    fn place(self, at: &impl Places) -> Result<ArgList, Unplaced> {
        at.arg_list(self.0).map(ArgList)
    }
}

/// The registers the fields of a constructor go to, in order, in a list kept
/// beside the code.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DstList(pub(crate) u32);

impl DstList {
    pub(crate) fn dsts<R>(self, lists: &Lists<R>) -> &[MaybeDst<R>] {
        lists.dsts.get(self.0 as usize).map_or(&[], |l| l)
    }
}

impl<R: Names> Part<R> for DstList {
    fn operands(self, lists: &Lists<R>, f: &mut impl FnMut(R, Role)) {
        for dst in self.dsts(lists) {
            dst.operands(lists, f);
        }
    }
}

impl Place for DstList {
    type Placed = DstList;
    fn place(self, at: &impl Places) -> Result<DstList, Unplaced> {
        at.dst_list(self.0).map(DstList)
    }
}

/// The operands of the instructions that have too many to hold. An instruction
/// is given its place only when the list it names is here ([`super::emit`]).
#[derive(Debug)]
pub(crate) struct Lists<R> {
    pub(crate) regs: Vec<Box<[R]>>,
    pub(crate) args: Vec<Box<[Arg<R>]>>,
    pub(crate) dsts: Vec<Box<[MaybeDst<R>]>>,
}

impl<R> Default for Lists<R> {
    fn default() -> Self {
        Lists {
            regs: Vec::new(),
            args: Vec::new(),
            dsts: Vec::new(),
        }
    }
}

impl<R> Lists<R> {
    fn regs(&self, i: u32) -> &[R] {
        self.regs.get(i as usize).map_or(&[], |l| l)
    }

    fn args(&self, i: u32) -> &[Arg<R>] {
        self.args.get(i as usize).map_or(&[], |l| l)
    }
}

/// Whether a newline is printed too.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Newline(pub(crate) bool);

/// A string the program spells, by its number in `Code::texts`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct TextId(pub(crate) u32);

/// A binary the program spells, by its number in `Code::binaries`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BinaryId(pub(crate) u32);

/// Why an [`Ins::Bad`] stops the run, by its number in `Code::whys`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct WhyId(pub(crate) u32);

/// A block a counted run counts, by its number in `Counted::blocks`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BlockId(pub(crate) u32);

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

/// The fields that name no register and no place in the code.
macro_rules! plain {
    ($($ty:ty),* $(,)?) => {$(
        impl<R: Names> Part<R> for $ty {}

        impl Place for $ty {
            type Placed = $ty;
            fn place(self, _: &impl Places) -> Result<$ty, Unplaced> {
                Ok(self)
            }
        }
    )*};
}

plain! {
    Newline, u16, u32, i64, f64, Value, SmallInt, Float, Tag, Ctor, TextId, BinaryId, WhyId,
    BlockId, BitsOp, NumOp, Intrinsic, FuncIdx, ConstId, GlobalSlot,
}
