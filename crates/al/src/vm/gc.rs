//! GC safepoints: the collection side of the rooting rule.
//!
//! [`cost`](super::cost) is the budget side — every allocating opcode
//! computes a worst-case word need from it. This module is where that budget
//! is spent: `VM::ensure` is the one point in the VM that may collect, and
//! [`VmRoots`] is the definition of what "live" means while it does. The
//! discipline (idea 5 of `al_core::heap`'s front door): the collector moves
//! objects, so a heap `Value` held in a Rust local across an allocation is a
//! dangling-pointer bug. Every allocating opcode therefore:
//!
//! 1. computes its worst-case allocation need in words UP FRONT, from the
//!    `cost` model, while every operand is still on the VM stack or in a
//!    frame (= rooted) — variable-size ops read operand sizes through the
//!    budget peeks below, or stage bytes in host memory (a Rust String/Vec
//!    is not a heap value) to compute their real need first;
//! 2. calls `self.ensure(need)` — the one point that may collect, with the
//!    operand stack and every frame's `captures` closure as the root set
//!    (the globals table holds frozen-published values and is not a root);
//! 3. only then pops operands and allocates. Allocation after `ensure`
//!    cannot fail and never collects (`ProcHeap::alloc_young` is a pure
//!    bump), and debug builds assert each charge fits the ensured budget
//!    (the watermark in `alloc_young`).
//!
//! Collection is not free time. `VM::collect` charges reductions
//! (words copied / divisor), accruing the debt into [`GcDebt`] for the
//! call-checkpoint arms to drain through `VM::take_gc_reds`, so a GC-heavy
//! process is preempted at its next call instead of riding free on the
//! scheduler. The checkpoints test the debt before draining it — it is zero
//! on every call that did not collect, and an unconditional drain puts a
//! store on the hottest path in the VM. The debt never crosses processes:
//! `execute_slice` drops any leftover at slice entry.
//!
//! `AL_GC_STRESS=1` turns every `ensure` into a collection — the decision
//! lives in `ProcHeap::needs_collect`, so stress is a heap property, not a
//! call-site property — and the suite run under it proves opcode by opcode
//! that the rule is followed.

use al_core::bytecode::{Value, ValueView};

use super::{CallFrame, VM, cost, range_len};

/// Words of collection work that cost one reduction. Charging the copy
/// (`words processed / divisor`) means a collection-heavy process is
/// preempted at its next call instead of riding free on the scheduler.
pub(super) const GC_WORDS_PER_REDUCTION: usize = 64;

/// Reduction debt accrued by collections (`VM::collect`), scheduler-wide.
#[derive(Default)]
pub(super) struct GcDebt {
    /// Reductions owed for collection work since the last call checkpoint;
    /// drained by `VM::take_gc_reds` in the `Call`/`CallSelf` arms and
    /// dropped at slice entry (`execute_slice`): the charge shrinks the
    /// slice it accrued in and never bills the next process to run.
    pub(super) pending_reds: i32,
}

/// The rooting-rule root set: every value on the
/// operand stack plus each frame's `captures` closure, nothing else (the
/// globals table holds frozen-published words — not roots). The collector
/// rewrites each root in place as it evacuates.
struct VmRoots<'a> {
    stack: &'a mut [Value],
    frames: &'a mut [CallFrame],
}

impl al_core::heap::Roots for VmRoots<'_> {
    fn for_each(&mut self, f: &mut dyn FnMut(&mut Value)) {
        for v in self.stack.iter_mut() {
            f(v);
        }
        for frame in self.frames.iter_mut() {
            f(&mut frame.captures);
        }
    }
}

impl VM {
    /// The GC safepoint: guarantee `need` words of young headroom in the
    /// running process's arena, collecting first when the space is short —
    /// or unconditionally under `AL_GC_STRESS=1` (`ProcHeap::needs_collect`
    /// routes the decision so stress mode is a heap property, not a call-site
    /// property).
    #[inline]
    pub(super) fn ensure(&mut self, need: usize) {
        if self.heap.needs_collect(need) {
            self.collect(need);
        } else {
            self.heap.note_ensured(need);
        }
    }

    /// Collect the running process. The roots — the operand stack and every
    /// frame's `captures` closure, nothing else — are handed to the heap's
    /// evacuating collector (`ProcHeap::ensure` → minor/major Cheney), which
    /// moves the live graph and rewrites the roots in place. Collection work
    /// charges reductions (words copied / divisor) so a GC-heavy
    /// process still yields fairly; applied at the next call checkpoint.
    #[cold]
    #[inline(never)]
    pub(super) fn collect(&mut self, need: usize) {
        let copied = {
            let mut roots = VmRoots {
                stack: &mut self.stack,
                frames: &mut self.frames,
            };
            self.heap.ensure(need, &mut roots)
        };
        let reds = 1 + copied / GC_WORDS_PER_REDUCTION;
        self.gc.pending_reds = self
            .gc
            .pending_reds
            .saturating_add(reds.min(i32::MAX as usize) as i32);
    }

    /// Reductions owed for collection work since the last checkpoint;
    /// consumed by the `Call`/`TailCall`/`CallSelf`/`TailCallSelf` arms.
    /// Callers test `gc.pending_reds` for zero first: the drain's store
    /// belongs off the per-call fast path, where it is pure overhead on
    /// every call that did not collect.
    #[inline]
    pub(super) fn take_gc_reds(&mut self) -> i32 {
        std::mem::take(&mut self.gc.pending_reds)
    }

    // --- Budget peeks ---------------------------------------------------------
    //
    // Worst-case needs are computed from operands while they are still on the
    // stack (rooted); these helpers read without popping. `d` counts slots
    // down from the top (0 = top of stack).

    /// Byte length of the string `d` slots below the top; 0 when absent or
    /// not a string (the opcode's type error path allocates nothing).
    pub(super) fn peek_str_len(&self, d: usize) -> usize {
        self.peek_at(d).and_then(|v| v.as_str()).map_or(0, str::len)
    }

    /// Logical byte length of the binary `d` slots below the top; 0 when
    /// absent or not a binary.
    pub(super) fn peek_bin_len(&self, d: usize) -> usize {
        self.peek_at(d)
            .and_then(|v| v.as_binary())
            .map_or(0, |b| (b.bit_len() / 8) as usize)
    }

    /// Length and materialization cost of the sequence operand `d` slots
    /// below the top: an Array is already a tree (cost 0), a Range is
    /// materialized into one (a fresh build). Anything else is the opcode's
    /// error path (no allocation).
    pub(super) fn peek_seq(&self, d: usize) -> (usize, usize) {
        match self.peek_at(d).map(Value::kind) {
            Some(ValueView::Array(a)) => (a.len(), 0),
            Some(ValueView::Range(s, e)) => {
                let len = range_len(s, e).max(0) as usize;
                // Far-out-of-range bounds spill each element to a big-int
                // box on top of the tree itself.
                let spill = if Value::fits_small_int(s) && Value::fits_small_int(e) {
                    0
                } else {
                    len * cost::BIG_INT
                };
                (len, cost::seq_build(len) + spill)
            }
            _ => (0, 0),
        }
    }

    pub(super) fn peek_at(&self, d: usize) -> Option<&Value> {
        self.stack.len().checked_sub(1 + d).map(|i| &self.stack[i])
    }

    /// Budget pre-scan for `Op::HttpParseHead`: an upper bound on the
    /// CRLF-terminated lines the head parser can produce from the binary `d`
    /// slots below the top, counting line feeds in at most
    /// `cost::HTTP_SCAN_CAP` bytes from byte offset `off`. The window is
    /// sound here because the parser's head cap (`MAX_HEAD`, half this
    /// window) measures from the request-line start, which only drifts past
    /// `off` over leading empty lines — two bytes each, every one adding to
    /// the count; the one line that may straddle the window is the count's
    /// `+ 1`.
    pub(super) fn peek_http_head_lines(&self, d: usize, off: i64) -> usize {
        self.peek_http_lines(d, off, cost::HTTP_SCAN_CAP, usize::MAX)
    }

    /// The most trailer fields `http`'s chunk decoder can build in one call:
    /// its trailer-block cap (`MAX_TRAILER_BLOCK`, 64 KiB) over the smallest
    /// field line (`a:\r\n`, four bytes), plus the one line that may straddle
    /// the cap — the cap is checked where a line starts, not where it ends.
    const MAX_TRAILER_FIELDS: usize = 64 * 1024 / 4 + 1;

    /// Budget pre-scan for `Op::HttpChunkDecode`: an upper bound on the
    /// trailer fields the decoder can produce from the binary `d` slots
    /// below the top. Unlike the head, the trailer block's cap bounds its
    /// size, not its distance from `off` — the block sits past the chunk
    /// data, arbitrarily deep into the buffer — so a fixed window from `off`
    /// could under-count it and break the rooting rule. Count line feeds
    /// over the whole remaining buffer instead (a decode pass copies those
    /// bytes anyway), clamped to [`Self::MAX_TRAILER_FIELDS`] so a line-feed
    /// flood cannot demand an absurd reservation.
    pub(super) fn peek_http_trailer_lines(&self, d: usize, off: i64) -> usize {
        self.peek_http_lines(d, off, usize::MAX, Self::MAX_TRAILER_FIELDS)
    }

    /// Line feeds in at most `scan` bytes of the binary `d` slots below the
    /// top from byte offset `off` — counting at most `most`, stopping early
    /// once reached — plus one for a final line the window cuts off.
    /// Byte-aligned views — every socket buffer — are scanned in place; the
    /// pathological unaligned case falls back to its byte length (every byte
    /// could start a line).
    fn peek_http_lines(&self, d: usize, off: i64, scan: usize, most: usize) -> usize {
        match self.peek_at(d).and_then(|v| v.as_binary()) {
            Some(b) => {
                let len = (b.bit_len() / 8) as usize;
                let start = (off.max(0) as usize).min(len);
                let end = len.min(start.saturating_add(scan));
                let feeds = if b.bit_offset().is_multiple_of(8) {
                    let base = (b.bit_offset() / 8) as usize;
                    let window = &b.backing()[base + start..base + end];
                    memchr::memchr_iter(b'\n', window).take(most).count()
                } else {
                    (end - start).min(most)
                };
                feeds + 1
            }
            None => 0,
        }
    }
}
