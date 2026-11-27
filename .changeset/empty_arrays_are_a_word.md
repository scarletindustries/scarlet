---
default: patch
---

An empty array takes no memory of its own, however it was made. Programs that keep many of them, or walk many short arrays, run up to 10% fewer machine instructions. `scarlet dis --instrs` lists `[]` as a constant.
