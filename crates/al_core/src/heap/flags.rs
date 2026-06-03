//! Environment flags the heap consults: `AL_GC_STRESS` and `AL_HEAP_STATS`.
//!
//! Both are read once per program and cached — they sit on the hottest VM
//! path, and neither can meaningfully change mid-run.

use std::ffi::OsStr;
use std::sync::OnceLock;

/// Is GC stress mode on? Setting `AL_GC_STRESS=1` (any value other than
/// empty or `"0"`) forces every `ensure()` to collect unconditionally.
///
/// This is the enforcement mechanism for the rooting rule: a collection at
/// *every* safepoint deterministically moves any object a Rust local is
/// holding across an allocation, turning a latent dangling-pointer bug into
/// an immediate, reproducible test failure. The full test suite and the
/// example programs are run under it to prove the rule holds everywhere.
pub fn gc_stress() -> bool {
    static STRESS: OnceLock<bool> = OnceLock::new();
    *STRESS.get_or_init(|| env_flag_truthy(std::env::var_os("AL_GC_STRESS").as_deref()))
}

/// Should per-heap statistics print when a heap that did any work is
/// dropped? `AL_HEAP_STATS=1`, same truthiness rules as [`gc_stress`].
/// Debug aid only.
pub(super) fn heap_stats_enabled() -> bool {
    static STATS: OnceLock<bool> = OnceLock::new();
    *STATS.get_or_init(|| env_flag_truthy(std::env::var_os("AL_HEAP_STATS").as_deref()))
}

/// Pure parse of a truthy env flag: set, non-empty, and not `"0"`. Split out
/// so the logic is unit-testable (the cached readers above consult the
/// process environment exactly once).
pub(super) fn env_flag_truthy(value: Option<&OsStr>) -> bool {
    match value {
        Some(v) => !v.is_empty() && v != "0",
        None => false,
    }
}
