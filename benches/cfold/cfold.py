# cfold: build an expression tree, reassociate it and fold its constants, many times over.
# It stresses allocating small nodes of a sum type and matching on them, with deep recursion.
# Ported from test/bench/koka/cfold.kk in Koka's benchmarks (Apache-2.0), itself from Lean 4's const_fold.

import sys

# A node is a tuple (tag, a, b): a Var or Val keeps its number in slot 1.
VAR, VAL, ADD, MUL = 1, 2, 3, 4


def mk_expr(n, v):
    if n == 0:
        if v == 0:
            return (VAR, 1)
        return (VAL, v)
    return (ADD, mk_expr(n - 1, v + 1), mk_expr(n - 1, 0 if v == 0 else v - 1))


def append_add(e0, e3):
    if e0[0] == ADD:
        return (ADD, e0[1], append_add(e0[2], e3))
    return (ADD, e0, e3)


def append_mul(e0, e3):
    if e0[0] == MUL:
        return (MUL, e0[1], append_mul(e0[2], e3))
    return (MUL, e0, e3)


def reassoc(e):
    tag = e[0]
    if tag == ADD:
        return append_add(reassoc(e[1]), reassoc(e[2]))
    if tag == MUL:
        return append_mul(reassoc(e[1]), reassoc(e[2]))
    return e


def cfold(e):
    tag = e[0]
    if tag == ADD:
        f1 = cfold(e[1])
        f2 = cfold(e[2])
        if f1[0] == VAL:
            a = f1[1]
            tag2 = f2[0]
            if tag2 == VAL:
                return (VAL, a + f2[1])
            if tag2 == ADD:
                l = f2[1]
                r = f2[2]
                if r[0] == VAL:
                    return (ADD, (VAL, a + r[1]), l)
                if l[0] == VAL:
                    return (ADD, (VAL, a + l[1]), r)
        return (ADD, f1, f2)
    if tag == MUL:
        f1 = cfold(e[1])
        f2 = cfold(e[2])
        if f1[0] == VAL:
            a = f1[1]
            tag2 = f2[0]
            if tag2 == VAL:
                return (VAL, a * f2[1])
            if tag2 == MUL:
                l = f2[1]
                r = f2[2]
                if r[0] == VAL:
                    return (MUL, (VAL, a * r[1]), l)
                if l[0] == VAL:
                    return (MUL, (VAL, a * l[1]), r)
        return (MUL, f1, f2)
    return e


def eval_expr(e):
    tag = e[0]
    if tag == VAR:
        return 0
    if tag == VAL:
        return e[1]
    if tag == ADD:
        return eval_expr(e[1]) + eval_expr(e[2])
    return eval_expr(e[1]) * eval_expr(e[2])


# An order-sensitive hash of the tree, so the output pins down the folded tree's
# shape and not only its value, which folding leaves unchanged.
def shape(e):
    tag = e[0]
    if tag == VAR:
        return 3 * e[1] + 1
    if tag == VAL:
        return 3 * e[1] + 2
    if tag == ADD:
        return (31 * shape(e[1]) + 17 * shape(e[2]) + 3) % 1000003
    return (37 * shape(e[1]) + 19 * shape(e[2]) + 5) % 1000003


# The tree is rebuilt every round, from a start value that walks 0 to 7, so some
# trees are mostly Var leaves and some mostly Val. Depth 12 is as deep as it goes
# because the reassociated tree is one chain 4096 long, and LuaJIT's stack ends near 10000.
def main():
    sys.setrecursionlimit(20000)
    before = 0
    after = 0
    shapes = 0
    for i in range(700):
        e = mk_expr(12, i % 8)
        before += eval_expr(e)
        f = cfold(reassoc(e))
        after += eval_expr(f)
        shapes += shape(f)
    print(before)
    print(after)
    print(shapes)


main()
