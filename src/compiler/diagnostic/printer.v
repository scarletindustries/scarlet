module diagnostic

const color_reset = '\x1b[0m'
const color_bold = '\x1b[1m'
const color_red = '\x1b[31m'
const color_yellow = '\x1b[33m'
const color_cyan = '\x1b[36m'
const color_blue = '\x1b[34m'

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

fn get_source_line(source string, line_number int) string {
	lines := source.split_into_lines()
	if line_number < 1 || line_number > lines.len {
		return ''
	}
	return lines[line_number - 1]
}

pub fn format_diagnostic(d Diagnostic, source string, file_path string) string {
	mut result := ''

	color := severity_color(d.severity)
	label := severity_label(d.severity)
	result += '${color_bold}${color}${label}${color_reset}: ${d.message}\n'

	result += '${color_blue}  -->${color_reset} ${file_path}:${d.span.start_line}:${d.span.start_column}\n'

	line_num_width := '${d.span.start_line}'.len
	padding := ' '.repeat(line_num_width)
	result += '${color_blue}${padding}   |${color_reset}\n'

	source_line := get_source_line(source, d.span.start_line)
	result += '${color_blue}${d.span.start_line}  |${color_reset} ${source_line}\n'

	mut caret_padding := ''
	for i := 0; i < d.span.start_column; i++ {
		if i < source_line.len && source_line[i] == `\t` {
			caret_padding += '\t'
		} else {
			caret_padding += ' '
		}
	}
	result += '${color_blue}${padding}   |${color_reset} ${caret_padding}${color}^${color_reset}\n'

	return result
}

pub fn print_diagnostics(diagnostics []Diagnostic, source string, file_path string) {
	for d in diagnostics {
		println(format_diagnostic(d, source, file_path))
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

		println('Found ${parts.join(' and ')}')
	}
}
