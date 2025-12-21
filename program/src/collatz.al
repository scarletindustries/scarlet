// Collatz Conjecture (3n+1 problem)
// Count steps to reach 1

fn collatz_step(n Int) Int {
	match n % 2 {
		0 -> n / 2,
		else -> 3 * n + 1,
	}
}

fn count_steps(n Int, steps Int) Int {
	match n {
		1 -> steps,
		else -> count_steps(collatz_step(n), steps + 1),
	}
}

fn collatz_length(n Int) Int {
	count_steps(n, 0)
}

struct CollatzResult {
	start Int,
	steps Int,
}

// Famous starting values
[CollatzResult{
	start: 1,
	steps: collatz_length(1),
}, CollatzResult{
	start: 7,
	steps: collatz_length(7),
}, CollatzResult{
	start: 27,
	steps: collatz_length(27),
}, CollatzResult{
	start: 97,
	steps: collatz_length(97),
}]
