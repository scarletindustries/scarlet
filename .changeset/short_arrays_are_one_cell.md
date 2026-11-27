---
default: patch
---

An array of 32 elements or fewer is one cell. When nothing else holds it, a write, a push at either end, taking the first element, a slice and a join change it where it is, and make no new cell until it has outgrown its own or a slice has left a small part of it. A slice and a join now take over an array that nothing reads after them, as a push does, and an array literal hands its elements over. An element that only such an array held is given up when the array loses it, so a socket in one closes then and never later than it did. Programs that use short arrays run up to 45% fewer machine instructions. `scarlet dis --instrs` shows what is handed over.
