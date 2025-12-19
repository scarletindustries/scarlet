// Run this with --expose-debug-builtins flag to enable __stack_depth__ builtin

slightly_deeper = fn(n) {
    println('slightly_deeper: ${n} is ${__stack_depth__()} frames deep')
}

countdown = fn(n) {
    println(n)
    println('countdown: ${n} is ${__stack_depth__()} frames deep')

    if n > 0 {
        countdown(n - 1)
    }
}

countdown(5)