---
default: major
---

A map with Int keys lists its entries in a different order than before. An Int that fits 64 bits now hashes through a fixed permutation of 64-bit words instead of SipHash, which is a few multiplies instead of seven rounds, and which gives no two such Ints the same hash at all. The order still depends only on what the map holds, the same on every run and every machine. Looking up Int keys is about a third faster.
