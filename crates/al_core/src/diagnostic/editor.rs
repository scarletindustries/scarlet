use std::env;
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Editor {
    Vscode,
    VscodeInsiders,
    Vscodium,
    Cursor,
    Sublime,
    Atom,
    Jetbrains,
    Zed,
}

// Lowercase substring patterns, checked in order (more specific first).
const PATTERNS: &[(&str, Editor)] = &[
    ("insiders", Editor::VscodeInsiders),
    ("codium", Editor::Vscodium),
    ("code", Editor::Vscode),
    ("cursor", Editor::Cursor),
    ("subl", Editor::Sublime),
    ("atom", Editor::Atom),
    ("intellij", Editor::Jetbrains),
    ("idea", Editor::Jetbrains),
    ("webstorm", Editor::Jetbrains),
    ("phpstorm", Editor::Jetbrains),
    ("pycharm", Editor::Jetbrains),
    ("rubymine", Editor::Jetbrains),
    ("goland", Editor::Jetbrains),
    ("rider", Editor::Jetbrains),
    ("zed", Editor::Zed),
];

fn match_editor(haystack: &str) -> Option<Editor> {
    // Strip "xcode" so the bare "code" pattern doesn't false-positive on macOS Xcode.app.
    let lower = haystack.to_lowercase().replace("xcode", "");
    PATTERNS
        .iter()
        .find(|(pat, _)| lower.contains(pat))
        .map(|(_, e)| *e)
}

pub fn detect_editor() -> Option<Editor> {
    for env_var in ["VISUAL", "EDITOR"] {
        if let Ok(editor) = env::var(env_var)
            && let Some(detected) = match_editor(&editor)
        {
            return Some(detected);
        }
    }

    let ps_args: &[&str] = if cfg!(target_os = "macos") {
        &["x", "-o", "comm="]
    } else if cfg!(target_os = "linux") {
        &["x", "--no-heading", "-o", "comm"]
    } else {
        return None;
    };

    Command::new("ps")
        .args(ps_args)
        .output()
        .ok()
        .filter(|r| r.status.success())
        .and_then(|r| {
            String::from_utf8_lossy(&r.stdout)
                .lines()
                .find_map(match_editor)
        })
}

pub fn build_editor_url(editor: Editor, abs_path: &str, line: i32, col: i32) -> String {
    match editor {
        Editor::Vscode => format!("vscode://file{abs_path}:{line}:{col}"),
        Editor::VscodeInsiders => format!("vscode-insiders://file{abs_path}:{line}:{col}"),
        Editor::Vscodium => format!("vscodium://file{abs_path}:{line}:{col}"),
        Editor::Cursor => format!("cursor://file{abs_path}:{line}:{col}"),
        Editor::Sublime => format!("subl://open?url=file://{abs_path}&line={line}&column={col}"),
        Editor::Atom => {
            format!("atom://core/open/file?filename={abs_path}&line={line}&column={col}")
        }
        Editor::Jetbrains => format!("idea://open?file={abs_path}&line={line}&column={col}"),
        Editor::Zed => format!("zed://file{abs_path}:{line}:{col}"),
    }
}
