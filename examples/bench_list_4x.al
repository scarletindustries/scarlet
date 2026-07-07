// Timing bench, size 4x. Triple: bench_list_{1x,2x,4x}.al — identical
// bodies, sizes N / 2N / 4N. The doubling ratio is scale-invariant and
// intrinsic to the algorithm's complexity class: O(n²) => ~4x per
// doubling, O(n)/O(n log n) => ~2x. Both ratios ≈2 across the curve is
// the airtight proof the list path is no longer quadratic.
//
// Body is strictly tail-recursive / fold-based (fold, length, contains,
// reverse, index loop) so it survives huge N without a stack overflow —
// map/filter (non-tail) stay covered by bench_list.al + the golden.
// Exercises BOTH the consumer destructure path (Drop) and the producer
// prepend path (reverse = fold + `[x, ..acc]`).

import al/array

fn idx(arr Array(Int), i Int, acc Int) Int {
	if i < 0 {
		acc
	} else {
		idx(arr, i - 1, acc + { arr[i] or 0 })
	}
}

n = 2000000
xs = [..{ 0..n }, ..[]]

a = array.fold(xs, 0, fn(p, q) p + q)
b = array.length(xs)
c = array.contains(xs, -1)
r = array.reverse(xs)
d = array.length(r)
e = idx(xs, n - 1, 0)

println(a + b + d + e + { if c { 1 } else { 0 } })
