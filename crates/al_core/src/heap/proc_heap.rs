//! [`ProcHeap`]: one process's allocator handle under non-atomic reference
//! counting.
//!
//! `ProcHeap` is a thin handle over [`MiHeap`] (a zero-sized marker for
//! mimalloc's per-thread default heap). Objects are cheap to allocate and are
//! freed individually as their refcount reaches zero (`bytecode::value`); a
//! process's graph is reclaimed as its roots (stack, frames, result) drop at
//! death. The heap is acyclic by construction (immutable values), so reference
//! counting is complete without a cycle collector.
//!
//! There is no collector here: no rooting rule, no generations, no safepoints.

use super::mimheap::{MiHeap, rc_copy_graph, rc_publish_graph};
use crate::bytecode::value::Value;
use crate::frozen::FrozenBuilder;

/// A process's allocator handle: a zero-sized [`MiHeap`] marker over mimalloc's
/// per-thread default heap.
///
/// `ProcHeap` is trivially `Send` (static assert in `mod.rs`) — it holds no
/// thread-bound state. Objects are allocated from mimalloc's per-thread default
/// heap and freed with `mi_free`, which is cross-thread safe, so migrating the
/// owning `Process` to another scheduler is a plain move.
pub struct ProcHeap {
    mi: MiHeap,
}

impl ProcHeap {
    /// A fresh allocator handle for a new process.
    pub fn new() -> ProcHeap {
        ProcHeap { mi: MiHeap::new() }
    }

    // ---- allocation -------------------------------------------------------

    /// Allocate one reference-counted object of `words` (header + payload)
    /// words from mimalloc's per-thread default heap. The returned pointer is
    /// the object header; the refcount slot sits one word before it,
    /// initialized to 1. See [`MiHeap::alloc_object`].
    pub fn alloc_object(&self, words: usize) -> *mut u64 {
        self.mi.alloc_object(words).as_ptr()
    }

    // ---- spawn and frozen publish -----------------------------------------

    /// Spawn-side graph copy: deep-copy the closure graph reachable from
    /// `root` into a fresh child heap, returning the child heap and its own
    /// root. Sharing is preserved, `Binary` backings are shared by `Arc` bump
    /// (no byte copy), frozen references are shared untouched, and the parent
    /// graph is left intact. See [`rc_copy_graph`].
    pub fn spawn_copy(&mut self, root: &Value) -> (ProcHeap, Value) {
        let child = ProcHeap::new();
        let child_root = rc_copy_graph(root, &child.mi);
        (child, child_root)
    }

    /// Publish the graph reachable from `root` into the frozen area, returning
    /// the frozen root. References already in the frozen area are shared as-is.
    /// See [`rc_publish_graph`].
    pub fn publish_frozen(&mut self, builder: &mut FrozenBuilder, root: Value) -> Value {
        rc_publish_graph(&root, builder)
    }
}

/// The placeholder heap left behind by `suspend_current`'s `mem::take`. A
/// fresh empty mimalloc heap; cheap to create and destroy.
impl Default for ProcHeap {
    fn default() -> ProcHeap {
        ProcHeap::new()
    }
}

// `ProcHeap` needs no `Drop`: a process's objects are reference-counted, so
// they are freed as its `Value`s (stack, frames, result) drop at death — there
// is no per-process heap to tear down.
