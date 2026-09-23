---
default: major
---

Named imports are gone: `import scarlet/list.{fold}` no longer parses. Import the module and name what you use through it, `list.fold(...)`, `json.Object(...)`, `fn f(s socket.Socket)`, and `json.Str(s) ->` in a pattern. `as` still renames a module: `import scarlet/list as l`. The parser's error says what to write instead: ``Import the module and name it at the use: `import scarlet/list`, then `list.fold` ``.
