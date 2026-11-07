---
default: major
---

A module's members are reached with `::`, not `.`: `import scarlet/array`, then `array::map(xs, f)`. The same goes for types, like `fn f(m http::Method)`, and for constructors in patterns, like `io::NotFound(path) ->`. A `.` is now only ever a record field or a tuple index, so a value can share its name with an imported module and both still work: `one.go` reads the field, `one::go` reaches the module. Writing `array.map(xs, f)` says to use `array::map`.
