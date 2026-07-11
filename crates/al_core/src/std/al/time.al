/**
 * Monotonic time.
 *
 * monotonic() returns an Instant representing "now" on a clock that only ever
 * moves forward — it is unaffected by wall-clock adjustments (NTP steps, leap
 * seconds, the user changing the system time) — so it is the right primitive
 * for measuring durations, computing deadlines, and timing out I/O.
 *
 * Instant is opaque: it can only be obtained from monotonic() and only moved
 * with add_ms / compared with since_ms, so a wall-clock ms count or a bare
 * duration cannot be passed where an absolute monotonic deadline is expected.
 */

pub opaque type Instant {
	ms Int
}

@vm(time__monotonic)
fn monotonic_ms() Int

pub fn monotonic() Instant {
	Instant(monotonic_ms())
}

// The instant `ms` milliseconds after `t`. Use this to compute a deadline.
pub fn add_ms(t Instant, ms Int) Instant {
	match t {
		Instant(n) -> Instant(n + ms)
	}
}

// Milliseconds elapsed from `earlier` to `later`; negative if `later` is
// actually before `earlier`.
pub fn since_ms(later Instant, earlier Instant) Int {
	match (later, earlier) {
		(Instant(a), Instant(b)) -> a - b
	}
}

// Unwrap to raw monotonic ms for the @vm boundary. Prefer add_ms / since_ms.
pub fn to_deadline_ms(t Instant) Int {
	match t {
		Instant(n) -> n
	}
}
