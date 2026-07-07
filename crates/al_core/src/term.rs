use std::io::IsTerminal;

/// Decide whether to emit ANSI color for the given stream. Honors the de-facto
/// standards: `NO_COLOR` (non-empty disables, per no-color.org) and
/// `CLICOLOR_FORCE` (non-empty, non-"0" forces on even when piped); otherwise
/// on only when the stream is a real terminal.
pub fn color_enabled(s: &impl IsTerminal) -> bool {
    if std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()) {
        return false;
    }
    if std::env::var_os("CLICOLOR_FORCE").is_some_and(|v| !v.is_empty() && v != "0") {
        return true;
    }
    s.is_terminal()
}

/// Resolved ANSI palette. Every field is either a real escape sequence or an
/// empty string, decided once based on the environment so call sites can
/// interpolate unconditionally.
#[derive(Clone, Copy)]
pub struct Palette {
    pub reset: &'static str,
    pub bold: &'static str,
    pub dim: &'static str,
    pub red: &'static str,
    pub cyan: &'static str,
    pub blue: &'static str,
    pub error: &'static str,
    pub link_open: &'static str,
    pub link_close: &'static str,
}

const ON: Palette = Palette {
    reset: "\x1b[0m",
    bold: "\x1b[1m",
    dim: "\x1b[2m",
    red: "\x1b[31m",
    cyan: "\x1b[36m",
    blue: "\x1b[34m",
    error: "\x1b[1;31m",
    link_open: "\x1b[4;36m",
    link_close: "\x1b[0m",
};

const OFF: Palette = Palette {
    reset: "",
    bold: "",
    dim: "",
    red: "",
    cyan: "",
    blue: "",
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

    pub fn enabled(&self) -> bool {
        !self.reset.is_empty()
    }
}
