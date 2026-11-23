//! The Scarlet VM: runs a compiled `Program`.
//!
//! It reads nothing but `scarlet_ir`, the contract with the compiler. The plan
//! it follows, and why, is `docs/vm-design.md`. It is being built one feature
//! at a time, so a program that needs something not built yet stops with
//! [`Stop::NotBuiltYet`], saying what.

#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        clippy::string_slice,
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss,
    )
)]
#![forbid(unsafe_code)]

// Heap words are 64 bits, and lengths and indices read from them are used as
// `usize` (see `word::to_usize`).
#[cfg(not(target_pointer_width = "64"))]
compile_error!("the Scarlet VM needs a 64-bit target");

mod array;
mod bigint;
mod binary;
mod code;
mod counts;
mod eq;
mod exec;
mod float;
mod hash;
mod heap;
mod host;
mod http;
mod json;
mod listing;
mod map;
mod os_host;
mod show;
mod stop;
mod value;
mod word;

use scarlet_ir::core_ir::Program;

pub use counts::Counts;
pub use host::{
    Deadline, Handle, Host, IoError, IpAddr, NetError, NoRandomness, OutputClosed, Received,
    SocketAddr, TlsError,
};
pub use listing::{Listing, listing};
pub use os_host::OsHost;
pub use stop::Stop;

/// What a run does once its program has ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AtExit {
    /// Nothing. The heap goes with the run, as it does when a process ends.
    Leave,
    /// Give up everything the run still holds, and stop with a bad-program
    /// [`Stop`] if any cell is left, because then some reference count was
    /// wrong.
    Audit,
}

/// Run `program` in `host`'s world.
pub fn run(program: &Program, host: &mut dyn Host, at_exit: AtExit) -> Result<(), Stop> {
    let code = code::load(program)?;
    let mut machine = exec::Machine::new(&code, host);
    let ended = machine.run()?;
    machine.finish(ended, at_exit)
}

/// Run `program` as [`run`] does, and say what the run counted: how often
/// each kind of instruction ran, and how many heap cells were made and
/// reused. A run that stops early has no counts, because the counting is by
/// whole blocks of instructions and a stop leaves some of them half run.
pub fn run_counted(
    program: &Program,
    host: &mut dyn Host,
    at_exit: AtExit,
) -> Result<Counts, Stop> {
    let counted = code::load_counted(program)?;
    let mut machine = exec::Machine::counting(&counted, host);
    let ended = machine.run()?;
    // Counted before the audit, which frees what the program still held.
    let counts = machine.counts(&counted, &ended);
    machine.finish(ended, at_exit)?;
    Ok(counts)
}

/// Run `program` as [`run`] does, and return its last value as `println`
/// shows it, or `None` when that value is `Nil`. The REPL runs each entry this
/// way, because an entry's last expression is the program's last value.
pub fn run_showing(program: &Program, host: &mut dyn Host) -> Result<Option<String>, Stop> {
    let code = code::load(program)?;
    let mut machine = exec::Machine::new(&code, host);
    let ended = machine.run()?;
    machine.shown(ended)
}
