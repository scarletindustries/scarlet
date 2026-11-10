---
default: major
---

`time::to_deadline_ms` is now `time::to_monotonic_ms`. It gives the raw reading of the monotonic clock, and the old name made it look like the time left until the deadline.
