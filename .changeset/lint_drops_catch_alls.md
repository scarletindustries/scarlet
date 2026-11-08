---
default: patch
---

`scarlet lint` no longer reports catch-all arms over a sum type. The compiler now refuses them, so the census had nothing left to find there. It still reports constructors with several `Bool` fields.
