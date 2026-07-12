/**
 * Immutable arrays.
 *
 * Every operation returns a new array; the input is never modified. The
 * combinators here (`map`, `filter`, `fold`, ...) are the ordinary
 * structural-recursion suite over `Array(a)`.
 */

// A new array of `f` applied to each element, in order.
pub fn map(xs Array(a), f fn(a) b) Array(b) {
	fold(xs, [], fn(acc, x) [..acc, f(x)])
}

// The elements for which `p` holds, in their original order.
pub fn filter(xs Array(a), p fn(a) Bool) Array(a) {
	fold(xs, [], fn(acc, x) if p(x) {
		[..acc, x]
	} else {
		acc
	})
}

// Left fold: `f` combines the accumulator with each element from first to
// last, starting from `init`.
pub fn fold(xs Array(a), init b, f fn(b, a) b) b {
	match xs {
		[] -> init
		[h, ..t] -> fold(t, f(init, h), f)
	}
}

// Runs f on every element for its side effect; if you want a value out, use map or fold.
pub fn each(xs Array(a), f fn(a) Nil) Nil {
	match xs {
		[] -> Nil
		[h, ..t] -> {
			f(h)
			each(t, f)
		}
	}
}

// The elements in reverse order.
pub fn reverse(xs Array(a)) Array(a) {
	fold(xs, [], fn(acc, x) [x, ..acc])
}

// The number of elements. O(1).
@vm(array__length)
pub fn length(xs Array(a)) Int

// Whether any element equals `target`, by structural equality.
pub fn contains(xs Array(a), target a) Bool {
	match xs {
		[] -> False
		[h, ..t] -> if h == target {
			True
		} else {
			contains(t, target)
		}
	}
}

// The first element for which `p` holds, or `None`.
pub fn find(xs Array(a), p fn(a) Bool) Option(a) {
	match xs {
		[] -> None
		[h, ..t] -> if p(h) {
			Some(h)
		} else {
			find(t, p)
		}
	}
}

// Whether `p` holds for at least one element. False on the empty array.
pub fn any(xs Array(a), p fn(a) Bool) Bool {
	match xs {
		[] -> False
		[h, ..t] -> if p(h) {
			True
		} else {
			any(t, p)
		}
	}
}

// Whether `p` holds for every element. True on the empty array.
pub fn all(xs Array(a), p fn(a) Bool) Bool {
	match xs {
		[] -> True
		[h, ..t] -> if p(h) {
			all(t, p)
		} else {
			False
		}
	}
}

// Concatenate two arrays. Backed by the native concat op: structure is shared,
// not deep-copied.
pub fn concat(xs Array(a), ys Array(a)) Array(a) {
	[..xs, ..ys]
}
