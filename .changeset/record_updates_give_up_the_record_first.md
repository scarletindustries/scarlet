---
default: major
---

A record update reads the fields it keeps as soon as its base has run, before any field written after it, so the old record is given up before those fields run. In `State(..s, v: array::put(s.v, i, x))` the write now finds the array with one holder and changes it in place, where it used to copy the nodes down to the slot on every update. Filling a 5,000-element array held in a record goes from 24,984 cells to the 5,000 records. A `map::set` on a map the record held is in place the same way. A base that is a name, or a field or tuple element read off one, may be written anywhere: `State(v: array::put(s.v, i, x), ..s)` costs the same. The base and the fields you name still run in the order they are written.

One thing a program can see changes. When the old record held the last reference to a handle, such as a listener or a connection, the handle is now closed before the later fields run and not after the new record is made. In `Held(..h, conn: redial(h.port))` the old connection is closed before `redial` runs. To keep it open until the new one is made, write `next = redial(h.port)` first and then `Held(..h, conn: next)`.

A field written after the write that reads the old record, such as `count: s.count + 1`, still keeps it until then, so put that field before the write.
