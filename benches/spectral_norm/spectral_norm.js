// spectral-norm: the power method on an implicit matrix, 10 rounds. It stresses
// float arithmetic over arrays. Ported from the Computer Language Benchmarks
// Game (benchmarksgame-team.pages.debian.net/benchmarksgame, BSD-3 licence).

// Entry (i, j) of the infinite matrix, counting from zero.
function a(i, j) {
  return 1.0 / ((i + j) * (i + j + 1) / 2 + i + 1);
}

function av(x, y, n) {
  for (let i = 0; i < n; i++) {
    let s = 0.0;
    for (let j = 0; j < n; j++) s = s + x[j] * a(i, j);
    y[i] = s;
  }
}

function atv(x, y, n) {
  for (let i = 0; i < n; i++) {
    let s = 0.0;
    for (let j = 0; j < n; j++) s = s + x[j] * a(j, i);
    y[i] = s;
  }
}

function atav(x, y, t, n) {
  av(x, t, n);
  atv(t, y, n);
}

// The same 30 Newton steps in every language, as Scarlet has no square root.
function sqrt(s) {
  let x = 1.0;
  for (let k = 0; k < 30; k++) x = 0.5 * (x + s / x);
  return x;
}

// Prints the norm, scaled to an integer, and then floor(u.v). The second number
// depends on every round, where the norm alone has converged after three.
function spectralNorm(n) {
  const u = [], v = [], t = [];
  for (let i = 0; i < n; i++) { u.push(1.0); v.push(0.0); t.push(0.0); }
  for (let k = 0; k < 10; k++) {
    atav(u, v, t, n);
    atav(v, u, t, n);
  }
  let vBv = 0.0, vv = 0.0;
  for (let i = 0; i < n; i++) {
    const ui = u[i], vi = v[i];
    vBv = vBv + ui * vi;
    vv = vv + vi * vi;
  }
  console.log(String(Math.floor(sqrt(vBv / vv) * 1000000000.0 + 0.5)));
  console.log(String(Math.floor(vBv)));
}

spectralNorm(100);
spectralNorm(700);
