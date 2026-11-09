---
default: major
---

Errors in the standard library are types now, never text. `oauth2::register`, `oauth2::parse_authorize` and `oauth2::token` return a `RegisterError`, `AuthorizeError` and `TokenError`, and `register_error_code`, `authorize_error_code` and `token_error_code` give the OAuth `error` value to send back. `json::ParseError` holds a `reason`, a `ParseReason` like `UnexpectedEnd` or `BadNumber`, where it used to hold a `message` string. `json::parse_error_message` still turns it into text.
