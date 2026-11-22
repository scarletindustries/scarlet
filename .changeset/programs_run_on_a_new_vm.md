---
default: major
---

Programs run on a new VM, built a feature at a time (`docs/vm-design.md`). A program that reaches something it does not run yet stops with `cannot run: the new VM does not run ... yet`, naming it. Not built yet: processes, ports (`scarlet/os/port`) and `wire`. Sockets, TLS and address lookup run, but each wait blocks the program until processes come, and `net.serve` and `http.serve` need processes, so they stop. The JIT is gone until the new VM has one.
