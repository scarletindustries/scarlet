//! The value word. Every Scarlet value is one 64-bit word.
//!
//! A Float is stored as itself. Every other kind of value is a quiet NaN, a
//! word with bits 51 to 62 all set, which no stored Float is. The top 16
//! bits of such a word say what it holds:
//!
//! | top 16 bits     | holds                                               |
//! |-----------------|-----------------------------------------------------|
//! | `0xFFF8` and up | a small Int: the low 51 bits, two's complement      |
//! | `0x7FF8`        | an immediate: `Nil` (0), `False` (1) or `True` (2)  |
//! | `0x7FF9`        | a function with no captures, by its `FuncIdx`       |
//! | `0x7FFC`        | a cell in the process's heap                        |
//! | `0x7FFD`        | a constructor with no fields: variant and type      |
//! | `0x7FFE`        | kept for a literal frozen in the loaded program     |
//!
//! Immediate 3 is kept for the empty array. This table is the only place a
//! tag is given out, so a new kind of word is added here first. A
//! constructor with no fields holds its variant in bits 32 to 47 and its
//! type in bits 0 to 31. A small Int is the 13 top bits set, so two words
//! are both small Ints when their `&` is at least `0xFFF8` shifted to the top.
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
use crate::word::{low_u16, low_u32};

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

/// What a [`Value`] holds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum View {
    Float(f64),
    Int(i64),
    Nil,
    Bool(bool),
    Func(FuncIdx),
    Cell(Cell),
    /// A constructor with no fields, like `None`.
    Nullary(VariantRef),
}

impl Value {
    pub(crate) const NIL: Value = Value(TAG_IMMEDIATE);
    const FALSE: Value = Value(TAG_IMMEDIATE | 1);
    const TRUE: Value = Value(TAG_IMMEDIATE | 2);

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
    fn finite(x: f64) -> Option<Value> {
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

    /// The small Int this word holds, if it holds one.
    #[inline]
    pub(crate) fn small_int(self) -> Option<i64> {
        (self.0 >= TAG_INT).then(|| Value::untag_int(self.0))
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
        // than these three.
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
                _ => View::Nil,
            },
        }
    }
}

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.view().fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nullary(t: i32, v: u16) -> VariantRef {
        VariantRef {
            type_id: TypeId(t),
            variant_idx: v,
        }
    }

    /// A value of every kind that is not a Float, at the ends of what each
    /// constructor takes.
    fn tagged() -> Vec<Value> {
        let ints = [0, 1, -1, 42, -42, SMALL_INT_MIN, SMALL_INT_MAX];
        let funcs = [0, 7, u32::MAX];
        let cells = [0, 1, 1 << 28, PAYLOAD];
        let nullaries = [
            (0, 0),
            (263, 1),
            (i32::MAX, u16::MAX),
            (-1, 7),
            (i32::MIN, 0),
        ];
        let mut all = vec![Value::NIL, Value::bool(false), Value::bool(true)];
        all.extend(ints.iter().filter_map(|&n| Value::int(n)));
        all.extend(funcs.iter().map(|&f| Value::func(FuncIdx(f))));
        all.extend(cells.iter().map(|&c| Value::cell(Cell::from_bits(c))));
        all.extend(
            nullaries
                .iter()
                .map(|&(t, v)| Value::nullary(nullary(t, v))),
        );
        assert_eq!(all.len(), 22);
        all
    }

    #[test]
    fn small_ints_round_trip_at_both_ends() {
        for n in [0, 1, -1, 42, -42, SMALL_INT_MIN, SMALL_INT_MAX] {
            let v = Value::int(n).expect("in range");
            assert_eq!(v.view(), View::Int(n));
            assert_eq!(v.small_int(), Some(n));
            assert!(v.as_cell().is_none());
        }
        assert_eq!(SMALL_INT_MIN, -1_125_899_906_842_624);
        assert_eq!(SMALL_INT_MAX, 1_125_899_906_842_623);
    }

    #[test]
    fn an_int_past_either_end_needs_a_big_int() {
        assert!(Value::int(SMALL_INT_MAX + 1).is_none());
        assert!(Value::int(SMALL_INT_MIN - 1).is_none());
        assert!(Value::int(i64::MAX).is_none());
        assert!(Value::int(i64::MIN).is_none());
    }

    #[test]
    fn immediates_and_functions_are_themselves() {
        assert_eq!(Value::NIL.view(), View::Nil);
        assert_eq!(Value::bool(true).view(), View::Bool(true));
        assert_eq!(Value::bool(false).view(), View::Bool(false));
        assert_eq!(Value::func(FuncIdx(7)).view(), View::Func(FuncIdx(7)));
        assert_eq!(
            Value::func(FuncIdx(u32::MAX)).view(),
            View::Func(FuncIdx(u32::MAX))
        );
    }

    #[test]
    fn a_cell_is_itself() {
        for bits in [0, 1, 1 << 28, PAYLOAD] {
            let cell = Cell::from_bits(bits);
            let v = Value::cell(cell);
            assert_eq!(v.view(), View::Cell(cell));
            assert_eq!(v.as_cell(), Some(cell));
            assert!(v.small_int().is_none());
        }
    }

    #[test]
    fn a_float_is_never_nan_or_infinite() {
        assert_eq!(Value::float(f64::NAN).view(), View::Float(0.0));
        assert_eq!(Value::float(-f64::NAN).view(), View::Float(0.0));
        assert_eq!(Value::float(f64::INFINITY).view(), View::Float(f64::MAX));
        assert_eq!(
            Value::float(f64::NEG_INFINITY).view(),
            View::Float(f64::MIN)
        );
        assert_eq!(Value::float(-2.5).view(), View::Float(-2.5));
        // `-0.0` stays itself, sign and all.
        assert_eq!(Value::float(-0.0).bits(), (-0.0_f64).to_bits());
    }

    /// `finite` refuses exactly what is not a Float, and changes nothing
    /// else. The NaNs are the two a chip makes, a signalling one, and one
    /// carrying each tagged word's bits.
    #[test]
    fn finite_lets_in_every_float_and_nothing_else() {
        let floats = [
            0.0,
            -0.0,
            1.5,
            -1.5,
            f64::MAX,
            f64::MIN,
            f64::MIN_POSITIVE,
            -f64::MIN_POSITIVE,
            f64::from_bits(1),
            -f64::from_bits(1),
            f64::EPSILON,
        ];
        for x in floats {
            let v = Value::finite(x).expect("finite");
            assert_eq!(v.bits(), x.to_bits(), "{x:e}");
            assert_eq!(v.view(), View::Float(x), "{x:e}");
            assert_eq!(Value::float(x).bits(), x.to_bits(), "{x:e}");
        }
        let refused = [
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NAN,
            f64::from_bits(0x7FF8_0000_0000_0000),
            f64::from_bits(0xFFF8_0000_0000_0000),
            f64::from_bits(0x7FF0_0000_0000_0001),
            f64::from_bits(0xFFF0_0000_0000_0001),
        ];
        for x in refused {
            assert!(Value::finite(x).is_none(), "{:#x}", x.to_bits());
        }
        for v in tagged() {
            assert!(Value::finite(f64::from_bits(v.bits())).is_none(), "{v:?}");
        }
        // What this machine's own divide gives for an answer that is none.
        let zero = std::hint::black_box(0.0_f64);
        assert!(Value::finite(zero / zero).is_none());
        assert!(Value::finite(1.0 / zero).is_none());
        assert!(Value::finite(f64::MAX * std::hint::black_box(2.0)).is_none());
    }

    #[test]
    fn a_nullary_constructor_keeps_its_type_and_variant() {
        for (t, v) in [
            (0, 0),
            (263, 1),
            (i32::MAX, u16::MAX),
            (-1, 7),
            (i32::MIN, 0),
        ] {
            let variant = nullary(t, v);
            assert_eq!(Value::nullary(variant).view(), View::Nullary(variant));
        }
    }

    /// The layout rests on this. A Float operation handed a tagged word gets
    /// a NaN, so its answer is a NaN, and `finite` refuses it.
    #[test]
    fn every_tagged_word_is_a_nan() {
        for v in tagged() {
            assert!(f64::from_bits(v.bits()).is_nan(), "{v:?}");
            assert!(!matches!(v.view(), View::Float(_)), "{v:?}");
        }
        // Over the whole word, not only what the constructors make: every
        // top 16 bits there are, above a few payloads.
        let payloads = [0, 1, 2, 3, 1 << 31, 1 << 32, 1 << 47, PAYLOAD];
        for top in 0..=u64::from(u16::MAX) {
            for low in payloads {
                let bits = top << 48 | low;
                let x = f64::from_bits(bits);
                match Value::from_bits(bits).view() {
                    View::Float(f) => assert_eq!(f.to_bits(), bits),
                    tagged @ (View::Int(_)
                    | View::Nil
                    | View::Bool(_)
                    | View::Func(_)
                    | View::Cell(_)
                    | View::Nullary(_)) => assert!(x.is_nan(), "{bits:#x} is {tagged:?}"),
                }
                if x.is_finite() {
                    assert_eq!(Value::from_bits(bits).view(), View::Float(x));
                }
                // The two short tests agree with `view` on every word.
                let v = Value::from_bits(bits);
                assert_eq!(
                    v.as_cell().map(View::Cell),
                    Some(v.view()).filter(|v| matches!(v, View::Cell(_))),
                    "{bits:#x}"
                );
                assert_eq!(
                    v.small_int().map(View::Int),
                    Some(v.view()).filter(|v| matches!(v, View::Int(_))),
                    "{bits:#x}"
                );
            }
        }
    }

    /// What the table at the top of this file says a word holds, read off
    /// its top 16 bits the long way. A tag the table gives to nothing, and an
    /// immediate past `True`, reads as `Nil`.
    fn by_the_table(bits: u64) -> View {
        let payload = bits & PAYLOAD;
        let low_32 = u32::try_from(payload & 0xFFFF_FFFF).expect("32 bits");
        match bits >> 48 {
            0xFFF8..=0xFFFF => {
                let low = i64::try_from(bits & ((1 << 51) - 1)).expect("51 bits");
                View::Int(if low < 1 << 50 { low } else { low - (1 << 51) })
            }
            0x7FF8 => match payload {
                1 => View::Bool(false),
                2 => View::Bool(true),
                _ => View::Nil,
            },
            0x7FF9 => View::Func(FuncIdx(low_32)),
            0x7FFC => View::Cell(Cell::from_bits(payload)),
            0x7FFD => View::Nullary(nullary(
                low_32.cast_signed(),
                u16::try_from(payload >> 32).expect("16 bits"),
            )),
            0x7FFA | 0x7FFB | 0x7FFE | 0x7FFF => View::Nil,
            _ => View::Float(f64::from_bits(bits)),
        }
    }

    /// `view` reads the tags a bit at a time, and this checks it against the
    /// table for every top 16 bits there are, the tags and immediates that
    /// no constructor makes among them. A new kind of word changes its row
    /// in `by_the_table`, and this fails until `view` reads it.
    #[test]
    fn view_reads_every_word_as_the_table_says() {
        // A Float goes by its bits, because a word outside the tags may be a
        // NaN, which is not equal to itself.
        let float_bits = |v: View| match v {
            View::Float(f) => Some(f.to_bits()),
            View::Int(_)
            | View::Nil
            | View::Bool(_)
            | View::Func(_)
            | View::Cell(_)
            | View::Nullary(_) => None,
        };
        let payloads = [0, 1, 2, 3, 4, 1 << 31, 1 << 32, 1 << 47, PAYLOAD];
        for top in 0..=u64::from(u16::MAX) {
            for low in payloads {
                let bits = top << 48 | low;
                let got = Value::from_bits(bits).view();
                let want = by_the_table(bits);
                assert_eq!(float_bits(got), float_bits(want), "{bits:#x}");
                if float_bits(want).is_none() {
                    assert_eq!(got, want, "{bits:#x}");
                }
            }
        }
    }

    /// No two kinds share a word: each constructor's word reads back as what
    /// went in, so the tags do not overlap each other or the small Ints.
    #[test]
    fn each_kind_has_its_own_words() {
        let all = tagged();
        for (i, a) in all.iter().enumerate() {
            for b in all.iter().skip(i.saturating_add(1)) {
                assert_ne!(a.bits(), b.bits(), "{a:?} and {b:?}");
                assert_ne!(a.view(), b.view());
            }
        }
        // The tags the table keeps for later are in none of them.
        const FROZEN: u64 = 0x7FFE;
        const EMPTY_ARRAY: u64 = TAG_IMMEDIATE | 3;
        for v in all {
            assert_ne!(v.bits() >> 48, FROZEN, "{v:?}");
            assert_ne!(v.bits(), EMPTY_ARRAY, "{v:?}");
        }
    }

    /// The two words the file's header names: a NaN from either chip would
    /// read as a value of another type.
    #[test]
    fn a_stray_nan_would_be_nil_or_zero() {
        let arm = Value::from_bits(0x7FF8_0000_0000_0000);
        let x86 = Value::from_bits(0xFFF8_0000_0000_0000);
        assert_eq!(arm.view(), View::Nil);
        assert_eq!(x86.view(), View::Int(0));
        assert_eq!(arm.bits(), Value::NIL.bits());
        assert_eq!(Some(x86.bits()), Value::int(0).map(Value::bits));
    }

    /// Two words are both small Ints exactly when their `&` is one, which
    /// is the test an Int instruction makes on its two operands at once.
    #[test]
    fn two_small_ints_are_told_by_one_and() {
        let ints = [0, 1, -1, SMALL_INT_MIN, SMALL_INT_MAX];
        let mut words: Vec<Value> = tagged();
        words.extend([0.0, -0.0, 1.5, -1.5, f64::MAX, f64::MIN].map(Value::float));
        for a in &words {
            for b in &words {
                let both = a.small_int().is_some() && b.small_int().is_some();
                assert_eq!(a.bits() & b.bits() >= TAG_INT, both, "{a:?} {b:?}");
            }
        }
        for n in ints {
            assert!(Value::int(n).is_some_and(|v| v.bits() >= TAG_INT));
        }
    }
}
