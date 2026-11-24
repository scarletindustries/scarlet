---
default: patch
---

Arithmetic and comparisons on Ints and Floats cost less. Each operator on two Ints is an instruction of its own, and so is each on two Floats but `==` and `!=`, so the VM no longer looks at which operator it is while it runs. An operator beside a constant holds the constant wherever there is an instruction that holds that side, for Floats too, and an `if` on `<`, `<=`, `>` or `>=` of two Floats compares and jumps in one, as one on two Ints did. `float::from_int` and `%` on two Floats are instructions as well. What a program prints does not change. `scarlet dis --instrs` and `scarlet run --counters` name the new instructions, such as `IntAdd`, `FloatMulK` and `JumpUnlessIntLt`, where they used to say `Int`, `IntImm`, `Float`, `JumpUnlessCmp` and `JumpUnlessCmpImm`.
