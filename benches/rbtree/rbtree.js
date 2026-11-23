// Stresses pattern matching on nested constructors, allocation, and reuse of
// nodes in place: insert into a functional red-black tree (Okasaki balance), then fold.
// Ported from Koka's test/bench/koka/rbtree.kk (https://github.com/koka-lang/koka, Apache-2.0).

const RED = 0, BLACK = 1

// A node is an object; a leaf is null.
function node(c, l, k, v, r) {
  return { c, l, k, v, r }
}

function isRed(t) {
  return t !== null && t.c === RED
}

function balanceLeft(l, k, v, r) {
  if (l === null) return null
  const ll = l.l, lr = l.r
  if (isRed(ll)) {
    return node(RED, node(BLACK, ll.l, ll.k, ll.v, ll.r), l.k, l.v, node(BLACK, lr, k, v, r))
  } else if (isRed(lr)) {
    return node(RED, node(BLACK, ll, l.k, l.v, lr.l), lr.k, lr.v, node(BLACK, lr.r, k, v, r))
  } else {
    return node(BLACK, node(RED, ll, l.k, l.v, lr), k, v, r)
  }
}

function balanceRight(l, k, v, r) {
  if (r === null) return null
  const rl = r.l, rr = r.r
  if (isRed(rl)) {
    return node(RED, node(BLACK, l, k, v, rl.l), rl.k, rl.v, node(BLACK, rl.r, r.k, r.v, rr))
  } else if (isRed(rr)) {
    return node(RED, node(BLACK, l, k, v, rl), r.k, r.v, node(BLACK, rr.l, rr.k, rr.v, rr.r))
  } else {
    return node(BLACK, l, k, v, node(RED, rl, r.k, r.v, rr))
  }
}

function ins(t, k, v) {
  if (t === null) return node(RED, null, k, v, null)
  const kx = t.k
  if (t.c === RED) {
    if (k < kx) return node(RED, ins(t.l, k, v), kx, t.v, t.r)
    else if (k > kx) return node(RED, t.l, kx, t.v, ins(t.r, k, v))
    else return node(RED, t.l, k, v, t.r)
  } else {
    if (k < kx) {
      if (isRed(t.l)) return balanceLeft(ins(t.l, k, v), kx, t.v, t.r)
      else return node(BLACK, ins(t.l, k, v), kx, t.v, t.r)
    } else if (k > kx) {
      if (isRed(t.r)) return balanceRight(t.l, kx, t.v, ins(t.r, k, v))
      else return node(BLACK, t.l, kx, t.v, ins(t.r, k, v))
    } else {
      return node(BLACK, t.l, k, v, t.r)
    }
  }
}

function setBlack(t) {
  if (t === null) return t
  return node(BLACK, t.l, t.k, t.v, t.r)
}

function insert(t, k, v) {
  return setBlack(ins(t, k, v))
}

function fold(t, b, f) {
  if (t === null) return b
  return fold(t.r, f(t.k, t.v, fold(t.l, b, f)), f)
}

function height(t) {
  if (t === null) return 0
  const hl = height(t.l)
  const hr = height(t.r)
  if (hl > hr) return hl + 1
  else return hr + 1
}

function makeTree(n) {
  let t = null
  while (n > 0) {
    n = n - 1
    t = insert(t, n, n % 10 === 0)
  }
  return t
}

const t = makeTree(300000)
const count = fold(t, 0, (k, v, r) => (v ? r + 1 : r))
console.log(count)
console.log(fold(t, 0, (k, v, r) => (r * 31 + k) % 1000000007))
console.log(height(t))
