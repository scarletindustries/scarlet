module diagnostic

import os

const color_reset = '\x1b[0m'
const color_bold = '\x1b[1m'
const color_dim = '\x1b[2m'
const color_red = '\x1b[31m'
const color_yellow = '\x1b[33m'
const color_cyan = '\x1b[36m'
const color_blue = '\x1b[34m'
const link_start = '\x1b]8;;'
const link_end = '\x07'

fn severity_color(severity Severity) string {
	return match severity {
		.error { color_red }
		.warning { color_yellow }
		.hint { color_cyan }
	}
}

fn severity_label(severity Severity) string {
	return match severity {
		.error { 'error' }
		.warning { 'warning' }
		.hint { 'hint' }
	}
}

fn get_source_line(lines []string, line_number int) string {
	if line_number < 1 || line_number > lines.len {
		return ''
	}
	return lines[line_number - 1]
}

fn format_diagnostic_with_lines(d Diagnostic, lines []string, file_path string) string {
	mut result := ''

	color := severity_color(d.severity)
	label := severity_label(d.severity)

	abs_path := os.real_path(file_path)
	location := '${file_path}:${d.span.start_line}:${d.span.start_column}'
	editor := detect_editor()
	link_url := build_editor_url(editor, abs_path, d.span.start_line, d.span.start_column)

	result += '${color_bold}${color}${label}${color_reset}: ${d.message} ${color_dim}at ${link_start}${link_url}${link_end}${location}${link_start}${link_end}${color_reset}\n'

	line_num_width := '${d.span.start_line}'.len
	padding := ' '.repeat(line_num_width)

	source_line := get_source_line(lines, d.span.start_line)
	result += '${color_blue}${d.span.start_line}  |${color_reset} ${source_line}\n'

	mut caret_padding := ''
	for i := 1; i < d.span.start_column; i++ {
		if i < source_line.len && source_line[i] == `\t` {
			caret_padding += '\t'
		} else {
			caret_padding += ' '
		}
	}

	caret_len := if d.span.end_column > d.span.start_column {
		d.span.end_column - d.span.start_column
	} else {
		1
	}
	carets := '^'.repeat(caret_len)
	result += '${padding}    ${caret_padding}${color}${carets}${color_reset}'

	return result
}

pub fn print_diagnostics(diagnostics []Diagnostic, source string, file_path string) {
	lines := source.split_into_lines()

	mut output := []string{}
	for d in diagnostics {
		output << format_diagnostic_with_lines(d, lines, file_path)
	}

	error_count := count_errors(diagnostics)
	warning_count := count_warnings(diagnostics)

	if error_count > 0 || warning_count > 0 {
		mut parts := []string{}

		if error_count > 0 {
			noun := if error_count == 1 { 'error' } else { 'errors' }
			parts << '${color_bold}${color_red}${error_count} ${noun}${color_reset}'
		}

		if warning_count > 0 {
			noun := if warning_count == 1 { 'warning' } else { 'warnings' }
			parts << '${color_bold}${color_yellow}${warning_count} ${noun}${color_reset}'
		}

		output << 'Found ${parts.join(' and ')}'
	}

	if output.len > 0 {
		println(output.join('\n'))
	}
}
