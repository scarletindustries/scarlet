// Testing parameter type inference

fn double(x) { x * 2 }
fn add(a, b) { a + b }
fn greet(name) { 'Hello, ' + name }
fn is_positive(n) { n > 0 }
fn identity(x) { x }

println(double(21))
println(add(10, 5))
println(greet('World'))
println(is_positive(42))
println(identity('polymorphic'))

countdown = fn(n) {
	if n > 0 {
		println(n)
		countdown(n - 1)
	}
}

countdown(3)

fn countdown2(n) {
	if n > 0 {
		println(n)
		countdown(n - 1)
	}
}
countdown2(3)

fn apply_twice(x, f) {
	f(f(x))
}
println(apply_twice(5, fn(n) { n + 1 }))
