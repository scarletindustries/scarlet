// closures: map, filter and fold over a 1000-element array with closures that
// capture variables, a composed pipeline of small functions, and a counter
// whose step function returns the next counter. It stresses making closures
// and calling them. Written for this suite, not a port.

const adder = (k) => (x) => x + k;

const scaler = (k, m) => (x) => (x * k) % m;

const compose = (f, g) => (x) => g(f(x));

function counter(value, step) {
  return { value, next: (d) => counter((value + step * d) % 1000003, step + 1) };
}

function count(c, n) {
  while (n !== 0) {
    c = c.next((n % 5) + 1);
    n -= 1;
  }
  return c.value;
}

function rounds(xs, r, acc) {
  while (r !== 0) {
    const k = r % 7;
    const scaled = xs.map((x) => (x * r + acc) % 1009);
    const kept = scaled.filter((x) => x % 7 !== k);
    const direct = kept.reduce((s, x) => (s + x * r) % 1000003, 0);
    const steps = [adder(r), scaler(3, 1013), adder(acc % 13), scaler(k + 2, 1019)];
    const pipeline = steps.reduce(compose, (x) => x);
    const piped = kept.map(pipeline).reduce((s, x) => (s + x) % 1000003, 0);
    const counted = count(counter(acc % 97, r), 200);
    acc = (acc * 31 + direct + piped + counted) % 1000003;
    r -= 1;
  }
  return acc;
}

const xs = [];
for (let i = 0; i < 1000; i++) xs.push(i);
console.log(rounds(xs, 1500, 1));
