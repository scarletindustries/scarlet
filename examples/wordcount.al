// Counts word frequencies in a paragraph. AL has no map type, so the tallies
// live in an assoc list: an Array((String, Int)) updated by recursion.

import al/list
import al/string

// Add one occurrence of a word. New words go on the end, so the list stays
// in first-appearance order.
fn bump(counts Array((String, Int)), word String) Array((String, Int)) {
	match counts {
		[] -> [(word, 1)]
		[(w, n), ..rest] -> {
			if w == word {
				[(w, n + 1), ..rest]
			} else {
				[(w, n), ..bump(rest, word)]
			}
		}
	}
}

fn count_words(words Array(String)) Array((String, Int)) {
	list.fold(words, [], bump)
}

// Print one 'word: count' line per entry.
fn print_counts(counts Array((String, Int))) Nil {
	match counts {
		[] -> Nil
		[(word, n), ..rest] -> {
			println('${word}: ${n}')
			print_counts(rest)
		}
	}
}

// The entry with the highest count, picked by a fold over the assoc list.
fn most_frequent(counts Array((String, Int))) (String, Int) {
	list.fold(counts, ('', 0), fn(best, entry) if entry.1 > best.1 { entry } else { best })
}

paragraph = 'the quick brown fox jumps over the lazy dog while the dog naps in the sun'

counts = count_words(string.split(paragraph, ' '))
print_counts(counts)

(word, n) = most_frequent(counts)
println('most frequent: ${word} (${n} times)')
