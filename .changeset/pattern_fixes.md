---
default: patch
---

A binary segment with a negative `size(...)` no longer matches, so the next arm runs. The error for a catch-all after a guarded arm now names that arm's variant too, so following its advice compiles.
