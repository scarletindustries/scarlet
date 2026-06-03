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
#[derive(Default)]
pub struct Classifier {
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
            self.ranges
                .push((base, base + space.used() * std::mem::size_of::<u64>()));
        }
    }

    pub fn covers(&self, addr: usize) -> bool {
        self.ranges.iter().any(|&(lo, hi)| addr >= lo && addr < hi)
    }
}
