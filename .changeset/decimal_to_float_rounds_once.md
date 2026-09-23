---
default: patch
---

`decimal.to_float` rounds `units / 10^scale` once, to the nearest Float. It used to turn each side into a Float first, so a number past the largest Float on either side came out wrong: `10^400` at 399 places gave `1.0` instead of `10.0`. A decimal too large to be a Float gives the largest Float, as Float arithmetic does.
