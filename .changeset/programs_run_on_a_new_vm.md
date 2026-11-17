---
default: major
---

Programs run on a new VM, built a feature at a time (`docs/vm-design.md`). A program that reaches something it does not run yet stops with `cannot run: the new VM does not run ... yet`, naming it. Not built yet: processes, networking (sockets, TLS, ports and address lookup) and `wire`. The JIT is gone until the new VM has one.
