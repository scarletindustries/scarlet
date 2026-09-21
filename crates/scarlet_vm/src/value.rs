//! The value word. Every Scarlet value is one 64-bit word.
//!
//! A float is stored as itself. Every other kind of value uses bit patterns
//! no stored float can have: a word whose top 13 bits are all set is a
//! negative quiet NaN, and Scarlet never stores a NaN (a float operation turns
//! one into `0.0` before it is kept). Such a word's top 16 bits say what it
//! holds, and its low 48 bits hold it:
//!
//! | top 16 bits | holds                                         |
//! |-------------|-----------------------------------------------|
//! | `0xFFF9`    | a small Int, 48-bit two's complement          |
//! | `0xFFFA`    | `Nil` (0), `False` (1) or `True` (2)          |
//! | `0xFFFB`    | a function with no captures, by its `FuncIdx` |
//! | `0xFFFC`    | a cell in the process's heap                  |
//!
//! Every other word is a float.

use std::fmt;

use scarlet_ir::core_ir::FuncIdx;

use crate::heap::Cell;

/// Set in every word that is not a float.
const TAGGED: u64 = 0xFFF8_0000_0000_0000;
const TAG: u64 = 0xFFFF_0000_0000_0000;
const PAYLOAD: u64 = 0x0000_FFFF_FFFF_FFFF;

const TAG_INT: u64 = 0xFFF9_0000_0000_0000;
const TAG_IMMEDIATE: u64 = 0xFFFA_0000_0000_0000;
const TAG_FUNC: u64 = 0xFFFB_0000_0000_0000;
const TAG_CELL: u64 = 0xFFFC_0000_0000_0000;

/// The range a small Int covers. Outside it an Int is a big int, which the VM
/// does not build yet.
const SMALL_INT_MIN: i64 = -(1 << 47);
const SMALL_INT_MAX: i64 = (1 << 47) - 1;

/// One value.
///
/// `Copy` on purpose: it is a word. A cell's reference count is kept by the
/// VM explicitly, against the process's own heap, not by Rust's `Clone` and
/// `Drop`: copying a `Value` does not add a reference.
///
/// No `PartialEq`: comparing the bits is not Scarlet's `==` (`0.0 == -0.0`, and
/// structural equality looks inside values).
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
}

impl Value {
    pub(crate) const NIL: Value = Value(TAG_IMMEDIATE);
    const FALSE: Value = Value(TAG_IMMEDIATE | 1);
    const TRUE: Value = Value(TAG_IMMEDIATE | 2);

    /// `n` as a small Int, or `None` when it needs a big int.
    pub(crate) fn int(n: i64) -> Option<Value> {
        (SMALL_INT_MIN..=SMALL_INT_MAX)
            .contains(&n)
            .then_some(Value(TAG_INT | (n as u64 & PAYLOAD)))
    }

    pub(crate) fn bool(b: bool) -> Value {
        if b { Value::TRUE } else { Value::FALSE }
    }

    /// A function that captures nothing. It needs no heap cell: its index is
    /// the whole of it.
    pub(crate) fn func(f: FuncIdx) -> Value {
        Value(TAG_FUNC | u64::from(f.0))
    }

    /// A heap cell. The value holds the one reference the caller gives it.
    pub(crate) fn cell(c: Cell) -> Value {
        Value(TAG_CELL | c.bits())
    }

    /// The cell this value points at, if it is one.
    pub(crate) fn as_cell(self) -> Option<Cell> {
        (self.0 & TAG == TAG_CELL).then(|| Cell::from_bits(self.0 & PAYLOAD))
    }

    pub(crate) fn view(self) -> View {
        let bits = self.0;
        if bits & TAGGED != TAGGED {
            return View::Float(f64::from_bits(bits));
        }
        let payload = bits & PAYLOAD;
        match bits & TAG {
            // Shift the payload's sign bit up to bit 63 and back, so the
            // arithmetic shift copies it into the top 16 bits.
            TAG_INT => View::Int(((payload << 16) as i64) >> 16),
            TAG_FUNC => View::Func(FuncIdx(payload as u32)),
            TAG_CELL => View::Cell(Cell::from_bits(payload)),
            // Only the constructors above make a tagged word, so what is
            // left is an immediate.
            _ => {
                debug_assert_eq!(bits & TAG, TAG_IMMEDIATE, "an untagged word {bits:#x}");
                match payload {
                    0 => View::Nil,
                    1 => View::Bool(false),
                    _ => View::Bool(true),
                }
            }
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

    #[test]
    fn small_ints_round_trip_at_both_ends() {
        for n in [0, 1, -1, 42, -42, SMALL_INT_MIN, SMALL_INT_MAX] {
            let v = Value::int(n).expect("in range");
            assert_eq!(v.view(), View::Int(n));
        }
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

    /// Every float Scarlet can store, which is every float but NaN, decodes
    /// as a float: none of them has a tagged word's top bits.
    #[test]
    fn no_float_looks_tagged() {
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
            f64::INFINITY,
            f64::NEG_INFINITY,
        ];
        for x in floats {
            let v = Value(x.to_bits());
            assert_eq!(v.view(), View::Float(x), "{x:e} decoded as {v:?}");
        }
    }
}
