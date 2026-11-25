---
default: major
---

A value is given up at its last use inside a branch of an `if`, `match`, `&&` or `||` whose value is used, and so is one whose type is a type variable. Before, such a value was kept until the function ended or its place was needed. Two things follow. A handle, such as a listener or a connection, that only such a value held is now closed there, which is earlier than before and never later. And a write to an array or a map that only such a value still held is now done in place: a record updated inside a branch, `t = if c { State(..s, v: array::put(s.v, i, x)) } else { s }`, no longer copies the array.

A `map::set` or `map::delete` of a map that nothing reads afterwards changes it in place whenever nothing else holds it. Before, it copied when the key or the value was a cell that was bound before the map and given up there too, as in `State(..s, tasks: map::set(s.tasks, k, t))` with `t` a parameter.

A map lookup that is matched on straight away makes no `Option`, wherever the match is.

`scarlet dis` prints the Core IR in its new form: `join`, `jump`, `tail call`, `ret` of a local and `set global`, in place of `letj`, `letc`, `goto`, `ret` of anything and `@g`. The arm of an or-pattern that binds names is printed once. `scarlet dis --instrs` and `--counters` show fewer moves and drops.
