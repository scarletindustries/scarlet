---
default: major
---

A program's heap is now one block of memory that doubles when it fills, where it was a list of chunks. A program can hold 16 GiB of heap values in all and stops with "the program ran out of heap" past that. Before, the heap was full once its values of ordinary size reached 4.2 GiB, and a value big enough for a chunk of its own was counted apart and limited only by the machine's memory. So a program that held more than 16 GiB in such big values ran before and stops now. One string, binary or other value is still at most 2 GiB. A value of 2 KB or more has memory of its own, which is freed when the value is. Whether the process gets smaller then is up to the system's allocator. Memory taken by smaller values is used again by the program and goes back when it ends.
