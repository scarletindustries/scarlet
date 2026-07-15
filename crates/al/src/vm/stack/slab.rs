//! Pooled per-process stack regions: one big mapping carved into fixed
//! slots, recycled through freelists. **Never mmap-per-spawn**: measured on
//! mercury (6.12, x86_64), mmap+mprotect+fault+munmap per spawn costs
//! 8,456 ns against AL's real 155 ns spawn — 54× — and it cannot scale,
//! because `mmap`/`munmap` take `mmap_lock` for write on the one address
//! space every scheduler thread shares (105k spawn+free/s at 1 thread →
//! 118k at 16). The pooled slab measures 0.5 ns single-thread, 7.1 ns at 8.
//!
//! Slot layout, low address to high:
//!
//! ```text
//!   [ guard page(s): PROT_NONE ][ ....... STACK_BYTES usable ....... ] <- top
//! ```
//!
//! The guard sits at the LOW end because stacks grow down: an overflow walks
//! into PROT_NONE and faults (named by [`super::fault`]) instead of silently
//! writing into the next slot — which in a pooled slab is another process's
//! stack. Measured cost of keeping it: allocation 5,524 ns vs 2,471 ns
//! without, and it doubles the VMA count; both are absorbed by pooling, and
//! the alternative is cross-process corruption. It stays.
//!
//! Sizing (measured, one page touched per process, NOHUGEPAGE + guard):
//! RSS/proc is flat at ~4,097 B across every reserve size — only page tables
//! (`reserve/512` B/proc) and VSZ scale with the reserve. 256K costs
//! 4,610 B/proc total at 100k live (461 MB, BEAM-comparable) and bounds
//! non-tail native recursion at ~4,000 frames; 1M quadruples the page tables
//! for depth nothing needs yet. The reserve is a hard cap — a stack that
//! stores its own interior addresses cannot be relocated to grow.

#![allow(unsafe_code)]

use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use super::super::{VmError, lock};
use super::fault;

/// Usable bytes per process stack, not counting the guard.
pub const STACK_BYTES: usize = 256 * 1024;

/// Stacks carved per slab mapping. 64 slots ≈ 16.6 MB VSZ per slab at 4K
/// pages: coarse enough that steady-state spawn touches no syscalls, small
/// enough that an idle program reserves little.
const SLOTS_PER_SLAB: usize = 64;

/// VMAs left for everything that is not a process stack (binary + libs,
/// mimalloc arenas, JIT mappings, thread stacks). Each live slot costs
/// exactly 2 VMAs (guard + usable — measured 2.00/proc at every reserve
/// size and strategy).
const VMA_HEADROOM: usize = 2048;

/// Global count of live (acquired, not yet released) stacks, shared by all
/// schedulers' pools: the VMA budget is per address space, not per pool.
static LIVE_STACKS: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// This thread's share of the syscall counters — what lets a test
    /// assert "MY acquire/release loop issued zero mmaps" (and "my slabs
    /// were all advised") while parallel tests map slabs of their own.
    static TL_MMAP_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static TL_MADVISE_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Slots released by a scheduler other than the one that will next spawn
/// (process died while migrated, or a pool dropped): any pool may adopt
/// them. Checked only when the local freelist is empty.
static ORPHANED_SLOTS: Mutex<Vec<Slot>> = Mutex::new(Vec::new());

fn page_size() -> usize {
    static PAGE: OnceLock<usize> = OnceLock::new();
    // SAFETY: sysconf is a plain query.
    *PAGE.get_or_init(|| unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize })
}

/// Guard + usable bytes per slot. The guard is one system page (16K on
/// Apple Silicon — still one page).
fn slot_bytes() -> usize {
    page_size() + STACK_BYTES
}

/// The pool's high-water mark, derived from the kernel's VMA ceiling: each
/// live stack costs exactly 2 VMAs, so spawn must fail *as a value* before
/// mmap fails with a raw ENOMEM mid-slab. Kernel default `vm.max_map_count`
/// is 65,530 → ~31k stacks; systemd ≥ 256 raises it to 1,048,576. Non-Linux
/// targets have no map-count ceiling.
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

/// One free slot: the base of its guard page. Plain data; the mapping it
/// points into is owned by the process-wide slab registry and never
/// unmapped, so a Slot is never dangling.
struct Slot {
    guard_base: *mut u8,
}

// SAFETY: a slot is an address into a never-unmapped slab; handing it to
// another thread hands exclusive permission to run on it (enforced by the
// acquire/release discipline, one handle per slot).
unsafe impl Send for Slot {}

/// Map one slab, protect every slot's guard, advise NOHUGEPAGE, register it
/// with the fault handler, and return its slots. Called only when every
/// freelist is empty; the mapping is process-lifetime by design (slots
/// recycle; slabs never unmap, so parked stacks can migrate between
/// schedulers without lifetime bookkeeping).
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

    // MADV_NOHUGEPAGE is not optional. Measured on mercury (kernel 6.12,
    // THP=always): a touched 8M reserve costs 230,787 B RSS per process
    // without it, 4,103 B with it — 56×; on this slab's 64K-class slots the
    // measured difference is 18,116 B/proc vs 4,098 B — 4.4×. And
    // `AnonHugePages` in /proc/self/status reports 0 EITHER WAY under
    // 6.12's anon-folio accounting, so the cost is invisible exactly where
    // someone would go looking. Advisory: absent kernels just skip it.
    #[cfg(target_os = "linux")]
    {
        // SAFETY: advising our own fresh mapping.
        unsafe { libc::madvise(base.cast::<c_void>(), total, libc::MADV_NOHUGEPAGE) };
        TL_MADVISE_CALLS.with(|c| c.set(c.get() + 1));
    }

    let mut slots = Vec::with_capacity(SLOTS_PER_SLAB);
    for i in 0..SLOTS_PER_SLAB {
        // SAFETY: i * slot < total; protecting page-aligned guard ranges
        // inside our own mapping.
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

/// An acquired stack region: guard page + `STACK_BYTES` of usable space,
/// exclusively owned until [`StackPool::release`] (or drop, which orphans
/// the slot back to the shared list). Owning the handle is the permission
/// to run on the region; the mapping itself is process-lifetime.
#[derive(Debug)]
pub struct StackHandle {
    guard_base: *mut u8,
}

// SAFETY: exclusive ownership of a never-unmapped region; migrating a
// parked process moves the handle and with it the only permission to touch
// the slot.
unsafe impl Send for StackHandle {}

impl StackHandle {
    /// The initial stack pointer: the slot's high end, 16-aligned (the whole
    /// slot is page-aligned, so the high end already is — asserted rather
    /// than rounded so layout drift is loud).
    pub fn top(&self) -> *mut u8 {
        // SAFETY: one past our own slot's end; never dereferenced by us.
        let end = unsafe { self.guard_base.add(slot_bytes()) };
        debug_assert_eq!(end as usize % 16, 0);
        end
    }

    /// Record which AL process runs on this stack, for the fault handler's
    /// overflow report. 0 = unowned.
    pub fn set_owner(&self, pid: u64) {
        fault::set_slot_owner(self.guard_base as usize, pid);
    }
}

impl Drop for StackHandle {
    /// A handle dropped without [`StackPool::release`] (process died on a
    /// foreign scheduler, pool already gone) orphans its slot to the shared
    /// list instead of leaking it.
    fn drop(&mut self) {
        self.set_owner(0);
        LIVE_STACKS.fetch_sub(1, Ordering::Relaxed);
        lock(&ORPHANED_SLOTS).push(Slot {
            guard_base: self.guard_base,
        });
    }
}

/// A per-scheduler stack pool: a plain freelist over the shared slabs.
/// Not `Sync` — each scheduler owns one, exactly like its socket tables.
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

    /// Take a stack for process `pid`. Steady state pops the freelist and
    /// issues no syscall; growth maps one slab. Fails as a `VmError` — a
    /// value a spawn can surface — never a raw ENOMEM from mid-slab mmap.
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
                        // One slot returns now; the rest seed this
                        // scheduler's freelist. A slab always carves
                        // SLOTS_PER_SLAB > 0 slots.
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
    /// as-is — no zeroing, no unmap; the next owner overwrites what it uses
    /// (a fresh acquire is indistinguishable from a reused one by contract:
    /// nothing may read a stack it has not written).
    pub fn release(&mut self, handle: StackHandle) {
        handle.set_owner(0);
        LIVE_STACKS.fetch_sub(1, Ordering::Relaxed);
        self.free.push(Slot {
            guard_base: handle.guard_base,
        });
        std::mem::forget(handle);
    }

    /// Test hook: (slab mmaps, NOHUGEPAGE madvises) issued by the calling
    /// thread. Thread-local so parallel tests cannot cross-contaminate.
    #[cfg(test)]
    pub fn thread_syscalls() -> (usize, usize) {
        (
            TL_MMAP_CALLS.with(std::cell::Cell::get),
            TL_MADVISE_CALLS.with(std::cell::Cell::get),
        )
    }
}

impl Drop for StackPool {
    /// A dying pool's free slots go to the shared list — slots are
    /// process-lifetime resources, and losing a freelist would strand them.
    fn drop(&mut self) {
        lock(&ORPHANED_SLOTS).append(&mut self.free);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // §5.4/6 — steady-state spawn is allocation-free: after warmup, N
    // acquire/release cycles issue zero mmaps.
    #[test]
    fn slab_reuse_is_allocation_free() {
        let mut pool = StackPool::new();
        let warm = pool.acquire(1).expect("warmup stack");
        pool.release(warm);
        // Thread-local counter: parallel tests map slabs of their own.
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

    // §5.4/8 — the NOHUGEPAGE advice actually happened (the RSS effect
    // itself is a bench, not a unit test).
    #[cfg(target_os = "linux")]
    #[test]
    fn nohugepage_is_set() {
        // Hold handles until this thread is forced to map a fresh slab
        // (the freelist and orphan list may satisfy early acquires).
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

    // §5.4/7 — the VMA budget surfaces as a value, not an ENOMEM: an
    // artificially low cap makes acquire fail with the dedicated VmError.
    #[test]
    fn vma_budget_caps_spawn() {
        let mut pool = StackPool::with_high_water(0);
        match pool.acquire(1) {
            Err(VmError::StackBudget { .. }) => {}
            other => panic!("expected StackBudget, got {other:?}"),
        }
    }

    // Reuse returns a *clean* handle: same slot, full usable range writable,
    // top 16-aligned. (Spec test 6's "returns a clean stack" half.)
    #[test]
    fn released_slot_is_reusable_and_writable() {
        let mut pool = StackPool::new();
        let a = pool.acquire(7).expect("stack");
        let a_base = a.guard_base as usize;
        let top = a.top();
        // Touch the whole usable range: faults would fail the test.
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
