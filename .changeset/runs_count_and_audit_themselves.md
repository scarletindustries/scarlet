---
default: minor
---

Three tools for looking at what the VM does. `scarlet dis --instrs` prints, after each function's Core IR, the register instructions the VM loads it as. `scarlet run --counters FILE` writes how many times each kind of instruction ran and how many heap cells were made and reused, as JSON, and the counts are the same on every run of the same program with the same input. `scarlet run --audit` gives up everything the program still holds when it ends and fails if a heap cell is left. A reference count that goes wrong now stops the run with an internal error instead of freeing a cell twice in silence.
