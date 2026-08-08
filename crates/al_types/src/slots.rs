//! Slotting positional and labeled constructor arguments into declared-field
//! order. One algorithm shared by the typechecker's `slot_ctor_args`, the
//! elaborator ([`super::elaborate`] / [`super::elaborate_pat`]), and the
//! exhaustiveness checker — so all three agree, item for item, on which
//! argument lands in which field and which arguments are malformed.

use smallvec::SmallVec;

/// One slot per declared constructor field. Constructor arity is small, so the
/// common case stays off the heap.
pub type Slots<T> = SmallVec<[Option<T>; 4]>;

/// A malformed argument list. Each variant hands back the offending item
/// itself, so a caller that needs its span carries the span in `T` rather than
/// re-indexing the sequence it passed in. Elaboration and the exhaustiveness
/// checker discard these (the typechecker has already reported them); the
/// compiler renders them.
pub enum SlotError<L, T> {
    /// Positional item past the last declared field.
    ExtraPositional(T),
    /// Label naming no declared field, with the label.
    UnknownLabel(L, T),
    /// Item that landed on an already-filled field, with that field's index.
    Duplicate(T, usize),
}

/// Slot positional and labeled items into declared-field order: positionals
/// fill slots left-to-right, a labeled item lands on its declared index.
/// Overflowing positionals and unknown labels fill no slot. Generic over the
/// label representation `L` — the typechecker's `slot_ctor_args` and the
/// elaborator pass interned `StrId`s, the exhaustiveness checker passes
/// `&str`s — so all consumers run the one algorithm.
///
/// `fields` is the one declared-fields description: one entry per field, in
/// declared order, `None` for a field with no label (tuple/array positions).
/// Both the slot count and the label lookup come from this same slice, so a
/// label table that disagrees with the arity is unspellable — there is no
/// second width to drift from.
pub fn slot_labeled<L: PartialEq + Copy, T>(
    fields: &[Option<L>],
    items: impl IntoIterator<Item = (Option<L>, T)>,
) -> (Slots<T>, Vec<SlotError<L, T>>) {
    let mut by_pos: Slots<T> = fields.iter().map(|_| None).collect();
    let mut errors = Vec::new();
    let mut next_pos = 0usize;
    for (label, val) in items {
        let idx = match label {
            None => {
                let i = next_pos;
                next_pos += 1;
                if i >= fields.len() {
                    errors.push(SlotError::ExtraPositional(val));
                    continue;
                }
                i
            }
            Some(label) => match fields.iter().position(|&f| f == Some(label)) {
                Some(i) => i,
                None => {
                    errors.push(SlotError::UnknownLabel(label, val));
                    continue;
                }
            },
        };
        // First item to claim a field keeps it; every later one is reported.
        if by_pos[idx].is_some() {
            errors.push(SlotError::Duplicate(val, idx));
        } else {
            by_pos[idx] = Some(val);
        }
    }
    (by_pos, errors)
}
