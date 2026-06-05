//! Where a collection starts ([`Roots`]) and which pointers it may touch
//! ([`Classifier`]).
//!
//! These two types are the entire safety boundary of the collector:
//!
//! - The **root set** answers *"what is live?"* — everything reachable from
//!   the roots survives; everything else is garbage by omission. The VM
//!   implements [`Roots`] over its operand stack and call frames; if a
//!   `Value` is reachable from neither, the collector does not know it
//!   exists (this is exactly what the rooting rule protects).
//! - The **classifier** answers *"what is mine to move?"* — an address-range
//!   whitelist of the spaces being collected. A pointer that fails every
//!   range (a frozen-area constant, another process's object, the old
//!   generation during a minor) is left untouched and untraced.

use super::space::Space;
use crate::bytecode::value::Value;

/// The GC root set: every `Value` slot a collection must treat as live and
/// rewrite when its object moves.
pub trait Roots {
    fn for_each(&mut self, f: &mut dyn FnMut(&mut Value));
}

impl Roots for Vec<Value> {
    fn for_each(&mut self, f: &mut dyn FnMut(&mut Value)) {
        for v in self.iter_mut() {
            f(v);
        }
    }
}

impl<const N: usize> Roots for [Value; N] {
    fn for_each(&mut self, f: &mut dyn FnMut(&mut Value)) {
        for v in self.iter_mut() {
            f(v);
        }
    }
}

impl Roots for &mut [Value] {
    fn for_each(&mut self, f: &mut dyn FnMut(&mut Value)) {
        for v in self.iter_mut() {
            f(v);
        }
    }
}

/// Address-range whitelist of the spaces a copy evacuates from.
///
/// Ranges are disjoint (each is a distinct slab's used prefix) and kept
/// sorted by `push_space`, so [`covers`](Classifier::covers) — called once
/// per traced value slot in the Cheney inner loop — is a binary search
/// rather than a linear scan over every old-generation segment.
#[derive(Default)]
pub struct Classifier {
    /// Disjoint `(lo, hi)` ranges, sorted by `lo`.
    ranges: Vec<(usize, usize)>,
}

impl Classifier {
    pub fn new() -> Classifier {
        Classifier::default()
    }

    /// Whitelist a space's *used* range (nothing points into the slack).
    pub fn push_space(&mut self, space: &Space) {
        if space.used() > 0 {
            let base = space.base_addr();
            let range = (base, base + space.used() * std::mem::size_of::<u64>());
            let idx = self.ranges.partition_point(|&(lo, _)| lo < range.0);
            debug_assert!(
                self.ranges.get(idx).is_none_or(|&(lo, _)| range.1 <= lo)
                    && (idx == 0 || self.ranges[idx - 1].1 <= range.0),
                "classifier ranges must be disjoint"
            );
            self.ranges.insert(idx, range);
        }
    }

    pub fn covers(&self, addr: usize) -> bool {
        // The candidate range is the last one starting at or below `addr`.
        let idx = self.ranges.partition_point(|&(lo, _)| lo <= addr);
        idx > 0 && addr < self.ranges[idx - 1].1
    }
}
