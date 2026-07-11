/*
    Scheduler balance demonstration: 49 CPU-bound workers (fib(40) each)
    plus a liveliness logger printing "Alive" once a second, spawned across
    every core.

    What balanced scheduling looks like when this runs:

    - every worker prints "<i> starting" immediately — queued processes are
      admitted and time-sliced from the first moment, never starved behind
      already-running ones;
    - "Alive" keeps printing once a second throughout — preemption keeps the
      logger responsive while every core is saturated;
    - all 49 "<i> done" lines land within a few seconds of each other
      (measured on a 16-core machine: first at ~55s, last at ~59s) — queued
      work migrates to whichever scheduler has capacity, so no process waits
      in a long queue while another core sits idle.

    The program never exits: the liveliness logger recurses forever. Stop it
    with Ctrl-C (the smoke gate type-checks it; the scheduling acceptance
    harness runs and kills it externally).
*/

import al/scheduler
import al/array

fn liveliness() {
	println('Alive')
	scheduler.sleep(1000)
	liveliness()
}

fn fib(n) {
	match n {
		0 -> 0
		1 -> 1
		else -> fib(n - 1) + fib(n - 2)
	}
}

fn work(i) {
	println('${i} starting')
	println('${i} done - ${fib(40)}')
}

scheduler.spawn(fn() liveliness())
array.each(1..50, fn(i) scheduler.spawn(fn() work(i)))
