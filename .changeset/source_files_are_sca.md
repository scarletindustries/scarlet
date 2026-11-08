---
default: major
---

Scarlet source files now end in `.sca` instead of `.scrl`. Imports, `scarlet fmt`, `scarlet lint`, the language server and the editor extensions all look for `.sca`, so rename your files: `for f in **/*.scrl; do mv "$f" "${f%.scrl}.sca"; done`.
