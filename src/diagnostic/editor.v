module diagnostic

import os

pub enum Editor {
	unknown
	vscode
	vscode_insiders
	vscodium
	cursor
	sublime
	atom
	intellij
	webstorm
	phpstorm
	pycharm
	rubymine
	goland
	rider
	emacs
	vim
	neovim
	zed
}

const macos_editors = {
	'Code':            Editor.vscode
	'Code - Insiders': Editor.vscode_insiders
	'VSCodium':        Editor.vscodium
	'Cursor':          Editor.cursor
	'Sublime Text':    Editor.sublime
	'Atom':            Editor.atom
	'IntelliJ IDEA':   Editor.intellij
	'idea':            Editor.intellij
	'WebStorm':        Editor.webstorm
	'webstorm':        Editor.webstorm
	'PhpStorm':        Editor.phpstorm
	'phpstorm':        Editor.phpstorm
	'PyCharm':         Editor.pycharm
	'pycharm':         Editor.pycharm
	'RubyMine':        Editor.rubymine
	'rubymine':        Editor.rubymine
	'GoLand':          Editor.goland
	'goland':          Editor.goland
	'Rider':           Editor.rider
	'rider':           Editor.rider
	'Emacs':           Editor.emacs
	'emacs':           Editor.emacs
	'Vim':             Editor.vim
	'vim':             Editor.vim
	'nvim':            Editor.neovim
	'MacVim':          Editor.vim
	'Zed':             Editor.zed
	'zed':             Editor.zed
}

const linux_editors = {
	'code':          Editor.vscode
	'code-insiders': Editor.vscode_insiders
	'vscodium':      Editor.vscodium
	'codium':        Editor.vscodium
	'cursor':        Editor.cursor
	'sublime_text':  Editor.sublime
	'subl':          Editor.sublime
	'atom':          Editor.atom
	'idea':          Editor.intellij
	'idea.sh':       Editor.intellij
	'webstorm':      Editor.webstorm
	'webstorm.sh':   Editor.webstorm
	'phpstorm':      Editor.phpstorm
	'phpstorm.sh':   Editor.phpstorm
	'pycharm':       Editor.pycharm
	'pycharm.sh':    Editor.pycharm
	'rubymine':      Editor.rubymine
	'rubymine.sh':   Editor.rubymine
	'goland':        Editor.goland
	'goland.sh':     Editor.goland
	'rider':         Editor.rider
	'rider.sh':      Editor.rider
	'emacs':         Editor.emacs
	'vim':           Editor.vim
	'nvim':          Editor.neovim
	'gvim':          Editor.vim
	'zed':           Editor.zed
}

pub fn detect_editor() Editor {
	for env_var in ['VISUAL', 'EDITOR'] {
		if editor := os.getenv_opt(env_var) {
			if detected := editor_from_command(editor) {
				return detected
			}
		}
	}

	$if macos {
		return detect_editor_macos()
	} $else $if linux {
		return detect_editor_linux()
	} $else {
		return .unknown
	}
}

fn editor_from_command(cmd string) ?Editor {
	lower := cmd.to_lower()
	if lower.contains('code-insiders') {
		return .vscode_insiders
	}
	if lower.contains('codium') || lower.contains('vscodium') {
		return .vscodium
	}
	if lower.contains('code') || lower.contains('vscode') {
		return .vscode
	}
	if lower.contains('cursor') {
		return .cursor
	}
	if lower.contains('subl') || lower.contains('sublime') {
		return .sublime
	}
	if lower.contains('atom') {
		return .atom
	}
	if lower.contains('idea') {
		return .intellij
	}
	if lower.contains('webstorm') {
		return .webstorm
	}
	if lower.contains('phpstorm') {
		return .phpstorm
	}
	if lower.contains('pycharm') {
		return .pycharm
	}
	if lower.contains('rubymine') {
		return .rubymine
	}
	if lower.contains('goland') {
		return .goland
	}
	if lower.contains('rider') {
		return .rider
	}
	if lower.contains('emacs') {
		return .emacs
	}
	if lower.contains('nvim') || lower.contains('neovim') {
		return .neovim
	}
	if lower.contains('vim') {
		return .vim
	}
	if lower.contains('zed') {
		return .zed
	}
	return none
}

fn detect_editor_macos() Editor {
	result := os.execute('ps x -o comm=')
	if result.exit_code != 0 {
		return .unknown
	}

	processes := result.output

	if processes.contains('Code - Insiders') || processes.contains('code-insiders') {
		return .vscode_insiders
	}
	if processes.contains('VSCodium') || processes.contains('vscodium') {
		return .vscodium
	}
	if processes.contains('Visual Studio Code') || processes.contains('Code Helper')
		|| processes.contains('Code.app') {
		return .vscode
	}
	if processes.contains('Cursor.app') || processes.contains('/Cursor/') {
		return .cursor
	}
	if processes.contains('Sublime Text') {
		return .sublime
	}
	if processes.contains('Atom.app') || processes.contains('Atom Helper') {
		return .atom
	}
	if processes.contains('IntelliJ IDEA') || processes.contains('idea') {
		return .intellij
	}
	if processes.contains('WebStorm') || processes.contains('webstorm') {
		return .webstorm
	}
	if processes.contains('PhpStorm') || processes.contains('phpstorm') {
		return .phpstorm
	}
	if processes.contains('PyCharm') || processes.contains('pycharm') {
		return .pycharm
	}
	if processes.contains('RubyMine') || processes.contains('rubymine') {
		return .rubymine
	}
	if processes.contains('GoLand') || processes.contains('goland') {
		return .goland
	}
	if processes.contains('Rider') || processes.contains('rider') {
		return .rider
	}
	if processes.contains('Zed.app') || processes.contains('/zed') {
		return .zed
	}
	if processes.contains('Emacs') || processes.contains('emacs') {
		return .emacs
	}
	if processes.contains('nvim') || processes.contains('neovim') {
		return .neovim
	}
	if processes.contains('MacVim') || processes.contains('vim') {
		return .vim
	}

	return .unknown
}

fn detect_editor_linux() Editor {
	result := os.execute('ps x --no-heading -o comm')
	if result.exit_code != 0 {
		return .unknown
	}

	for line in result.output.split_into_lines() {
		process := line.trim_space()
		if editor := linux_editors[process] {
			return editor
		}
	}
	return .unknown
}

pub fn build_editor_url(editor Editor, abs_path string, line int, col int) string {
	return match editor {
		.vscode {
			'vscode://file${abs_path}:${line}:${col}'
		}
		.vscode_insiders {
			'vscode-insiders://file${abs_path}:${line}:${col}'
		}
		.vscodium {
			'vscodium://file${abs_path}:${line}:${col}'
		}
		.cursor {
			'cursor://file${abs_path}:${line}:${col}'
		}
		.sublime {
			'subl://open?url=file://${abs_path}&line=${line}&column=${col}'
		}
		.atom {
			'atom://core/open/file?filename=${abs_path}&line=${line}&column=${col}'
		}
		.intellij, .webstorm, .phpstorm, .pycharm, .rubymine, .goland, .rider {
			'idea://open?file=${abs_path}&line=${line}&column=${col}'
		}
		.zed {
			'zed://file${abs_path}:${line}:${col}'
		}
		else {
			'file://${abs_path}'
		}
	}
}
