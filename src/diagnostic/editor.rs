use std::env;
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Editor {
    Unknown,
    Vscode,
    VscodeInsiders,
    Vscodium,
    Cursor,
    Sublime,
    Atom,
    Intellij,
    Webstorm,
    Phpstorm,
    Pycharm,
    Rubymine,
    Goland,
    Rider,
    Emacs,
    Vim,
    Neovim,
    Zed,
}

#[allow(dead_code)]
pub const MACOS_EDITORS: &[(&str, Editor)] = &[
    ("Code", Editor::Vscode),
    ("Code - Insiders", Editor::VscodeInsiders),
    ("VSCodium", Editor::Vscodium),
    ("Cursor", Editor::Cursor),
    ("Sublime Text", Editor::Sublime),
    ("Atom", Editor::Atom),
    ("IntelliJ IDEA", Editor::Intellij),
    ("idea", Editor::Intellij),
    ("WebStorm", Editor::Webstorm),
    ("webstorm", Editor::Webstorm),
    ("PhpStorm", Editor::Phpstorm),
    ("phpstorm", Editor::Phpstorm),
    ("PyCharm", Editor::Pycharm),
    ("pycharm", Editor::Pycharm),
    ("RubyMine", Editor::Rubymine),
    ("rubymine", Editor::Rubymine),
    ("GoLand", Editor::Goland),
    ("goland", Editor::Goland),
    ("Rider", Editor::Rider),
    ("rider", Editor::Rider),
    ("Emacs", Editor::Emacs),
    ("emacs", Editor::Emacs),
    ("Vim", Editor::Vim),
    ("vim", Editor::Vim),
    ("nvim", Editor::Neovim),
    ("MacVim", Editor::Vim),
    ("Zed", Editor::Zed),
    ("zed", Editor::Zed),
];

#[allow(dead_code)]
pub const LINUX_EDITORS: &[(&str, Editor)] = &[
    ("code", Editor::Vscode),
    ("code-insiders", Editor::VscodeInsiders),
    ("vscodium", Editor::Vscodium),
    ("codium", Editor::Vscodium),
    ("cursor", Editor::Cursor),
    ("sublime_text", Editor::Sublime),
    ("subl", Editor::Sublime),
    ("atom", Editor::Atom),
    ("idea", Editor::Intellij),
    ("idea.sh", Editor::Intellij),
    ("webstorm", Editor::Webstorm),
    ("webstorm.sh", Editor::Webstorm),
    ("phpstorm", Editor::Phpstorm),
    ("phpstorm.sh", Editor::Phpstorm),
    ("pycharm", Editor::Pycharm),
    ("pycharm.sh", Editor::Pycharm),
    ("rubymine", Editor::Rubymine),
    ("rubymine.sh", Editor::Rubymine),
    ("goland", Editor::Goland),
    ("goland.sh", Editor::Goland),
    ("rider", Editor::Rider),
    ("rider.sh", Editor::Rider),
    ("emacs", Editor::Emacs),
    ("vim", Editor::Vim),
    ("nvim", Editor::Neovim),
    ("gvim", Editor::Vim),
    ("zed", Editor::Zed),
];

pub fn detect_editor() -> Editor {
    for env_var in ["VISUAL", "EDITOR"] {
        if let Ok(editor) = env::var(env_var)
            && let Some(detected) = editor_from_command(&editor)
        {
            return detected;
        }
    }

    if cfg!(target_os = "macos") {
        detect_editor_macos()
    } else if cfg!(target_os = "linux") {
        detect_editor_linux()
    } else {
        Editor::Unknown
    }
}

fn editor_from_command(cmd: &str) -> Option<Editor> {
    let lower = cmd.to_lowercase();
    if lower.contains("code-insiders") {
        return Some(Editor::VscodeInsiders);
    }
    if lower.contains("codium") || lower.contains("vscodium") {
        return Some(Editor::Vscodium);
    }
    if lower.contains("code") || lower.contains("vscode") {
        return Some(Editor::Vscode);
    }
    if lower.contains("cursor") {
        return Some(Editor::Cursor);
    }
    if lower.contains("subl") || lower.contains("sublime") {
        return Some(Editor::Sublime);
    }
    if lower.contains("atom") {
        return Some(Editor::Atom);
    }
    if lower.contains("idea") {
        return Some(Editor::Intellij);
    }
    if lower.contains("webstorm") {
        return Some(Editor::Webstorm);
    }
    if lower.contains("phpstorm") {
        return Some(Editor::Phpstorm);
    }
    if lower.contains("pycharm") {
        return Some(Editor::Pycharm);
    }
    if lower.contains("rubymine") {
        return Some(Editor::Rubymine);
    }
    if lower.contains("goland") {
        return Some(Editor::Goland);
    }
    if lower.contains("rider") {
        return Some(Editor::Rider);
    }
    if lower.contains("emacs") {
        return Some(Editor::Emacs);
    }
    if lower.contains("nvim") || lower.contains("neovim") {
        return Some(Editor::Neovim);
    }
    if lower.contains("vim") {
        return Some(Editor::Vim);
    }
    if lower.contains("zed") {
        return Some(Editor::Zed);
    }
    None
}

#[allow(dead_code)]
fn detect_editor_macos() -> Editor {
    let result = Command::new("ps").args(["x", "-o", "comm="]).output();
    let Ok(result) = result else {
        return Editor::Unknown;
    };
    if !result.status.success() {
        return Editor::Unknown;
    }

    let processes = String::from_utf8_lossy(&result.stdout);

    if processes.contains("Code - Insiders") || processes.contains("code-insiders") {
        return Editor::VscodeInsiders;
    }
    if processes.contains("VSCodium") || processes.contains("vscodium") {
        return Editor::Vscodium;
    }
    if processes.contains("Visual Studio Code")
        || processes.contains("Code Helper")
        || processes.contains("Code.app")
    {
        return Editor::Vscode;
    }
    if processes.contains("Cursor.app") || processes.contains("/Cursor/") {
        return Editor::Cursor;
    }
    if processes.contains("Sublime Text") {
        return Editor::Sublime;
    }
    if processes.contains("Atom.app") || processes.contains("Atom Helper") {
        return Editor::Atom;
    }
    if processes.contains("IntelliJ IDEA") || processes.contains("idea") {
        return Editor::Intellij;
    }
    if processes.contains("WebStorm") || processes.contains("webstorm") {
        return Editor::Webstorm;
    }
    if processes.contains("PhpStorm") || processes.contains("phpstorm") {
        return Editor::Phpstorm;
    }
    if processes.contains("PyCharm") || processes.contains("pycharm") {
        return Editor::Pycharm;
    }
    if processes.contains("RubyMine") || processes.contains("rubymine") {
        return Editor::Rubymine;
    }
    if processes.contains("GoLand") || processes.contains("goland") {
        return Editor::Goland;
    }
    if processes.contains("Rider") || processes.contains("rider") {
        return Editor::Rider;
    }
    if processes.contains("Zed.app") || processes.contains("/zed") {
        return Editor::Zed;
    }
    if processes.contains("Emacs") || processes.contains("emacs") {
        return Editor::Emacs;
    }
    if processes.contains("nvim") || processes.contains("neovim") {
        return Editor::Neovim;
    }
    if processes.contains("MacVim") || processes.contains("vim") {
        return Editor::Vim;
    }

    Editor::Unknown
}

#[allow(dead_code)]
fn detect_editor_linux() -> Editor {
    let result = Command::new("ps")
        .args(["x", "--no-heading", "-o", "comm"])
        .output();
    let Ok(result) = result else {
        return Editor::Unknown;
    };
    if !result.status.success() {
        return Editor::Unknown;
    }

    let output = String::from_utf8_lossy(&result.stdout);
    for line in output.lines() {
        let process = line.trim();
        for (name, editor) in LINUX_EDITORS {
            if *name == process {
                return *editor;
            }
        }
    }
    Editor::Unknown
}

pub fn build_editor_url(editor: Editor, abs_path: &str, line: i32, col: i32) -> String {
    match editor {
        Editor::Vscode => format!("vscode://file{abs_path}:{line}:{col}"),
        Editor::VscodeInsiders => format!("vscode-insiders://file{abs_path}:{line}:{col}"),
        Editor::Vscodium => format!("vscodium://file{abs_path}:{line}:{col}"),
        Editor::Cursor => format!("cursor://file{abs_path}:{line}:{col}"),
        Editor::Sublime => {
            format!("subl://open?url=file://{abs_path}&line={line}&column={col}")
        }
        Editor::Atom => {
            format!("atom://core/open/file?filename={abs_path}&line={line}&column={col}")
        }
        Editor::Intellij
        | Editor::Webstorm
        | Editor::Phpstorm
        | Editor::Pycharm
        | Editor::Rubymine
        | Editor::Goland
        | Editor::Rider => {
            format!("idea://open?file={abs_path}&line={line}&column={col}")
        }
        Editor::Zed => format!("zed://file{abs_path}:{line}:{col}"),
        _ => format!("file://{abs_path}"),
    }
}
