// Factorial - compute n!

fn factorial(n Int) Int {
	match n {
		0 -> 1,
		1 -> 1,
		else -> n * factorial(n - 1),
	}
}

// Calculate some factorials
struct FactResult {
	n Int
	factorial Int
}

[FactResult{
	n: 0,
	factorial: factorial(0),
}, FactResult{
	n: 5,
	factorial: factorial(5),
}, FactResult{
	n: 10,
	factorial: factorial(10),
}]
