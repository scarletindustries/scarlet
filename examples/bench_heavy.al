fn fib(n Int) Int {
	if n < 2 {
		n
	} else {
		fib(n - 1) + fib(n - 2)
	}
}

fn count(n Int, acc Int) Int {
	if n == 0 {
		acc
	} else {
		count(n - 1, acc + 1)
	}
}

println(fib(33))
println(count(10000000, 0))
