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

// Spawn `f` pinned to the current core. The child runs on the core that
// spawned it — and any socket it captured stays there too, so no file
// descriptor moves between cores. This is what keeps a connection on the core
// that accepted it; for general work prefer `spawn`, which load-balances.
@vm(scheduler__spawn_local)
pub fn spawn_local(f fn() a) Nil

// Spawn one copy of `f` on every core. Each copy runs independently with no
// shared state. Used to fan an accept loop out across all cores, so each core
// accepts and serves connections from its own kernel queue.
@vm(scheduler__spawn_on_each)
pub fn spawn_on_each(f fn() a) Nil

@vm(scheduler__sleep)
pub fn sleep(ms Int) Nil
