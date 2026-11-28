---
default: minor
---

`array::repeat(x, n)` makes an array of `n` elements that are all `x`, and `[]` when `n` is 0 or less. For a long array it costs about a tenth of as many pushes, or a seventh when `x` is a string or a record.

`array::map` runs 29% to 40% fewer machine instructions, `array::filter` 23% to 33% fewer and `array::reverse` 16% to 38% fewer. They call back in the same order as before and give up what they have passed at the same time. `array::contains` reads each element where it is and no longer takes the array apart. On an array that something else holds that is mostly 1% to 31% cheaper, and up to 57% when it finds its element early. It is up to 5% dearer there when the array has 64 elements or tens of thousands, mostly 5% to 12% dearer on one that nothing else holds (20% on one of 64 Ints), and up to 20% dearer on a range.

An index or an end of a slice that is too big to be a small Int (2^50 and more) now finds its element in a range that long, where it found nothing. A write to a range, a push on one or a join with one, where the heap has no room for an array of its elements, stops with "the program ran out of heap" before it makes any of them.
