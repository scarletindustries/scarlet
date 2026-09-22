---
default: minor
---

A map lists its entries in an order that depends only on what it holds: two maps that are `==` give the same `map.keys`, `map.to_list` and printed text, however each was built, and the same on every run and every machine. The order is not the one the old VM used.
