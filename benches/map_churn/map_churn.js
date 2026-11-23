// Stresses the built-in map (a Map): insert scattered int keys, look them all
// up several times (with a miss probe beside each hit), then delete the oldest
// key and insert a new one in a long churn loop.
// Written for this suite, starting from examples/bench_map.sca.

// A full-period generator, so the keys never repeat. Its products stay far
// below 2^53, which keeps the numbers exact.
function nextKey(x) {
  return (x * 69069 + 1) % 4294967296;
}

function lookups(m, x, n, acc) {
  for (let i = 0; i < n; i++) {
    x = nextKey(x);
    let v = m.get(x);
    if (v !== undefined) acc = (acc * 31 + v) % 1000000007;
    v = m.get(x + 1);
    if (v !== undefined) acc = acc + v;
  }
  return acc;
}

const n = 200000;
const seed = 42;

const m = new Map();
let lead = seed;
for (let i = 0; i < n; i++) {
  lead = nextKey(lead);
  m.set(lead, i);
}

let acc = 0;
for (let r = 0; r < 10; r++) {
  acc = lookups(m, seed, n, acc);
}
console.log(acc);

// The map is a sliding window over the key sequence: `trail` names the oldest
// key, which goes, and `lead` the next new one, which comes in.
let trail = seed;
for (let i = 0; i < 3000000; i++) {
  trail = nextKey(trail);
  lead = nextKey(lead);
  m.delete(trail);
  m.set(lead, i);
}
// The window's keys must all be there and the first `n`, long deleted, must
// all be gone: a hit in the second pass would change the sum.
const window = lookups(m, trail, n, 0);
console.log(lookups(m, seed, n, window));
