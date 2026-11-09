---
default: major
---

Line breaks now follow one rule. A line break ends a statement only when it sits directly in a `{ }` or at the top of the file, the line so far could end an expression, and the next line could start a statement. Inside `( )` and `[ ]`, line breaks are just spaces, and a line that ends in an operator or starts with `|>`, `.`, `else` or `=` carries on. Three things change: constructor fields split over several lines need commas, like everywhere else; statements and match arms in a block each go on their own line; and `x` then `= 1` on the next line is a binding. `scarlet fmt` checks its own output against the same rule, so it can no longer add a line break that changes what a program means.
