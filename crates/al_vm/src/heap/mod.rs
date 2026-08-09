//! Per-process memory: mimalloc storage and non-atomic reference counting.
//!
//! A process owns all of its values outright, so a suspended process migrates
//! between scheduler threads as a plain Rust move.
//!
//! Objects are laid out `[ rc ][ header ][ payload… ]` and allocated with
//! `mi_malloc` from the calling thread's default heap. There is deliberately no
//! per-process `mi_heap_t`: a mimalloc heap is bound to its creating thread, so
//! a heap built on the spawner and used on a worker would allocate off-thread
//! (UB). `mi_free` is cross-thread safe, so freeing after a migration is fine.
//!
//! al values are immutable and acyclic by construction, so reference counting
//! needs no cycle collector. The `Clone`/`Drop` counting and the free-at-zero
//! work list (explicit, so deep lists cannot overflow the stack) live in
//! [`crate::bytecode::value`]. Frozen and global values are immortal and skip
//! counting; a `Binary`'s bytes sit behind a shared `Arc<[u8]>`.

mod proc_heap;

pub use proc_heap::ProcHeap;

// A `ProcHeap` crosses OS threads inside a migrating `Process`.
const _: () = crate::assert_send::<ProcHeap>();
