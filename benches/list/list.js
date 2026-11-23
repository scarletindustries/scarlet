// AWFY List: builds linked lists of a user-defined type and runs the recursive
// tail (Takeuchi) comparison over them. It stresses allocation and recursion.
// Ported from Are We Fast Yet (github.com/smarr/are-we-fast-yet, MIT licence).
'use strict';

class Element {
  constructor(v) {
    this.val = v;
    this.next = null;
  }

  length() {
    if (this.next === null) {
      return 1;
    }
    return 1 + this.next.length();
  }
}

function makeList(length) {
  if (length === 0) {
    return null;
  }
  const e = new Element(length);
  e.next = makeList(length - 1);
  return e;
}

function isShorterThan(x, y) {
  let xTail = x;
  let yTail = y;
  while (yTail !== null) {
    if (xTail === null) {
      return true;
    }
    xTail = xTail.next;
    yTail = yTail.next;
  }
  return false;
}

function tail(x, y, z) {
  if (isShorterThan(y, x)) {
    return tail(tail(x.next, y, z), tail(y.next, z, x), tail(z.next, x, y));
  }
  return z;
}

function benchmark() {
  return tail(makeList(15), makeList(10), makeList(6)).length();
}

let total = 0;
for (let i = 0; i < 2000; i += 1) {
  total += benchmark();
}
console.log(total);
