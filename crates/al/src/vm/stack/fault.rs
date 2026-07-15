//! Naming stack overflows: a SIGSEGV handler that recognizes guard-page
//! hits in the pooled slabs and reports *which AL process* overflowed its
//! native stack before aborting.
//!
//! Why this exists: std's stack-overflow handler only knows the *current
//! thread's* guard range, so a fault in a process stack's guard page is
//! otherwise a bare `SIGSEGV` with no hint that it was a stack overflow at
//! all, let alone whose. This module owns every slab base and guard address
//! (registered by [`super::slab`]), so the classification is exact.
//!
//! Async-signal-safety: the handler reads only fixed statics and leaked
//! arrays (no locks, no allocation), formats integers into a stack buffer,
//! and uses only `write(2)` and `abort(2)` — both AS-safe. Faults that are
//! not a guard hit re-raise into whatever handler was installed before ours
//! (std's, for ordinary thread-stack overflows), preserved at install time.

#![allow(unsafe_code)]

use std::mem::MaybeUninit;
use std::sync::Once;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// Registered slab mappings, written once each by [`register_slab`] and
/// scanned (never modified) by the signal handler. Fixed capacity: at
/// 16.6 MB of VSZ per slab, 1024 entries is ~17 GB of stack reservations —
/// far beyond the VMA budget; registration beyond it is refused (those
/// stacks still work, their overflows just fall through unnamed).
const MAX_SLABS: usize = 1024;

struct SlabEntry {
    /// Base address of the mapping; 0 = empty. Written LAST with `Release`
    /// so a handler that sees it nonzero sees the whole entry.
    base: AtomicUsize,
    total: AtomicUsize,
    slot: AtomicUsize,
    guard: AtomicUsize,
    /// Leaked `[AtomicU64]` of per-slot owner pids, one per slot.
    owners: AtomicUsize,
}

impl SlabEntry {
    const fn new() -> Self {
        SlabEntry {
            base: AtomicUsize::new(0),
            total: AtomicUsize::new(0),
            slot: AtomicUsize::new(0),
            guard: AtomicUsize::new(0),
            owners: AtomicUsize::new(0),
        }
    }
}

static SLABS: [SlabEntry; MAX_SLABS] = [const { SlabEntry::new() }; MAX_SLABS];
static SLAB_COUNT: AtomicUsize = AtomicUsize::new(0);

static INSTALL: Once = Once::new();
static mut OLD_SEGV: MaybeUninit<libc::sigaction> = MaybeUninit::uninit();
#[cfg(target_os = "macos")]
static mut OLD_BUS: MaybeUninit<libc::sigaction> = MaybeUninit::uninit();

/// Register one slab with the handler (called by [`super::slab::map_slab`]
/// under its own serialization; entries are append-only). Installs the
/// signal handler on first use — before that, no guard page exists to hit.
pub(super) fn register_slab(base: usize, total: usize, slot: usize, guard: usize) {
    install();
    let idx = SLAB_COUNT.fetch_add(1, Ordering::Relaxed);
    if idx >= MAX_SLABS {
        return;
    }
    let owners: Box<[AtomicU64]> = (0..total / slot).map(|_| AtomicU64::new(0)).collect();
    let owners = Box::leak(owners).as_ptr() as usize;
    let e = &SLABS[idx];
    e.total.store(total, Ordering::Relaxed);
    e.slot.store(slot, Ordering::Relaxed);
    e.guard.store(guard, Ordering::Relaxed);
    e.owners.store(owners, Ordering::Relaxed);
    e.base.store(base, Ordering::Release);
}

/// Record the AL pid running on the slot whose guard page starts at
/// `guard_base` (0 clears it). No-op for unregistered addresses.
pub(super) fn set_slot_owner(guard_base: usize, pid: u64) {
    let count = SLAB_COUNT.load(Ordering::Relaxed).min(MAX_SLABS);
    for e in &SLABS[..count] {
        let base = e.base.load(Ordering::Acquire);
        if base == 0 {
            continue;
        }
        let total = e.total.load(Ordering::Relaxed);
        if guard_base < base || guard_base >= base + total {
            continue;
        }
        let slot = e.slot.load(Ordering::Relaxed);
        let owners = e.owners.load(Ordering::Relaxed) as *const AtomicU64;
        let idx = (guard_base - base) / slot;
        // SAFETY: owners is a leaked array with one entry per slot; idx is
        // in range by the containment check above.
        unsafe { (*owners.add(idx)).store(pid, Ordering::Relaxed) };
        return;
    }
}

fn install() {
    INSTALL.call_once(|| {
        // SAFETY: constructing a sigaction for our extern "C" handler and
        // saving the previous one; SA_ONSTACK rides the sigaltstack std
        // installs per thread (mandatory: the faulting stack has no room
        // below its own guard for a signal frame).
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            let h: extern "C" fn(i32, *mut libc::siginfo_t, *mut libc::c_void) = handler;
            sa.sa_sigaction = h as usize;
            sa.sa_flags = libc::SA_SIGINFO | libc::SA_ONSTACK;
            libc::sigemptyset(&raw mut sa.sa_mask);
            libc::sigaction(libc::SIGSEGV, &sa, (&raw mut OLD_SEGV).cast());
            // macOS reports guard-page hits as SIGBUS.
            #[cfg(target_os = "macos")]
            libc::sigaction(libc::SIGBUS, &sa, (&raw mut OLD_BUS).cast());
        }
    });
}

/// Format `n` into `buf`'s tail, returning the written slice. AS-safe by
/// construction (no allocation, no locks).
fn format_u64(n: u64, buf: &mut [u8; 20]) -> &[u8] {
    let mut i = buf.len();
    let mut n = n;
    loop {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    &buf[i..]
}

extern "C" fn handler(sig: i32, info: *mut libc::siginfo_t, _ctx: *mut libc::c_void) {
    // SAFETY: the kernel hands a valid siginfo for SA_SIGINFO handlers.
    #[cfg(target_os = "linux")]
    let addr = unsafe { (*info).si_addr() } as usize;
    #[cfg(target_os = "macos")]
    let addr = unsafe { (*info).si_addr } as usize;

    let count = SLAB_COUNT.load(Ordering::Relaxed).min(MAX_SLABS);
    for e in &SLABS[..count] {
        let base = e.base.load(Ordering::Acquire);
        if base == 0 || addr < base {
            continue;
        }
        let total = e.total.load(Ordering::Relaxed);
        if addr >= base + total {
            continue;
        }
        let slot = e.slot.load(Ordering::Relaxed);
        let guard = e.guard.load(Ordering::Relaxed);
        let off = addr - base;
        if off % slot >= guard {
            // Inside a slab but not in a guard page: a wild pointer, not an
            // overflow. Fall through to the previous handler.
            break;
        }
        let owners = e.owners.load(Ordering::Relaxed) as *const AtomicU64;
        // SAFETY: one owner per slot, index in range by containment.
        let pid = unsafe { (*owners.add(off / slot)).load(Ordering::Relaxed) };

        let mut msg = [0u8; 128];
        let mut len = 0;
        let mut push = |s: &[u8]| {
            let n = s.len().min(msg.len() - len);
            msg[len..len + n].copy_from_slice(&s[..n]);
            len += n;
        };
        push(b"AL process ");
        let mut nbuf = [0u8; 20];
        push(format_u64(pid, &mut nbuf));
        push(b" overflowed its native stack (");
        let kb = ((slot - guard) / 1024) as u64;
        push(format_u64(kb, &mut nbuf));
        push(b" KB reserved)\n");
        // SAFETY: write(2) on stderr with a stack buffer; AS-safe.
        unsafe { libc::write(libc::STDERR_FILENO, msg.as_ptr().cast(), len) };
        // SAFETY: abort(2) is AS-safe; this process is done.
        unsafe { libc::abort() };
    }

    // Not ours: reinstall the saved handler and return. The faulting
    // instruction re-executes and re-faults into the original disposition
    // (std's overflow report for thread stacks, or the default core dump).
    // SAFETY: OLD_* was written by `install` before this handler could run.
    unsafe {
        #[cfg(target_os = "macos")]
        if sig == libc::SIGBUS {
            libc::sigaction(sig, (&raw const OLD_BUS).cast(), std::ptr::null_mut());
            return;
        }
        let _ = sig;
        libc::sigaction(
            libc::SIGSEGV,
            (&raw const OLD_SEGV).cast(),
            std::ptr::null_mut(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::super::slab::StackPool;
    use super::super::switch::run_on_stack;
    use std::ffi::c_void;
    use std::process::Command;

    const CHILD_ENV: &str = "AL_TEST_STACK_OVERFLOW_CHILD";

    /// Walk down from a stack local in 1K steps until the guard page stops
    /// us. Volatile so the loop cannot be optimized into thin air.
    unsafe extern "C" fn overflower(_arg: *mut c_void) -> u64 {
        let probe = 0u8;
        let mut p = (&raw const probe) as *mut u8;
        loop {
            // SAFETY: deliberately walks into the guard; the fault is the test.
            unsafe { p.write_volatile(1) };
            p = p.wrapping_sub(1024);
        }
    }

    // The crashing half, run in a subprocess: inert (instant pass) unless
    // the parent set CHILD_ENV. It acquires a stack owned by pid 4242 and
    // overflows it; the fault handler must name the pid and abort.
    #[test]
    fn guard_page_overflow_child() {
        if std::env::var(CHILD_ENV).is_err() {
            return;
        }
        let mut pool = StackPool::new();
        let h = pool.acquire(4242).expect("stack");
        unsafe {
            run_on_stack(
                std::ptr::null_mut(),
                h.top(),
                overflower,
                std::ptr::null_mut(),
            )
        };
        unreachable!("the overflow must fault before the entry returns");
    }

    // §5.4/5 — a guard-page overflow is caught and NAMED: the handler
    // reports the owning AL pid on stderr instead of a bare SIGSEGV.
    #[test]
    fn guard_page_faults_and_is_named() {
        let exe = std::env::current_exe().expect("test binary path");
        let out = Command::new(exe)
            .args([
                "vm::stack::fault::tests::guard_page_overflow_child",
                "--exact",
                "--nocapture",
            ])
            .env(CHILD_ENV, "1")
            .output()
            .expect("spawn child test");
        assert!(
            !out.status.success(),
            "the child must die on the overflow, not exit cleanly"
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("AL process 4242 overflowed its native stack"),
            "overflow must be named; child stderr was:\n{stderr}"
        );
    }
}
