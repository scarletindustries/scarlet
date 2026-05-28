// List-path correctness + shape oracle for the sequence representation.
// Reifies a real Array (NOT a lazy Range) via the double-spread idiom
// `[..{0..n}, ..[]]`, then exercises every al/list consumer and producer
// plus a random-access micro-loop. Prints only reduced scalars so the
// golden is tiny and stable across any semantics-preserving rep change.

import al/list

fn idx_sum(a Array(Int), i Int, acc Int) Int {
	if i < 0 {
		acc
	} else {
		idx_sum(a, i - 1, acc + {a[i] or 0})
	}
}

n = 60
xs = [..{0..n}, ..[]]

// consumers (recurse via [h, ..t])
total = list.fold(xs, 0, fn(a, b) a + b)
len = list.length(xs)
has_30 = list.contains(xs, 30)

// producers (rebuild via [h, ..rest])
doubled = list.map(xs, fn(x) x * 2)
small = list.filter(xs, fn(x) x < 30)
rev = list.reverse(xs)

doubled_total = list.fold(doubled, 0, fn(a, b) a + b)
small_len = list.length(small)
rev_head = rev[0] or -1

// random-access guard
idx_total = idx_sum(xs, len - 1, 0)

println(total)
println(len)
println(has_30)
println(doubled_total)
println(small_len)
println(rev_head)
println(idx_total)
