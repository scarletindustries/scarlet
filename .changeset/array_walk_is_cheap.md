---
default: patch
---

Walking an array with `[h, ..t]` no longer cuts its tree at each element, but once every 32. Summing a 300,000-element array one element at a time goes from 0.66 s to 0.20 s.
