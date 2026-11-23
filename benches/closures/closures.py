# closures: map, filter and fold over a 1000-element list with closures that
# capture variables, a composed pipeline of small functions, and a counter
# whose step function returns the next counter. It stresses making closures
# and calling them. Written for this suite, not a port.

from functools import reduce


def adder(k):
    return lambda x: x + k


def scaler(k, m):
    return lambda x: x * k % m


def compose(f, g):
    return lambda x: g(f(x))


def counter(value, step):
    return (value, lambda d: counter((value + step * d) % 1000003, step + 1))


def count(c, n):
    while n != 0:
        c = c[1](n % 5 + 1)
        n -= 1
    return c[0]


def rounds(xs, r, acc):
    while r != 0:
        k = r % 7
        scaled = list(map(lambda x: (x * r + acc) % 1009, xs))
        kept = list(filter(lambda x: x % 7 != k, scaled))
        direct = reduce(lambda s, x: (s + x * r) % 1000003, kept, 0)
        steps = [adder(r), scaler(3, 1013), adder(acc % 13), scaler(k + 2, 1019)]
        pipeline = reduce(compose, steps, lambda x: x)
        piped = reduce(lambda s, x: (s + x) % 1000003, list(map(pipeline, kept)), 0)
        counted = count(counter(acc % 97, r), 200)
        acc = (acc * 31 + direct + piped + counted) % 1000003
        r -= 1
    return acc


print(rounds(list(range(1000)), 1500, 1))
