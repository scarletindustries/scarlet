---
default: major
---

When a guard turns down one alternative of an or-pattern, the next alternative is tried with its own bindings, as in Gleam and Rust. `(x, _) | (_, x) if x > 5` now matches `(1, 9)`.
