---
default: major
---

`C(..base, ...)` on a type with several variants may leave out only the fields every variant shares at the same position, the fields `base.field` can read. It used to accept any base of the same type and fill the fields by position, so `Circle(..square, r: 1)` could take a Circle's `x` from the Square's `side`. A type with one variant is unaffected.
