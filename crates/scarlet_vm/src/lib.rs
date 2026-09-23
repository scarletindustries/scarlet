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
mod value;

use std::io::Write;

use scarlet_ir::core_ir::Program;

pub use host::Host;

/// Why a run ended before the program did.
#[derive(Debug, PartialEq, Eq)]
pub enum Stop {
    /// The program reached something the VM does not run yet.
    NotBuiltYet(String),
    /// Where the program's output goes was closed, like a pipe into `head`.
    /// Nothing is wrong with the program, so this stops it quietly.
    OutputClosed,
    /// The program's heap grew past what the VM can address. A limit of the
    /// machine, like running out of memory, not a bug in the program.
    HeapFull,
    /// The program broke a promise the compiler makes about every program,
    /// like a `match` having an arm for every value. Only a compiler bug
    /// gives one, so this says what, rather than guessing on.
    BadProgram(String),
}

/// Run `program` in `host`'s world, writing what it prints to `out`.
pub fn run(program: &Program, host: &Host, out: &mut dyn Write) -> Result<(), Stop> {
    let code = code::load(program);
    exec::Machine::new(&code, host, out).run()?;
    Ok(())
}
