// cfold: build an expression tree, reassociate it and fold its constants, many times over.
// It stresses allocating small nodes of a sum type and matching on them, with deep recursion.
// Ported from test/bench/koka/cfold.kk in Koka's benchmarks (Apache-2.0), itself from Lean 4's const_fold.

// A node is {tag, l, r} or {tag, v}.
const VAR = 1, VAL = 2, ADD = 3, MUL = 4;

function mkExpr(n, v) {
  if (n === 0) {
    return v === 0 ? { tag: VAR, v: 1 } : { tag: VAL, v: v };
  }
  return { tag: ADD, l: mkExpr(n - 1, v + 1), r: mkExpr(n - 1, v === 0 ? 0 : v - 1) };
}

function appendAdd(e0, e3) {
  if (e0.tag === ADD) {
    return { tag: ADD, l: e0.l, r: appendAdd(e0.r, e3) };
  }
  return { tag: ADD, l: e0, r: e3 };
}

function appendMul(e0, e3) {
  if (e0.tag === MUL) {
    return { tag: MUL, l: e0.l, r: appendMul(e0.r, e3) };
  }
  return { tag: MUL, l: e0, r: e3 };
}

function reassoc(e) {
  switch (e.tag) {
    case ADD:
      return appendAdd(reassoc(e.l), reassoc(e.r));
    case MUL:
      return appendMul(reassoc(e.l), reassoc(e.r));
    default:
      return e;
  }
}

function cfold(e) {
  switch (e.tag) {
    case ADD: {
      const f1 = cfold(e.l);
      const f2 = cfold(e.r);
      if (f1.tag === VAL) {
        const a = f1.v;
        if (f2.tag === VAL) {
          return { tag: VAL, v: a + f2.v };
        }
        if (f2.tag === ADD) {
          const l = f2.l, r = f2.r;
          if (r.tag === VAL) {
            return { tag: ADD, l: { tag: VAL, v: a + r.v }, r: l };
          }
          if (l.tag === VAL) {
            return { tag: ADD, l: { tag: VAL, v: a + l.v }, r: r };
          }
        }
      }
      return { tag: ADD, l: f1, r: f2 };
    }
    case MUL: {
      const f1 = cfold(e.l);
      const f2 = cfold(e.r);
      if (f1.tag === VAL) {
        const a = f1.v;
        if (f2.tag === VAL) {
          return { tag: VAL, v: a * f2.v };
        }
        if (f2.tag === MUL) {
          const l = f2.l, r = f2.r;
          if (r.tag === VAL) {
            return { tag: MUL, l: { tag: VAL, v: a * r.v }, r: l };
          }
          if (l.tag === VAL) {
            return { tag: MUL, l: { tag: VAL, v: a * l.v }, r: r };
          }
        }
      }
      return { tag: MUL, l: f1, r: f2 };
    }
    default:
      return e;
  }
}

function evalExpr(e) {
  switch (e.tag) {
    case VAR:
      return 0;
    case VAL:
      return e.v;
    case ADD:
      return evalExpr(e.l) + evalExpr(e.r);
    default:
      return evalExpr(e.l) * evalExpr(e.r);
  }
}

// An order-sensitive hash of the tree, so the output pins down the folded tree's
// shape and not only its value, which folding leaves unchanged.
function shape(e) {
  switch (e.tag) {
    case VAR:
      return 3 * e.v + 1;
    case VAL:
      return 3 * e.v + 2;
    case ADD:
      return (31 * shape(e.l) + 17 * shape(e.r) + 3) % 1000003;
    default:
      return (37 * shape(e.l) + 19 * shape(e.r) + 5) % 1000003;
  }
}

// The tree is rebuilt every round, from a start value that walks 0 to 7, so some
// trees are mostly Var leaves and some mostly Val. Depth 12 is as deep as it goes
// because the reassociated tree is one chain 4096 long, and LuaJIT's stack ends near 10000.
let before = 0;
let after = 0;
let shapes = 0;
for (let i = 0; i < 700; i++) {
  const e = mkExpr(12, i % 8);
  before += evalExpr(e);
  const f = cfold(reassoc(e));
  after += evalExpr(f);
  shapes += shape(f);
}
console.log(before);
console.log(after);
console.log(shapes);
