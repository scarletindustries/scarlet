// Map-heavy workload. The rest of the bench suite touches no maps at all, so
// nothing else in it would notice a regression in the HAMT builtins.

import al/map

fn fill(m Map(Int, Int), i Int) Map(Int, Int) {
	if i == 0 {
		m
	} else {
		fill(map.set(m, i, i * 2), i - 1)
	}
}

fn sum_lookups(m Map(Int, Int), i Int, acc Int) Int {
	if i == 0 {
		acc
	} else {
		next = match map.get(m, i) {
			Some(v) -> acc + v
			None -> acc
		}
		sum_lookups(m, i - 1, next)
	}
}

fn churn(m Map(Int, Int), i Int) Int {
	if i == 0 {
		map.size(m)
	} else {
		churn(map.delete(map.set(m, i, i), i - 1), i - 1)
	}
}

// Sized so the run is ~1s: below that the 10ms timer granularity dominates
// and the benchmark cannot resolve the regressions it exists to catch.
fn repeat_lookups(m Map(Int, Int), rounds Int, acc Int) Int {
	if rounds == 0 {
		acc
	} else {
		repeat_lookups(m, rounds - 1, acc + sum_lookups(m, 50000, 0))
	}
}

m = fill(map.new(), 50000)
println(repeat_lookups(m, 100, 0))
println(churn(m, 2000000))
