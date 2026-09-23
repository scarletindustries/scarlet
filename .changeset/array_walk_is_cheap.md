---
default: patch
---

Walking an array with `[h, ..t]` no longer copies. Dropping from the front keeps the same leaf and counts what it left behind, and when a leaf runs out the next one is moved up whole, so the tree is cut once every 32 elements instead of once an element. Summing a 300,000-element array one element at a time goes from 0.66 s to 0.20 s.
