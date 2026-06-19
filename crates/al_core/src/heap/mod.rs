//! Per-process memory: arena heaps and the copying collector.
//!
//! This module is the al runtime's entire memory model. It exists so that a
//! lightweight process *owns* all of its values outright — which is what
//! lets a suspended process migrate between scheduler threads as a plain
//! Rust move, and what lets process death free everything in one shot.
//!
//! # The model in one diagram
//!
//! ```text
//!   Process ──owns──> ProcHeap
//!                      ├─ young from-space   [ live ██████ | free ... ]  ← alloc bumps here
//!                      ├─ young to-space     [ empty until a minor GC ]
//!                      ├─ old (tenured)      [ seg ][ seg ][ seg ]
//!                      └─ off-heap list      Binary boxes that own an Arc<[u8]>
//!
//!   stack / frames ──point into──> the arena spaces      (the GC root set)
//!   constants / globals ──live in──> the frozen area     (never collected,
//!                                                         shared by all threads,
//!                                                         skipped by every GC)
//! ```
//!
//! # The six ideas (everything else is consequence)
//!
//! 1. **A `Value` is a tagged word.** Immediates (ints, floats, …) are
//!    self-contained; heap values carry a raw pointer into an arena or the
//!    frozen area. No `Rc`, no refcounts — cloning a `Value` copies a `u64`.
//!    (Owned by `bytecode::value`; this module consumes its layout hooks.)
//! 2. **Allocation is a pointer bump** in a space the process owns
//!    ([`space`]).
//! 3. **Reclamation is evacuation**: copy what is still reachable to a fresh
//!    space, drop the rest by never visiting it. One Cheney loop,
//!    [`copy::copy_graph`], serves minor GC, major GC, spawn, and frozen
//!    publish — they differ only in source ranges, destination, and whether
//!    the source survives ([`copy`], destinations in [`dest`]).
//! 4. **al values are immutable and acyclic**, so an object can only
//!    reference older objects. Old→young pointers are *impossible*: the
//!    generational collector needs no write barriers and no remembered
//!    sets, and tenuring by address (the high-water mark) is sound
//!    ([`dest::MinorDest`]).
//! 5. **The rooting rule**: the collector moves objects and can only fix
//!    references it can see — the operand stack and frames. So every opcode
//!    reserves its allocation up front (`ensure(need)`) *while its operands
//!    are still on the VM stack*, then pops and allocates collection-free.
//!    Enforced by `AL_GC_STRESS=1` (collect at every safepoint) and a debug
//!    watermark in `alloc_young` ([`flags`], [`proc_heap`]).
//! 6. **What the arena cannot own lives on lists the collector sweeps**:
//!    `Binary` bytes sit off-heap behind an `Arc` whose count the off-heap
//!    list releases for dead boxes — Cheney runs no destructors
//!    ([`copy::sweep_off_heap_list`]).
//!
//! # Reading order
//!
//! | file           | the one thing it does                                  |
//! |----------------|--------------------------------------------------------|
//! | [`space`]      | own words; bump-allocate them                          |
//! | [`roots`]      | say what is live ([`Roots`]) and what is movable       |
//! |                | ([`Classifier`])                                       |
//! | [`copy`]       | the Cheney loop: evacuate, forward, preserve sharing   |
//! | [`dest`]       | the four places a copy can land                        |
//! | [`proc_heap`]  | policy: when to collect / tenure / grow / shrink;      |
//! |                | spawn and frozen publish                               |
//! | [`sizing`]     | how big the next space is                              |
//! | [`flags`]      | `AL_GC_STRESS`, `AL_HEAP_STATS`                        |
//!
//! # The life of a value
//!
//! `"a" ++ "b"` in a process: the concat opcode peeks both lengths (operands
//! still rooted), calls `ensure(need)` — which may run a minor GC, moving
//! both strings and rewriting the stack slots that point at them — then pops
//! the (fresh) pointers, bump-allocates the result, and pushes it. If the
//! result survives until the next minor it is evacuated to the to-space; if
//! it survives another it tenures into old; if the process spawns a closure
//! capturing it, `spawn_seed` clones it into the child's fresh arena; if it
//! is published as a global, `publish_frozen` clones it into the frozen
//! area where every thread can read it forever. If none of those: it is
//! never visited again, and the next collection reclaims its space by
//! omission.

mod copy;
mod dest;
mod flags;
mod mimheap;
mod proc_heap;
mod roots;
mod sizing;
mod space;

#[cfg(test)]
mod tests;

// The crate-facing surface is deliberately this small: the VM owns a
// `ProcHeap` and implements `Roots` over its operand stack and frames —
// nothing else. `Classifier`, `HeapStats`, and `Space` are public only
// because they appear in that surface's signatures (`ProcHeap::classifier`,
// `ProcHeap::stats`, `Classifier::push_space`). The copy engine, the
// destinations, sizing, and the env flags are internal machinery; submodules
// reach them by path, not through re-exports.
pub use proc_heap::{HeapStats, ProcHeap};
pub use roots::{Classifier, Roots};
pub use space::Space;

// A `ProcHeap` crosses OS threads inside a migrating `Process`; migration is
// a plain move, which is only sound because the heap owns its memory.
const _: () = {
    const fn assert_send<T: Send>() {}
    assert_send::<ProcHeap>();
    assert_send::<Space>();
};
