// Primes up to 50 by trial division.
// Builds the number range recursively, filters it with is_prime, prints the result.

import al/array

fn is_divisible(n Int, d Int) Bool {
	n % d == 0
}

fn check_divisors(n Int, d Int) Bool {
	if d * d > n {
		True
	} else if is_divisible(n, d) {
		False
	} else {
		check_divisors(n, d + 2)
	}
}

fn is_prime(n Int) Bool {
	if n == 2 {
		True
	} else if n < 2 || is_divisible(n, 2) {
		False
	} else {
		check_divisors(n, 3)
	}
}

// Numbers from `from` to `to`, inclusive.
fn range(from Int, to Int) Array(Int) {
	if from > to {
		[]
	} else {
		[from, ..range(from + 1, to)]
	}
}

const limit = 50

primes = array.filter(range(2, limit), is_prime)

println('primes up to ${limit}:')
println(primes)
println('${array.length(primes)} primes found')
