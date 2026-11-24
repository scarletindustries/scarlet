use super::*;

#[test]
fn process_name_exact_match() {
    assert_eq!(match_process_name("code"), Some(Editor::Vscode));
    assert_eq!(
        match_process_name("/usr/local/bin/code"),
        Some(Editor::Vscode)
    );
    assert_eq!(match_process_name("Zed"), Some(Editor::Zed));
    // Substring false positives:
    assert_eq!(match_process_name("encoder"), None);
    assert_eq!(match_process_name("barcode-scanner"), None);
    assert_eq!(match_process_name("Xcode"), None);
    assert_eq!(
        match_process_name("/Applications/Xcode.app/Contents/MacOS/Xcode"),
        None
    );
}

#[test]
fn env_var_exact_basename_match() {
    assert_eq!(match_env_var("code"), Some(Editor::Vscode));
    assert_eq!(
        match_env_var("/usr/local/bin/code --wait"),
        Some(Editor::Vscode)
    );
    assert_eq!(match_env_var("PyCharm"), Some(jetbrains("pycharm")));
    // Unquoted flag-free path containing spaces:
    assert_eq!(
        match_env_var("/Applications/Sublime Text.app/Contents/SharedSupport/bin/subl"),
        Some(Editor::Sublime)
    );
    // Quoted path with flags:
    assert_eq!(
        match_env_var(
            "\"/Applications/Visual Studio Code.app/Contents/Resources/app/bin/code\" -w"
        ),
        Some(Editor::Vscode)
    );
    assert_eq!(
        match_env_var("'/Applications/Sublime Text.app/Contents/SharedSupport/bin/subl' -w"),
        Some(Editor::Sublime)
    );
    // Substring false positives:
    assert_eq!(match_env_var("encoder"), None);
    assert_eq!(match_env_var("/opt/xcode/bin/edit"), None);
    assert_eq!(match_env_var("nvim"), None);
}

#[test]
fn url_path_is_percent_encoded() {
    let url = build_editor_url(Editor::Vscode, "/Users/me/My Project/a#b.sca", 3, 7);
    assert_eq!(url, "vscode://file/Users/me/My%20Project/a%23b%2Esca:3:7");
    let url = build_editor_url(jetbrains("idea"), "/tmp/a b?.sca", 1, 1);
    assert_eq!(url, "idea://open?file=/tmp/a%20b%3F%2Esca&line=1&column=1");
}

#[test]
fn jetbrains_url_uses_detected_product_scheme() {
    let url = build_editor_url(jetbrains("pycharm"), "/tmp/a.sca", 2, 3);
    assert_eq!(url, "pycharm://open?file=/tmp/a%2Esca&line=2&column=3");
    let url = build_editor_url(jetbrains("clion"), "/tmp/a.sca", 2, 3);
    assert_eq!(url, "clion://open?file=/tmp/a%2Esca&line=2&column=3");
}
