// Fibonacci sequence - compute the nth Fibonacci number

fn fib(n Int) Int {
	match n {
		0 -> 0,
		1 -> 1,
		else -> fib(n - 1) + fib(n - 2),
	}
}

struct FibResult {
	n Int
	fib Int
}

// Get some fibonacci numbers
[FibResult{
	n: 0,
	fib: fib(0),
}, FibResult{
	n: 5,
	fib: fib(5),
}, FibResult{
	n: 10,
	fib: fib(10),
}, FibResult{
	n: 15,
	fib: fib(15),
}]
