// Tail-call optimization: the recursive call below is in tail position, so each
// level reuses the current stack frame and the reported depth never grows.

import al/internal

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
