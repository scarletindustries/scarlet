// sieve: the sieve of Eratosthenes over an array of 5000 flags, run many times.
// It stresses array writes in a tight loop. Ported from the Are We Fast Yet
// suite (https://github.com/smarr/are-we-fast-yet, MIT licence).

function sieve(flags, size) {
  let primeCount = 0;
  for (let i = 2; i <= size; i += 1) {
    if (flags[i - 1]) {
      primeCount += 1;
      let k = i + i;
      while (k <= size) {
        flags[k - 1] = false;
        k += i;
      }
    }
  }
  return primeCount;
}

let total = 0;
for (let n = 0; n < 45; n += 1) {
  const flags = [];
  for (let i = 0; i < 5000; i += 1) {
    flags.push(true);
  }
  total += sieve(flags, 5000);
}
console.log(total);
