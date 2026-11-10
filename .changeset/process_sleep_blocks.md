---
default: minor
---

`process::sleep` now runs on the new VM. Until there is a scheduler, it blocks the whole program while it waits. `examples/life.sca` uses it instead of spinning on the clock, so it no longer keeps a core busy. It also asks the terminal its size, so it no longer needs a width and height passed in.
