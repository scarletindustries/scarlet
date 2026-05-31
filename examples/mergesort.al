// Merge sort: split the array in half, sort each half, merge the sorted halves.
// The merge walks both arrays at once by matching on a tuple of array patterns.

import al/list
import al/string

// First n elements and the rest, as a pair.
fn split_at(xs Array(t), n Int) (Array(t), Array(t)) {
	if n <= 0 {
		([], xs)
	} else {
		match xs {
			[] -> ([], [])
			[head, ..tail] -> {
				(front, back) = split_at(tail, n - 1)
				([head, ..front], back)
			}
		}
	}
}

// Merge two sorted arrays into one sorted array. Matching on the pair keeps
// both heads visible in the same pattern, so the comparison reads directly.
fn merge(left Array(Int), right Array(Int)) Array(Int) {
	match (left, right) {
		([], ys) -> ys
		(xs, []) -> xs
		([x, ..xs], [y, ..ys]) -> {
			if x <= y {
				[x, ..merge(xs, right)]
			} else {
				[y, ..merge(left, ys)]
			}
		}
	}
}

fn sort(xs Array(Int)) Array(Int) {
	n = list.length(xs)
	if n < 2 {
		xs
	} else {
		(front, back) = split_at(xs, n / 2)
		merge(sort(front), sort(back))
	}
}

values = [38, 27, 43, 3, 9, 82, 10]
println('before: ${string.inspect(values)}')
println('after:  ${string.inspect(sort(values))}')

// Duplicates land next to each other and sorted input passes through unchanged.
println('dupes:  ${string.inspect(sort([5, 1, 4, 4, 2, 8, 5, 7, 1, 3]))}')
println('sorted: ${string.inspect(sort([1, 2, 3, 4]))}')
