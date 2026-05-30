fn fib(n) {
	if n < 2 {
		n
	} else {
		fib(n - 1) + fib(n - 2)
	}
}

fn count(n, acc) {
	if n == 0 {
		acc
	} else {
		count(n - 1, acc + 1)
	}
}

println(fib(26))
println(count(200000, 0))
