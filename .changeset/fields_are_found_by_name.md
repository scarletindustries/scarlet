---
default: major
---

A field is found by its name. `s.x` works whenever every variant of the type has a field `x` of the same type, wherever `x` sits in each; it used to need the same position in every variant. `C(..base, ...)` fills each field it leaves out with `base.field`, so on a type with several variants it may leave out only the fields they all have. It used to accept any base of the same type and fill the fields by position, so `Circle(..square, r: 1)` could take a Circle's `x` from the Square's `side`.
