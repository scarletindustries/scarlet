---
default: patch
---

A value that can never be a heap cell is not given up with an instruction of its own any more. That is a Float, and a value of a type whose constructors have no fields, like a Bool or `Nil`. Neither is what a `match` arm found to be a constructor with no fields. A program runs up to 6% fewer machine instructions, and what it prints does not change. `scarlet dis` and `scarlet run --counters` show fewer drops, and `scarlet run --audit=each` also checks that no value is a cell where its type says it is none.
