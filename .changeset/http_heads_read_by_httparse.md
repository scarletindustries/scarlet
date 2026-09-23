---
default: major
---

`h1.parse_request` and `h1.parse_response` read heads with `httparse`, the parser `hyper` uses. This refuses four things the old parser did not, each as the HTTP RFCs allow:

- A request line with a token after the version is `Bad(400)`. It used to be `Bad(505)`.
- A line ending in a bare LF or CR is `Bad(400)` at once. It used to wait for more bytes until the read deadline.
- A NUL in a header value is `Bad(400)`. It used to parse.

A `Content-Length` past 9223372036854775807 is refused.
