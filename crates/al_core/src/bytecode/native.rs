//! Per-program native-code entry table: the dispatch surface between the
//! interpreter and JIT-compiled function bodies.
//!
//! [`NativeTable`] is a fixed-size slice of code-pointer slots, one per
//! [`Program::functions`](super::Program::functions) entry and indexed by the
//! same [`FuncIdx`] numbering (`CoreProgram.fns` / `Program.functions` /
//! `TypedProgram::fns` share it — see `core_ir/mod.rs`). A populated slot
//! means "this function has a compiled body: call it instead of
//! interpreting"; an empty slot means the bytecode is the only body. Bytecode
//! is kept for every function regardless — it is the fallback and the
//! resume-after-suspension path.
//!
//! The slice lives behind an `Arc`, so cloning the owning [`Program`]
//! (each worker scheduler runs a private clone, load-bearing fact 3 of
//! `bytecode/mod.rs`) shares one table: an entry published by the load-time
//! compile pass is visible to every scheduler. Slots are [`AtomicPtr`]s
//! rather than `Option<fn>`s so the table can be sized when the function
//! list is final and populated afterwards, without threading `&mut Program`
//! through the backend.
//!
//! The pointers stored here address one shared, immortal executable mapping:
//! compiled code is never freed (processes migrate across scheduler threads
//! mid-flight, so no thread can prove a code address unreachable). That is
//! what makes handing raw code pointers across threads sound.
//!
//! [`Program`]: super::Program

use std::sync::Arc;
use std::sync::atomic::{AtomicPtr, Ordering};

use crate::core_ir::FuncIdx;
use crate::tivec::Idx;

/// One-word status a native entry returns to its caller (native or
/// trampoline). It round-trips the interpreter's step outcomes across the
/// `extern "C"` boundary: any status other than [`NativeStatus::Done`]
/// unwinds every native frame by plain returns, and the trampoline then does
/// exactly what the interpreter's dispatch loop does today (yield to the
/// scheduler, suspend-and-park, or raise). All process state needed to act
/// on the status — the parked wait, the pending error — is already recorded
/// in the VM by the shim that produced it; the status itself carries none.
///
/// `repr(u64)` with pinned discriminants: JIT-compiled code materialises
/// these exact machine words in the return register, so the values are ABI,
/// not an implementation detail.
#[repr(u64)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeStatus {
    /// The function ran to completion; its result is in the frame slot the
    /// calling convention assigns. The caller's frame is the top frame again.
    Done = 0,
    /// Reduction budget exhausted. The yielding frame's `ip` names its resume
    /// point (0 = re-enter from the top); the scheduler re-runs the process
    /// later exactly as it does for an interpreter yield.
    Yield = 1,
    /// A callee parked the process on I/O or a timer. The whole native call
    /// chain unwinds; resume re-enters the interpreter at the parked frame's
    /// `ip`.
    Parked = 2,
    /// A runtime error was raised. The VM's pending-error state carries the
    /// error value; native frames just unwind.
    Error = 3,
    /// A cross-function tail call collapsed the top frame in place (the
    /// interpreter's `TailCallKnown` frame surgery); the trampoline driving
    /// the returning function must now dispatch the *new* top frame. Compiled
    /// code returns this verbatim — a tail call site is
    /// `return al_rt_tail_call(...)` — so tail chains unwind to one driver
    /// loop instead of stacking machine frames. Unlike the other statuses it
    /// never reaches the interpreter boundary: every entry invocation runs
    /// under a trampoline that consumes it.
    TailCall = 4,
}

/// The compiled-function calling convention: arguments are already in the
/// callee's frame slots (the interpreter layout — args at
/// `[base_slot, base_slot + arity)`), the `CallFrame` is pushed, and the
/// entry runs against the VM behind `vmx`.
///
/// `vmx` is the `al` crate's VM, opaque here: `al_core` compiles bodies and
/// owns this table, but only the VM crate can name the concrete type. Both
/// sides of the boundary cast through this alias, so the signature is
/// written down exactly once.
pub type NativeEntry = extern "C" fn(vmx: *mut core::ffi::c_void) -> NativeStatus;

/// The per-program entry table. See the module docs for the sharing and
/// lifetime story.
///
/// `Clone` is shallow (one more `Arc` handle to the same slots) — that is
/// the point: per-scheduler `Program` clones must observe one table.
#[derive(Clone, Default)]
pub struct NativeTable {
    entries: Arc<[AtomicPtr<()>]>,
}

impl NativeTable {
    /// A table with `fn_count` empty slots. Size it from the *final*
    /// `Program::functions` length — [`FuncIdx`] is minted against that
    /// numbering, and this table must never perturb or outgrow it.
    pub fn new(fn_count: usize) -> NativeTable {
        NativeTable {
            entries: (0..fn_count)
                .map(|_| AtomicPtr::new(std::ptr::null_mut()))
                .collect(),
        }
    }

    /// Number of slots (== the function count the table was sized for).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Publish `entry` as `fn_idx`'s compiled body. Called by the load-time
    /// compile pass after the JIT finalises the function's code; panics if
    /// `fn_idx` is outside the numbering the table was sized for, because
    /// that means the caller compiled against a different function list.
    ///
    /// `Release` pairs with [`NativeTable::get`]'s `Acquire`: a scheduler
    /// that observes the pointer also observes the finalised code and icache
    /// flush that happened before the store.
    pub fn set(&self, fn_idx: FuncIdx, entry: NativeEntry) {
        self.entries[fn_idx.index()].store(entry as *mut (), Ordering::Release);
    }

    /// The compiled body for `fn_idx`, or `None` when the function is
    /// interpreter-only. Out-of-range indices are `None`, not a panic: a
    /// REPL session grows `Program::functions` past a table sized for an
    /// earlier line, and those newer functions simply interpret.
    ///
    /// The one `unsafe` here reverses `set`'s `NativeEntry -> *mut ()` cast;
    /// non-null slots are written only by `set`, so the pointer is always a
    /// valid `NativeEntry`.
    #[inline]
    #[allow(unsafe_code)]
    pub fn get(&self, fn_idx: FuncIdx) -> Option<NativeEntry> {
        let ptr = self.entries.get(fn_idx.index())?.load(Ordering::Acquire);
        if ptr.is_null() {
            None
        } else {
            // Non-null slots are written only by `set`, which takes a
            // `NativeEntry`; the transmute reverses that cast.
            Some(unsafe { std::mem::transmute::<*mut (), NativeEntry>(ptr) })
        }
    }

    /// Every populated slot, in `FuncIdx` order — the perf-map writer and
    /// `dis --native` walk this.
    pub fn compiled(&self) -> impl Iterator<Item = (FuncIdx, NativeEntry)> + '_ {
        (0..self.entries.len())
            .map(FuncIdx::from_usize)
            .filter_map(|idx| self.get(idx).map(|entry| (idx, entry)))
    }
}

impl std::fmt::Debug for NativeTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let compiled = self.compiled().count();
        write!(f, "NativeTable({compiled}/{} compiled)", self.len())
    }
}

// ============================================================================
// AL_NATIVE / AL_NATIVE_SEED: the mode contract
// ============================================================================

/// What `AL_NATIVE` asked for. Read exactly once per process, at program
/// construction — the compile pass consults [`config`] before its first body
/// materialises. See [`config`] for the seed and stderr rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NativeMode {
    /// Interpret everything; no function is handed to the native backend.
    Off,
    /// Compile every eligible function (the default when `AL_NATIVE` is
    /// unset).
    #[default]
    Native,
    /// Compile a seeded random per-function subset, for shaking out
    /// native/interpreter boundary bugs.
    Mix,
}

impl NativeMode {
    fn as_str(self) -> &'static str {
        match self {
            NativeMode::Off => "off",
            NativeMode::Native => "native",
            NativeMode::Mix => "mix",
        }
    }
}

/// The process-wide native-backend configuration: the mode plus the seed
/// that fixes `mix`'s per-function subset.
#[derive(Debug, Clone, Copy)]
pub struct NativeConfig {
    pub mode: NativeMode,
    /// Subset seed. Meaningful only under [`NativeMode::Mix`]; zero
    /// otherwise.
    pub seed: u64,
}

/// The outcome of parsing the two env values, side-effect free so tests can
/// drive it without touching the process environment. `seed: None` under
/// `Mix` means "draw from entropy"; [`config`] fills it in and knows not to
/// echo a seed nobody chose.
struct Parsed {
    mode: NativeMode,
    seed: Option<u64>,
    warnings: Vec<String>,
}

fn parse(mode: Option<&str>, seed: Option<&str>) -> Parsed {
    let mut warnings = Vec::new();
    let mode = match mode {
        None => NativeMode::Native,
        Some("off") => NativeMode::Off,
        Some("native") => NativeMode::Native,
        Some("mix") => NativeMode::Mix,
        Some(other) => {
            warnings.push(format!(
                "al: unknown AL_NATIVE value {other:?} (expected off|native|mix); using native"
            ));
            NativeMode::Native
        }
    };
    let seed = match (mode, seed) {
        (NativeMode::Mix, Some(s)) => match s.parse::<u64>() {
            Ok(n) => Some(n),
            Err(_) => {
                warnings.push(format!(
                    "al: AL_NATIVE_SEED {s:?} is not a u64; drawing a random seed"
                ));
                None
            }
        },
        _ => None,
    };
    Parsed {
        mode,
        seed,
        warnings,
    }
}

/// The process-wide config, reading `AL_NATIVE` / `AL_NATIVE_SEED` on first
/// use and never again. In `mix` mode the seed is echoed to stderr only when
/// `AL_NATIVE_SEED` was explicitly set — a default run's stderr stays empty
/// (golden tests assert it), and `mix` without a chosen seed stays quiet too
/// so the whole suite can run under `AL_NATIVE=mix` unchanged.
pub fn config() -> &'static NativeConfig {
    static CONFIG: std::sync::OnceLock<NativeConfig> = std::sync::OnceLock::new();
    CONFIG.get_or_init(|| {
        let mode_var = std::env::var("AL_NATIVE").ok();
        let seed_var = std::env::var("AL_NATIVE_SEED").ok();
        let parsed = parse(mode_var.as_deref(), seed_var.as_deref());
        for w in &parsed.warnings {
            eprintln!("{w}");
        }
        let seed = match (parsed.mode, parsed.seed) {
            (NativeMode::Mix, Some(seed)) => {
                eprintln!("al: native mix seed = {seed}");
                seed
            }
            (NativeMode::Mix, None) => entropy_seed(),
            _ => 0,
        };
        NativeConfig {
            mode: parsed.mode,
            seed,
        }
    })
}

/// A seed nobody chose: hashed process entropy, no extra dependency.
fn entropy_seed() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish()
}

impl NativeConfig {
    /// Whether the mode selects `idx` for native compilation — the gate the
    /// compile pass applies before firing its native hook. Deterministic in
    /// `(seed, idx)`, so a `mix` run reproduces from its printed seed.
    pub fn includes(&self, idx: FuncIdx) -> bool {
        match self.mode {
            NativeMode::Off => false,
            NativeMode::Native => true,
            NativeMode::Mix => splitmix64(self.seed ^ idx.index() as u64) & 1 == 0,
        }
    }
}

/// SplitMix64 finaliser: one well-mixed word per `(seed, idx)` pair.
fn splitmix64(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Whether `AL_NATIVE_DEBUG` asked for native-backend diagnostics. Read once.
pub fn debug() -> bool {
    static DEBUG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *DEBUG.get_or_init(|| std::env::var_os("AL_NATIVE_DEBUG").is_some())
}

/// One debug line per selected function, so a `mix` subset (or `native`'s
/// full sweep) is observable: which functions the mode handed to the backend.
pub fn log_selected(idx: FuncIdx, name: &str) {
    if debug() {
        eprintln!("al-native: selected {idx} {name}");
    }
}

/// Whole-unit accounting for the compile-all-at-load pass: how many bodies
/// the mode selected and how long the native hook spent on them, checked
/// against the <100ms-per-unit budget.
#[derive(Debug, Default)]
pub struct UnitStats {
    pub selected: usize,
    pub elapsed: std::time::Duration,
}

impl UnitStats {
    pub fn record(&mut self, elapsed: std::time::Duration) {
        self.selected += 1;
        self.elapsed += elapsed;
    }

    /// The whole-unit summary, printed only under `AL_NATIVE_DEBUG`.
    pub fn log_summary(&self, instrs: usize) {
        if !debug() {
            return;
        }
        let ms = self.elapsed.as_secs_f64() * 1000.0;
        let over = if self.elapsed > std::time::Duration::from_millis(100) {
            " OVER BUDGET"
        } else {
            ""
        };
        eprintln!(
            "al-native: mode={} selected {} fns; in-compile hook {ms:.2}ms \
             over {instrs} instrs (unit budget 100ms){over}",
            config().mode.as_str(),
            self.selected,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    extern "C" fn stub(_vmx: *mut core::ffi::c_void) -> NativeStatus {
        NativeStatus::Done
    }

    #[test]
    fn empty_table_answers_none_for_any_idx() {
        let t = NativeTable::default();
        assert!(t.is_empty());
        assert!(t.get(FuncIdx(0)).is_none());
        assert!(t.get(FuncIdx(41)).is_none());
    }

    #[test]
    fn set_then_get_round_trips_the_entry() {
        let t = NativeTable::new(3);
        assert!(t.get(FuncIdx(1)).is_none());
        t.set(FuncIdx(1), stub);
        let entry = t.get(FuncIdx(1)).expect("just set");
        assert!(std::ptr::fn_addr_eq(entry, stub as NativeEntry));
        assert_eq!(entry(std::ptr::null_mut()), NativeStatus::Done);
        assert!(t.get(FuncIdx(0)).is_none());
        assert!(t.get(FuncIdx(2)).is_none());
    }

    #[test]
    fn clones_share_one_table() {
        let a = NativeTable::new(2);
        let b = a.clone();
        a.set(FuncIdx(0), stub);
        assert!(b.get(FuncIdx(0)).is_some());
    }

    #[test]
    fn out_of_range_get_is_none_not_a_panic() {
        let t = NativeTable::new(1);
        assert!(t.get(FuncIdx(7)).is_none());
    }

    #[test]
    fn compiled_walks_populated_slots_in_order() {
        let t = NativeTable::new(4);
        t.set(FuncIdx(2), stub);
        t.set(FuncIdx(0), stub);
        let idxs: Vec<FuncIdx> = t.compiled().map(|(i, _)| i).collect();
        assert_eq!(idxs, vec![FuncIdx(0), FuncIdx(2)]);
    }

    #[test]
    fn status_discriminants_are_abi() {
        assert_eq!(NativeStatus::Done as u64, 0);
        assert_eq!(NativeStatus::Yield as u64, 1);
        assert_eq!(NativeStatus::Parked as u64, 2);
        assert_eq!(NativeStatus::Error as u64, 3);
        assert_eq!(NativeStatus::TailCall as u64, 4);
        assert_eq!(std::mem::size_of::<NativeStatus>(), 8);
    }
}

#[cfg(test)]
mod mode_tests {
    use super::*;

    #[test]
    fn unset_defaults_to_native() {
        let p = parse(None, None);
        assert_eq!(p.mode, NativeMode::Native);
        assert!(p.seed.is_none());
        assert!(p.warnings.is_empty());
    }

    #[test]
    fn parses_all_three_modes() {
        assert_eq!(parse(Some("off"), None).mode, NativeMode::Off);
        assert_eq!(parse(Some("native"), None).mode, NativeMode::Native);
        assert_eq!(parse(Some("mix"), None).mode, NativeMode::Mix);
    }

    #[test]
    fn unknown_mode_warns_and_defaults_to_native() {
        let p = parse(Some("on"), None);
        assert_eq!(p.mode, NativeMode::Native);
        assert_eq!(p.warnings.len(), 1);
    }

    #[test]
    fn seed_is_a_mix_only_knob() {
        assert_eq!(parse(Some("mix"), Some("42")).seed, Some(42));
        assert_eq!(parse(Some("native"), Some("42")).seed, None);
        assert_eq!(parse(Some("off"), Some("42")).seed, None);
    }

    #[test]
    fn bad_seed_warns_and_falls_back_to_entropy() {
        let p = parse(Some("mix"), Some("banana"));
        assert_eq!(p.mode, NativeMode::Mix);
        assert!(p.seed.is_none());
        assert_eq!(p.warnings.len(), 1);
    }

    #[test]
    fn off_selects_nothing_native_selects_everything() {
        let off = NativeConfig {
            mode: NativeMode::Off,
            seed: 0,
        };
        let native = NativeConfig {
            mode: NativeMode::Native,
            seed: 0,
        };
        for i in 0..64 {
            assert!(!off.includes(FuncIdx(i)));
            assert!(native.includes(FuncIdx(i)));
        }
    }

    #[test]
    fn mix_is_deterministic_in_seed_and_a_proper_subset() {
        let a = NativeConfig {
            mode: NativeMode::Mix,
            seed: 42,
        };
        let b = NativeConfig {
            mode: NativeMode::Mix,
            seed: 42,
        };
        let picks: Vec<bool> = (0..256).map(|i| a.includes(FuncIdx(i))).collect();
        let again: Vec<bool> = (0..256).map(|i| b.includes(FuncIdx(i))).collect();
        assert_eq!(picks, again);
        // Neither empty nor everything: the subset actually mixes.
        assert!(picks.iter().any(|&p| p));
        assert!(picks.iter().any(|&p| !p));
    }

    #[test]
    fn mix_subsets_differ_across_seeds() {
        let a = NativeConfig {
            mode: NativeMode::Mix,
            seed: 1,
        };
        let b = NativeConfig {
            mode: NativeMode::Mix,
            seed: 2,
        };
        let differs = (0..256).any(|i| a.includes(FuncIdx(i)) != b.includes(FuncIdx(i)));
        assert!(differs);
    }
}
