// FizzBuzz for 1 through 20: multiples of 3 print Fizz, multiples of 5 print
// Buzz, multiples of both print FizzBuzz, everything else prints itself.

fn fizzbuzz(n Int) String {
	match (n % 3, n % 5) {
		(0, 0) -> 'FizzBuzz'
		(0, _) -> 'Fizz'
		(_, 0) -> 'Buzz'
		else -> '${n}'
	}
}

fn run(n Int, last Int) Nil {
	if n > last {
		Nil
	} else {
		println(fizzbuzz(n))
		run(n + 1, last)
	}
}

run(1, 20)
