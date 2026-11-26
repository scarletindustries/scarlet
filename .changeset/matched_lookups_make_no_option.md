---
default: patch
---

A `match` or an `or` straight on `xs[i]`, on a slice, on `array::set` or on `map::get` makes no `Option` or `Result` unless an arm names the whole of it, and the default of an `or` runs only when there is nothing there. `array::put(xs, i, x)` in a loop is one instruction, and `array::swap` makes no cells. `scarlet dis` prints `probe` for these, and `--instrs` and `--counters` show `ArrayGet`, `ArraySlice` and `MapGet` in place of `ArrayIndex`, `ArrayIndexOr` and `MapFind`.
