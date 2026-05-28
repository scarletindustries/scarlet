fn fib(x Int) String {
	println('fib ${x}')
	if x < 1 {
		'()'
	} else {
		'(${fib(x - 1)})'
	}
}

println(fib(2))
println(fib(30))
println(fib(400))
