use std::io::IsTerminal;

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

/// Decide whether to emit color. Honors the de-facto standards: `NO_COLOR`
/// (presence disables, per no-color.org), `CLICOLOR_FORCE` (non-zero forces
/// on), otherwise on only when stdout is a real terminal.
fn color_enabled() -> bool {
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    if let Some(force) = std::env::var_os("CLICOLOR_FORCE") {
        return force != "0";
    }
    std::io::stdout().is_terminal()
}

impl Palette {
    pub fn resolve() -> Self {
        if color_enabled() { ON } else { OFF }
    }
}
