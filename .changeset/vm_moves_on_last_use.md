---
default: patch
---

A call whose argument the caller is done with hands over its own reference instead of adding one. The callee then holds the only reference, which is what lets it overwrite the cell rather than allocate: a walk that rebuilds a list now allocates nothing at all, where it used to allocate one cell per element. Three hundred passes over a 3,000-element list go from 0.09 s to 0.05 s.
