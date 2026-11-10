---
default: patch
---

Looking up an array element no longer copies the node it sits in, and an element of a range, or any other small Int a built-in gives back, no longer goes through a big Int first. One 200 by 48 step of Conway's Game of Life goes from 28 ms to 19 ms.
