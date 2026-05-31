// Parser error recovery: this file does not compile, on purpose.
// Run `al check examples/demo_syntax_errors.al` to see every error reported
// in one pass, with recovery at statement, array, and match-arm boundaries.

// 1. Top-level error - recovers at next identifier
x = )
y = 10

// 2. Block error - recovers at next statement
fn block_errors() {
    a = 1
    b = ]
    c = 3
}

// 3. Array error - recovers at comma or ]
arr = [1, ), 3]

// 4. Match error - recovers at next arm or }
fn match_errors(n) {
    match n {
        1 -> ),
        2 -> 'two'
    }
}

// Final valid code to show recovery worked
final = 42
