---
default: patch
---

An array pattern is one instruction, which asks for the length and finds the elements it names. A walk by `[h, ..t]` runs about two fifths fewer machine instructions, and `[x, ..xs]` and `[..xs, x]` are cheaper too. `scarlet dis`, `--instrs` and `--counters` show the new operations.
