//! Per-program native-code entry table: the dispatch surface between the
//! interpreter and JIT-compiled function bodies.
//!
//! [`NativeTable`] is one code-pointer slot per
//! [`Program::functions`](super::Program::functions) entry, indexed by the same
//! [`FuncIdx`] numbering that `CoreProgram.fns` and `TypedProgram::fns` use. A
//! populated slot means "call this instead of interpreting". Bytecode is kept
//! for every function regardless: it is the fallback and the
//! resume-after-suspension path.
//!
//! The slice is behind an `Arc`, so every worker scheduler's private [`Program`]
//! clone shares one table. Slots are [`AtomicPtr`]s so the table can be sized
//! once the function list is final and populated afterwards, without threading
//! `&mut Program` through the backend.
//!
//! Compiled code is never freed — processes migrate across scheduler threads
//! mid-flight, so no thread can prove a code address unreachable. That is what
//! makes handing raw code pointers across threads sound.
//!
//! [`Program`]: super::Program

use std::sync::Arc;
use std::sync::atomic::{AtomicPtr, Ordering};

use crate::FuncIdx;
use crate::tivec::Idx;

/// One-word status a native entry returns to its caller. Anything other than
/// [`NativeStatus::Done`] unwinds every native frame by plain returns, and the
/// trampoline then does what the interpreter's dispatch loop would. The status
/// carries no payload: the shim that produced it already recorded the parked
/// wait or pending error in the VM.
///
/// The discriminants are ABI, not an implementation detail — JIT-compiled code
/// materialises these exact machine words in the return register.
#[repr(u64)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeStatus {
    /// Ran to completion; the result is in the frame slot the calling
    /// convention assigns.
    Done = 0,
    /// Reduction budget exhausted. The yielding frame's `ip` names its resume
    /// point (0 = re-enter from the top).
    Yield = 1,
    /// A callee parked the process on I/O or a timer. Resume re-enters the
    /// interpreter at the parked frame's `ip`.
    Parked = 2,
    /// A runtime error was raised; the VM holds the error value.
    Error = 3,
    /// A cross-function tail call collapsed the top frame in place, so the
    /// trampoline must dispatch the NEW top frame. Tail chains therefore
    /// unwind to one driver loop instead of stacking machine frames. Never
    /// reaches the interpreter boundary: every entry invocation runs under a
    /// trampoline that consumes it.
    TailCall = 4,
}

/// What the pinned register (x86_64 r15, aarch64 x21) points at while a
/// compiled body runs, and the argument every [`NativeEntry`] receives.
/// `repr(C)` because generated code bakes the field offsets in as load offsets.
///
/// The indirection exists so nothing scheduler-derived is resident in a
/// compiled frame across a suspension point. The VM pointer lives here,
/// re-published by every `VM::call_native`, and compiled code reloads it before
/// each runtime call — so a resume on another scheduler reads that scheduler's
/// VM by construction.
#[repr(C)]
#[derive(Debug)]
pub struct NativeCtx {
    /// Offset 0, the hottest load. Dormant today: shims spend the VM's own
    /// counter until reds checks move into generated code.
    pub reds: i64,
    /// Offset 8. This scheduler's `&mut VM`, opaque at codegen time.
    pub vm: *mut core::ffi::c_void,
}

impl NativeCtx {
    /// Load offsets baked into generated code (`core_ir::clif`). Reordering
    /// the fields without updating these is silent memory corruption.
    pub const REDS_OFFSET: i32 = 0;
    pub const VM_OFFSET: i32 = 8;

    pub fn new() -> NativeCtx {
        NativeCtx {
            reds: 0,
            vm: core::ptr::null_mut(),
        }
    }
}

impl Default for NativeCtx {
    fn default() -> Self {
        Self::new()
    }
}

/// The compiled-function calling convention: arguments are already in the
/// callee's frame slots (`[base_slot, base_slot + arity)`, the interpreter
/// layout) and the `CallFrame` is pushed before the entry runs. `ctx` is a
/// [`NativeCtx`], opaque so both sides cast through this one alias; the
/// prologue moves it into the pinned register.
pub type NativeEntry = extern "C" fn(ctx: *mut core::ffi::c_void) -> NativeStatus;

/// The per-program entry table. `Clone` is shallow on purpose: per-scheduler
/// `Program` clones must observe one table.
#[derive(Clone, Default)]
pub struct NativeTable {
    entries: Arc<[AtomicPtr<()>]>,
}

impl NativeTable {
    /// A table with `fn_count` empty slots. Size it from the FINAL
    /// `Program::functions` length; [`FuncIdx`] is minted against that
    /// numbering.
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

    /// Publish `entry` as `fn_idx`'s compiled body. Panics if `fn_idx` is
    /// outside the numbering the table was sized for — that means the caller
    /// compiled against a different function list.
    ///
    /// `Release` pairs with [`NativeTable::get`]'s `Acquire`: a scheduler that
    /// observes the pointer also observes the finalised code and icache flush.
    pub fn set(&self, fn_idx: FuncIdx, entry: NativeEntry) {
        self.entries[fn_idx.index()].store(entry as *mut (), Ordering::Release);
    }

    /// The compiled body for `fn_idx`, or `None` when the function is
    /// interpreter-only. Out-of-range is `None`, not a panic: a REPL session
    /// grows `Program::functions` past a table sized for an earlier line, and
    /// those newer functions simply interpret.
    #[inline]
    #[allow(unsafe_code)]
    pub fn get(&self, fn_idx: FuncIdx) -> Option<NativeEntry> {
        let ptr = self.entries.get(fn_idx.index())?.load(Ordering::Acquire);
        if ptr.is_null() {
            None
        } else {
            // SAFETY: non-null slots are written only by `set`, which takes a
            // `NativeEntry`; this reverses that cast.
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

/// What `AL_NATIVE` asked for. Read exactly once per process. See [`config`]
/// for the seed and stderr rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NativeMode {
    /// Interpret everything; no function is handed to the native backend.
    Off,
    /// Compile every eligible function. The default when `AL_NATIVE` is unset.
    #[default]
    Native,
    /// Compile a seeded random subset, to shake out native/interpreter
    /// boundary bugs.
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

/// The process-wide native-backend configuration.
#[derive(Debug, Clone, Copy)]
pub struct NativeConfig {
    pub mode: NativeMode,
    /// Subset seed. Meaningful only under [`NativeMode::Mix`]; zero otherwise.
    pub seed: u64,
}

/// The outcome of parsing the two env values, side-effect free so tests can
/// drive it without touching the process environment. `seed: None` under `Mix`
/// means "draw from entropy".
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
/// use and never again. The seed is echoed to stderr only when `AL_NATIVE_SEED`
/// was explicitly set: golden tests assert an empty stderr, and the whole suite
/// must run under `AL_NATIVE=mix` unchanged.
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
    /// Whether the mode selects `idx` for native compilation. Deterministic in
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

/// One debug line per selected function, so a `mix` subset is observable.
pub fn log_selected(idx: FuncIdx, name: &str) {
    if debug() {
        eprintln!("al-native: selected {idx} {name}");
    }
}

/// Whole-unit accounting for the compile-all-at-load pass, checked against the
/// 100ms-per-unit budget.
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
        // Neither empty nor everything.
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

/// Call a JIT entry, preserving the pinned register the JIT clobbers.
///
/// `enable_pinned_reg` gives generated code the pinned register (r15 on
/// x86_64, x21 on aarch64) by dropping it from Cranelift's callee-save list, so
/// a compiled entry's prologue writes it and no epilogue puts it back — while
/// every Rust caller on the path is entitled by the platform ABI to assume it
/// survives. This restores that assumption. It belongs to the [`NativeEntry`]
/// contract, not to any one caller, because anyone calling an entry needs it.
///
/// # Safety
/// `entry` must be a finalized JIT entry and `ctx` a live context of the
/// shape that entry was compiled against.
#[allow(unsafe_code)] // the pinned-register bracket the JIT ABI requires; contract above
#[cfg(target_arch = "x86_64")]
#[unsafe(naked)]
pub unsafe extern "C" fn call_entry_preserving_pinned(
    ctx: *mut core::ffi::c_void,
    entry: NativeEntry,
) -> NativeStatus {
    // rdi = ctx, rsi = entry. Entry rsp is 8 (mod 16); one push makes it 0,
    // which is what the callee's `call` requires.
    core::arch::naked_asm!("push r15", "call rsi", "pop r15", "ret")
}

/// See the x86_64 sibling. AAPCS64 makes x19-x28 callee-saved; the pinned
/// register is x21.
///
/// # Safety
/// As the x86_64 sibling.
#[allow(unsafe_code)] // the pinned-register bracket the JIT ABI requires; contract above
#[cfg(target_arch = "aarch64")]
#[unsafe(naked)]
pub unsafe extern "C" fn call_entry_preserving_pinned(
    ctx: *mut core::ffi::c_void,
    entry: NativeEntry,
) -> NativeStatus {
    // x0 = ctx, x1 = entry; sp stays 16-aligned; the result rides in x0.
    core::arch::naked_asm!(
        "stp x21, x30, [sp, #-16]!",
        "blr x1",
        "ldp x21, x30, [sp], #16",
        "ret",
    )
}
