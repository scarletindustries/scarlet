enum Result {
	Ok(String)
	Err(String)
}

a Result = Ok('hello')
b Result = Ok('world')
c Result = Ok('hello')

println('a = Ok("hello")')
println('b = Ok("world")')
println('c = Ok("hello")')
println('')

println('a == b (should be false):')
println(a == b)

println('')
println('a == c (should be true):')
println(a == c)

println('')
println('Payload comparison works correctly!')
