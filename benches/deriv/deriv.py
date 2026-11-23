# deriv: differentiate x^x symbolically nine times over, simplifying as it goes, and do that twice.
# It stresses allocating small nodes of a sum type and matching on pairs of them.
# Ported from test/bench/koka/deriv.kk in Koka's benchmarks (Apache-2.0), itself from Lean 4's deriv.ml.
import sys

sys.setrecursionlimit(1000000)

VAL, VAR, ADD, MUL, POW, LN = 0, 1, 2, 3, 4, 5


def pown(a, b):
    acc = 1
    while b > 0:
        acc *= a
        b -= 1
    return acc


def add(n0, m0):
    mt = m0[0]
    nt = n0[0]
    if mt == VAL:
        m = m0[1]
        if nt == VAL:
            return (VAL, n0[1] + m)
        if m == 0:
            return n0
        return add((VAL, m), n0)
    if mt == ADD:
        ml = m0[1]
        if nt == VAL:
            n = n0[1]
            if n == 0:
                return m0
            if ml[0] == VAL:
                return add((VAL, n + ml[1]), m0[2])
            return (ADD, n0, m0)
        if ml[0] == VAL:
            return add((VAL, ml[1]), add(n0, m0[2]))
        if nt == ADD:
            return add(n0[1], add(n0[2], m0))
        return (ADD, n0, m0)
    if nt == VAL:
        if n0[1] == 0:
            return m0
        return (ADD, n0, m0)
    if nt == ADD:
        return add(n0[1], add(n0[2], m0))
    return (ADD, n0, m0)


def mul(n0, m0):
    mt = m0[0]
    nt = n0[0]
    if mt == VAL:
        m = m0[1]
        if nt == VAL:
            return (VAL, n0[1] * m)
        if m == 0:
            return (VAL, 0)
        if m == 1:
            return n0
        return mul((VAL, m), n0)
    if mt == MUL:
        ml = m0[1]
        if nt == VAL:
            n = n0[1]
            if n == 0:
                return (VAL, 0)
            if n == 1:
                return m0
            if ml[0] == VAL:
                return mul((VAL, n * ml[1]), m0[2])
            return (MUL, n0, m0)
        if ml[0] == VAL:
            return mul((VAL, ml[1]), mul(n0, m0[2]))
        if nt == MUL:
            return mul(n0[1], mul(n0[2], m0))
        return (MUL, n0, m0)
    if nt == VAL:
        n = n0[1]
        if n == 0:
            return (VAL, 0)
        if n == 1:
            return m0
        return (MUL, n0, m0)
    if nt == MUL:
        return mul(n0[1], mul(n0[2], m0))
    return (MUL, n0, m0)


def powr(m0, n0):
    if n0[0] == VAL:
        n = n0[1]
        if m0[0] == VAL:
            return (VAL, pown(m0[1], n))
        if n == 0:
            return (VAL, 1)
        if n == 1:
            return m0
        return (POW, m0, n0)
    if m0[0] == VAL and m0[1] == 0:
        return (VAL, 0)
    return (POW, m0, n0)


def ln(n):
    if n[0] == VAL and n[1] == 1:
        return (VAL, 0)
    return (LN, n)


def d(x, e):
    t = e[0]
    if t == VAL:
        return (VAL, 0)
    if t == VAR:
        return (VAL, 1) if x == e[1] else (VAL, 0)
    if t == ADD:
        return add(d(x, e[1]), d(x, e[2]))
    if t == MUL:
        f = e[1]
        g = e[2]
        return add(mul(f, d(x, g)), mul(g, d(x, f)))
    if t == POW:
        f = e[1]
        g = e[2]
        return mul(
            powr(f, g),
            add(mul(mul(g, d(x, f)), powr(f, (VAL, -1))), mul(ln(f), d(x, g))),
        )
    f = e[1]
    return mul(d(x, f), powr(f, (VAL, -1)))


def count(e):
    t = e[0]
    if t == VAL or t == VAR:
        return 1
    if t == LN:
        return count(e[1])
    return count(e[1]) + count(e[2])


def nest(n, f):
    for i in range(n):
        f = d("x", f)
        print(f"{i + 1} count: {count(f)}")
    return f


def main():
    x = (VAR, "x")
    nest(9, powr(x, x))
    nest(9, powr(x, x))
    print("done")


main()
