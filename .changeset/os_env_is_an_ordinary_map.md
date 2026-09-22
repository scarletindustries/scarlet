---
default: minor
---

`os.env()` is an ordinary map of the environment, made the first time a program asks for it and shared after, rather than a live view. It holds every variable whose name and value are UTF-8. When a name is listed twice, the first value wins, as with `getenv`. It prints its entries like any other map, where it used to print `<map env>`.
