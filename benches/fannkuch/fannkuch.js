// fannkuch-redux: flips the front of every permutation of 0..N-1 until 0 leads,
// and keeps a checksum and the longest run of flips. It stresses small-array
// reads, writes and reversal. Ported from Mike Pall's Lua program in the Computer
// Language Benchmarks Game (benchmarksgame-team.pages.debian.net, BSD-3 licence).
'use strict';

const N = 9;

function fannkuch(n) {
  const p = [], q = [], s = [];
  let sign = 1, maxflips = 0, sum = 0;
  for (let i = 0; i < n; i++) { p[i] = i; q[i] = i; s[i] = i; }
  for (;;) {
    // Copy and flip. The first element lives in q1, not in q.
    let q1 = p[0];
    if (q1 !== 0) {
      for (let i = 1; i < n; i++) q[i] = p[i];
      let flips = 1;
      for (;;) {
        const qq = q[q1];
        if (qq === 0) {
          sum += sign * flips;
          if (flips > maxflips) maxflips = flips;
          break;
        }
        q[q1] = q1;
        if (q1 >= 3) {
          let i = 1, j = q1 - 1;
          do {
            const t = q[i]; q[i] = q[j]; q[j] = t;
            i++; j--;
          } while (i < j);
        }
        q1 = qq;
        flips++;
      }
    }
    // Permute.
    if (sign === 1) {
      const t = p[1]; p[1] = p[0]; p[0] = t;
      sign = -1;
    } else {
      const t = p[1]; p[1] = p[2]; p[2] = t;
      sign = 1;
      for (let i = 2; i < n; i++) {
        const sx = s[i];
        if (sx !== 0) { s[i] = sx - 1; break; }
        if (i === n - 1) return [sum, maxflips];
        s[i] = i;
        // Rotate the first i + 2 elements left by one.
        const t0 = p[0];
        for (let j = 0; j <= i; j++) p[j] = p[j + 1];
        p[i + 1] = t0;
      }
    }
  }
}

const [sum, flips] = fannkuch(N);
console.log(sum + '\nPfannkuchen(' + N + ') = ' + flips);
