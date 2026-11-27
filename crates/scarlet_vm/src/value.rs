//! The value word. Every Scarlet value is one 64-bit word.
//!
//! A Float is stored as itself. Every other kind of value is a quiet NaN, a
//! word with bits 51 to 62 all set, which no stored Float is. The top 16
//! bits of such a word say what it holds:
//!
//! | top 16 bits     | holds                                               |
//! |-----------------|-----------------------------------------------------|
//! | `0xFFF8` and up | a small Int: the low 51 bits, two's complement      |
//! | `0x7FF8`        | an immediate: `Nil` (0), `False` (1), `True` (2) or |
//! |                 | the empty array (3)                                 |
//! | `0x7FF9`        | a function with no captures, by its `FuncIdx`       |
//! | `0x7FFC`        | a cell in the process's heap                        |
//! | `0x7FFD`        | a constructor with no fields: variant and type      |
//! | `0x7FFE`        | kept for a literal frozen in the loaded program     |
//!
//! This table is the only place a tag is given out, so a new kind of word is
//! added here first. A cell's word holds the index of the cell's header in the
//! heap's arena, in bits 0 to 30, and its variant in bits 32 to 47. That is
//! which of its type's variants a constructor's cell is, and 0 for any other
//! cell. A constructor with no fields holds its variant there too, and its type
//! in bits 0 to 31. So the top half of a word says which variant of its type a
//! value is ([`Tag`]). A small Int is the 13 top bits set, so two words are both
//! small Ints when their `&` is at least `0xFFF8` shifted to the top.
//!
//! Scarlet has no NaN and no infinity (`docs/semantics.md`), and the layout
//! needs that. The NaN an ARM chip gives for `0.0 / 0.0` is
//! `0x7FF8_0000_0000_0000`, the word for `Nil`, and the one an x86 chip
//! gives is `0xFFF8_0000_0000_0000`, the word for the Int 0. A NaN that got
//! in would be a value of another type. `Value::finite` and
//! [`Value::float`] are the only ways to make a value from an `f64`, and
//! neither lets one through.

use std::fmt;

use scarlet_ir::TypeId;
use scarlet_ir::core_ir::{FuncIdx, VariantRef};

use crate::heap::Cell;
use crate::word::{high_u32, low_u16, low_u32};

/// Set in every word that is not a Float, and in no Float that is stored.
const QUIET_NAN: u64 = 0x7FF8_0000_0000_0000;
const TAG: u64 = 0xFFFF_0000_0000_0000;
const PAYLOAD: u64 = 0x0000_FFFF_FFFF_FFFF;

/// Every word from here up is a small Int.
const TAG_INT: u64 = 0xFFF8_0000_0000_0000;
const TAG_IMMEDIATE: u64 = 0x7FF8_0000_0000_0000;
const TAG_FUNC: u64 = 0x7FF9_0000_0000_0000;
const TAG_CELL: u64 = 0x7FFC_0000_0000_0000;
const TAG_NULLARY: u64 = 0x7FFD_0000_0000_0000;

/// The bits that tell the tags apart in a word that is a quiet NaN. The sign
/// is set in a small Int and in nothing else.
const SIGN: u64 = 1 << 63;
const TAG_BIT_50: u64 = 1 << 50;
const TAG_BIT_49: u64 = 1 << 49;
const TAG_BIT_48: u64 = 1 << 48;

/// How far a small Int's 51 bits sit below the top of the word.
const INT_SHIFT: u32 = 13;

/// The range a small Int covers. An Int outside it is a big int, kept in a
/// heap cell (`bigint.rs`).
const SMALL_INT_MIN: i64 = -(1 << 50);
const SMALL_INT_MAX: i64 = (1 << 50) - 1;

/// One value.
///
/// It is `Copy` on purpose, because it is a word. The VM keeps a cell's
/// reference count itself, in the process's own heap, rather than through
/// Rust's `Clone` and `Drop`. So copying a `Value` does not add a reference.
///
/// It has no `PartialEq`, because comparing the bits is not Scarlet's `==`
/// (`0.0 == -0.0`, and structural equality looks inside values).
#[derive(Clone, Copy)]
pub(crate) struct Value(u64);

/// A small Int, held with its 51 bits at the top of the word and 0 below
/// them. Adding or subtracting two of these overflows an `i64` exactly when
/// the answer is no small Int, so the CPU's overflow flag is the range check.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct SmallInt(i64);

impl SmallInt {
    /// `n`, or `None` when it is out of a small Int's range.
    pub(crate) fn new(n: i64) -> Option<SmallInt> {
        Value::int(n).and_then(Value::as_small)
    }

    /// The number itself.
    #[inline]
    pub(crate) fn get(self) -> i64 {
        self.0 >> INT_SHIFT
    }

    /// The number times 2^13, which is how it is held.
    #[inline]
    pub(crate) fn raised(self) -> i64 {
        self.0
    }

    /// The value for an answer worked out on [`Self::raised`] numbers. The
    /// bits it has below the 51 are dropped.
    #[inline]
    pub(crate) fn lower(raised: i64) -> Value {
        Value(TAG_INT | raised.cast_unsigned() >> INT_SHIFT)
    }

    #[inline]
    pub(crate) fn value(self) -> Value {
        SmallInt::lower(self.0)
    }
}

/// A value that is a Float.
#[derive(Clone, Copy)]
pub(crate) struct Float(Value);

impl Float {
    /// `x`, as [`Value::float`] makes a Float of it.
    pub(crate) fn new(x: f64) -> Float {
        Float(Value::float(x))
    }

    #[inline]
    pub(crate) fn value(self) -> Value {
        self.0
    }
}

/// The top half of a constructor's word: whether it has fields, and which of
/// its type's variants it is. Two values of one type are the same variant when
/// these are the same.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Tag(u32);

impl Tag {
    /// The tag of variant `variant`, which has fields.
    pub(crate) fn of_cell(variant: u16) -> Tag {
        Tag(high_u32(TAG_CELL) | u32::from(variant))
    }

    /// Which variant this is the tag of, and whether it has fields.
    pub(crate) fn parts(self) -> (u16, bool) {
        let variant = self.variant();
        (variant, self == Tag::of_cell(variant))
    }

    /// Which of its type's variants a value with this tag is, whether it has fields
    /// or not.
    #[inline(always)]
    pub(crate) fn variant(self) -> u16 {
        low_u16(u64::from(self.0))
    }

    /// Whether a value with this tag is a cell.
    pub(crate) fn is_cell(self) -> bool {
        self.parts().1
    }
}

/// What a [`Value`] holds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum View {
    Float(f64),
    Int(i64),
    Nil,
    Bool(bool),
    EmptyArray,
    Func(FuncIdx),
    Cell(Cell),
    /// A constructor with no fields, like `None`.
    Nullary(VariantRef),
}

impl Value {
    pub(crate) const NIL: Value = Value(TAG_IMMEDIATE);
    /// What fills room for values before they are there. It is the Float 0.0,
    /// and all of its bits are 0.
    pub(crate) const UNSET: Value = Value(0);
    const FALSE: Value = Value(TAG_IMMEDIATE | 1);
    const TRUE: Value = Value(TAG_IMMEDIATE | 2);
    /// The array with no elements. No cell is one ([`crate::array`]).
    pub(crate) const EMPTY_ARRAY: Value = Value(TAG_IMMEDIATE | 3);

    /// `n` as a small Int, or `None` when it needs a big int.
    pub(crate) fn int(n: i64) -> Option<Value> {
        // A number in range has bits 50 to 63 all alike, so setting the tag
        // changes nothing below it.
        (SMALL_INT_MIN..=SMALL_INT_MAX)
            .contains(&n)
            .then_some(Value(TAG_INT | n.cast_unsigned()))
    }

    /// `x` as a Float, or `None` when it is NaN or infinite, which no Float
    /// is (`docs/semantics.md`). So no Float's word is a tagged one.
    #[inline]
    pub(crate) fn finite(x: f64) -> Option<Value> {
        x.is_finite().then_some(Value(x.to_bits()))
    }

    /// `x` as a Float, for an answer that has to be one. An answer too large
    /// stops at the largest Float, keeping its sign, and one with no answer
    /// at all is `0.0`.
    #[inline]
    pub(crate) fn float(x: f64) -> Value {
        match Value::finite(x) {
            Some(v) => v,
            None => Value::clamped(x),
        }
    }

    /// The Float that stands for a NaN or an infinity. It is out of line so
    /// that [`Self::float`] is one test and a store when `x` is finite.
    #[cold]
    #[inline(never)]
    fn clamped(x: f64) -> Value {
        if x.is_nan() {
            Value(0.0_f64.to_bits())
        } else {
            Value(f64::MAX.copysign(x).to_bits())
        }
    }

    pub(crate) fn bool(b: bool) -> Value {
        if b { Value::TRUE } else { Value::FALSE }
    }

    /// A function that captures nothing. It needs no heap cell, because its
    /// index says everything about it.
    pub(crate) fn func(f: FuncIdx) -> Value {
        Value(TAG_FUNC | u64::from(f.0))
    }

    /// A heap cell. The value holds the one reference the caller gives it.
    pub(crate) fn cell(c: Cell) -> Value {
        Value(TAG_CELL | c.bits())
    }

    /// A constructor with no fields, like `None`. It needs no heap cell,
    /// because which constructor it is says everything about it.
    pub(crate) fn nullary(v: VariantRef) -> Value {
        Value(TAG_NULLARY | u64::from(v.variant_idx) << 32 | u64::from(v.type_id.0.cast_unsigned()))
    }

    /// The word itself, for storing in a heap cell.
    pub(crate) fn bits(self) -> u64 {
        self.0
    }

    /// A word [`Self::bits`] gave.
    pub(crate) fn from_bits(bits: u64) -> Value {
        Value(bits)
    }

    /// The word's top half, which says which variant a constructor is.
    #[inline]
    pub(crate) fn tag(self) -> Tag {
        Tag(high_u32(self.0))
    }

    /// The small Int this word holds, if it holds one.
    #[inline]
    pub(crate) fn small_int(self) -> Option<i64> {
        (self.0 >= TAG_INT).then(|| Value::untag_int(self.0))
    }

    /// [`Self::small_int`], in the form the Int instructions work on.
    #[inline]
    pub(crate) fn as_small(self) -> Option<SmallInt> {
        (self.0 >= TAG_INT).then_some(SmallInt((self.0 << INT_SHIFT).cast_signed()))
    }

    /// Both words as small Ints, when both are. A word is one when its 13 top
    /// bits are all set, so one test of the two words' `&` asks it of both.
    #[inline]
    pub(crate) fn both_small(self, other: Value) -> Option<(SmallInt, SmallInt)> {
        (self.0 & other.0 >= TAG_INT).then_some((
            SmallInt((self.0 << INT_SHIFT).cast_signed()),
            SmallInt((other.0 << INT_SHIFT).cast_signed()),
        ))
    }

    /// The word as an `f64`, whatever it holds. Every word that is not a
    /// Float is a NaN, so this is for an operation that a NaN cannot get
    /// through (`float.rs`).
    #[inline]
    pub(crate) fn unchecked_float(self) -> f64 {
        f64::from_bits(self.0)
    }

    /// The number in a small Int's word. The shift up drops the tag and the
    /// shift back down copies the number's sign into the bits it left.
    #[inline]
    fn untag_int(bits: u64) -> i64 {
        (bits << INT_SHIFT).cast_signed() >> INT_SHIFT
    }

    /// The cell this value points at, if it is one.
    pub(crate) fn as_cell(self) -> Option<Cell> {
        (self.0 & TAG == TAG_CELL).then(|| Cell::from_bits(self.0 & PAYLOAD))
    }

    /// The function this value is, if it is one that captures nothing.
    pub(crate) fn as_func(self) -> Option<FuncIdx> {
        (self.0 & TAG == TAG_FUNC).then(|| FuncIdx(low_u32(self.0)))
    }

    /// What the word holds. Past the Float test this reads one bit at a
    /// time, which costs a test each and lets a caller that wants one kind
    /// skip the rest: the sign is an Int, bit 50 a cell or a constructor with
    /// no fields, and bit 48 tells each pair apart.
    pub(crate) fn view(self) -> View {
        let bits = self.0;
        if bits & QUIET_NAN != QUIET_NAN {
            return View::Float(f64::from_bits(bits));
        }
        if bits & SIGN != 0 {
            return View::Int(Value::untag_int(bits));
        }
        let payload = bits & PAYLOAD;
        // No constructor makes a tag with bit 49 set, or an immediate other
        // than these four.
        if bits & TAG_BIT_49 != 0 {
            return View::Nil;
        }
        match (bits & TAG_BIT_50 != 0, bits & TAG_BIT_48 != 0) {
            (true, false) => View::Cell(Cell::from_bits(payload)),
            (true, true) => View::Nullary(VariantRef {
                type_id: TypeId(low_u32(payload).cast_signed()),
                variant_idx: low_u16(payload >> 32),
            }),
            (false, true) => View::Func(FuncIdx(low_u32(payload))),
            (false, false) => match payload {
                1 => View::Bool(false),
                2 => View::Bool(true),
                3 => View::EmptyArray,
                _ => View::Nil,
            },
        }
    }
}

impl fmt::Debug for SmallInt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.get().fmt(f)
    }
}

impl fmt::Debug for Float {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.view().fmt(f)
    }
}

#[cfg(test)]
mod tests;
