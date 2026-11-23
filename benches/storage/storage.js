// Storage builds a tree of nested arrays, four children to a node and seven
// levels deep, over and over. It stresses allocation and freeing.
// Ported from Are We Fast Yet (github.com/smarr/are-we-fast-yet), MIT licence.

class Storage {
  constructor() {
    this.seed = 74755;
    this.count = 0;
    this.slots = 0;
  }

  next() {
    this.seed = (this.seed * 1309 + 13849) % 65536;
    return this.seed;
  }

  build(depth) {
    this.count += 1;
    if (depth === 1) {
      const n = (this.next() % 10) + 1;
      this.slots += n;
      return new Array(n);
    }
    const arr = new Array(4);
    for (let i = 0; i < 4; i += 1) {
      arr[i] = this.build(depth - 1);
    }
    return arr;
  }
}

let totalCount = 0;
let totalSlots = 0;
for (let round = 0; round < 500; round += 1) {
  const storage = new Storage();
  storage.build(7);
  totalCount += storage.count;
  totalSlots += storage.slots;
}
console.log(totalCount);
console.log(totalSlots);
