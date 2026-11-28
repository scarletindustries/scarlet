---
default: major
---

A constructor, a tuple or a closure takes one word less of memory, a `match` tells a value's variant without looking at the heap, and reading a field, making a value and freeing one cost less. Programs run up to 24% fewer machine instructions, and those that hold many small values use up to a quarter less memory. What a program prints does not change, and neither does the order of a map's keys. One limit comes with it. A program may have 65,521 constructors with fields among the types it uses, the stdlib's with them, and one with more stops before it runs with "the program has more constructors with fields than the VM can number", where it used to run. `scarlet dis --instrs` and `scarlet run --counters` call a variant's test `JumpUnlessTag`, which is listed with the variant's number and no type, and a read of a tuple's element `Field`, as a constructor's field is.
