// Tail recursion. A call in tail position — the last thing a function does
// before handing its answer back — reuses the current stack frame instead of
// pushing a new one. So a loop written as recursion runs in constant stack
// space, and `internal.stack_depth` is the proof: it stays flat no matter how
// deep the recursion goes.

import al/internal

// The accumulator is what makes the call a tail call. Without it the `+` would
// have to happen after the recursive call returned, and every frame would have
// to stay alive waiting to do its addition.
fn sum_to(n Int, acc Int) Int {
	if n == 0 {
		acc
	} else {
		sum_to(n - 1, acc + n)
	}
}

// Recurse `hops` times and report the live frame count at the bottom.
fn depth_after(hops Int) Int {
	if hops == 0 {
		internal.stack_depth()
	} else {
		depth_after(hops - 1)
	}
}

// Mutual recursion is optimised the same way: is_even's call to is_odd is in
// tail position, and so is is_odd's call back, so the two swap places in one
// frame rather than stacking a million of them.
fn is_even(n Int) Bool {
	if n == 0 {
		True
	} else {
		is_odd(n - 1)
	}
}

fn is_odd(n Int) Bool {
	if n == 0 {
		False
	} else {
		is_even(n - 1)
	}
}

println(sum_to(1000000, 0))

// Two frames: the top-level one, plus the single live frame of depth_after.
println('depth after 1 hop: ${depth_after(1)}')
println('depth after 1000000 hops: ${depth_after(1000000)}')

println('1000001 is even: ${is_even(1000001)}')
