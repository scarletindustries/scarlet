module token

pub enum TriviaKind {
	whitespace     // spaces, tabs
	newline        // \n
	line_comment   // // comment
	block_comment  // /* */ comment
	doc_comment    // /** */ comment
}

pub struct Trivia {
pub:
	kind TriviaKind
	text string
}

pub fn (t Trivia) str() string {
	return t.text
}
