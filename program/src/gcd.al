// Greatest Common Divisor using Euclidean algorithm

fn gcd(a Int, b Int) Int {
	match b {
		0 -> a,
		else -> gcd(b, a % b),
	}
}

// Least Common Multiple
fn lcm(a Int, b Int) Int {
	a * b / gcd(a, b)
}

// Test with some numbers
struct GcdResult {
	a Int,
	b Int,
	gcd Int,
	lcm Int,
}

fn compute(a Int, b Int) GcdResult {
	GcdResult{
		a: a,
		b: b,
		gcd: gcd(a, b),
		lcm: lcm(a, b),
	}
}

[compute(48, 18), compute(54, 24), compute(17, 13), compute(100, 35)]
