---
default: patch
---

An Int from -2^50 to 2^50 - 1 now lives inside the value word, where the range used to be -2^47 to 2^47 - 1. Arithmetic that stays inside it makes no heap cell, so a step like `{ x * 69069 + 1 } % 4294967296`, whose product reaches about 2^48, no longer makes one on about half its turns. Nothing a program prints changes: an Int is still exact at any size, and no map changes its order.
