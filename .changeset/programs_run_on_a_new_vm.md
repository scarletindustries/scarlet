---
default: major
---

Programs run on a new VM, built a feature at a time (`docs/vm-design.md`). A program that reaches something it does not run yet stops with `cannot run: the new VM does not run ... yet`, naming it. Not built yet: maps, processes, files and networking, `wire`, and most string, binary and array built-ins. The JIT is gone until the new VM has one.
