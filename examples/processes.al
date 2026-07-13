// Concurrency with al/scheduler: lightweight processes.
//
// A process is not an OS thread. It costs a few hundred bytes and about a
// microsecond to start, so a process per task — or per connection — is the
// cheap option, not the extravagant one. The runtime schedules them across
// every core, preemptively, on a reduction budget: a process that loops for
// ever cannot starve the others, and one that blocks on I/O parks alone.
// Processes share no mutable state; values are immutable, so anything that
// crosses between them behaves as a copy.
//
// The program exits once every process has finished. There is no join — nothing
// here waits — so these lines are printed by processes racing each other and
// their *order* is not fixed. That, and a fan-out whose width is the core count
// of whatever machine you are on, is why this example has no golden output.

import al/scheduler
import al/time

// A spawned process has nowhere to send a result, so the closure spawn takes
// returns Nil: a process that produces something must do something with it
// itself — print it, write it to a socket, hand it to a service.
fn worker(id Int, work_ms Int) Nil {
	// sleep parks only this process. The other three keep running, and the main
	// process below has already finished.
	scheduler.sleep(work_ms)
	println('worker ${id} finished after ${work_ms}ms')
}

// spawn lets the runtime place the process wherever it has room. Work that
// touches no core-local resource wants that load balancing, so this is the
// default choice. The four workers report in sleep order, not spawn order.
println('spawning four workers')

scheduler.spawn(fn() worker(1, 40))
scheduler.spawn(fn() worker(2, 10))
scheduler.spawn(fn() worker(3, 30))
scheduler.spawn(fn() worker(4, 20))

// time.monotonic is a clock that only moves forward, so a duration measured
// across a sleep survives a wall-clock adjustment mid-nap.
fn timed(label String) Nil {
	start = time.monotonic()
	scheduler.sleep(5)
	elapsed = time.since_ms(time.monotonic(), start)
	println('${label}: slept at least 5ms: ${elapsed >= 5}')
}

// spawn_local pins the child to the core that spawned it. That matters when the
// child captured something the runtime should not move between cores — a socket
// above all: al/net's accept loop hands each connection to a spawn_local'd
// process, so a connection is served on the core that accepted it and its file
// descriptor never migrates.
scheduler.spawn_local(fn() timed('pinned child'))

// spawn_per_core starts one independent copy of the closure on every core. It
// is how a server fans an accept loop out so each core drains the shared listen
// queue itself (net.serve is exactly this); here each copy just reports in.
scheduler.spawn_per_core(fn() println('acceptor reporting for duty'))

println('main process done — the program stays up until the others finish')
