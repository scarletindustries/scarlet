// Mandelbrot: the bit-packed mandelbrot checksum. It stresses float arithmetic
// and integer bit operations in nested loops.
// Ported from Are We Fast Yet (github.com/smarr/are-we-fast-yet, MIT licence),
// which took it from the Computer Language Benchmarks Game (revised BSD).
'use strict';

function mandelbrot(size) {
  let sum = 0;
  let byteAcc = 0;
  let bitNum = 0;

  let y = 0;
  while (y < size) {
    const ci = (2.0 * y / size) - 1.0;
    let x = 0;

    while (x < size) {
      let zrzr = 0.0;
      let zi = 0.0;
      let zizi = 0.0;
      const cr = (2.0 * x / size) - 1.5;

      let z = 0;
      let notDone = true;
      let escape = 0;
      while (notDone && z < 50) {
        const zr = zrzr - zizi + cr;
        zi = 2.0 * zr * zi + ci;

        zrzr = zr * zr;
        zizi = zi * zi;

        if (zrzr + zizi > 4.0) {
          notDone = false;
          escape = 1;
        }
        z += 1;
      }

      byteAcc = (byteAcc << 1) + escape;
      bitNum += 1;

      if (bitNum === 8) {
        sum ^= byteAcc;
        byteAcc = 0;
        bitNum = 0;
      } else if (x === size - 1) {
        byteAcc <<= (8 - bitNum);
        sum ^= byteAcc;
        byteAcc = 0;
        bitNum = 0;
      }
      x += 1;
    }
    y += 1;
  }
  return sum;
}

console.log(mandelbrot(1));
console.log(mandelbrot(500));
console.log(mandelbrot(750));
