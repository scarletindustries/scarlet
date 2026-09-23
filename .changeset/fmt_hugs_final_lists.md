---
default: patch
---

`scarlet fmt` keeps a final list argument against its call, as `Object([` on one line with the items below, where it used to put the `[` on a line of its own. A long chain of operators like `a || b || c` now breaks at every operator, indented under the first operand, where it used to leave the first few on one line and put the rest one per line with no indent.
