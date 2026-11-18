---
default: major
---

Bindings follow OCaml's relaxed value restriction. A plain value (a function literal, a literal, or a constructor, tuple or array of plain values) is generic as before. A binding made by a call is generic only over type variables whose values come out of it and never go in, so `m = map::new()` can still be used at two types, but a function that takes an `a` pins `a`. This closes a hole where a `Subject` hidden inside closures let one mailbox be written at `Int` and read at `String`. Each type's parameters now have a variance, worked out from its fields. The standard library marks `Array` and `Map` with `@covariant`. Every other host type is invariant.
