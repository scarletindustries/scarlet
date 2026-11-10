---
default: minor
---

`os::terminal_size()` gives the terminal's size as `(columns, rows)`. It is an `Err(Nil)` when standard output is not a terminal, such as a pipe or a file.
