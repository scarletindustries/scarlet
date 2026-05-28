type Outcome {
	Good(value String)
	Bad(value String)
}

a Outcome = Good('hello')
b Outcome = Good('world')
c Outcome = Good('hello')

println('a = Good("hello")')
println('b = Good("world")')
println('c = Good("hello")')
println('')

println('a == b (should be False):')
println(a == b)

println('')
println('a == c (should be True):')
println(a == c)

println('')
println('Payload comparison works correctly!')
