---
default: major
---

`scarlet/decimal` drops the limits it kept from when an `Int` was 64 bits. `parse` takes any number of digits and fails only on text that isn't a number, and `div` and `round` take any number of places. `div` returns `Err(Nil)` for a zero divisor, as `int.divide` does, so `DivError` and its `DividedByZero` and `ScaleOutOfRange` are gone. `from_float` can't fail any more, so it returns a `Decimal`, not a `Result`. It rounds the float's exact value, so `from_float(0.1, 20)` is `0.10000000000000000555`.
