---
default: major
---

`or` on a `Result` can no longer throw away an error that carries data. `read_text(path) or ''` is now a compile error, and you write `read_text(path) or e -> ...` and handle `e`. A receiver starting with `_` is refused too, because it drops the error just as quietly. A `Result(_, Nil)` works like `Option` and still takes a plain `or`. Because of this, `net::serve`, `http::serve` and their `_in` and `_on` forms take a required `on_error fn(net::ConnectionError) Nil`, which gets each failed connection with the client's address. A `net` connection handler now returns `Result(Nil, NetError)`.
