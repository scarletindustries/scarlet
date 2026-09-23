---
default: major
---

`json.int` reads every integer a document holds, since an `Int` has no bounds. An integer above 9223372036854775807 used to give `None`. An integer past 64 bits still does not parse.
