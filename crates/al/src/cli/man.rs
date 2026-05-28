use std::io::{self, Write};

use clap::Command;

/// Emit a roff(7) man page to stdout, generated from the clap command model.
/// The man page is a standardized format — its content is entirely our own
/// `about`/doc-comment metadata, just rendered as `man(1)` expects.
///
/// Usage: `al man > al.1` then `man ./al.1`, or `al man | man -l -`.
pub fn render(cmd: &Command) -> io::Result<()> {
    let man = clap_mangen::Man::new(cmd.clone());
    man.render(&mut io::stdout())?;
    io::stdout().flush()
}
