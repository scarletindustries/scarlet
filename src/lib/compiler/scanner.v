module compiler

pub struct Scanner {
	input string
mut:
	state ScannerState = ScannerState{}
}

pub struct ScannerState {
mut:
	pos int // The current position in the input (points to current char)
	kind Kind
	literal string
}

pub fn new_scanner() &Scanner {
	return &Scanner{
		input: ''
		pos: 0
	}
}

fn (mut s &Scanner) scan() {
	
}

fn (mut s &Scanner) read_char() byte {
	mut ch := s.input[s.pos]
	s.pos++

	return ch
}
