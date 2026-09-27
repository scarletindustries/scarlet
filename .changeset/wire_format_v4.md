---
default: patch
---

`docs/wire-format.md` describes version 4, which is what the new VM will write: an Int past 64 bits, and a closure that names its code rather than the run that made it, so it can cross a network to a peer holding the same code. Its captures decode against a descriptor rather than carrying their own tags, which is what makes dropping the run stamp safe. Nothing is built yet.
