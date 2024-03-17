module state

// default_column_n exists because we don't like
// magic numbers, and we reuse the value in multiple
// places
const default_column_n = 0

pub struct ScannerState {
mut:
	pos    int // current position in input (points to current char)
	column int = state.default_column_n
	line   int = 1
}

@[inline]
pub fn (mut s ScannerState) get_pos() int {
	return s.pos
}

@[inline]
pub fn (mut s ScannerState) set_pos(pos int) {
	s.pos = pos
}

@[inline]
pub fn (mut s ScannerState) set_line(line int) {
	s.line = line
}

@[inline]
pub fn (mut s ScannerState) set_column(column int) {
	s.column = column
}

@[inline]
pub fn (mut s ScannerState) incr_pos() {
	s.pos++
}

@[inline]
pub fn (s ScannerState) get_line() int {
	return s.line
}

@[inline]
pub fn (s ScannerState) get_column() int {
	return s.column
}

@[inline]
pub fn (mut s ScannerState) decr_pos() {
	s.pos--
}

@[inline]
pub fn (mut s ScannerState) decr_line() {
	s.line--
}

@[inline]
pub fn (mut s ScannerState) decr_column() {
	s.column--
}

pub fn (mut s ScannerState) incr_line() {
	s.line++
	s.column = state.default_column_n
}

pub fn (mut s ScannerState) incr_column() {
	s.column++
}
