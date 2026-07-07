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
