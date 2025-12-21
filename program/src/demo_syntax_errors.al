// Demo: Error recovery across different parsing contexts
// Run with: al build program/src/demo_syntax_errors.al

// 1. Top-level error - recovers at next identifier
/* error: Unexpected '/' */
Unexpected
/* error: Unexpected '/' */
y = 10

// 2. Block error - recovers at next statement
fn block_errors() {
	/* error: Unexpected '*' */
	Unexpected
	/* error: Unexpected '/' */
	c = 3
}

// 3. Array error - recovers at comma or ]
arr = [1, /* error: Unexpected '/' */, 3]

// 4. Match error - recovers at next arm or }
fn match_errors(n) {
	match n {
		1 -> /* error: Unexpected '/' */,
		2 -> 'two',
	}
}

// Final valid code to show recovery worked
final = 42
