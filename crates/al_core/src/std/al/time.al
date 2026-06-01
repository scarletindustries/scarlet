// Monotonic time.
//
// monotonic() returns the number of milliseconds elapsed since a fixed,
// process-global origin. The clock only ever moves forward — it is unaffected
// by wall-clock adjustments (NTP steps, leap seconds, the user changing the
// system time) — so it is the right primitive for measuring durations,
// computing deadlines, and timing out I/O.

@vm(time__monotonic)
pub fn monotonic() Int
