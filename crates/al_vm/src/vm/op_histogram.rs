//! Op histogram instrumentation for the interpreter's dispatch loop.
//!
//! Compiled only under the `op-histogram` feature — a default build has no
//! counters, no branch at slice entry, and writes nothing to stderr (the
//! goldens assert an empty stderr). Build the measuring binary with
//! `cargo build --release --features op-histogram`.
//!
//! Two histograms are accumulated, both keyed by the identity of the thing
//! counted rather than by anything positional: `OP_COUNTS` by opcode
//! discriminant, `FN_COUNTS` by `FuncIdx` (the index into
//! `Program::functions`, which is also how the interpreter names the frame
//! it is running). `TOTAL_OPS` is the denominator for both. Every scheduler
//! thread bumps the same relaxed counters; the numbers are a profile, not a
//! synchronization mechanism, so relaxed ordering with no per-thread
//! batching is exactly the contract we want.
//!
//! The dump fires once, from slice entry, as soon as `TOTAL_OPS` passes a
//! threshold (`AL_OP_HISTOGRAM_OPS`, default 20M) — a served-load run is
//! never asked to stop, so "after N ops" is the only place a full profile
//! can be printed. Opcode and function names are resolved at dump time from
//! the live `Program`, so no name table has to be carried through the hot
//! path.
//!
//! Only *interpreted* ops are counted: a natively-compiled body is entered
//! through `native_entry_check!` and never reaches the dispatch loop head,
//! so it contributes to neither histogram nor to `TOTAL_OPS`. `AL_NATIVE`
//! defaults to `native`, so the measuring invocation is
//!
//! ```text
//! AL_NATIVE=off ./target/release/al run …
//! ```
//!
//! Without that, a JIT'd function reads 0.00% and every interpreted
//! function's share is inflated by the missing denominator — a hot function
//! appears to vanish with no change to the AL code. The dump names the mode
//! it ran under so a skewed profile cannot be quoted as a share of AL ops.

use crate::bytecode::native::{self, NativeMode};
use crate::bytecode::{Op, Program};
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// `Op` is `#[repr(u8)]`, so its discriminant indexes this directly.
const OP_SLOTS: usize = 256;

/// Functions counted individually; anything above lands in the overflow slot
/// so the histogram's denominator stays honest.
const FN_SLOTS: usize = 4096;

const DEFAULT_THRESHOLD: u64 = 20_000_000;

pub static OP_COUNTS: [AtomicU64; OP_SLOTS] = [const { AtomicU64::new(0) }; OP_SLOTS];
pub static FN_COUNTS: [AtomicU64; FN_SLOTS + 1] = [const { AtomicU64::new(0) }; FN_SLOTS + 1];
pub static TOTAL_OPS: AtomicU64 = AtomicU64::new(0);

static DUMPED: AtomicBool = AtomicBool::new(false);

/// One executed instruction, attributed to the function whose frame is live.
#[inline(always)]
pub fn record(op: Op, func_idx: i32) {
    TOTAL_OPS.fetch_add(1, Ordering::Relaxed);
    OP_COUNTS[op as u8 as usize].fetch_add(1, Ordering::Relaxed);
    let slot = if (0..FN_SLOTS as i32).contains(&func_idx) {
        func_idx as usize
    } else {
        FN_SLOTS
    };
    FN_COUNTS[slot].fetch_add(1, Ordering::Relaxed);
}

/// Slice-entry check: dump once, the first time the sample is big enough.
#[inline]
pub fn maybe_dump(program: &Program) {
    if TOTAL_OPS.load(Ordering::Relaxed) < threshold() {
        return;
    }
    if DUMPED.swap(true, Ordering::SeqCst) {
        return;
    }
    dump_op_counts(program);
}

fn threshold() -> u64 {
    static CACHED: AtomicU64 = AtomicU64::new(0);
    match CACHED.load(Ordering::Relaxed) {
        0 => {
            let t = std::env::var("AL_OP_HISTOGRAM_OPS")
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
                .filter(|t| *t > 0)
                .unwrap_or(DEFAULT_THRESHOLD);
            CACHED.store(t, Ordering::Relaxed);
            t
        }
        t => t,
    }
}

/// Write both histograms to stderr, biggest share first.
///
/// The header names the native mode because the sample's *scope* depends on
/// it: under anything but `AL_NATIVE=off` the percentages are shares of
/// interpreted ops, not of AL ops, and the bodies that are missing are
/// exactly the hot ones the JIT chose to compile.
pub fn dump_op_counts(program: &Program) {
    let total = TOTAL_OPS.load(Ordering::Relaxed).max(1);
    let mode = native::config().mode;
    let mut out = String::new();
    out.push_str(&format!(
        "\n=== op histogram ({} ops sampled, AL_NATIVE={}) ===\n",
        TOTAL_OPS.load(Ordering::Relaxed),
        mode_name(mode)
    ));
    if mode != NativeMode::Off {
        out.push_str(
            "!!! INCOMPLETE SAMPLE: natively-compiled bodies bypass the dispatch\n\
             !!! loop and are NOT counted — every share below is a share of\n\
             !!! *interpreted* ops, not of AL ops. Re-run with AL_NATIVE=off\n\
             !!! before comparing these numbers to an interpreter baseline.\n",
        );
    }

    out.push_str("--- by function ---\n");
    let mut fns: Vec<(usize, u64)> = FN_COUNTS
        .iter()
        .enumerate()
        .map(|(i, c)| (i, c.load(Ordering::Relaxed)))
        .filter(|(_, c)| *c > 0)
        .collect();
    fns.sort_unstable_by_key(|&(_, count)| std::cmp::Reverse(count));
    for (idx, count) in fns {
        let name = if idx == FN_SLOTS {
            "<overflow>"
        } else {
            program
                .functions
                .get(idx)
                .map(|f| &*f.name)
                .unwrap_or("<unknown>")
        };
        out.push_str(&format!(
            "{:6.2}%  fn#{:<4} {:<40} {}\n",
            count as f64 * 100.0 / total as f64,
            idx,
            name,
            count
        ));
    }

    out.push_str("--- by opcode ---\n");
    let names = opcode_names(program);
    let mut ops: Vec<(usize, u64)> = OP_COUNTS
        .iter()
        .enumerate()
        .map(|(i, c)| (i, c.load(Ordering::Relaxed)))
        .filter(|(_, c)| *c > 0)
        .collect();
    ops.sort_unstable_by_key(|&(_, count)| std::cmp::Reverse(count));
    for (disc, count) in ops {
        out.push_str(&format!(
            "{:6.2}%  op#{:<4} {:<40} {}\n",
            count as f64 * 100.0 / total as f64,
            disc,
            names[disc].as_deref().unwrap_or("<unknown>"),
            count
        ));
    }

    let mut err = std::io::stderr().lock();
    let _ = err.write_all(out.as_bytes());
    let _ = err.flush();
}

/// `AL_NATIVE`'s spelling of the live mode, so the header quotes back the
/// value an operator would set.
fn mode_name(mode: NativeMode) -> &'static str {
    match mode {
        NativeMode::Off => "off",
        NativeMode::Native => "native",
        NativeMode::Mix => "mix",
    }
}

/// Discriminant → mnemonic, recovered from the program's own code: every
/// opcode with a nonzero count was fetched from it, so no `u8 → Op` cast is
/// needed and the mapping cannot go stale as opcodes are added.
fn opcode_names(program: &Program) -> Vec<Option<String>> {
    let mut names: Vec<Option<String>> = vec![None; OP_SLOTS];
    for instr in &program.code {
        let slot = &mut names[instr.op as u8 as usize];
        if slot.is_none() {
            *slot = Some(format!("{:?}", instr.op));
        }
    }
    names
}

const _: fn() = || {
    // The counters are keyed by `Op`'s discriminant; a wider repr would index
    // out of `OP_SLOTS`.
    let _: [(); 1] = [(); (std::mem::size_of::<Op>() == 1) as usize];
};
