---
default: minor
---

The REPL runs entries again, on the new VM, and prints each one's value. Typing an imported module's name alone lists its first 15 public names, functions first. `:exit` leaves, as `:quit` does. Two errors now say what is wrong: a stdlib module used with no import names the import it needs (`Add import scarlet/int`), and a module used as a value says so and shows a use of it (`like int.abs`).
