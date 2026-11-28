---
default: major
---

An array of more than 32 elements is a strict trie, 32 wide, with a short array at each end. Building one by pushes, walking it and writing to it by index run a third to a half fewer machine instructions when nothing else holds it. Reading it by index is a tenth fewer at 33 to 40 elements, and a fifth to a third fewer at several hundred. Taking the first element off an array that nothing else holds hands it over, so what is done to it next can be done in place. What an array has lost, and that is a handle such as a listener or a connection or may have one in it, is never kept by what is left of it. So a handle may be closed earlier than before and is never closed later.

Joining two arrays now costs the shorter one. A join of two long arrays whose shorter side has fewer than about 250 elements is mostly cheaper than it was, but one of 32 and 32 is a third dearer, and one of a few elements with a long array that something else holds is up to 15% dearer at the back and up to 45% dearer in front. A join of 1,000 and 1,000 costs two to three times as much, one of 5,000 and 5,000 nine times, one of 100,000 sixty to a hundred and thirty times, and an array that is joined to itself over and over takes memory in proportion to its length. So inserting in the middle of an array of 5,000 by cutting it and joining it costs twice as much.

A program that keeps many tails of one long array, as a backtracking parser does or a recursion that uses its array after the call, runs from 20% fewer to 20% more machine instructions when the elements are numbers or strings, and up to 40% more when they are small records or tuples of such, and takes as much memory as it did. When the elements hold more than that it is up to twice as dear and takes up to twice the memory. An array of 33 to 100 elements that was built by pushes takes up to 70% more memory.

What a program prints does not change. `scarlet dis --instrs` and `--counters` call a join `ArrayJoin`. A program may have 65,521 constructors with fields, one more than before.
