module diagnostic

pub struct Span {
pub:
	start_line   int
	start_column int
	end_line     int
	end_column   int
}

pub fn point_span(line int, column int) Span {
	return Span{
		start_line:   line
		start_column: column
		end_line:     line
		end_column:   column + 1
	}
}

pub fn range_span(line int, start_column int, end_column int) Span {
	return Span{
		start_line:   line
		start_column: start_column
		end_line:     line
		end_column:   end_column
	}
}

pub enum Severity {
	error
	warning
	hint
}

pub struct Diagnostic {
pub:
	span     Span
	severity Severity
	message  string
}

pub fn error_at(line int, column int, message string) Diagnostic {
	return Diagnostic{
		span:     point_span(line, column)
		severity: .error
		message:  message
	}
}

pub fn warning_at(line int, column int, message string) Diagnostic {
	return Diagnostic{
		span:     point_span(line, column)
		severity: .warning
		message:  message
	}
}

pub fn has_errors(diagnostics []Diagnostic) bool {
	for d in diagnostics {
		if d.severity == .error {
			return true
		}
	}
	return false
}

pub fn count_errors(diagnostics []Diagnostic) int {
	mut count := 0
	for d in diagnostics {
		if d.severity == .error {
			count++
		}
	}
	return count
}

pub fn count_warnings(diagnostics []Diagnostic) int {
	mut count := 0
	for d in diagnostics {
		if d.severity == .warning {
			count++
		}
	}
	return count
}
