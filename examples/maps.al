// Maps: an immutable, persistent key/value store (a hash array mapped trie).
// `set`/`delete` return a *new* map and leave the original untouched, so old
// versions stay valid and unchanged subtrees are shared between them.

import al/map
import al/array
import al/option
import al/string

// Count how many times each word appears by threading an immutable map through
// a fold: each step looks up the running tally and `set`s the incremented count
// — no mutation, just a new map per word.
fn word_counts(words Array(String)) Map(String, Int) {
	array.fold(words, map.new(), fn(counts, word) {
		current = option.unwrap(map.get(counts, word), 0)
		map.set(counts, word, current + 1)
	})
}

paragraph = 'the quick brown fox the lazy dog the dog naps the fox runs'
counts = word_counts(string.split(paragraph, ' '))

// Look up individual counts. `get` returns an Option; `unwrap` supplies a
// default for absent keys.
println('the: ${option.unwrap(map.get(counts, 'the'), 0)}')
println('dog: ${option.unwrap(map.get(counts, 'dog'), 0)}')
println('fox: ${option.unwrap(map.get(counts, 'fox'), 0)}')
println('cat: ${option.unwrap(map.get(counts, 'cat'), 0)}')

// `size` is the number of distinct keys; folding the values totals them all.
println('distinct words: ${map.size(counts)}')
println('total words: ${map.fold(counts, 0, fn(sum, _word, n) sum + n)}')
println('has "lazy": ${map.has(counts, 'lazy')}')

// Persistence: `delete` yields a new map and the original is left intact.
fewer = map.delete(counts, 'the')
println('deleted "the" -> fewer has it: ${map.has(fewer, 'the')}')
println('deleted "the" -> counts still has it: ${map.has(counts, 'the')}')

// `filter` keeps only the entries that satisfy a predicate.
repeats = map.filter(counts, fn(_word, n) n > 1)
println('words seen more than once: ${map.size(repeats)}')

// `merge` overlays one map on another; on a shared key, the second map wins.
defaults = map.from_list([('the', 0), ('newword', 7)])
merged = map.merge(defaults, counts)
println('merged "the": ${option.unwrap(map.get(merged, 'the'), 0)}')
println('merged "newword": ${option.unwrap(map.get(merged, 'newword'), 0)}')
