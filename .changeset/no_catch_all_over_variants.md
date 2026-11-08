---
default: major
---

A `match` can no longer use a catch-all for variants it has started to name. Once an arm names some variants of a value, like `Present(v)`, the others have to be named too, like `Absent | Null ->`, instead of `_` or a bare name standing in for them. So a variant added later can never fall silently into an old arm. A catch-all is still fine where the values can't be listed (numbers, strings, binaries, array lengths), and for a whole value no arm takes apart, like `Some(_)`. The error names the missing variants. An or-pattern can now sit inside a constructor, as in `Err(io::NotFound(_) | io::PermissionDenied(_))`. `@exhaustive` is gone, since every match now works the way it asked for.
