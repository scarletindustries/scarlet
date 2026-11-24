//! What a run counts about itself, for `scarlet run --counters`.
//!
//! Every number here is the same on every run of the same program with the
//! same input, on any machine. That is what makes them fit to judge a change
//! to the VM by, where time is not.
//!
//! Counting each instruction in the interpreter's loop would slow every run
//! or need a second copy of the loop. So a counted run loads the program
//! with one extra instruction at the head of each basic block, which counts
//! the block (`code::load_counted`), and an ordinary run loads none. A block
//! is a run of instructions that is only entered at its first and only left
//! at its last, so when the program has run to its end, each instruction ran
//! exactly as often as its block did.

/// What one run counted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Counts {
    /// Every kind of instruction the VM has, by name, and how many times the
    /// run ran it. A kind that never ran is listed with 0.
    pub ops: Vec<(&'static str, u64)>,
    /// Cells the heap handed out, from the arena's end or from a free list.
    pub cells_made: usize,
    /// Cells a constructor wrote over in place instead of taking a new one.
    pub cells_reused: usize,
    /// Cells not yet freed when the program ended.
    pub cells_live: usize,
}
