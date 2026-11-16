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
mod eq;
mod exec;
mod float;
mod hash;
mod heap;
mod host;
mod http;
mod json;
mod map;
mod show;
mod stop;
mod value;
mod word;

use std::io::Write;

use scarlet_ir::core_ir::Program;

pub use host::Host;
pub use stop::Stop;

/// Run `program` in `host`'s world, writing what it prints to `out`.
pub fn run(program: &Program, host: &Host, out: &mut dyn Write) -> Result<(), Stop> {
    let code = code::load(program)?;
    exec::Machine::new(&code, host, out).run()?;
    Ok(())
}

/// Run `program` as [`run`] does, and return its last value as `println`
/// shows it, or `None` when that value is `Nil`. The REPL runs each entry this
/// way, because an entry's last expression is the program's last value.
pub fn run_showing(
    program: &Program,
    host: &Host,
    out: &mut dyn Write,
) -> Result<Option<String>, Stop> {
    let code = code::load(program)?;
    let mut machine = exec::Machine::new(&code, host, out);
    let last = machine.run()?;
    machine.shown(last)
}
