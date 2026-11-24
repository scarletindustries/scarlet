---
default: minor
---

Making and freeing a heap value costs less, and a heap keeps no list of its freed values beside them. `scarlet run --audit` now checks every reference count when the program ends and names the first value whose count is wrong, where it only counted the values left over. `scarlet run --audit=each` makes the same check before every instruction, which is slow and stops where the count first went wrong. A run that takes another reference to a value already freed stops with an internal error there. An Int literal past 51 bits is made once a run, where it was made each time it was read.
