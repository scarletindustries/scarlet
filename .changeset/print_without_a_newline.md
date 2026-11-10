---
default: minor
---

`print` writes a value without a newline after it, so a program can redraw one line or end a frame on the same line. Output to a terminal now shows as soon as it is written. Before, `scarlet run` held it back until the program ended or 8 KB had built up. Output to a pipe or a file is still buffered.
