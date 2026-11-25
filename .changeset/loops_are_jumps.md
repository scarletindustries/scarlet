---
default: patch
---

A call in tail position to a function that is known when the program loads is a jump, with moves only for the arguments that are not where they go already. So a loop that only counts costs about half of what it did, a program runs up to 31% fewer machine instructions, and what it prints does not change. `scarlet dis --instrs` and `scarlet run --counters` show such a call as `Move`, `Move2`, `Move3`, `Drop` and `DropWide` and then `Jump` or `Enter`, where they showed `TailCallSelf` and `TailCall`, and `scarlet dis` may list the arguments of one in another order. A big Int that a turn of a loop works out and does not pass on may be held until the register it is in is written again, or until the function returns or goes on to another.
