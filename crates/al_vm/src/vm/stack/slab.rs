//! Pooled per-process stack regions: one big mapping carved into fixed slots,
//! recycled through freelists. Never mmap per spawn — `mmap`/`munmap` take
//! `mmap_lock` for write on the one address space every scheduler thread
//! shares, so it does not scale past a couple of threads.
//!
//! Slot layout, low address to high:
//!
//! ```text
//!   [ guard page(s): PROT_NONE ][ ....... STACK_BYTES usable ....... ] <- top
//! ```
//!
//! The guard is at the LOW end because stacks grow down. An overflow faults
//! (named by [`super::fault`]) instead of writing into the next slot, which in
//! a pooled slab is another process's stack.
//!
//! The reserve is a hard cap: a stack storing its own interior addresses
//! cannot be relocated to grow.

#![allow(unsafe_code)]

use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use super::super::{VmError, lock};
use super::fault;

/// Usable bytes per process stack, not counting the guard.
pub const STACK_BYTES: usize = 256 * 1024;

/// Stacks carved per slab mapping. 64 slots ≈ 16.6 MB VSZ per slab at 4K pages.
const SLOTS_PER_SLAB: usize = 64;

/// How many slots a scheduler may cache locally before spilling to
/// [`ORPHANED_SLOTS`].
const LOCAL_FREE_CAP: usize = SLOTS_PER_SLAB;

/// Live (acquired, not yet released) stacks, shared by every scheduler's pool:
/// the VMA budget is per address space, not per pool.
static LIVE_STACKS: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// Per-thread syscall counters, so a test can assert on its own slabs
    /// while parallel tests map slabs of their own.
    static TL_MMAP_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static TL_MADVISE_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Slots released by a scheduler other than the one that will next spawn. Any
/// pool may adopt them; checked only when the local freelist is empty.
static ORPHANED_SLOTS: Mutex<Vec<Slot>> = Mutex::new(Vec::new());

fn page_size() -> usize {
    static PAGE: OnceLock<usize> = OnceLock::new();
    // SAFETY: sysconf is a plain query.
    *PAGE.get_or_init(|| unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize })
}

/// Guard + usable bytes per slot. The guard is one system page.
fn slot_bytes() -> usize {
    page_size() + STACK_BYTES
}

/// The pool's high-water mark, from the kernel's VMA ceiling. Each live stack
/// costs 2 VMAs, so spawn must fail as a value before mmap hits a raw ENOMEM
/// mid-slab. Non-Linux targets have no map-count ceiling.
fn default_high_water() -> usize {
    static CAP: OnceLock<usize> = OnceLock::new();
    *CAP.get_or_init(|| {
        #[cfg(target_os = "linux")]
        {
            let max_map_count = std::fs::read_to_string("/proc/sys/vm/max_map_count")
                .ok()
                .and_then(|s| s.trim().parse::<usize>().ok())
                .unwrap_or(65_530);
            max_map_count.saturating_sub(VMA_HEADROOM) / 2
        }
        #[cfg(not(target_os = "linux"))]
        {
            usize::MAX / 2
        }
    })
}

/// One free slot: the base of its guard page. The slab it points into is never
/// unmapped, so a Slot is never dangling.
struct Slot {
    guard_base: *mut u8,
}

// SAFETY: an address into a never-unmapped slab. The acquire/release
// discipline keeps one handle per slot, so sending it moves exclusive access.
unsafe impl Send for Slot {}

/// Map one slab, protect every slot's guard, advise NOHUGEPAGE, register it
/// with the fault handler, and return its slots. Slabs are never unmapped, so
/// parked stacks can migrate between schedulers with no lifetime bookkeeping.
fn map_slab() -> Result<Vec<Slot>, VmError> {
    let slot = slot_bytes();
    let total = slot * SLOTS_PER_SLAB;

    #[cfg(target_os = "linux")]
    const MAP_FLAGS: i32 = libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_STACK;
    #[cfg(not(target_os = "linux"))]
    const MAP_FLAGS: i32 = libc::MAP_PRIVATE | libc::MAP_ANON;

    // SAFETY: fresh anonymous mapping, constant arguments.
    let base = unsafe {
        libc::mmap(
            ptr::null_mut(),
            total,
            libc::PROT_READ | libc::PROT_WRITE,
            MAP_FLAGS,
            -1,
            0,
        )
    };
    TL_MMAP_CALLS.with(|c| c.set(c.get() + 1));
    if base == libc::MAP_FAILED {
        return Err(VmError::Io(std::io::Error::last_os_error()));
    }
    let base = base.cast::<u8>();

    // MADV_NOHUGEPAGE is not optional: with THP on, each process costs several
    // times the RSS. Under 6.12's anon-folio accounting `AnonHugePages` reads 0
    // either way, so the cost is invisible where someone would look for it.
    #[cfg(target_os = "linux")]
    {
        // SAFETY: advising our own fresh mapping.
        unsafe { libc::madvise(base.cast::<c_void>(), total, libc::MADV_NOHUGEPAGE) };
        TL_MADVISE_CALLS.with(|c| c.set(c.get() + 1));
    }

    let mut slots = Vec::with_capacity(SLOTS_PER_SLAB);
    for i in 0..SLOTS_PER_SLAB {
        // SAFETY: i * slot < total, so this is inside our own mapping, and
        // guard ranges are page-aligned.
        let guard_base = unsafe { base.add(i * slot) };
        // SAFETY: as above.
        let rc =
            unsafe { libc::mprotect(guard_base.cast::<c_void>(), page_size(), libc::PROT_NONE) };
        if rc != 0 {
            return Err(VmError::Io(std::io::Error::last_os_error()));
        }
        slots.push(Slot { guard_base });
    }

    fault::register_slab(base as usize, total, slot, page_size());
    Ok(slots)
}

/// An acquired stack region: guard page + `STACK_BYTES` usable, exclusively
/// owned until [`StackPool::release`] or drop. Holding the handle is the
/// permission to run on the region.
#[derive(Debug)]
pub struct StackHandle {
    guard_base: *mut u8,
}

// SAFETY: exclusive ownership of a never-unmapped region. The handle is the
// only permission to touch the slot, so moving it moves the access.
unsafe impl Send for StackHandle {}

impl StackHandle {
    /// The initial stack pointer: the slot's high end. Asserted 16-aligned
    /// rather than rounded, so layout drift is loud.
    pub fn top(&self) -> *mut u8 {
        // SAFETY: one past our own slot's end; never dereferenced by us.
        let end = unsafe { self.guard_base.add(slot_bytes()) };
        debug_assert_eq!(end as usize % 16, 0);
        end
    }

    /// Record which AL process runs on this stack, for the fault handler's
    /// overflow report. 0 is unowned.
    pub fn set_owner(&self, pid: u64) {
        fault::set_slot_owner(self.guard_base as usize, pid);
    }
}

impl Drop for StackHandle {
    /// A handle dropped without [`StackPool::release`] orphans its slot to the
    /// shared list instead of leaking it.
    fn drop(&mut self) {
        self.set_owner(0);
        LIVE_STACKS.fetch_sub(1, Ordering::Relaxed);
        lock(&ORPHANED_SLOTS).push(Slot {
            guard_base: self.guard_base,
        });
    }
}

/// A per-scheduler stack pool: a plain freelist over the shared slabs.
pub struct StackPool {
    free: Vec<Slot>,
    high_water: usize,
}

impl Default for StackPool {
    fn default() -> Self {
        Self::new()
    }
}

impl StackPool {
    pub fn new() -> StackPool {
        StackPool {
            free: Vec::new(),
            high_water: default_high_water(),
        }
    }

    /// A pool with an artificially low cap — tests only.
    #[cfg(test)]
    pub fn with_high_water(cap: usize) -> StackPool {
        StackPool {
            free: Vec::new(),
            high_water: cap,
        }
    }

    /// Take a stack for process `pid`. Steady state pops the freelist with no
    /// syscall; growth maps one slab. Exhaustion is a `VmError` a spawn can
    /// surface, never a raw ENOMEM from a mid-slab mmap.
    pub fn acquire(&mut self, pid: u64) -> Result<StackHandle, VmError> {
        let live = LIVE_STACKS.load(Ordering::Relaxed);
        if live >= self.high_water {
            return Err(VmError::StackBudget {
                live,
                cap: self.high_water,
            });
        }
        let slot = match self.free.pop() {
            Some(s) => s,
            None => {
                let mut orphaned = lock(&ORPHANED_SLOTS);
                match orphaned.pop() {
                    Some(s) => s,
                    None => {
                        drop(orphaned);
                        let mut fresh = map_slab()?;
                        // One slot returns now; the rest seed the freelist.
                        let Some(first) = fresh.pop() else {
                            return Err(VmError::internal("slab mapped zero slots"));
                        };
                        self.free.extend(fresh);
                        first
                    }
                }
            }
        };
        LIVE_STACKS.fetch_add(1, Ordering::Relaxed);
        let handle = StackHandle {
            guard_base: slot.guard_base,
        };
        handle.set_owner(pid);
        Ok(handle)
    }

    /// Return a stack to this scheduler's freelist. The region is recycled
    /// as-is, no zeroing: callers must not read a stack they have not written.
    pub fn release(&mut self, handle: StackHandle) {
        handle.set_owner(0);
        LIVE_STACKS.fetch_sub(1, Ordering::Relaxed);
        let slot = Slot {
            guard_base: handle.guard_base,
        };
        // A process can spawn on one scheduler and die on another, so slots do
        // not come home. Unbounded, a scheduler that mostly frees would hoard
        // slots while the one that mostly spawns carves fresh slabs, drifting
        // past `vm.max_map_count` without `LIVE_STACKS` ever noticing. The cap
        // keeps the same-scheduler case off the mutex and spills only surplus.
        if self.free.len() < LOCAL_FREE_CAP {
            self.free.push(slot);
        } else {
            lock(&ORPHANED_SLOTS).push(slot);
        }
        std::mem::forget(handle);
    }

    /// Test hook: (slab mmaps, NOHUGEPAGE madvises) issued by this thread.
    #[cfg(test)]
    pub fn thread_syscalls() -> (usize, usize) {
        (
            TL_MMAP_CALLS.with(std::cell::Cell::get),
            TL_MADVISE_CALLS.with(std::cell::Cell::get),
        )
    }
}

impl Drop for StackPool {
    /// A dying pool's free slots go to the shared list, or they would strand.
    fn drop(&mut self) {
        lock(&ORPHANED_SLOTS).append(&mut self.free);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slab_reuse_is_allocation_free() {
        let mut pool = StackPool::new();
        let warm = pool.acquire(1).expect("warmup stack");
        pool.release(warm);
        let (mmaps_before, _) = StackPool::thread_syscalls();
        for i in 0..10_000 {
            let h = pool.acquire(i).expect("pooled stack");
            pool.release(h);
        }
        let (mmaps_after, _) = StackPool::thread_syscalls();
        assert_eq!(
            mmaps_after, mmaps_before,
            "acquire/release must not touch mmap after warmup"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn nohugepage_is_set() {
        // Hold handles until this thread is forced to map a fresh slab: the
        // freelist and orphan list may satisfy early acquires.
        let mut pool = StackPool::new();
        let (m0, _) = StackPool::thread_syscalls();
        let mut held = Vec::new();
        while StackPool::thread_syscalls().0 == m0 {
            held.push(pool.acquire(1).expect("stack"));
        }
        let (mmaps, madvises) = StackPool::thread_syscalls();
        assert!(mmaps > m0);
        assert_eq!(mmaps, madvises, "every mapped slab must carry the advice");
        for h in held {
            pool.release(h);
        }
    }

    #[test]
    fn vma_budget_caps_spawn() {
        let mut pool = StackPool::with_high_water(0);
        match pool.acquire(1) {
            Err(VmError::StackBudget { .. }) => {}
            other => panic!("expected StackBudget, got {other:?}"),
        }
    }

    #[test]
    fn released_slot_is_reusable_and_writable() {
        let mut pool = StackPool::new();
        let a = pool.acquire(7).expect("stack");
        let a_base = a.guard_base as usize;
        let top = a.top();
        // Touch the whole usable range; a fault would fail the test.
        unsafe {
            for off in (1..=STACK_BYTES).step_by(4096) {
                top.sub(off).write(0xAB);
            }
        }
        pool.release(a);
        let b = pool.acquire(8).expect("reused stack");
        assert_eq!(
            b.guard_base as usize, a_base,
            "LIFO freelist reuses the slot"
        );
        assert_eq!(b.top() as usize % 16, 0);
        pool.release(b);
    }
}
