import al/internal

_slightly_deeper = fn(n) println('slightly_deeper: ${n} is ${internal.stack_depth()} frames deep')

countdown = fn(n) {
	println(n)
	println('countdown: ${n} is ${internal.stack_depth()} frames deep')

	if n > 0 {
		countdown(n - 1)
	} else {
		Nil
	}
}

countdown(5)
