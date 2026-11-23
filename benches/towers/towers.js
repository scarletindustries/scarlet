// Towers of Hanoi with explicit disk stacks: 13 disks moved between three piles,
// many times over. It stresses small record updates and linked-list pushes and pops.
// Ported from the Are We Fast Yet suite (github.com/smarr/are-we-fast-yet, MIT licence).
'use strict';

class TowersDisk {
  constructor(size) {
    this.size = size;
    this.next = null;
  }
}

class Towers {
  constructor() {
    this.piles = null;
    this.movesDone = 0;
  }

  benchmark() {
    this.piles = new Array(3).fill(null);
    this.buildTowerAt(0, 13);
    this.movesDone = 0;
    this.moveDisks(13, 0, 1);
    return this.movesDone;
  }

  pushDisk(disk, pile) {
    const top = this.piles[pile];
    if (top !== null && disk.size >= top.size) {
      throw new Error('Cannot put a big disk on a smaller one');
    }

    disk.next = top;
    this.piles[pile] = disk;
  }

  popDiskFrom(pile) {
    const top = this.piles[pile];
    if (top === null) {
      throw new Error('Attempting to remove a disk from an empty pile');
    }

    this.piles[pile] = top.next;
    top.next = null;
    return top;
  }

  moveTopDisk(fromPile, toPile) {
    this.pushDisk(this.popDiskFrom(fromPile), toPile);
    this.movesDone += 1;
  }

  buildTowerAt(pile, disks) {
    for (let i = disks; i >= 0; i -= 1) {
      this.pushDisk(new TowersDisk(i), pile);
    }
  }

  moveDisks(disks, fromPile, toPile) {
    if (disks === 1) {
      this.moveTopDisk(fromPile, toPile);
    } else {
      const otherPile = (3 - fromPile) - toPile;
      this.moveDisks(disks - 1, fromPile, otherPile);
      this.moveTopDisk(fromPile, toPile);
      this.moveDisks(disks - 1, otherPile, toPile);
    }
  }

  weigh(pile) {
    let acc = 0;
    let depth = 1;
    for (let disk = this.piles[pile]; disk !== null; disk = disk.next) {
      acc += disk.size * depth;
      depth += 1;
    }
    return acc;
  }
}

let moves = 0;
let weight = 0;
for (let i = 0; i < 600; i += 1) {
  const towers = new Towers();
  moves += towers.benchmark();
  weight += towers.weigh(1) + towers.weigh(0);
}
console.log(moves);
console.log(weight);
