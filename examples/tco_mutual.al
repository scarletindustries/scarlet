// Mutually-recursive even/odd: every call to the *other* function sits in tail
// position, so the cross-function tail-call reuses the current frame instead of
// pushing a new one. The stack stays a constant 2 frames deep (the top-level
// frame plus the one live ev/od frame) however deep the recursion goes. A
// million mutual hops complete without ever growing the stack.
import al/internal

fn ev(n Int) Bool {
	if n == 1000000 {
		println('ev   ${n}: ${internal.stack_depth()} frames')
	} else {
		Nil
	}
	if n == 500000 {
		println('ev   ${n}: ${internal.stack_depth()} frames')
	} else {
		Nil
	}
	if n == 0 {
		True
	} else {
		od(n - 1)
	}
}

fn od(n Int) Bool {
	if n == 999999 {
		println('od   ${n}: ${internal.stack_depth()} frames')
	} else {
		Nil
	}
	if n == 1 {
		println('od   ${n}: ${internal.stack_depth()} frames')
	} else {
		Nil
	}
	if n == 0 {
		False
	} else {
		ev(n - 1)
	}
}

println(ev(1000000))
