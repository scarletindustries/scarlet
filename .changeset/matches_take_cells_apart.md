---
default: major
---

A match that has no more use for the value it matched takes it apart in one step, and the next value of its size gets the space. So a loop that gives a value up and builds another of the same size needs no more memory than one turn does, and programs that match a lot run up to 30% fewer machine instructions. `scarlet/internal` gains `cells_made`, for debugging and for the tests that pin how many cells a program makes.

A program can see a handle, such as a connection, closed sooner in two places. One in a value that a function made and passes on for the last time is closed by the function it passes it to, where the caller could keep it until the call returned. One in a field that a match leaves out is closed as the match takes the value apart. A value made inside an `if` or a `match` that gives a value is still held until its register is used again, which for some is now sooner and for some later.
