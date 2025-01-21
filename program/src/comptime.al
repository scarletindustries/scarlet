// al has a feature called comptime.
// it allows you to write code that is evaluated at compile time.
// it is useful for things like type checking, optimizations, and code generation.

// comptime is a keyword that you can use to mark a block of code that should be evaluated at compile time.

fn ct_hello_world() {
    comptime age := 10 * 10
    println(age)
}

// It also works with function arguments

fn ct_hello_world_with_arg(comptime arg int) {
    comptime age := arg * 10
    println(age)
}

// We've marked this `10` as comptime, so it will be evaluated at compile time.
ct_hello_world_with_arg(comptime 10)

// Here's an example of calculating fibonacci at compile time.
fn fibonacci(n int) int {
    if n <= 1 {
        return n
    }

    return fibonacci(n - 1) + fibonacci(n - 2)
}

comptime fib := fibonacci(10) // this will be evaluated at compile time
println(fib)
