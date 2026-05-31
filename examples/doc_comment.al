// A tour of AL comment styles: doc comments on functions, constants, types and
// variants, next to plain block comments that are not documentation.

/** This is a doc comment */
fn documented() {
	42
}

println('documented() returns ${documented()}')

/* This is a regular block comment */
fn undocumented() {
	0
}

/** The mathematical constant PI */
const PI = 3.14159

/** The answer to life, the universe, and everything */
const ANSWER = 42

/** A random number */
_random = 50

/** A user in the system */
type User {
	name String
	age Int
}

user = User(name: 'Alice', age: 30)

println('${user.name} is ${user.age} years old')

/** Represents the status of an operation */
type Status {
	/** Operation completed successfully */
	Success
	/** Operation is still in progress */
	Pending
	/** Operation failed with an error */
	Failed
}

println('status: ${Success}')

/** This is a very long doc comment that spans a single line but contains a lot of text to test how the formatter handles long documentation strings that might need to be wrapped or preserved */
fn long_doc_single_line() {
	1
}

/**
 * This is a multi-line doc comment
 * with the typical asterisk-prefixed style
 * that you see in many programming languages.
 */
fn long_doc_multi_line() {
	2
}

/* This is a very long block comment that is NOT a doc comment - it's just a regular comment that happens to be quite lengthy and might cause formatting issues if not handled properly */
fn long_block_single_line() {
	3
}

/* This is a multi-line block comment also with the asterisk style but it's not a doc comment */
fn long_block_multi_line() {
	4
}

/* A block comment without asterisks just plain text on multiple lines to see how that gets formatted */
fn plain_block_multi_line() {
	5
}

/** A struct with long field comments */
type Config {
	host String
	port Int
	secure Bool
}

/** An enum with various comment styles on variants */
type LogLevel {
	/** Debug level - very verbose output for development and troubleshooting purposes only, should not be used in production */
	Debug
	/* Info level - general information about program execution (this is a block comment, not doc) */
	Info
	/** Warning level - something unexpected happened but the program can continue */
	Warning
	/** Error level - something went wrong and the operation failed */
	LogError
}
