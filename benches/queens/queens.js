// Queens from the Are We Fast Yet suite: place eight queens by backtracking over
// boolean arrays of free rows and diagonals. Stresses array reads and writes.
// Ported from https://github.com/smarr/are-we-fast-yet (MIT licence).
'use strict';

class Queens {
  constructor() {
    this.freeMaxs = null;
    this.freeRows = null;
    this.freeMins = null;
    this.queenRows = null;
  }

  queens() {
    this.freeRows = new Array(8).fill(true);
    this.freeMaxs = new Array(16).fill(true);
    this.freeMins = new Array(16).fill(true);
    this.queenRows = new Array(8).fill(-1);

    return this.placeQueen(0);
  }

  placeQueen(c) {
    for (let r = 0; r < 8; r += 1) {
      if (this.getRowColumn(r, c)) {
        this.queenRows[r] = c;
        this.setRowColumn(r, c, false);

        if (c === 7) {
          return true;
        }

        if (this.placeQueen(c + 1)) {
          return true;
        }
        this.setRowColumn(r, c, true);
      }
    }
    return false;
  }

  getRowColumn(r, c) {
    return this.freeRows[r] && this.freeMaxs[c + r] && this.freeMins[c - r + 7];
  }

  setRowColumn(r, c, v) {
    this.freeRows[r] = v;
    this.freeMaxs[c + r] = v;
    this.freeMins[c - r + 7] = v;
  }

  // The column of each row's queen, read as one decimal number.
  digits() {
    let acc = 0;
    for (let r = 0; r < 8; r += 1) {
      acc = acc * 10 + this.queenRows[r];
    }
    return acc;
  }
}

const board = new Queens();
let solved = 0;
let sum = 0;
for (let i = 0; i < 3500; i += 1) {
  if (board.queens()) {
    solved += 1;
    sum += board.digits();
  }
}
console.log(String(solved));
console.log(String(sum));
