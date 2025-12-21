fn fizzbuzz(n Int) String {
	match true {
		n % 15 == 0 -> 'FizzBuzz',
		n % 3 == 0 -> 'Fizz',
		n % 5 == 0 -> 'Buzz',
		else -> '${n}',
	}
}

// Show FizzBuzz for 1-20
[fizzbuzz(1), fizzbuzz(2), fizzbuzz(3), fizzbuzz(4), fizzbuzz(5), fizzbuzz(6), fizzbuzz(7), fizzbuzz(8), fizzbuzz(9), fizzbuzz(10), fizzbuzz(11), fizzbuzz(12), fizzbuzz(13), fizzbuzz(14), fizzbuzz(15), fizzbuzz(16), fizzbuzz(17), fizzbuzz(18), fizzbuzz(19), fizzbuzz(20)]
