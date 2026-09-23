---
default: patch
---

`int.abs(int.min_value)` is `9223372036854775808`, its exact distance from 0. It used to give `int.max_value`, a wrong answer from when an Int was 64 bits and could not hold the right one. `int.min_value` and `int.max_value` are documented as the edges of 64 bits, not of an Int, which has none.
