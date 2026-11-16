---
default: patch
---

The VM does less work per instruction. Int arithmetic on small numbers skips the big-number path, a comparison or sum against a small constant is one instruction, an `if` on a comparison compares and jumps in one, and a call no longer allocates for its arguments. `map::set` on a map nothing else holds changes it in place instead of copying the path to the key. `examples/bench_heavy.sca` goes from 0.87 s to 0.40 s, and `examples/bench_map.sca` from 2.60 s to 1.19 s.
