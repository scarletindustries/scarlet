// Arrays and maps. Both are immutable: every operation returns a new value and
// leaves the old one alone, so a collection can be shared without copying.

import al/array
import al/map
import al/option

// A range `a..b` is half-open — it holds a, a+1, ... b-1 — and it *is* an
// Array, so it feeds straight into the combinators. A backwards range is empty.
println('1..6      ${1..6}')
println('6..1      ${6..1}')

xs = [3, 1, 4, 1, 5, 9, 2, 6]

// The core combinator suite. Each takes a lambda; a lambda has no return-type
// slot, so it is `fn(x) body` or `fn(x Int) { body }`, never `fn(x Int) Int`.
println('map       ${array.map(xs, fn(x) x * x)}')
println('filter    ${array.filter(xs, fn(x) x % 2 == 0)}')
println('fold      ${array.fold(xs, 0, fn(acc, x) acc + x)}')
println('reverse   ${array.reverse(xs)}')
println('length    ${array.length(xs)}')
println('contains  ${array.contains(xs, 9)} ${array.contains(xs, 7)}')
println('any/all   ${array.any(xs, fn(x) x > 8)} ${array.all(xs, fn(x) x > 0)}')
println('concat    ${array.concat([1, 2], [3, 4])}')

// `each` is the one that exists for its effect: it returns Nil, not an array.
array.each([1, 2, 3], fn(n) println('each      ${n}'))

// Indexing is total: it hands back an Option, never a crash. `or` supplies the
// value for the missing case.
println('xs[2]     ${xs[2] or 0}')
println('xs[99]    ${xs[99] or 0}')

// `find` also returns an Option, so it composes with al/option.
println('find      ${option.unwrap(array.find(xs, fn(x) x > 4), 0)}')
println('find none ${option.is_none(array.find(xs, fn(x) x > 100))}')

// `..a` spreads an array into a literal — the cheap way to prepend, append, or
// splice without a builder.
head = [0]
tail = [7, 8]
println('spread    ${[..head, 42, ..tail]}')

// What fold desugars to: an array is a head and a tail, and a pattern can say
// so. The three arms are exhaustive — the checker rejects a missing `[_]`.
fn sum(ns Array(Int)) Int {
	match ns {
		[] -> 0
		[only] -> only
		[h, ..t] -> h + sum(t)
	}
}

println('sum       ${sum(xs)} == ${array.fold(xs, 0, fn(a, b) a + b)}')
println('')

// A Map is a persistent hash map: `set` and `delete` return a new map and the
// original keeps its old contents.
words = ['the', 'cat', 'sat', 'on', 'the', 'mat', 'the', 'cat']

counts = array.fold(words, map.new(), fn(m, w) map.set(m, w, { map.get(m, w) or 0 } + 1))

// Iteration order over a hash map is unspecified, so nothing below prints
// `keys`, `values`, or `to_list` directly — output is driven from a canonical
// key list, or from order-independent aggregates like `size` and `fold`.
vocab = ['cat', 'mat', 'on', 'sat', 'the']

array.each(vocab, fn(w) println('${w} -> ${map.get(counts, w) or 0}'))

println('size      ${map.size(counts)}')
println('total     ${map.fold(counts, 0, fn(acc, _, n) acc + n)}')
println('has       ${map.has(counts, 'the')} ${map.has(counts, 'dog')}')
println('keys      ${array.length(map.keys(counts))} distinct')
println('values    ${array.fold(map.values(counts), 0, fn(a, b) a + b)} words')
println('to_list   ${array.length(map.to_list(counts))} pairs')

// delete is persistent: `counts` still has 'the' afterwards.
fewer = map.delete(counts, 'the')
println('delete    ${map.has(fewer, 'the')}, original ${map.has(counts, 'the')}')

// filter takes the key *and* the value.
repeats = map.filter(counts, fn(_, n) n > 1)
println('filter    ${array.length(map.keys(repeats))} words seen twice or more')

// merge prefers the right-hand map on a clash — the usual "defaults, then
// overrides" shape.
defaults = map.from_list([('mode', 'fast'), ('retries', '3')])
overrides = map.from_list([('retries', '5')])
settings = map.merge(defaults, overrides)
mode = map.get(settings, 'mode') or '?'
retries = map.get(settings, 'retries') or '?'
println('merge     mode=${mode} retries=${retries}')
