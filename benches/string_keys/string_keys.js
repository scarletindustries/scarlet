// Counts how often each word turns up in a stream of short words, in a Map
// keyed by string, then looks a second stream up in it. It stresses building
// short strings, hashing them and maps keyed by strings. Written for this suite.

const letters = ["a", "e", "i", "n", "r", "t"];

function next(seed) {
  return (seed * 16807) % 2147483647;
}

// A word of 2 to `lengths + 1` letters, all of it decided by `r`.
function word(r, lengths) {
  const n = 2 + (r % lengths);
  r = Math.floor(r / lengths);
  let w = "";
  for (let i = 0; i < n; i++) {
    w += letters[r % 6];
    r = Math.floor(r / 6);
  }
  return w;
}

const counts = new Map();
let seed = 42;
for (let i = 0; i < 1000000; i++) {
  seed = next(seed);
  const w = word(seed, 5);
  counts.set(w, (counts.get(w) || 0) + 1);
}

let squares = 0;
for (const n of counts.values()) {
  squares += n * n;
}

// The second stream has longer words in it too, so some lookups miss.
let check = 0;
seed = 2024;
for (let i = 0; i < 1000000; i++) {
  seed = next(seed);
  const n = counts.get(word(seed, 6)) || 0;
  check = (check * 31 + n) % 1000000007;
}

console.log(counts.size);
console.log(squares);
console.log(check);
