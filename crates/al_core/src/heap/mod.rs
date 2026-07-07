//! Per-process memory: mimalloc storage and non-atomic reference counting.
//!
//! This module is the al runtime's memory model. A lightweight process *owns*
//! all of its values outright — which is what lets a suspended process migrate
//! between scheduler threads as a plain Rust move.
//!
//! # The model in one diagram
//!
//! ```text
//!   Process ──owns──> ProcHeap (a zero-sized allocator marker)
//!                          │
//!                          └─ allocates from mimalloc's per-thread DEFAULT heap
//!                             (mi_malloc); each object carries a refcount word:
//!                             [ rc ][ header ][ payload… ]
//!
//!   stack / frames ──hold owned──> Value handles (each edge is one count)
//!   constants / globals ──live in──> the frozen area (immortal, never counted,
//!                                                     shared by every thread)
//! ```
//!
//! # The ideas (everything else is consequence)
//!
//! 1. **A `Value` is a tagged word** ([`crate::bytecode::value`]). Immediates
//!    (ints, floats, …) are self-contained; a heap value carries a raw pointer
//!    to an object's header. A `Value` is **not `Copy`**: `Clone` increments
//!    the object's refcount, `Drop` decrements it and frees at zero.
//! 2. **Allocation is `mi_malloc`** from the calling thread's default heap, with
//!    a refcount word prefixed before the header ([`proc_heap`]). There is no
//!    per-process `mi_heap_t`: a heap is bound to its creating thread, so a
//!    per-process heap built on the spawner and run on a worker would be
//!    allocated off-thread (UB). `mi_free` is cross-thread safe, so an object
//!    allocated on one core and freed on another after a migration is fine.
//! 3. **Reclamation is reference counting.** An object is freed the instant its
//!    last referrer drops; a process's whole graph is reclaimed as its roots
//!    (stack, frames, result) drop at death. Freeing walks an explicit work
//!    list ([`crate::bytecode::value`]), never native `Drop` recursion, so a
//!    deep list cannot overflow the stack.
//! 4. **al values are immutable and acyclic by construction**, so reference
//!    counting is *complete* — no cycle can form, so there is no cycle
//!    collector. (Closures capture by value; self-recursion uses the live call
//!    frame; mutual recursion resolves through the immortal global table.)
//! 5. **Two things are shared and keep their own treatment**: frozen/global
//!    values are *immortal* (the header's immortal bit; reference counting skips
//!    them), and a `Binary`'s bytes sit behind an `Arc<[u8]>` (atomic, shared
//!    cross-process) whose count is released when the box's count hits zero.
//!
//! # Reading order
//!
//! | file           | the one thing it does                                  |
//! |----------------|--------------------------------------------------------|
//! | [`proc_heap`]  | the per-process allocator handle (`ProcHeap`):         |
//! |                | allocate/free via mimalloc; spawn + freeze graph copies|
//!
//! The refcount semantics themselves (`Clone`/`Drop`, the free-at-zero work
//! list, `store_child`/`move_child`) live in [`crate::bytecode::value`].

mod proc_heap;

// The crate-facing surface is deliberately small: the VM owns a `ProcHeap`,
// allocates objects through it, and lets reference counting reclaim them.
pub use proc_heap::ProcHeap;

// A `ProcHeap` crosses OS threads inside a migrating `Process`. It is a
// zero-sized allocator marker, so the move is trivially sound; the objects it
// allocated travel as plain `Value` words and are freed (via `mi_free`, which
// is cross-thread safe) wherever the process next runs.
const _: () = {
    const fn assert_send<T: Send>() {}
    assert_send::<ProcHeap>();
};
