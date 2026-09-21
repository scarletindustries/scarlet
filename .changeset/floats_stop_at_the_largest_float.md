---
default: major
---

A Float result too large to represent is the largest Float, keeping its sign, where it used to become `0.0`, so it still compares greater than `1.0`. `x % 0.0` is `x`, as for Int. `float.floor`, `ceil`, `round` and `truncate` give the exact Int where they used to stop at the 64-bit bounds.
