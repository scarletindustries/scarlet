// Splitting and inspecting strings with al/string.

import al/string

// split by comma
csv = 'apple,banana,cherry'
fruits = string.split(csv, ',')
println('Split by comma:')
println(fruits)

// split by space
sentence = 'hello world from al'
words = string.split(sentence, ' ')
println('Split by space:')
println(words)

// split with multi-char delimiter
data = 'one::two::three'
parts = string.split(data, '::')
println('Split by double colon:')
println(parts)

// split with no matches (returns single-element array)
no_match = 'no delimiters here'
single = string.split(no_match, ',')
println('No delimiter found:')
println(single)

// empty parts from consecutive delimiters
consecutive = 'a,,b,,c'
with_empties = string.split(consecutive, ',')
println('Consecutive delimiters:')
println(with_empties)

// access individual elements
first_fruit = fruits[0] or '?'
second_word = words[1] or '?'
println('First fruit: ${first_fruit}')
println('Second word: ${second_word}')

// other ops
println('length: ${string.length(csv)}')
println('contains banana: ${string.contains(csv, 'banana')}')
println('trimmed: [${string.trim('  hi  ')}]')
