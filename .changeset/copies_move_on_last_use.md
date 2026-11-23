---
default: patch
---

A value copied to a new name and never read again under the old one now hands its reference over instead of adding one. So after `ys = if c { t = make() ... t } else { ... }` nothing but `ys` holds the value, and a push onto it writes in place where it used to copy first. `scarlet dis --instrs` prints such a copy as `Move move r2`.
