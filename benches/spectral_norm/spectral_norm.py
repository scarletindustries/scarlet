# spectral-norm: the power method on an implicit matrix, 10 rounds. It stresses
# float arithmetic over arrays. Ported from the Computer Language Benchmarks
# Game (benchmarksgame-team.pages.debian.net/benchmarksgame, BSD-3 licence).

import math


# Entry (i, j) of the infinite matrix, counting from zero.
def a(i, j):
    return 1.0 / ((i + j) * (i + j + 1) // 2 + i + 1)


def av(x, y, n):
    for i in range(n):
        s = 0.0
        for j in range(n):
            s = s + x[j] * a(i, j)
        y[i] = s


def atv(x, y, n):
    for i in range(n):
        s = 0.0
        for j in range(n):
            s = s + x[j] * a(j, i)
        y[i] = s


def atav(x, y, t, n):
    av(x, t, n)
    atv(t, y, n)


# The same 30 Newton steps in every language, as Scarlet has no square root.
def sqrt(s):
    x = 1.0
    for _ in range(30):
        x = 0.5 * (x + s / x)
    return x


# Prints the norm, scaled to an integer, and then floor(u.v). The second number
# depends on every round, where the norm alone has converged after three.
def spectral_norm(n):
    u = [1.0] * n
    v = [0.0] * n
    t = [0.0] * n
    for _ in range(10):
        atav(u, v, t, n)
        atav(v, u, t, n)
    vbv = 0.0
    vv = 0.0
    for i in range(n):
        ui = u[i]
        vi = v[i]
        vbv = vbv + ui * vi
        vv = vv + vi * vi
    print(math.floor(sqrt(vbv / vv) * 1000000000.0 + 0.5))
    print(math.floor(vbv))


spectral_norm(100)
spectral_norm(700)
