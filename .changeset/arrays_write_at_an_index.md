---
default: minor
---

`scarlet/array` can write at an index. `array::set(xs, i, x)` returns `Ok` of the array with `x` at index `i`, or `Err(Nil)` when `i` is outside the array. `array::put(xs, i, x)` returns the array, unchanged when `i` is outside it. `array::swap(xs, i, j)` changes two elements over. When nothing else holds the array and nothing reads it after the call, the write happens in place and copies nothing, so filling a 5,000-element array a slot at a time no longer rebuilds it at every step. A `match` on `array::set` whose `Err` arm gives `xs` back writes in place too. `xs` itself never changes for anything that still holds it.
