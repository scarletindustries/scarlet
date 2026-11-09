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
    )
)]
#![forbid(unsafe_code)]

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

use std::io::Write;

use scarlet_ir::core_ir::Program;

pub use host::Host;
pub use stop::Stop;

/// Run `program` in `host`'s world, writing what it prints to `out`.
pub fn run(program: &Program, host: &Host, out: &mut dyn Write) -> Result<(), Stop> {
    let code = code::load(program);
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
    let code = code::load(program);
    let mut machine = exec::Machine::new(&code, host, out);
    let last = machine.run()?;
    machine.shown(last)
}
