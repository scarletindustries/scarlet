//! Freezing value graphs into the program-wide frozen area.
//!
//! Publishing a global deep-copies the binding's value graph into the
//! [`FrozenArea`](crate::frozen::FrozenArea). Frozen objects are verbatim
//! images of arena objects, minus the refcount prefix and marked immortal, so
//! the published root word *is* a `Value` and `Op::PushGlobal` pushes it with
//! zero copying.
//!
//! Sharing is preserved by a `src -> frozen` address map, and an
//! already-immortal pointer is shared as-is: the area never copies out of
//! itself. Freezing a binary copies only its arena box and bumps the backing
//! `Arc`, a strong count the frozen box then holds for the rest of the
//! program because the area never runs destructors.
//!
//! Publication runs on the thread owning the source values, and the source is
//! only read, never moved, so its pointers stay valid throughout the copy.

// The `Send`/`Sync` impls for `FrozenValue` rest on the publication protocol
// documented on the type.
#![allow(unsafe_code)]

use crate::bytecode::Value;
use crate::frozen::FrozenBuilder;
use crate::heap::ProcHeap;

/// A published global: the root of a fully-written frozen value graph (or
/// an immediate), as a raw NaN-box word.
///
/// Publication order makes the word safe to share: it reaches
/// `publish_global` only after [`freeze_global`] wrote the segment, the table
/// store happens-before the `globals_version` release-bump, and readers
/// acquire-load that version first. The words are never written again, and
/// `Program::frozen` keeps the area alive at least as long as the table.
#[derive(Clone, Copy)]
pub(super) struct FrozenValue(u64);

// SAFETY: the word is either an immediate or points into immutable,
// fully-published frozen segments that outlive every table holding one.
unsafe impl Send for FrozenValue {}
unsafe impl Sync for FrozenValue {}

impl FrozenValue {
    /// Wrap an already-frozen root word. Private so only [`freeze_global`]
    /// can mint one and the `Send`/`Sync` contract holds by construction.
    fn new(v: Value) -> FrozenValue {
        FrozenValue(v.to_bits())
    }

    /// The frozen root as a `Value`: a plain word copy, loadable on any
    /// scheduler with no decode and no allocation.
    pub(super) fn value(self) -> Value {
        // SAFETY: minted by `freeze_global` from a published frozen graph, so
        // it is an immediate or immortal encoding that rc never touches.
        unsafe { Value::from_bits(self.0) }
    }
}

/// Deep-copy `root`'s value graph into the frozen area, preserving sharing.
/// The source is left as it was, immediates and already-frozen pointers come
/// back unchanged, and binary backings are bumped rather than copied.
pub(super) fn freeze_global(builder: &mut FrozenBuilder, root: &Value) -> FrozenValue {
    FrozenValue::new(ProcHeap::publish_frozen(builder, root).into_value())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frozen::FrozenArea;
    use std::sync::Arc;

    fn builder() -> FrozenBuilder {
        Arc::new(FrozenArea::new()).builder()
    }

    #[test]
    fn immediates_freeze_to_themselves() {
        let mut b = builder();
        for v in [
            Value::small_int(42),
            Value::float(1.5),
            Value::bool(true),
            Value::nil(),
        ] {
            let fv = freeze_global(&mut b, &v);
            assert_eq!(fv.value().to_bits(), v.to_bits());
        }
    }

    #[test]
    fn frozen_graph_loads_zero_copy_and_preserves_sharing() {
        let mut heap = ProcHeap::new();
        let s = Value::str_in(&mut heap, "shared");
        let t = Value::tuple_in(&mut heap, &[s.clone(), s.clone(), Value::small_int(7)]);

        let mut b = builder();
        let before = b.area().words_used();
        let fv = freeze_global(&mut b, &t);
        let loaded = fv.value();

        // The load is the published word itself (zero-copy).
        let elems = loaded.as_tuple().expect("frozen tuple");
        assert_eq!(elems.len(), 3);
        assert_eq!(elems[0].as_str(), Some("shared"));
        assert_eq!(elems[2].as_int(), Some(7));
        // One 6-byte string (2 + 1 words) plus the 3-tuple (2 + 3), not two
        // strings.
        assert_eq!(b.area().words_used() - before, 3 + 5);
        assert_eq!(
            elems[0].object_addr(),
            elems[1].object_addr(),
            "one frozen copy, two references"
        );
        // The source graph is still readable.
        let src = t.as_tuple().expect("source tuple intact");
        assert_eq!(src[0].as_str(), Some("shared"));
        assert_ne!(src[0].object_addr(), elems[0].object_addr());
    }

    #[test]
    fn frozen_pointers_are_shared_not_recopied() {
        let mut b = builder();
        let s = b.str("constant").into_value();
        let used = b.area().words_used();
        let fv = freeze_global(&mut b, &s);
        assert_eq!(b.area().words_used(), used, "no copy out of the area");
        assert_eq!(fv.value().to_bits(), s.to_bits());
    }

    #[test]
    fn binary_backing_is_shared_not_copied() {
        let backing: Arc<[u8]> = vec![0xAB; 1024].into();
        let mut heap = ProcHeap::new();
        let bin = Value::binary_from_arc_in(&mut heap, Arc::clone(&backing), 8192);

        let mut b = builder();
        let count_before = Arc::strong_count(&backing);
        // The source is only read; `bin` stays live through the assertion.
        let fv = freeze_global(&mut b, &bin);
        let loaded = fv.value();
        let view = loaded.as_binary().expect("frozen binary");
        assert_eq!(view.bit_len(), 8192);
        assert!(std::ptr::eq(
            view.backing().as_ptr(),
            backing.as_ref().as_ptr()
        ));
        // One bump, no byte copy; the frozen box holds that count forever.
        assert_eq!(Arc::strong_count(&backing), count_before + 1);
    }
}
