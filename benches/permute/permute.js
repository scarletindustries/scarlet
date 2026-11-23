// Permute: walks the permutations of a six-element array by swapping in place.
// It stresses array reads and writes and deep recursion.
// Ported from Are We Fast Yet (github.com/smarr/are-we-fast-yet), MIT licence.

class Permute {
  constructor() {
    this.count = 0;
    this.check = 0;
    this.v = null;
  }

  benchmark() {
    this.count = 0;
    this.v = [0, 1, 2, 3, 4, 5];
    this.permute(6);
    return this.count;
  }

  permute(n) {
    this.count += 1;
    if (n === 0) {
      this.check = (this.check * 31 + this.v[0]) % 1000003;
    } else {
      const n1 = n - 1;
      this.permute(n1);
      for (let i = n1; i >= 0; i -= 1) {
        this.swap(n1, i);
        this.permute(n1);
        this.swap(n1, i);
      }
    }
  }

  swap(i, j) {
    const tmp = this.v[i];
    this.v[i] = this.v[j];
    this.v[j] = tmp;
  }
}

const bench = new Permute();
let total = 0;
for (let k = 0; k < 200; k += 1) {
  total += bench.benchmark();
}
console.log(total);
console.log(bench.check);
