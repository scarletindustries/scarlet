---
default: major
---

Every binding is now one form: a pattern, an optional type on a plain name, then `=` and a value. `x = 1`, `x Int = 1`, `(a, b) = t` and `Point(x, y) = p` work as before, and any pattern that cannot fail is allowed. `Nil = println('x')` still works, because `Nil` is a pattern that matches every `Nil`. Checking any other type this way now needs a name: write `_ Int = f()` instead of `Int = f()`. A label on its own in a pattern binds the field to that name, so `User(name:, ..) = get_user()` binds `name`, by label rather than by position.
