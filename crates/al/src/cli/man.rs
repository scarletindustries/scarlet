use std::io::{self, Write};

use clap::Command;

/// Emit a roff(7) man page to stdout, generated from the clap command model.
/// Usage: `al man > al.1` then `man ./al.1`, or `al man | man -l -`.
pub fn render(cmd: &Command) -> io::Result<()> {
    let man = clap_mangen::Man::new(cmd.clone());
    man.render(&mut io::stdout())?;
    io::stdout().flush()
}
