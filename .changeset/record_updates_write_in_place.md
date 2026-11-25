---
default: patch
---

When a record update sets fields that hold numbers, or sets any field to something simple such as a name, a constant or a field read, and nothing else holds the record, it writes into the record and makes no cell. It then costs the same however many fields it keeps. Any other update makes a new record as before. A function that updates a record of more than 253 fields can now run where the update does not have to read every field.

`scarlet dis` shows such an update as `update`, and `scarlet dis --instrs` and `--counters` as `Update1`, `Update2`, `Update3` and `UpdateWide`.
