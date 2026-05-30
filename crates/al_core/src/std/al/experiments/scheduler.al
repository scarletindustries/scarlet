// Experimental lightweight processes.
//
// spawn(f) starts a new process running f. Processes are cheap (a few hundred
// bytes, about a microsecond to start) — spawn freely, one per connection or
// task. The runtime schedules them across every CPU core, preemptively, with
// a reduction budget; blocking I/O parks only the calling process, never the
// whole program. Processes share no mutable state: values are immutable, and
// anything that crosses between processes behaves as a copy.
//
// The program exits when every process has finished.

@vm(scheduler__spawn)
pub fn spawn(f fn() a) Nil

@vm(scheduler__sleep)
pub fn sleep(ms Int) Nil
