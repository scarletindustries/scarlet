//! Slotting positional and labeled constructor arguments into declared-field
//! order. The typechecker, the elaborator and the exhaustiveness checker all
//! call this, so all three agree on which argument lands in which field.

use smallvec::SmallVec;

/// One slot per declared constructor field. Constructor arity is small, so the
/// common case stays off the heap.
pub type Slots<T> = SmallVec<[Option<T>; 4]>;

/// A malformed argument list. Each variant hands back the offending item, so a
/// caller needing its span carries the span in `T`. Only the compiler renders
/// these; elaboration and exhaustiveness discard them as already reported.
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
/// Overflowing positionals and unknown labels fill no slot. Generic over `L`
/// so callers can pass either interned `StrId`s or `&str`s.
///
/// `fields` is one entry per declared field, `None` for a field with no label.
/// Both the slot count and the label lookup come from this slice, so there is
/// no second width for a label table to drift from.
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
