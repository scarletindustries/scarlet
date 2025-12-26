// Power/exponentiation - simple recursive version

fn power(base Int, exp Int) Int {
	match exp {
		0 -> 1,
		else -> base * power(base, exp - 1),
	}
}

struct PowerResult {
	base Int
	exp Int
	result Int
}

// Calculate some powers
[PowerResult{
	base: 2,
	exp: 0,
	result: power(2, 0),
}, PowerResult{
	base: 2,
	exp: 10,
	result: power(2, 10),
}, PowerResult{
	base: 3,
	exp: 5,
	result: power(3, 5),
}, PowerResult{
	base: 5,
	exp: 3,
	result: power(5, 3),
}]
