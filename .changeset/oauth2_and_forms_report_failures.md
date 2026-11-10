---
default: major
---

`scarlet/oauth2` and `scarlet/http/form` now report bad input instead of quietly hiding it. `form::decode` returns a `Result` and refuses a name or value that isn't UTF-8, and a segment with no `=` now counts as a name with an empty value. In `oauth2`, a repeated parameter is refused, as RFC 6749 says. The `/authorize` errors now say whether they may be sent to the redirect URI (`Refused`) or must be shown to the user (`Untrusted`). `approve`, `register` and `token` fail when the OS random source fails, instead of making an empty token. A missing `scope`, `state` or `resource` is `None`, not `''`, and a `Store` keeps a client's redirect URIs as an `Array(String)`.
