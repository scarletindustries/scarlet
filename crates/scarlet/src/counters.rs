//! What `scarlet run --counters FILE` writes: one run's counts, as JSON.
//!
//! `benches/run.py` reads this file. It holds the VM's own counts, which are
//! the same on every run of the same program with the same input, so two
//! builds can be compared by them exactly.

use scarlet_vm::Counts;
use serde_json::{Map, Value, json};

/// `counts` as a JSON object:
///
/// - `instructions`: how many VM instructions ran.
/// - `ops`: the same count split by kind of instruction. Every kind is
///   listed, with 0 for one that never ran. `scarlet dis --instrs` prints
///   instructions under the same names.
/// - `cells_made`: heap cells handed out.
/// - `cells_live`: cells not yet freed when the program ended.
/// - `heap_words`: the words of 8 bytes the heap grew to, less those of cells too
///   long to be in it.
pub fn json(counts: &Counts) -> String {
    let total = counts
        .ops
        .iter()
        .fold(0u64, |sum, (_, n)| sum.saturating_add(*n));
    let ops: Map<String, Value> = counts
        .ops
        .iter()
        .map(|(name, n)| ((*name).to_string(), json!(n)))
        .collect();
    let all = json!({
        "instructions": total,
        "ops": ops,
        "cells_made": counts.cells_made,
        "cells_live": counts.cells_live,
        "heap_words": counts.heap_words,
    });
    format!("{all:#}\n")
}

#[cfg(test)]
mod tests;
