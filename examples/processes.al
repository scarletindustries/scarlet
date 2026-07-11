// Concurrency with lightweight processes from al/scheduler.

import al/scheduler

// spawn(f) starts a lightweight process, not an OS thread. Processes are
// cheap (a few hundred bytes), preemptively scheduled, and the runtime spreads
// them across every CPU core. Each process sees the program's top-level
// bindings; values passed between processes behave as copies (they are
// immutable, so nothing is ever shared mutably). The program waits for every
// process to finish before exiting.

const greeting = 'hello from'

fn worker(id Int, delay Int) Nil {
	scheduler.sleep(delay)
	println('${greeting} worker ${id}')
}

println('main: spawning workers')

scheduler.spawn(fn() worker(1, 40))
scheduler.spawn(fn() worker(2, 80))
scheduler.spawn(fn() worker(3, 120))
scheduler.spawn(fn() worker(4, 160))

println('main: done spawning, waiting at exit')
