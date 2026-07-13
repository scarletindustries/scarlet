// Text. A String is immutable UTF-8; al/string works in Unicode scalars, and
// al/binary is the escape hatch down to the bytes underneath.

import al/array
import al/binary
import al/string

// Interpolation splices any expression into a literal, and it nests.
name = 'world'
greeting = 'hello, ${name}'
println(greeting)
println('${'nested: ${greeting}!'}')

// Concatenation is `+`. Strings are values, so `greeting` is untouched.
println(greeting + ' (still: ${greeting})')

// The escapes are \n \t \r \0 \" \' \\ and \$ — that last one is how you print
// a literal dollar sign next to a brace.
println("tab:\there\nquote:'q'\tprice:\t\$5")

// split on a single character, on a longer separator, and on nothing at all.
// An empty separator splits into scalars, which is how you walk characters.
println('${string.split('a,b,c', ',')}')
println('${string.split('one -> two -> three', ' -> ')}')
println('${string.split('héllo', '')}')

// Consecutive separators produce empty pieces (they are not skipped), and a
// separator that never appears gives back the whole string as one piece.
println('${string.split('a,,b', ',')}')
println('${string.split('a,,b', ';')}')

// length counts scalars, not bytes. 'héllo' is 5 scalars but 6 bytes, because
// é needs two. If you are indexing into storage, ask al/binary, not al/string.
text = 'héllo'
scalars = string.length(text)
bytes = binary.byte_size(binary.from_string(text))
println('${text}: ${scalars} scalars, ${bytes} bytes')

println('contains  ${string.contains(text, 'llo')} ${string.contains(text, 'z')}')
println('trim      [${string.trim('  padded \t\n')}]')

// inspect renders any value the way println would, but as a String you can
// keep working with — handy inside a message.
println('inspect   ${string.inspect([1, 2, 3])} ${string.inspect(Some('x'))}')

// A word tally, written with the pieces above: split, trim, drop the blanks.
line = '  the quick   brown  fox  '
words = array.filter(string.split(string.trim(line), ' '), fn(w) string.length(w) > 0)
longest = array.fold(
	words,
	'',
	fn(best, w) if string.length(w) > string.length(best) { w } else { best },
)
println('words     ${words}')
println('longest   ${longest}')

// There is no `join` in the stdlib, because it is a fold: seed the
// accumulator with the first piece, then glue the separator onto each of the
// rest. Splitting the result on the same separator gives the pieces back.
fn join(parts Array(String), sep String) String {
	match parts {
		[] -> ''
		[h, ..t] -> array.fold(t, h, fn(acc, p) acc + sep + p)
	}
}

joined = join(words, ', ')
println('join      ${joined}')
println('resplit   ${string.split(joined, ', ') == words}')

// Byte-level work: al/binary indexes the raw bytes. 'A' is 65, 'L' is 76.
raw = <<'AL'>>
all_upper = array.all(0..binary.byte_size(raw), fn(i) binary.byte_at(raw, i) < 91)
println('bytes     ${binary.byte_at(raw, 0)} ${binary.byte_at(raw, 1)}')
println('upper?    ${all_upper}')
