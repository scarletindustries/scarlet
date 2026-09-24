---
default: patch
---

`scarlet fmt` keeps a lambda's parameters on one line. A long lambda used to break them one per line, `fn(\n\tacc,\n\tcp,\n)`, which buried the `fn` under its own arguments and read as a call. A named function still breaks its parameters when the line is too long.
