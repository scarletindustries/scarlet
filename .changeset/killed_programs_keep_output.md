---
default: patch
---

Output to a pipe or a file waits in its buffer for a tenth of a second at most, so a program that is killed doesn't lose what it printed before that.
