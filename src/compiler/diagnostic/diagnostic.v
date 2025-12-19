module diagnostic

// Span represents a range in source code (LSP-compatible)
pub struct Span {
pub:
	start_line   int
	start_column int
	end_line     int
	end_column   int
}

// Create a span from a single point (for single-character errors)
pub fn point_span(line int, column int) Span {
	return Span{
		start_line:   line
		start_column: column
		end_line:     line
		end_column:   column + 1
	}
}

// Severity levels for diagnostics
pub enum Severity {
	error
	warning
	hint
}

// A single diagnostic (error/warning/hint)
pub struct Diagnostic {
pub:
	span     Span
	severity Severity
	message  string
}

// Create an error diagnostic at a specific location
pub fn error_at(line int, column int, message string) Diagnostic {
	return Diagnostic{
		span:     point_span(line, column)
		severity: .error
		message:  message
	}
}

// Create a warning diagnostic at a specific location
pub fn warning_at(line int, column int, message string) Diagnostic {
	return Diagnostic{
		span:     point_span(line, column)
		severity: .warning
		message:  message
	}
}

// Check if a list of diagnostics contains any errors
pub fn has_errors(diagnostics []Diagnostic) bool {
	for d in diagnostics {
		if d.severity == .error {
			return true
		}
	}
	return false
}

// Count errors in a list of diagnostics
pub fn count_errors(diagnostics []Diagnostic) int {
	mut count := 0
	for d in diagnostics {
		if d.severity == .error {
			count++
		}
	}
	return count
}

// Count warnings in a list of diagnostics
pub fn count_warnings(diagnostics []Diagnostic) int {
	mut count := 0
	for d in diagnostics {
		if d.severity == .warning {
			count++
		}
	}
	return count
}
