---
default: patch
---

The stdlib's comments are shorter and its files are formatted. Comment lines go from 3,052 to 1,046: what a name or a signature already said is gone, and what is left is at most three lines, or ten for a module's own doc. `scarlet/wire`'s byte-format spec moved to `docs/wire-format.md`, which is where whoever rebuilds it will want it. Three imports nothing used are gone. No behaviour changes.
