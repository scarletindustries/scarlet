---
default: patch
---

A `match` on a Bool with a guard on its last-but-one arm, as in `False -> .. True if g -> .. True -> ..`, no longer stops with an internal error when the guard fails.

A `match` is compiled to a decision tree, so each thing it asks of a value is asked once, and a field is read only on the ways that use it. `scarlet dis` prints `switch`, `open` and `no arm` in place of `match`, and `scarlet dis --instrs` and `--counters` show `Switch`, `SwitchInt` and `SwitchIntFar`.
