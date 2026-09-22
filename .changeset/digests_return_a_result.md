---
default: major
---

`crypto.sha1`, `crypto.sha256`, `crypto.sha512`, `crypto.hmac_sha256` and `crypto.websocket_accept` return `Result(Binary, Nil)`, with `Err(Nil)` for a binary that is not a whole number of bytes. They used to hash only the whole bytes and drop the rest, so two different binaries could share a digest. `crypto.sha1` now runs on aws-lc like the other digests, instead of being written in Scarlet. `crypto.const_eq` now compares every bit: it used to call `<<1, 2:4>>` and `<<1, 3:4>>` equal. A signature check given anything that is not whole bytes is `False`.
