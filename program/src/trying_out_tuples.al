(a, b) = (10, 'hello')
println('a=${a} b=${b}')

(a, b) = ('not', 'hello')
println('a=${a} b=${b}')

fn example(a) {
	(a)
}

println({
	a = example(42)
	a
})

println({
	a = example('a string')
	a
})
