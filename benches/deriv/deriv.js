// deriv: differentiate x^x symbolically nine times over, simplifying as it goes, and do that twice.
// It stresses allocating small nodes of a sum type and matching on pairs of them.
// Ported from test/bench/koka/deriv.kk in Koka's benchmarks (Apache-2.0), itself from Lean 4's deriv.ml.

const VAL = 0, VAR = 1, ADD = 2, MUL = 3, POW = 4, LN = 5;

const val = (value) => ({ tag: VAL, value });
const bin = (tag, l, r) => ({ tag, l, r });

function pown(a, b) {
  let acc = 1;
  while (b > 0) {
    acc *= a;
    b -= 1;
  }
  return acc;
}

function add(n0, m0) {
  const mt = m0.tag, nt = n0.tag;
  if (mt === VAL) {
    const m = m0.value;
    if (nt === VAL) return val(n0.value + m);
    if (m === 0) return n0;
    return add(val(m), n0);
  }
  if (mt === ADD) {
    const ml = m0.l;
    if (nt === VAL) {
      const n = n0.value;
      if (n === 0) return m0;
      if (ml.tag === VAL) return add(val(n + ml.value), m0.r);
      return bin(ADD, n0, m0);
    }
    if (ml.tag === VAL) return add(val(ml.value), add(n0, m0.r));
    if (nt === ADD) return add(n0.l, add(n0.r, m0));
    return bin(ADD, n0, m0);
  }
  if (nt === VAL) {
    if (n0.value === 0) return m0;
    return bin(ADD, n0, m0);
  }
  if (nt === ADD) return add(n0.l, add(n0.r, m0));
  return bin(ADD, n0, m0);
}

function mul(n0, m0) {
  const mt = m0.tag, nt = n0.tag;
  if (mt === VAL) {
    const m = m0.value;
    if (nt === VAL) return val(n0.value * m);
    if (m === 0) return val(0);
    if (m === 1) return n0;
    return mul(val(m), n0);
  }
  if (mt === MUL) {
    const ml = m0.l;
    if (nt === VAL) {
      const n = n0.value;
      if (n === 0) return val(0);
      if (n === 1) return m0;
      if (ml.tag === VAL) return mul(val(n * ml.value), m0.r);
      return bin(MUL, n0, m0);
    }
    if (ml.tag === VAL) return mul(val(ml.value), mul(n0, m0.r));
    if (nt === MUL) return mul(n0.l, mul(n0.r, m0));
    return bin(MUL, n0, m0);
  }
  if (nt === VAL) {
    const n = n0.value;
    if (n === 0) return val(0);
    if (n === 1) return m0;
    return bin(MUL, n0, m0);
  }
  if (nt === MUL) return mul(n0.l, mul(n0.r, m0));
  return bin(MUL, n0, m0);
}

function powr(m0, n0) {
  if (n0.tag === VAL) {
    const n = n0.value;
    if (m0.tag === VAL) return val(pown(m0.value, n));
    if (n === 0) return val(1);
    if (n === 1) return m0;
    return bin(POW, m0, n0);
  }
  if (m0.tag === VAL && m0.value === 0) return val(0);
  return bin(POW, m0, n0);
}

function ln(n) {
  if (n.tag === VAL && n.value === 1) return val(0);
  return { tag: LN, e: n };
}

function d(x, e) {
  switch (e.tag) {
    case VAL:
      return val(0);
    case VAR:
      return x === e.name ? val(1) : val(0);
    case ADD:
      return add(d(x, e.l), d(x, e.r));
    case MUL: {
      const f = e.l, g = e.r;
      return add(mul(f, d(x, g)), mul(g, d(x, f)));
    }
    case POW: {
      const f = e.l, g = e.r;
      return mul(
        powr(f, g),
        add(mul(mul(g, d(x, f)), powr(f, val(-1))), mul(ln(f), d(x, g))),
      );
    }
    default: {
      const f = e.e;
      return mul(d(x, f), powr(f, val(-1)));
    }
  }
}

function count(e) {
  switch (e.tag) {
    case VAL:
    case VAR:
      return 1;
    case LN:
      return count(e.e);
    default:
      return count(e.l) + count(e.r);
  }
}

function nest(n, f) {
  for (let i = 1; i <= n; i++) {
    f = d("x", f);
    console.log(i + " count: " + count(f));
  }
  return f;
}

const x = { tag: VAR, name: "x" };
nest(9, powr(x, x));
nest(9, powr(x, x));
console.log("done");
