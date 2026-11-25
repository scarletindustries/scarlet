---
default: patch
---

A call whose argument the caller is done with hands over its own reference instead of adding one. The callee then holds the only reference, so a walk that rebuilds a list frees each cell as it goes and the new list takes the space of the old one.
