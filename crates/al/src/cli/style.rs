use std::io::IsTerminal;

use al_core::term::color_enabled;

/// Resolved ANSI palette for CLI chrome. Every field is either a real escape
/// sequence or an empty string, decided once based on the environment so call
/// sites can interpolate unconditionally.
#[derive(Clone, Copy)]
pub struct Palette {
    pub reset: &'static str,
    pub bold: &'static str,
    pub dim: &'static str,
    pub accent: &'static str,
    pub heading: &'static str,
    pub error: &'static str,
    pub link_open: &'static str,
    pub link_close: &'static str,
}

const ON: Palette = Palette {
    reset: "\x1b[0m",
    bold: "\x1b[1m",
    dim: "\x1b[2m",
    accent: "\x1b[36m",
    heading: "\x1b[1m",
    error: "\x1b[1;31m",
    link_open: "\x1b[4;36m",
    link_close: "\x1b[0m",
};

const OFF: Palette = Palette {
    reset: "",
    bold: "",
    dim: "",
    accent: "",
    heading: "",
    error: "",
    link_open: "",
    link_close: "",
};

impl Palette {
    pub fn for_stream(s: &impl IsTerminal) -> Self {
        if color_enabled(s) { ON } else { OFF }
    }

    pub fn for_stdout() -> Self {
        Self::for_stream(&std::io::stdout())
    }

    pub fn for_stderr() -> Self {
        Self::for_stream(&std::io::stderr())
    }
}
