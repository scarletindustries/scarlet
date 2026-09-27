---
default: minor
---

`scarlet check` describes `scarlet/wire` types again. At each `wire.encode`/`wire.decode` the compiler builds a descriptor of the type the value crosses at and puts it in the program, so a type it could not reconstruct is refused when the program is compiled rather than at run time — and the refusal names the offending sub-type and the path down to it, `` `wire.encode` cannot encode `a` ... (through [1] -> [value] -> [element]) ``. Running a wire call still stops with "wire.encode is not built yet": the byte walks come next.
