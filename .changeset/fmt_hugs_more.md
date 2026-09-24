---
default: patch
---

`scarlet fmt` keeps two more shapes against their call. A binary literal that comes last stays as `Ok(<<` with its segments below, and a call that hugs may itself be hugged, so `Timer(waiting: spawn(fn() { … }))` closes its three brackets together instead of putting each on its own line.
