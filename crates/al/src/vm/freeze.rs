//! Freezing value graphs into the program-wide frozen area.
//!
//! Publishing a global deep-copies the binding's whole value graph into the
//! [`FrozenArea`](al_core::frozen::FrozenArea)
//! through the one shared Cheney primitive: [`freeze_global`] is
//! [`ProcHeap::publish_frozen`] — `al_core::heap::copy_graph` with the
//! frozen builder as the destination — so frozen publish, spawn-copy, and
//! GC are all the same evacuation loop. Frozen objects are verbatim images
//! of arena objects (same header, same payload layout), so the published
//! root word *is* a `Value`: every scheduler's globals table holds these
//! words and `Op::PushGlobal` pushes one with zero copying.
//!
//! Sharing is preserved by the copy's forwarding pointers: a subtree
//! referenced twice is frozen once and both referrers point at the same
//! frozen words. A pointer already inside the frozen area (a constant-pool
//! name, an already-published global referenced by a newer one) fails the
//! source classifier (the address-range check that identifies pointers into
//! the publishing process's own heap — only those are evacuated) and is
//! shared as-is — the frozen area never copies out of itself. The source
//! graph is untouched (Clone-mode copy restores every forwarded header), so
//! the publishing process keeps running on its own values.
//!
//! Binary backings stay off-heap and zero-copy: freezing a binary copies
//! only its arena box and bumps the backing `Arc` — a strong count the
//! frozen box holds for the rest of the program, because the frozen area is
//! never collected and never runs destructors — the price of an area
//! whose pointers must stay valid for the whole program.
//!
//! GC interaction: publication runs on the thread
//! that owns the source values, between safepoints — nothing here allocates
//! in the process arena or triggers a collection, and the destination is
//! the append-only frozen area, so source pointers stay valid throughout
//! the copy.

// Designated unsafe module: the `Send`/`Sync` impls for `FrozenValue` rest on
// the frozen-area publication protocol documented on the type.
#![allow(unsafe_code)]

use al_core::bytecode::Value;
use al_core::frozen::FrozenBuilder;
use al_core::heap::ProcHeap;

/// A published global: the root of a fully-written frozen value graph (or
/// an immediate), as a raw NaN-box word.
///
/// Publication order makes the word safe to share: `publish_global` is
/// handed the word only after [`freeze_global`] fully wrote the frozen
/// segment contents, the table store happens-before the `globals_version`
/// release-bump, and readers acquire-load the version before touching the
/// table. After publication the words are never written again, and the
/// [`FrozenArea`](al_core::frozen::FrozenArea) it points into is `Arc`-held
/// by the runtime's program (`Program::frozen`) for at least as long as
/// the table holding it, so the pointer cannot dangle while the value is
/// reachable.
#[derive(Clone, Copy)]
pub(super) struct FrozenValue(u64);

// SAFETY: see the type docs — the word either encodes an immediate or
// points into immutable, fully-published, never-moved frozen segments that
// outlive every table holding a `FrozenValue`.
unsafe impl Send for FrozenValue {}
unsafe impl Sync for FrozenValue {}

impl FrozenValue {
    /// Wrap an already-frozen root word. The caller must guarantee the
    /// `FrozenValue` contract: `v` is an immediate or points into fully
    /// written frozen segments ([`freeze_global`]'s return, or a word read
    /// back out of a globals table that was published through it).
    pub(super) fn new(v: Value) -> FrozenValue {
        FrozenValue(v.to_bits())
    }

    /// The frozen root as a `Value`: a plain word copy — frozen objects
    /// share the process-arena object layout, so the word is directly
    /// loadable on any scheduler with no decode and no allocation.
    pub(super) fn value(self) -> Value {
        Value::from_bits(self.0)
    }
}

/// Deep-copy `root`'s whole value graph out of `heap` into the frozen area
/// through `builder`, preserving sharing, and return the frozen root word.
///
/// This is the shared `copy_graph` in Clone mode with a frozen destination
/// ([`ProcHeap::publish_frozen`]): the source graph is left exactly as it
/// was, immediates and already-frozen pointers come back unchanged, and
/// `Arc` backings of binaries are bumped, not copied.
pub(super) fn freeze_global(
    heap: &mut ProcHeap,
    builder: &mut FrozenBuilder,
    root: Value,
) -> FrozenValue {
    // The second element counts words copied into the frozen area; only the
    // heap's own tests consume it (to assert sharing) — the VM keeps no
    // frozen-growth accounting, since the area is never collected.
    let (frozen, _words) = heap.publish_frozen(builder, root);
    FrozenValue::new(frozen)
}

#[cfg(test)]
mod tests {
    use super::*;
    use al_core::frozen::FrozenArea;
    use std::sync::Arc;

    fn builder() -> FrozenBuilder {
        Arc::new(FrozenArea::new()).builder()
    }

    #[test]
    fn immediates_freeze_to_themselves() {
        let mut heap = ProcHeap::with_young_capacity(64);
        let mut b = builder();
        for v in [
            Value::small_int(42),
            Value::float(1.5),
            Value::bool(true),
            Value::nil(),
        ] {
            let fv = freeze_global(&mut heap, &mut b, v);
            assert_eq!(fv.value().to_bits(), v.to_bits());
        }
    }

    #[test]
    fn frozen_graph_loads_zero_copy_and_preserves_sharing() {
        let mut heap = ProcHeap::with_young_capacity(4096);
        heap.note_ensured(4096);
        let s = Value::str_in(&mut heap, "shared");
        let t = Value::tuple_in(&mut heap, &[s, s, Value::small_int(7)]);

        let mut b = builder();
        let before = b.area().words_used();
        let fv = freeze_global(&mut heap, &mut b, t);
        let loaded = fv.value();

        // The load is the published word itself (zero-copy).
        let elems = loaded.as_tuple().expect("frozen tuple");
        assert_eq!(elems.len(), 3);
        assert_eq!(elems[0].as_str(), Some("shared"));
        assert_eq!(elems[2].as_int(), Some(7));
        // Sharing preserved, asserted via words allocated: one 6-byte string
        // (2 + 1 words) + the 3-tuple (2 + 3 words) — not two strings.
        assert_eq!(b.area().words_used() - before, 3 + 5);
        assert_eq!(
            elems[0].object_addr(),
            elems[1].object_addr(),
            "one frozen copy, two references"
        );
        // Clone-mode copy: the source graph is restored and still readable.
        let src = t.as_tuple().expect("source tuple intact");
        assert_eq!(src[0].as_str(), Some("shared"));
        assert_ne!(src[0].object_addr(), elems[0].object_addr());
    }

    #[test]
    fn frozen_pointers_are_shared_not_recopied() {
        let mut heap = ProcHeap::with_young_capacity(64);
        let mut b = builder();
        let s = b.str("constant");
        let used = b.area().words_used();
        let fv = freeze_global(&mut heap, &mut b, s);
        assert_eq!(b.area().words_used(), used, "no copy out of the area");
        assert_eq!(fv.value().to_bits(), s.to_bits());
    }

    #[test]
    fn binary_backing_is_shared_not_copied() {
        let backing: Arc<[u8]> = vec![0xAB; 1024].into();
        let mut heap = ProcHeap::with_young_capacity(256);
        heap.note_ensured(64);
        let bin = Value::binary_from_arc_in(&mut heap, Arc::clone(&backing), 8192);

        let mut b = builder();
        let count_before = Arc::strong_count(&backing);
        let fv = freeze_global(&mut heap, &mut b, bin);
        let loaded = fv.value();
        let view = loaded.as_binary().expect("frozen binary");
        assert_eq!(view.bit_len(), 8192);
        assert!(std::ptr::eq(
            view.backing().as_ptr(),
            backing.as_ref().as_ptr()
        ));
        // The frozen box holds its own strong count for the program
        // lifetime (the area never runs destructors): exactly one bump,
        // no byte copy.
        assert_eq!(Arc::strong_count(&backing), count_before + 1);
    }
}
