---
default: major
---

An Int is exact at any size, where it used to wrap at 64 bits: `int.max_value + 1` is `9223372036854775808`, not `int.min_value`. So `binary.parse_int` reads a number of any length, where one past 64 bits used to be `Err(Nil)`, and a binary pattern reads an Int wider than 64 bits exactly. An Int literal past 64 bits is still a compile error.
