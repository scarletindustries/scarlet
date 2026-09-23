---
default: minor
---

`if` has a one-line form: `if n < 0 then 'negative' else 'positive'`. The block form stays for branches of more than one line, and `else` may now take any expression, as in `if is_admin(user) { … } else not_found()`. `then` is a keyword only right after an `if` condition, so `result.then` and a field named `then` still work. `scarlet fmt` keeps each `if` in the form it was written in.
