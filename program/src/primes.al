// Prime number checker

fn is_divisible(n Int, d Int) Bool {
	n % d == 0
}

fn check_divisors(n Int, d Int) Bool {
	match true {
		d * d > n -> true,
		is_divisible(n, d) -> false,
		else -> check_divisors(n, d + 2),
	}
}

fn is_prime(n Int) Bool {
	match true {
		n == 2 -> true,
		n < 2 | is_divisible(n, 2) -> false,
		else -> check_divisors(n, 3),
	}
}

struct PrimeCheck {
	n Int,
	is_prime Bool,
}

// Check some numbers
[PrimeCheck{
	n: 2,
	is_prime: is_prime(2),
}, PrimeCheck{
	n: 7,
	is_prime: is_prime(7),
}, PrimeCheck{
	n: 13,
	is_prime: is_prime(13),
}, PrimeCheck{
	n: 15,
	is_prime: is_prime(15),
}, PrimeCheck{
	n: 97,
	is_prime: is_prime(97),
}]
