use std::io::{self, Write};

use clap::Command;

/// Writes a roff(7) man page to stdout, generated from the clap command model.
/// Usage: `scarlet man > scarlet.1` then `man ./scarlet.1`, or `scarlet man | man -l -`.
pub fn render(cmd: &Command) -> io::Result<()> {
    let man = clap_mangen::Man::new(cmd.clone());
    man.render(&mut io::stdout())?;
    io::stdout().flush()
}
