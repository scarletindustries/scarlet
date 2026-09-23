---
default: patch
---

A constructor now overwrites a cell nothing else holds any more, instead of allocating a new one, which is what Perceus reference counting is for. A tail-recursive loop that gives a value up and builds another of the same shape allocates once, not once a turn: a 1000-turn loop over a two-field constructor goes from 1000 allocations to 1. `scarlet/internal` gains `cells_made` and `cells_reused`, for debugging and for the tests that pin this.
