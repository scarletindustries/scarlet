// split by comma
csv = 'apple,banana,cherry'
fruits = str_split(csv, ',')
println('Split by comma:')
println(fruits)

// split by space
sentence = 'hello world from al'
words = str_split(sentence, ' ')
println('Split by space:')
println(words)

// split with multi-char delimiter
data = 'one::two::three'
parts = str_split(data, '::')
println('Split by double colon:')
println(parts)

// split with no matches (returns single-element array)
no_match = 'no delimiters here'
single = str_split(no_match, ',')
println('No delimiter found:')
println(single)

// empty parts from consecutive delimiters
consecutive = 'a,,b,,c'
with_empties = str_split(consecutive, ',')
println('Consecutive delimiters:')
println(with_empties)

// access individual elements
println('First fruit: ${fruits[0]}')
println('Second word: ${words[1]}')
