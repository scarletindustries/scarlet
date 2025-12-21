const source = 'println(\'hello\')'

struct Token {
	type string,
	value string,
}

/* error: Expected field name, got 'fn' */

fn copy() AL_Array {
	copy = AL_Array{
		data: [].concat(this.data),
	}

	return
	copy
}
/* error: Unexpected '}' */

fn isLetter(c string) boolean {
	isLetterLower = c >= 'a' && c <= 'z'
	isLetterUpper = c >= 'A' && c <= 'Z'

	return
	isLetterLower || isLetterUpper
}

fn isDigit(c string) boolean {
	return(c >= '0') && c <= '9'
}

fn lex(input string) []Token {}

struct Node {
}

fn parse(tokens []Token) Node {}

fn generate(node Node) string {}
export fn compile(input string) string {
	tokens = lex(input)
	ast = parse(tokens)
	return
	generate(ast)
}

fn main() {
	result = compile(source)
}
