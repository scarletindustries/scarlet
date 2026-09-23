---
default: minor
---

A constructor can be destructured through its module: `headers.Header(name, value) = h`. That used to fail to parse, with `Expected '='`. As with an unqualified one, the type must have only that constructor, and one that can fail says to use `match`. A constructor used by its bare name, when an imported module has it, now says how to reach it: ``Unknown constructor 'NotFound' in pattern. It is `io.NotFound` ``.
